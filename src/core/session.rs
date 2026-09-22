//! Main-thread policy, without devices or threads. The bridge owns paragraph order.
use crate::core::{
    decode::{Commit, Preview, Utterance, UtteranceId},
    frames::Frames,
    state::{self, Cause, Command, DiscardReason, Event, MAX_CAPTURE, State},
};
use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// What became of a capture's recovery WAV. Lives beside [`Notice`], the only
/// place the core cares about it: the shell's `CaptureRecorder` is the sole
/// producer, reporting the outcome of I/O the core never performs itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingStatus {
    Recorded(PathBuf),
    Truncated(PathBuf),
    NotRecorded,
}

/// Something the user has to know about the capture they just made. Exactly
/// one is shown at a time, in the editor's winbar and in every phase, until
/// the next key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    HeldTooBriefly,
    MicrophoneGap,
    MicrophoneUnavailable,
    CaptureIncomplete,
    NearlySilent,
    PreviewPaused,
    MemoryCap(RecordingStatus),
    /// A latched capture heard no speech for `capture.silence_timeout_s` and
    /// ended itself.
    SilenceTimeout,
    /// A capture ran for [`MAX_CAPTURE`] and ended itself.
    LengthLimit,
    /// The speech model was not ready, so the capture was to be kept on disk
    /// until it is, and no recording was written.
    NotKept,
    /// A recording kept until the model was ready could not be read back to
    /// transcribe it: deleted, or damaged.
    RecordingLost(PathBuf),
    /// The same, after its text through `through` was written: the rest is
    /// recovered with `spokenpad transcribe --from`.
    RecordingPartlyTranscribed {
        path: PathBuf,
        through: Duration,
    },
    /// A capture kept on disk while the model was not ready ran to the
    /// in-memory ceiling a live capture has, `minutes` long, and ended.
    KeptTooLong {
        minutes: u64,
    },
    /// The config file did not load when this window opened, for this
    /// reason; the window has the settings that were in use.
    ConfigInvalid(String),
    /// The rest are not about a capture but about the speech model, and come
    /// from [`Recognition::notice`]; `waiting` counts the recordings made
    /// while it was not ready, which it transcribes once it is.
    ModelDownloading {
        percent: u8,
        waiting: usize,
    },
    ModelLoading {
        waiting: usize,
    },
    ModelUnavailable {
        reason: String,
        waiting: usize,
    },
    TranscribingRecordings(usize),
}

/// How far the daemon is from transcribing, as the inference thread reports
/// it while it downloads and loads the speech model. The daemon accepts
/// presses from its first moment: until this is `Ready`, what they record is
/// kept on disk and transcribed afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recognition {
    Preparing(Preparing),
    Ready,
    /// Neither the download nor the load worked, for this reason. The next
    /// press tries again.
    Unavailable(String),
}

/// The two steps before the speech model is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preparing {
    /// The default model files are downloading: `done` of `total` bytes.
    Downloading { done: u64, total: u64 },
    /// The files are in place and the recognizer is being built.
    Loading,
}

impl Recognition {
    /// What the dictation window says about the speech model, given
    /// `waiting` recordings it has yet to transcribe. Nothing once it is
    /// ready and has caught up.
    pub fn notice(&self, waiting: usize) -> Option<Notice> {
        match self {
            Self::Preparing(Preparing::Downloading { done, total }) => {
                Some(Notice::ModelDownloading {
                    percent: (done.saturating_mul(100) / (*total).max(1)).min(100) as u8,
                    waiting,
                })
            }
            Self::Preparing(Preparing::Loading) => Some(Notice::ModelLoading { waiting }),
            Self::Ready => (waiting > 0).then_some(Notice::TranscribingRecordings(waiting)),
            Self::Unavailable(reason) => Some(Notice::ModelUnavailable {
                reason: reason.clone(),
                waiting,
            }),
        }
    }
}

/// "3 recordings", "1 recording".
fn recordings(count: usize) -> String {
    match count {
        1 => "1 recording".into(),
        n => format!("{n} recordings"),
    }
}

/// What happens to what is dictated before the model is ready.
fn kept(waiting: usize) -> String {
    let promise = "dictation is recorded and transcribed once it is ready";
    match waiting {
        0 => promise.into(),
        n => format!("{promise}; {} waiting", recordings(n)),
    }
}

/// A notice as the winbar draws it: a headline short enough to stand beside the
/// phase label in a narrow window, and the detail that explains it, which the
/// editor appends only when the window has room for all of it. The split is
/// made here rather than in Lua so that the wording, and the decision about
/// what a narrow window may lose, live in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeText {
    pub headline: &'static str,
    pub detail: Cow<'static, str>,
}

/// The recovery WAV as the notice names it. A path with no final component is
/// not something the recorder can have written, so falling back to the whole
/// path is more honest than an empty name.
fn file_name(path: &Path) -> Cow<'_, str> {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
}

