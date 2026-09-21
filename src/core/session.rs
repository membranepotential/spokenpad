//! Main-thread policy, without devices or threads. The bridge owns paragraph order.
use crate::core::{
    decode::{Commit, Preview, Utterance, UtteranceId},
    frames::Frames,
    state::{self, Command, DiscardReason, Event, State},
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
    /// Highest first, and this list is the definition:
    ///
    /// 1. `MemoryCap` — the capture is over and the audio is only in a WAV.
    /// 2. `CaptureIncomplete` — most of what was said never arrived.
    /// 3. `MicrophoneUnavailable` — audio is missing and did not come back.
    /// 4. `MicrophoneGap` — audio came back, with a hole in the recording.
    /// 5. `NearlySilent` — everything arrived and may still be worth nothing.
    /// 6. `HeldTooBriefly` — nothing was recorded, and nothing was lost.
    /// 7. `PreviewPaused` — cosmetic: only the live tail stopped.
    pub fn priority(&self) -> u8 {
        match self {
            Self::MemoryCap(_) => 6,
            Self::CaptureIncomplete => 5,
            Self::MicrophoneUnavailable => 4,
            Self::MicrophoneGap => 3,
            Self::NearlySilent => 2,
            Self::HeldTooBriefly => 1,
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
                "past 60 minutes; remaining audio is in {} — recover with spokenpad transcribe",
                file_name(path)
            )
            .into(),
            Self::MemoryCap(RecordingStatus::Truncated(_)) => {
                "past 60 minutes AND recording failed; stop and start a new recording".into()
            }
            Self::MemoryCap(RecordingStatus::NotRecorded) => {
                "past 60 minutes with no recovery recording; new audio is being discarded".into()
            }
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
    previews: Previews,
    warned_tick_failure: bool,
}
impl Session {
    pub fn new(enabled: bool, interval: Duration) -> Self {
        Self {
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
            previews: Previews::Running,
            warned_tick_failure: false,
        }
    }
    pub fn event(&mut self, event: Event) -> Command {
        let (state, command) = state::step(self.state, event);
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
            Command::Decode { .. } => {
                if let Some(u) = &self.current {
                    u.release();
                }
                self.due = None;
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
    /// Records that text has landed. Cancelling stops *decoding*; it never
    /// unwrites text the recognizer already produced, so there is no reject
    /// path here — only the live capture's hints move.
    pub fn note_commit(&mut self, c: &Commit) {
        if !self.is_current(c.utterance.id) {
            return;
        }
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
    /// The in-memory ceiling ends this capture: nothing more can be decoded,
    /// so release what is held and stop previewing for good.
    pub fn cap(&mut self, recovery: RecordingStatus) {
        self.previews = Previews::Capped;
        self.due = None;
        self.notify(Notice::MemoryCap(recovery));
        if let Some(u) = &self.current {
            u.release();
        }
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
    fn preview(text: &str, through: usize) -> Option<Preview> {
        Some(Preview {
            text: text.into(),
            through: Frames(through),
        })
    }
    #[test]
    fn first_recording_schedules_and_accepts_preview_before_any_commit() {
        let interval = Duration::from_millis(1100);
        let mut s = Session::new(true, interval);
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
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let old = s.current.clone().unwrap();
        release(&mut s, Instant::now() + Duration::from_secs(1));
        start(&mut s);
        s.finish(old.id);
        assert!(s.state.recording());
        s.note_commit(&Commit {
            utterance: old,
            text: "older".into(),
            through: Frames(999),
        });
        assert_eq!(s.committed_hint, Frames::ZERO);
    }
    #[test]
    fn a_cancelled_utterance_still_keeps_the_text_it_already_produced() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let u = s.current.clone().unwrap();
        cancel(&mut s);
        assert!(u.cancelled(), "cancelling stops further decoding");
        // The commit was produced before the cancel became visible; the hint
        // still moves, because this is the utterance the session owns.
        s.note_commit(&Commit {
            utterance: Arc::clone(&u),
            text: "already spoken".into(),
            through: Frames(4),
        });
        assert_eq!(s.committed_hint, Frames(4));
    }
    #[test]
    fn shrink_guard_resets_only_on_commit_epoch() {
        let mut s = Session::new(true, Duration::from_secs(1));
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
        let mut s = Session::new(true, Duration::from_secs(1));
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
        let mut s = Session::new(true, Duration::from_secs(1));
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
        let mut s = Session::new(true, Duration::from_millis(200));
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
    fn cap_releases_the_capture_and_survives_a_late_preview() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let u = s.current.clone().unwrap();
        let id = u.id;
        s.cap(RecordingStatus::NotRecorded);
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
            Notice::MicrophoneGap,
            Notice::NearlySilent,
            Notice::HeldTooBriefly,
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
    /// A preview that stopped is cosmetic; a microphone that dropped audio is
    /// not. The pause still happens -- it just does not take the winbar.
    #[test]
    fn a_paused_preview_never_hides_what_went_wrong_with_the_capture() {
        let mut s = Session::new(true, Duration::from_millis(200));
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
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let capped = Notice::MemoryCap(RecordingStatus::Recorded("/tmp/capture.wav".into()));
        s.cap(RecordingStatus::Recorded("/tmp/capture.wav".into()));
        for later in [
            Notice::MicrophoneGap,
            Notice::MicrophoneUnavailable,
            Notice::CaptureIncomplete,
            Notice::NearlySilent,
            Notice::HeldTooBriefly,
            Notice::PreviewPaused,
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
        let mut s = Session::new(true, Duration::from_secs(1));
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
    #[test]
    fn tick_failure_rearms_and_stale_tick_cannot_rearm() {
        let mut s = Session::new(true, Duration::from_secs(1));
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