impl Notice {
    /// How much this notice matters. Exactly one is shown per capture, so when
    /// two things happen to the same one this ranking decides which the user
    /// reads; [`Session::notify`] is the only place it is applied.
    ///
    /// Highest first, and this list is the definition. News about audio that
    /// was lost outranks news about a capture that ended cleanly: the two
    /// auto-stops below lost nothing, and everything spoken is in the editor.
    ///
    /// The speech model's own notices rank in the same list, because the
    /// window shows one notice: the capture's, unless the model's outranks
    /// it ([`Session::shown_notice`]). A model that cannot load at all is
    /// worse news than a gap in one capture; one that is on its way is less
    /// than anything that went wrong with the capture just made.
    ///
    /// 1. `MemoryCap` — the capture is over and the audio is only in a WAV.
    /// 2. `CaptureIncomplete` — most of what was said never arrived.
    /// 3. `MicrophoneUnavailable` — audio is missing and did not come back.
    /// 4. `NotKept` — the whole capture is gone: no model, no recording.
    /// 5. `RecordingLost` — a whole capture is gone after all.
    /// 6. `RecordingPartlyTranscribed` — the rest of one is, until recovered.
    /// 7. `ModelUnavailable` — nothing is transcribed until it loads.
    /// 8. `MicrophoneGap` — audio came back, with a hole in the recording.
    /// 9. `LengthLimit` — the capture ran to the length limit and ended.
    /// 10. `KeptTooLong` — the same, at the ceiling of a capture kept on disk.
    /// 11. `SilenceTimeout` — a latch was quiet long enough to end.
    /// 12. `ConfigInvalid` — an edit to the config did not take.
    /// 13. `NearlySilent` — everything arrived and may still be worth nothing.
    /// 14. `HeldTooBriefly` — nothing was recorded, and nothing was lost.
    /// 15. `ModelDownloading` — transcription waits, and nothing is lost.
    /// 16. `ModelLoading` — the same, for seconds.
    /// 17. `TranscribingRecordings` — the wait is over; text is on its way.
    /// 18. `PreviewPaused` — cosmetic: only the live tail stopped.
    pub fn priority(&self) -> u8 {
        match self {
            Self::MemoryCap(_) => 17,
            Self::CaptureIncomplete => 16,
            Self::MicrophoneUnavailable => 15,
            Self::NotKept => 14,
            Self::RecordingLost(_) => 13,
            Self::RecordingPartlyTranscribed { .. } => 12,
            Self::ModelUnavailable { .. } => 11,
            Self::MicrophoneGap => 10,
            Self::LengthLimit => 9,
            Self::KeptTooLong { .. } => 8,
            Self::SilenceTimeout => 7,
            Self::ConfigInvalid(_) => 6,
            Self::NearlySilent => 5,
            Self::HeldTooBriefly => 4,
            Self::ModelDownloading { .. } => 3,
            Self::ModelLoading { .. } => 2,
            Self::TranscribingRecordings(_) => 1,
            Self::PreviewPaused => 0,
        }
    }
    /// The short form, drawn in every window however narrow. Fixed wording per
    /// variant: it is the part the user learns to recognise at a glance.
    pub fn headline(&self) -> &'static str {
        match self {
            Self::HeldTooBriefly => "held too briefly",
            Self::MicrophoneGap => "microphone gap",
            Self::MicrophoneUnavailable => "microphone unavailable",
            Self::CaptureIncomplete => "capture incomplete",
            Self::NearlySilent => "nearly silent",
            Self::PreviewPaused => "preview paused",
            Self::MemoryCap(_) => "memory limit reached",
            Self::SilenceTimeout => "stopped after silence",
            Self::LengthLimit => "reached the time limit",
            Self::NotKept => "capture not kept",
            Self::RecordingLost(_) => "recording lost",
            Self::RecordingPartlyTranscribed { .. } => "recording partly transcribed",
            Self::KeptTooLong { .. } => "reached the time limit",
            Self::ConfigInvalid(_) => "config not reloaded",
            Self::ModelDownloading { .. } => "downloading the speech model",
            Self::ModelLoading { .. } => "loading the speech model",
            Self::ModelUnavailable { .. } => "no speech model",
            Self::TranscribingRecordings(_) => "transcribing recordings",
        }
    }
    /// Why it happened, or what to do about it. The memory-cap detail names the
    /// recovery WAV by file name alone: the winbar has a window's width rather
    /// than a terminal's, and the daemon logs the directory at the same moment.
    pub fn detail(&self) -> Cow<'static, str> {
        match self {
            Self::HeldTooBriefly => "hold the key while speaking".into(),
            Self::MicrophoneGap => {
                "the microphone stopped delivering audio; it was reopened, but this recording has a gap"
                    .into()
            }
            Self::MicrophoneUnavailable => "audio is missing".into(),
            Self::CaptureIncomplete => {
                "the microphone delivered less than half the expected audio".into()
            }
            Self::NearlySilent => "check microphone gain and device".into(),
            Self::PreviewPaused => "long uncommitted tail".into(),
            Self::MemoryCap(RecordingStatus::Recorded(path)) => format!(
                "past 60 minutes; the capture ended here and is in {} — recover with spokenpad transcribe",
                file_name(path)
            )
            .into(),
            Self::MemoryCap(RecordingStatus::Truncated(_)) => {
                "past 60 minutes AND the recording failed; press the key to start a new capture"
                    .into()
            }
            Self::MemoryCap(RecordingStatus::NotRecorded) => {
                "past 60 minutes with no recovery recording; press the key to start a new capture"
                    .into()
            }
            Self::SilenceTimeout => {
                "no speech for capture.silence_timeout_s; press the key to dictate again".into()
            }
            Self::LengthLimit => format!(
                "a capture ends after {} hours; press the key to dictate again",
                MAX_CAPTURE.as_secs() / 3600
            )
            .into(),
            Self::NotKept => {
                "the speech model was not ready and no recording was written (recording.enabled is off, or the disk failed)"
                    .into()
            }
            Self::RecordingLost(path) => format!(
                "{} could not be read back to transcribe it",
                file_name(path)
            )
            .into(),
            Self::RecordingPartlyTranscribed { path, through } => format!(
                "{0} failed after {1:.2}s; recover the rest with spokenpad transcribe --from {1:.2}",
                file_name(path),
                through.as_secs_f64()
            )
            .into(),
            Self::KeptTooLong { minutes } => format!(
                "a capture made before the speech model is ready ends after {minutes} minutes; it is transcribed once the model is; press the key to dictate again"
            )
            .into(),
            Self::ConfigInvalid(reason) => {
                format!("{reason}; this window keeps the settings in use").into()
            }
            Self::ModelDownloading { percent, waiting } => {
                format!("{percent}%; {}", kept(*waiting)).into()
            }
            Self::ModelLoading { waiting } => kept(*waiting).into(),
            Self::ModelUnavailable { reason, waiting } => {
                let kept = match waiting {
                    0 => String::new(),
                    n => format!("{} kept; ", recordings(*n)),
                };
                format!("{reason}; {kept}press the key to try again").into()
            }
            Self::TranscribingRecordings(waiting) => format!(
                "{} made while the speech model was not ready",
                recordings(*waiting)
            )
            .into(),
        }
    }
    /// Both halves, as the indicator carries them to the editor.
    pub fn text(&self) -> NoticeText {
        NoticeText {
            headline: self.headline(),
            detail: self.detail(),
        }
    }
}

impl fmt::Display for Notice {
    /// The whole notice on one line, which is what a log line has room for.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} — {}", self.headline(), self.detail())
    }
}

/// What the preview is doing. `Paused` only stops the cosmetic tail: ticks
/// keep running, so settled chunks still commit and the tail still shrinks.
/// `Capped` stops the ticks themselves, because past the in-memory ceiling
/// there is nothing left to commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Previews {
    Running,
    Paused,
    Capped,
}

pub struct Session {
    pub state: State,
    pub current: Option<Arc<Utterance>>,
    pub committed_hint: Frames,
    preview: String,
    notice: Option<Notice>,
    preview_epoch: Frames,
    next_id: u64,
    due: Option<Instant>,
    pending: Option<(UtteranceId, Instant)>,
    interval: Duration,
    enabled: bool,
    silence: Option<Duration>,
    /// Previews and silence timeout waiting for the capture that is recording
    /// to end: see [`Session::configure`].
    configured: Option<(bool, Option<Duration>)>,
    previews: Previews,
    warned_tick_failure: bool,
}
impl Session {
    /// `silence` is how long a latched capture may hear no speech before it
    /// ends itself. It belongs to the caller that knows whether anything can
    /// report speech at all: with no VAD model, or with the progressive tick
    /// off, the recognizer runs only at the release and there is no silence
    /// to measure, so it is `None`.
    pub fn new(enabled: bool, interval: Duration, silence: Option<Duration>) -> Self {
        Self {
            silence,
            state: State::Idle,
            current: None,
            committed_hint: Frames::ZERO,
            preview: String::new(),
            notice: None,
            preview_epoch: Frames::ZERO,
            next_id: 0,
            due: None,
            pending: None,
            interval,
            enabled,
            configured: None,
            previews: Previews::Running,
            warned_tick_failure: false,
        }
    }
    /// Replaces what [`Session::new`] was given, once the speech model has
    /// loaded and it is known whether a segmenter came with it. A capture
    /// that is recording keeps what it began with -- one made before the
    /// model was ready has no ticks, and so no speech to reset a silence
    /// timeout -- and the change takes effect once it ends.
    pub fn configure(&mut self, enabled: bool, silence: Option<Duration>) {
        self.configured = Some((enabled, silence));
        self.apply_configuration();
    }
    fn apply_configuration(&mut self) {
        if !self.state.recording()
            && let Some((enabled, silence)) = self.configured.take()
        {
            self.enabled = enabled;
            self.silence = silence;
        }
    }
    /// Ends a capture that is kept on disk only, because the speech model is
    /// not ready, at the in-memory ceiling a live capture has: it is the
    /// same bound on one capture, applied by length because such a capture
    /// holds nothing in memory. It is transcribed from its recording.
    pub fn cap_kept(&mut self, minutes: u64, now: Instant) -> Command {
        let command = self.event(Event::Exhausted { at: now });
        self.notify(Notice::KeptTooLong { minutes });
        command
    }
    /// The notice the window shows: this capture's own, unless `status`, the
    /// speech model's ([`Recognition::notice`]), outranks it.
    pub fn shown_notice(&self, status: Option<Notice>) -> Option<Notice> {
        match (&self.notice, status) {
            (Some(own), Some(status)) if status.priority() > own.priority() => Some(status),
            (Some(own), _) => Some(own.clone()),
            (None, status) => status,
        }
    }
    pub fn event(&mut self, event: Event) -> Command {
        self.apply_configuration();
        let (state, command) = state::step(self.state, event, self.silence);
        self.state = state;
        match command {
            Command::Start => {
                self.next_id += 1;
                self.current = Some(Utterance::new(self.next_id));
                self.committed_hint = Frames::ZERO;
                self.preview_epoch = Frames::ZERO;
                self.preview.clear();
                self.notice = None;
                self.previews = Previews::Running;
                self.warned_tick_failure = false;
                self.pending = None;
                self.due = self.enabled.then(|| Instant::now() + self.interval);
            }
            Command::Decode { cause, .. } => {
                if let Some(u) = &self.current {
                    u.release();
                }
                self.due = None;
                match cause {
                    // Nothing to say: the user's own key, or the ceiling,
                    // whose notice [`Session::cap`] raises with the file
                    // name that only it knows.
                    Cause::KeyPress | Cause::Memory => {}
                    Cause::Silence => self.notify(Notice::SilenceTimeout),
                    Cause::Length => self.notify(Notice::LengthLimit),
                }
            }
            Command::Discard(reason) => {
                if let Some(u) = &self.current {
                    u.cancel();
                }
                self.due = None;
                self.preview.clear();
                if reason == DiscardReason::TooShort {
                    self.notify(Notice::HeldTooBriefly);
                }
            }
            Command::Nothing => {}
        }
        command
    }
    pub fn is_current(&self, id: UtteranceId) -> bool {
        self.current.as_ref().is_some_and(|u| u.id == id)
    }
    /// An utterance id no capture of this session has or will have, for a
    /// recording a previous daemon left untranscribed.
    pub fn reserve_id(&mut self) -> UtteranceId {
        self.next_id += 1;
        UtteranceId(self.next_id)
    }
    /// Records that text has landed. Cancelling stops *decoding*; it never
    /// unwrites text the recognizer already produced, so there is no reject
    /// path here — only the live capture's hints move.
    pub fn note_commit(&mut self, c: &Commit, now: Instant) {
        if !self.is_current(c.utterance.id) {
            return;
        }
        self.heard(&c.text, now);
        self.committed_hint = self.committed_hint.max(c.through);
        if c.through > self.preview_epoch {
            self.preview_epoch = c.through;
            self.preview.clear();
        }
    }
    pub fn finish(&mut self, id: UtteranceId) {
        if !self.is_current(id) {
            return;
        }
        self.event(Event::Finished);
        // The notice outlives the decode: it explains what the user is about
        // to read, and only the next key press clears it.
        self.preview.clear();
    }
    pub fn tick_due(&self, now: Instant) -> bool {
        self.state.recording()
            && self.enabled
            && self.previews != Previews::Capped
            && self.pending.is_none()
            && self.due.is_some_and(|d| now >= d)
    }
    pub fn requested(&mut self, now: Instant) {
        if let Some(u) = &self.current {
            self.pending = Some((u.id, now));
            self.due = None;
        }
    }
    pub fn tick_finished(&mut self, id: UtteranceId, preview: Option<Preview>, now: Instant) {
        if !self.is_current(id) || !self.state.recording() {
            return;
        }
        let elapsed = self
            .pending
            .filter(|(i, _)| *i == id)
            .map(|(_, at)| now.saturating_duration_since(at))
            .unwrap_or_default();
        self.pending = None;
        // Re-armed even while the cosmetic tail is paused: those ticks are
        // what commit the settled chunks the pause is waiting for.
        if self.previews != Previews::Capped {
            self.due = Some(now + self.interval.saturating_sub(elapsed).max(elapsed));
        }
        let Some(Preview { text, through }) = preview else {
            return;
        };
        self.heard(&text, now);
        if through < self.committed_hint {
            return;
        }
        if through > self.preview_epoch {
            self.preview_epoch = through;
            self.preview.clear();
        }
        // A tail that comes back shorter than the last one is the recognizer
        // losing context, not the user unsaying words.
        if text.chars().count() >= self.preview.chars().count() {
            self.preview = text;
        }
    }
    /// The open tail is too long to decode cosmetically. The tick still runs
    /// and still commits, so the tail settles and previews resume by
    /// themselves.
    pub fn defer_previews(&mut self) {
        if self.previews == Previews::Capped {
            return;
        }
        self.previews = Previews::Paused;
        // The lowest-ranked notice there is: a paused preview must never push
        // aside the reason the capture itself is in trouble.
        self.notify(Notice::PreviewPaused);
    }
    pub fn resume_previews(&mut self) {
        if self.previews != Previews::Paused {
            return;
        }
        self.previews = Previews::Running;
        if self.notice == Some(Notice::PreviewPaused) {
            self.notice = None;
        }
    }
    /// Text the recognizer produced is the only proof that the user is still
    /// talking, wherever it came from: a settled commit or a live preview.
    /// Empty text is what settled silence and a silent preview look like, so
    /// it proves nothing and is not reported.
    ///
    /// `now` is when the *result arrived*, not when the audio it describes
    /// was spoken, and that is deliberate. The two differ by however long the
    /// worker took, so stamping by audio position would be the more accurate
    /// number and the more dangerous one: a worker that falls behind by more
    /// than the timeout would leave `last_speech` permanently in the past and
    /// end a capture the user is still talking into, because the words had
    /// not been decoded yet. Stamping on arrival can only ever delay the
    /// stop, by at most one decode. The rule is "no text has come back for
    /// this long", which is what the user can observe in the winbar.
    ///
    /// No command can follow [`Event::Speech`] (see [`state::step`]), which
    /// is why this returns nothing.
    fn heard(&mut self, text: &str, now: Instant) {
        if !text.trim().is_empty() {
            self.event(Event::Speech { at: now });
        }
    }

    /// The in-memory ceiling ends this capture: nothing more can be decoded,
    /// so it is decoded now and the recorder stops with it. Returns the
    /// command the shell has to carry out, like [`Session::event`] does.
    pub fn cap(&mut self, recovery: RecordingStatus, now: Instant) -> Command {
        self.previews = Previews::Capped;
        let command = self.event(Event::Exhausted { at: now });
        self.due = None;
        self.notify(Notice::MemoryCap(recovery));
        command
    }
    /// Raises `notice`, unless what is already shown matters more. Ties go to
    /// the newer one: the same kind of trouble, reported again, is the more
    /// recent fact about this capture. A key press is the only other way the
    /// slot changes, and it always clears it.
    pub fn notify(&mut self, notice: Notice) {
        if self
            .notice
            .as_ref()
            .is_none_or(|shown| notice.priority() >= shown.priority())
        {
            self.notice = Some(notice);
        }
    }
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }
    /// True once per capture, so a failing preview is logged but not spammed.
    pub fn should_warn_tick_failure(&mut self) -> bool {
        !std::mem::replace(&mut self.warned_tick_failure, true)
    }
    /// Whether previews are running: false once they are paused for a long
    /// uncommitted tail or for the memory cap.
    pub fn previewing(&self) -> bool {
        self.enabled && self.previews == Previews::Running
    }
    /// The live tail the indicator shows below the committed text. Never the
    /// notice: the two are rendered in different places and one must not hide
    /// the other.
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::control::{Received, Request};
    fn request(s: &mut Session, request: Request, at: Instant) -> Command {
        s.event(Event::Request(Received { request, at }))
    }
    fn start(s: &mut Session) {
        request(s, Request::Start, Instant::now());
    }
    fn cancel(s: &mut Session) {
        request(s, Request::Cancel, Instant::now());
    }
    /// A `stop` at `at`, and the clock closing its repeat window.
    fn release(s: &mut Session, at: Instant) -> Command {
        request(s, Request::Stop, at);
        s.event(Event::Clock {
            now: at + state::REPEAT_WINDOW + Duration::from_millis(1),
        })
    }
    /// A latch: the capture the silence timeout applies to.
    fn latch(s: &mut Session, at: Instant) {
        request(s, Request::Toggle, at);
    }
    fn commit(s: &mut Session, text: &str, now: Instant) {
        let utterance = s.current.clone().expect("a capture to commit into");
        s.note_commit(
            &Commit {
                utterance,
                text: text.into(),
                through: Frames(0),
            },
            now,
        );
    }
    fn preview(text: &str, through: usize) -> Option<Preview> {
        Some(Preview {
            text: text.into(),
            through: Frames(through),
        })
    }
    #[test]
    fn first_recording_schedules_and_accepts_preview_before_any_commit() {
        let interval = Duration::from_millis(1100);
        let mut s = Session::new(true, interval, None);
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        let due = s.due.expect("first recording must arm the preview timer");
        assert!(!s.tick_due(due - Duration::from_millis(1)));
        assert!(s.tick_due(due));
        s.requested(due);
        assert!(!s.tick_due(due + interval));
        s.tick_finished(
            id,
            preview("first live preview", 0),
            due + Duration::from_millis(100),
        );
        assert_eq!(s.committed_hint, Frames::ZERO);
        assert_eq!(s.preview(), "first live preview");
        assert!(s.previewing());
        assert!(s.tick_due(due + interval));
    }
    #[test]
    fn stale_finished_does_not_end_new_recording() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let old = s.current.clone().unwrap();
        release(&mut s, Instant::now() + Duration::from_secs(1));
        start(&mut s);
        s.finish(old.id);
        assert!(s.state.recording());
        s.note_commit(
            &Commit {
                utterance: old,
                text: "older".into(),
                through: Frames(999),
            },
            Instant::now(),
        );
        assert_eq!(s.committed_hint, Frames::ZERO);
    }
    #[test]
    fn a_cancelled_utterance_still_keeps_the_text_it_already_produced() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let u = s.current.clone().unwrap();
        cancel(&mut s);
        assert!(u.cancelled(), "cancelling stops further decoding");
        // The commit was produced before the cancel became visible; the hint
        // still moves, because this is the utterance the session owns.
        s.note_commit(
            &Commit {
                utterance: Arc::clone(&u),
                text: "already spoken".into(),
                through: Frames(4),
            },
            Instant::now(),
        );
        assert_eq!(s.committed_hint, Frames(4));
    }
    #[test]
    fn shrink_guard_resets_only_on_commit_epoch() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        for (text, through, expected) in [
            ("many words", 0, "many words"),
            ("short", 0, "many words"),
            ("new", 4, "new"),
        ] {
            s.tick_finished(id, preview(text, through), Instant::now());
            assert_eq!(s.preview(), expected);
        }
    }
    #[test]
    fn a_notice_stands_beside_the_preview_without_stopping_it() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        s.notify(Notice::MicrophoneGap);
        s.tick_finished(id, preview("still decoding", 0), Instant::now());
        assert_eq!(s.notice(), Some(&Notice::MicrophoneGap));
        assert_eq!(
            s.preview(),
            "still decoding",
            "the notice is rendered elsewhere; it must not swallow the live tail"
        );
        assert!(
            s.tick_due(Instant::now() + Duration::from_secs(2)),
            "a notice must not silently stop preview work"
        );
        cancel(&mut s);
        assert!(
            s.notice().is_some(),
            "a cancel does not hide what went wrong"
        );
        start(&mut s);
        assert!(s.notice().is_none(), "the next press clears the notice");
    }
    #[test]
    fn a_too_short_tap_tells_the_user_why_nothing_appeared() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        let at = Instant::now();
        request(&mut s, Request::Start, at);
        let command = release(&mut s, at + Duration::from_millis(50));
        assert_eq!(command, Command::Discard(DiscardReason::TooShort));
        assert_eq!(s.notice(), Some(&Notice::HeldTooBriefly));
        assert_eq!(s.preview(), "", "a discarded capture has no live tail");
        assert_eq!(s.state, State::Idle);
    }
    #[test]
    fn paused_previews_keep_ticking_and_resume_by_themselves() {
        let mut s = Session::new(true, Duration::from_millis(200), None);
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        let now = Instant::now();
        s.defer_previews();
        assert!(!s.previewing());
        assert_eq!(s.notice(), Some(&Notice::PreviewPaused));
        s.requested(now);
        s.tick_finished(id, None, now);
        assert!(
            s.tick_due(now + Duration::from_millis(200)),
            "a paused preview still commits: the tick has to be re-armed"
        );
        s.resume_previews();
        assert!(s.previewing());
        assert!(s.notice().is_none());
    }
    #[test]
    fn cap_ends_the_recording_and_survives_a_late_preview() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let u = s.current.clone().unwrap();
        let id = u.id;
        let command = s.cap(RecordingStatus::NotRecorded, Instant::now());
        // The shell carries this out: it is what stops the recorder. Marking
        // the utterance released is not enough -- the capture kept running.
        assert!(matches!(
            command,
            Command::Decode {
                cause: Cause::Memory,
                ..
            }
        ));
        assert!(!s.state.recording(), "the capture itself has to end");
        assert!(!u.ticking(), "the capture is released for its final decode");
        s.defer_previews();
        assert!(
            !s.tick_due(Instant::now() + Duration::from_secs(30)),
            "a paused preview cannot restart a capped capture"
        );
        s.tick_finished(id, preview("much longer cosmetic text", 0), Instant::now());
        s.tick_finished(id, None, Instant::now());
        assert_eq!(
            s.notice(),
            Some(&Notice::MemoryCap(RecordingStatus::NotRecorded))
        );
        assert!(!s.previewing());
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(30)));
    }
    /// The ranking is what makes "exactly one notice" a decision rather than a
    /// race between whichever code path spoke last.
    #[test]
    fn the_notice_ranking_is_strict_and_total() {
        let ranked = [
            Notice::MemoryCap(RecordingStatus::NotRecorded),
            Notice::CaptureIncomplete,
            Notice::MicrophoneUnavailable,
            Notice::NotKept,
            Notice::RecordingLost(PathBuf::new()),
            Notice::RecordingPartlyTranscribed {
                path: PathBuf::new(),
                through: Duration::ZERO,
            },
            Notice::ModelUnavailable {
                reason: String::new(),
                waiting: 0,
            },
            Notice::MicrophoneGap,
            Notice::LengthLimit,
            Notice::KeptTooLong { minutes: 60 },
            Notice::SilenceTimeout,
            Notice::ConfigInvalid(String::new()),
            Notice::NearlySilent,
            Notice::HeldTooBriefly,
            Notice::ModelDownloading {
                percent: 0,
                waiting: 0,
            },
            Notice::ModelLoading { waiting: 0 },
            Notice::TranscribingRecordings(1),
            Notice::PreviewPaused,
        ];
        for pair in ranked.windows(2) {
            assert!(
                pair[0].priority() > pair[1].priority(),
                "{:?} must outrank {:?}",
                pair[0],
                pair[1]
            );
        }
    }
    /// The window shows one notice: the capture's own, unless the speech
    /// model's outranks it. A model on its way never hides what went wrong
    /// with the capture; a model that cannot load hides a nearly silent one.
    #[test]
    fn the_model_status_shows_only_when_it_outranks_the_captures_notice() {
        let mut s = Session::new(false, Duration::from_secs(1), None);
        let loading = Recognition::Preparing(Preparing::Loading).notice(0);
        assert_eq!(
            s.shown_notice(loading.clone()),
            loading,
            "no notice of its own"
        );
        s.notify(Notice::MicrophoneGap);
        assert_eq!(s.shown_notice(loading), Some(Notice::MicrophoneGap));
        let broken = Recognition::Unavailable("offline".into()).notice(2);
        assert_eq!(s.shown_notice(broken.clone()), broken);
        assert_eq!(s.shown_notice(None), Some(Notice::MicrophoneGap));
    }

    #[test]
    fn a_ready_model_says_nothing_once_it_has_caught_up() {
        assert_eq!(Recognition::Ready.notice(0), None);
        assert_eq!(
            Recognition::Ready.notice(3),
            Some(Notice::TranscribingRecordings(3))
        );
        let downloading = Recognition::Preparing(Preparing::Downloading {
            done: 335,
            total: 670,
        })
        .notice(1)
        .unwrap();
        assert_eq!(
            downloading.detail(),
            "50%; dictation is recorded and transcribed once it is ready; 1 recording waiting"
        );
        let unavailable = Recognition::Unavailable("offline".into())
            .notice(0)
            .unwrap();
        assert_eq!(unavailable.detail(), "offline; press the key to try again");
    }

    /// A capture kept on disk ends at the ceiling like a live one: decoded
    /// (from its recording), with a notice saying why, and never twice.
    #[test]
    fn a_kept_capture_ends_at_the_ceiling_with_its_own_notice() {
        let mut s = Session::new(false, Duration::from_millis(200), None);
        let begun = Instant::now();
        latch(&mut s, begun);
        let command = s.cap_kept(60, begun + Duration::from_secs(3600));
        assert!(
            matches!(
                command,
                Command::Decode {
                    cause: Cause::Memory,
                    ..
                }
            ),
            "{command:?}"
        );
        assert_eq!(s.notice(), Some(&Notice::KeptTooLong { minutes: 60 }));
        assert!(!s.state.recording());
    }

    /// A capture made before the model loaded has no ticks, so it must not
    /// gain a silence timeout, or previews, halfway through: the new settings
    /// wait for it to end.
    #[test]
    fn a_new_configuration_waits_for_the_capture_that_is_recording() {
        let silence = Some(Duration::from_secs(5));
        let mut s = Session::new(false, Duration::from_millis(200), None);
        let begun = Instant::now();
        latch(&mut s, begun);
        s.configure(true, silence);
        let later = begun + Duration::from_secs(60);
        assert!(!s.tick_due(later), "the running capture stays tick-less");
        assert_eq!(
            s.event(Event::Clock { now: later }),
            Command::Nothing,
            "and has no silence timeout"
        );
        latch(&mut s, later);
        assert!(!s.state.recording(), "the second press ended the latch");
        start(&mut s);
        assert!(
            s.tick_due(Instant::now() + Duration::from_secs(1)),
            "the next capture ticks"
        );
    }

    /// A preview that stopped is cosmetic; a microphone that dropped audio is
    /// not. The pause still happens -- it just does not take the winbar.
    #[test]
    fn a_paused_preview_never_hides_what_went_wrong_with_the_capture() {
        let mut s = Session::new(true, Duration::from_millis(200), None);
        start(&mut s);
        s.notify(Notice::MicrophoneGap);
        s.defer_previews();
        assert!(!s.previewing(), "the pause itself still took effect");
        assert_eq!(s.notice(), Some(&Notice::MicrophoneGap));
        s.resume_previews();
        assert_eq!(
            s.notice(),
            Some(&Notice::MicrophoneGap),
            "resuming previews must not clear someone else's notice"
        );
    }
    /// Everything that happens after the ceiling is a consequence of it: the
    /// capture is over, and the user needs the sentence that says where the
    /// audio went, not the microphone trouble that follows from the shutdown.
    #[test]
    fn the_memory_cap_notice_outlives_every_later_complaint() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let capped = Notice::MemoryCap(RecordingStatus::Recorded("/tmp/capture.wav".into()));
        s.cap(
            RecordingStatus::Recorded("/tmp/capture.wav".into()),
            Instant::now(),
        );
        for later in [
            Notice::MicrophoneGap,
            Notice::MicrophoneUnavailable,
            Notice::CaptureIncomplete,
            Notice::NearlySilent,
            Notice::HeldTooBriefly,
            Notice::PreviewPaused,
            Notice::SilenceTimeout,
            Notice::LengthLimit,
        ] {
            s.notify(later.clone());
            assert_eq!(s.notice(), Some(&capped), "{later:?} displaced the ceiling");
        }
        // Through the release and its decode, and only then the next press.
        release(&mut s, Instant::now() + Duration::from_secs(1));
        assert_eq!(s.notice(), Some(&capped), "the release cleared it");
        start(&mut s);
        assert!(s.notice().is_none(), "the next press clears it");
    }
    /// Worse news replaces lesser news, and the same news replaces itself: a
    /// second report of the same trouble is the more recent one.
    #[test]
    fn a_worse_notice_replaces_a_lesser_one_and_never_the_reverse() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        s.notify(Notice::NearlySilent);
        s.notify(Notice::CaptureIncomplete);
        assert_eq!(s.notice(), Some(&Notice::CaptureIncomplete));
        s.notify(Notice::MicrophoneGap);
        assert_eq!(
            s.notice(),
            Some(&Notice::CaptureIncomplete),
            "a lesser notice must not overwrite a worse one"
        );
        s.notify(Notice::CaptureIncomplete);
        assert_eq!(s.notice(), Some(&Notice::CaptureIncomplete));
    }
    /// The winbar draws the headline always and the detail only when it fits,
    /// so the headline has to be short and the path in it a file name.
    #[test]
    fn a_notice_splits_into_a_short_headline_and_its_detail() {
        let notice = Notice::MemoryCap(RecordingStatus::Recorded(
            "/tmp/spokenpad/capture-example.wav".into(),
        ));
        let text = notice.text();
        assert_eq!(text.headline, "memory limit reached");
        assert!(
            text.headline.chars().count() <= 24,
            "a headline has to fit beside the phase label: {}",
            text.headline
        );
        assert!(
            text.detail.contains("capture-example.wav") && !text.detail.contains("/tmp"),
            "the winbar names the file, the log names the directory: {}",
            text.detail
        );
        assert_eq!(
            notice.to_string(),
            format!("{} — {}", text.headline, text.detail)
        );
    }
    /// The forgotten latch. The tail is decoded like any release, and the
    /// winbar says what happened until the next press.
    #[test]
    fn a_forgotten_latch_ends_itself_and_says_so() {
        let quiet = Duration::from_secs(300);
        let mut s = Session::new(true, Duration::from_secs(1), Some(quiet));
        let t = Instant::now();
        latch(&mut s, t);
        let u = s.current.clone().unwrap();
        assert_eq!(
            s.event(Event::Clock {
                now: t + quiet - Duration::from_secs(1)
            }),
            Command::Nothing
        );
        assert!(s.notice().is_none(), "nothing to say while it still runs");

        let ended = t + quiet;
        assert_eq!(
            s.event(Event::Clock { now: ended }),
            Command::Decode {
                released: ended,
                cause: Cause::Silence
            }
        );
        assert_eq!(s.notice(), Some(&Notice::SilenceTimeout));
        assert!(!u.ticking(), "released for its final decode");
        assert!(!s.state.recording());
        // Through the decode, and only the next press clears it.
        s.finish(u.id);
        assert_eq!(s.notice(), Some(&Notice::SilenceTimeout));
        start(&mut s);
        assert!(s.notice().is_none());
    }

    /// Text the recognizer produced is what says the user is still there,
    /// from a settled commit or from a live preview. Empty text is what
    /// settled silence looks like and proves nothing.
    #[test]
    fn text_restarts_the_silence_timeout_and_empty_text_does_not() {
        let quiet = Duration::from_secs(300);
        for label in ["commit", "preview"] {
            for (text, ends) in [("", true), ("a word", false)] {
                let mut s = Session::new(true, Duration::from_secs(1), Some(quiet));
                let t = Instant::now();
                latch(&mut s, t);
                let half = t + quiet / 2;
                s.event(Event::Clock { now: half });
                if label == "commit" {
                    commit(&mut s, text, half);
                } else {
                    let id = s.current.as_ref().unwrap().id;
                    s.tick_finished(id, preview(text, 0), half);
                }
                let command = s.event(Event::Clock { now: t + quiet });
                if ends {
                    assert!(
                        matches!(command, Command::Decode { .. }),
                        "{label}: empty text is not speech"
                    );
                } else {
                    assert_eq!(command, Command::Nothing, "{label} did not restart it");
                    assert_eq!(
                        s.event(Event::Clock { now: half + quiet }),
                        Command::Decode {
                            released: half + quiet,
                            cause: Cause::Silence
                        },
                        "{label}: the timeout runs from the last text"
                    );
                }
            }
        }
    }

    /// The length limit ends a capture that is still being talked into, and
    /// names itself in the winbar.
    #[test]
    fn the_length_limit_ends_a_busy_capture() {
        let mut s = Session::new(true, Duration::from_secs(1), Some(Duration::from_secs(300)));
        let t = Instant::now();
        latch(&mut s, t);
        let limit = t + MAX_CAPTURE;
        commit(&mut s, "still going", limit - Duration::from_secs(1));
        assert_eq!(
            s.event(Event::Clock { now: limit }),
            Command::Decode {
                released: limit,
                cause: Cause::Length
            }
        );
        assert_eq!(s.notice(), Some(&Notice::LengthLimit));
        assert!(!s.state.recording());
    }

    /// With no tick nothing can report speech, so there is no silence to
    /// measure and the rule is off. The length limit still ends it.
    #[test]
    fn with_no_speech_reports_a_latch_runs_to_the_length_limit() {
        let mut s = Session::new(false, Duration::from_secs(1), None);
        let t = Instant::now();
        latch(&mut s, t);
        assert_eq!(
            s.event(Event::Clock {
                now: t + MAX_CAPTURE - Duration::from_secs(1)
            }),
            Command::Nothing
        );
        assert!(s.notice().is_none());
        let limit = t + MAX_CAPTURE;
        assert!(matches!(
            s.event(Event::Clock { now: limit }),
            Command::Decode {
                cause: Cause::Length,
                ..
            }
        ));
    }

    /// The key the user presses when they come back. Neither a stop nor a
    /// cancel may touch the capture that already ended, or the notice that
    /// explains it; a press starts a fresh one and clears the slot.
    #[test]
    fn a_late_key_after_an_auto_stop_keeps_the_dictation_and_its_notice() {
        let quiet = Duration::from_secs(300);
        let mut s = Session::new(true, Duration::from_secs(1), Some(quiet));
        let t = Instant::now();
        latch(&mut s, t);
        let u = s.current.clone().unwrap();
        s.event(Event::Clock { now: t + quiet });
        s.finish(u.id);

        let later = t + quiet + Duration::from_secs(600);
        for late in [Request::Stop, Request::Cancel, Request::Stop] {
            assert_eq!(request(&mut s, late, later), Command::Nothing, "{late}");
        }
        assert_eq!(s.notice(), Some(&Notice::SilenceTimeout));
        assert!(!u.cancelled(), "a late cancel must not reach it");

        let again = later + Duration::from_secs(1);
        assert_eq!(
            request(&mut s, Request::Toggle, again),
            Command::Start,
            "the next press starts a fresh capture"
        );
        assert!(s.notice().is_none());
        assert!(s.state.latched());
        assert_ne!(s.current.as_ref().unwrap().id, u.id);
    }

    #[test]
    fn tick_failure_rearms_and_stale_tick_cannot_rearm() {
        let mut s = Session::new(true, Duration::from_secs(1), None);
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        s.requested(Instant::now());
        assert!(s.should_warn_tick_failure());
        assert!(!s.should_warn_tick_failure(), "warn once per capture");
        s.tick_finished(id, None, Instant::now());
        assert!(s.tick_due(Instant::now() + Duration::from_secs(2)));
        cancel(&mut s);
        s.tick_finished(id, None, Instant::now());
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(2)));
        start(&mut s);
        assert!(s.should_warn_tick_failure(), "a new capture warns again");
    }
}
