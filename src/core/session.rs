//! Main-thread policy, without devices or threads. The bridge owns paragraph order.
use crate::core::{
    decode::{Commit, Preview, Utterance, UtteranceId},
    frames::Frames,
    state::{self, Command, DiscardReason, Event, State},
};
use std::{
    borrow::Cow,
    path::PathBuf,
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
/// one is shown at a time, in place of the preview, until the next key press.
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

impl Notice {
    pub fn message(&self) -> Cow<'static, str> {
        match self {
            Self::HeldTooBriefly => "held too briefly — hold the key while speaking".into(),
            Self::MicrophoneGap => {
                "microphone stopped delivering audio; reopened, but this recording has a gap".into()
            }
            Self::MicrophoneUnavailable => "microphone unavailable; audio is missing".into(),
            Self::CaptureIncomplete => {
                "microphone delivered less than half the expected audio; this capture is incomplete"
                    .into()
            }
            Self::NearlySilent => {
                "capture is nearly silent; check microphone gain and device".into()
            }
            Self::PreviewPaused => "preview paused: long uncommitted tail".into(),
            Self::MemoryCap(RecordingStatus::Recorded(path)) => format!(
                "past the 60-minute memory limit; remaining audio is in {} — recover with spokenpad transcribe",
                path.display()
            )
            .into(),
            Self::MemoryCap(RecordingStatus::Truncated(_)) => {
                "past the 60-minute memory limit AND recording failed; stop and start a new recording"
                    .into()
            }
            Self::MemoryCap(RecordingStatus::NotRecorded) => {
                "past the 60-minute memory limit with no recovery recording; new audio is being discarded"
                    .into()
            }
        }
    }
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
    paused: bool,
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
            paused: false,
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
                self.paused = false;
                self.warned_tick_failure = false;
                self.pending = None;
                self.due = self.enabled.then(|| Instant::now() + self.interval);
            }
            Command::Decode => {
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
                    self.notice = Some(Notice::HeldTooBriefly);
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
        if !self.paused {
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
    /// The open tail is too long to decode cosmetically; look again at `retry`
    /// so previews resume by themselves once it settles.
    pub fn defer_previews(&mut self, retry: Instant) {
        self.paused = true;
        self.due = Some(retry);
        self.notice = Some(Notice::PreviewPaused);
    }
    pub fn resume_previews(&mut self) {
        if !self.paused {
            return;
        }
        self.paused = false;
        if self.notice == Some(Notice::PreviewPaused) {
            self.notice = None;
        }
    }
    /// The in-memory ceiling ends this capture: nothing more can be decoded,
    /// so release what is held and stop previewing for good.
    pub fn cap(&mut self, recovery: RecordingStatus) {
        self.paused = true;
        self.due = None;
        self.notice = Some(Notice::MemoryCap(recovery));
        if let Some(u) = &self.current {
            u.release();
        }
    }
    pub fn notify(&mut self, notice: Notice) {
        self.notice = Some(notice);
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
        self.enabled && !self.paused
    }
    /// What the indicator shows below the committed text.
    pub fn shown_preview(&self) -> Cow<'_, str> {
        match &self.notice {
            Some(notice) => notice.message(),
            None => Cow::Borrowed(&self.preview),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn start(s: &mut Session) {
        s.event(Event::Down {
            at: Instant::now(),
            latch: false,
        });
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
        assert_eq!(s.shown_preview(), "first live preview");
        assert!(s.previewing());
        assert!(s.tick_due(due + interval));
    }
    #[test]
    fn stale_finished_does_not_end_new_recording() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let old = s.current.clone().unwrap();
        s.event(Event::Up {
            at: Instant::now() + Duration::from_secs(1),
        });
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
        s.event(Event::Cancel);
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
            assert_eq!(s.shown_preview(), expected);
        }
    }
    #[test]
    fn a_notice_replaces_the_preview_without_stopping_it() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        s.notify(Notice::MicrophoneGap);
        s.tick_finished(id, preview("still decoding", 0), Instant::now());
        assert_eq!(s.shown_preview(), Notice::MicrophoneGap.message());
        assert!(
            s.tick_due(Instant::now() + Duration::from_secs(2)),
            "a notice must not silently stop preview work"
        );
        s.event(Event::Cancel);
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
        s.event(Event::Down { at, latch: false });
        let command = s.event(Event::Up {
            at: at + Duration::from_millis(50),
        });
        assert_eq!(command, Command::Discard(DiscardReason::TooShort));
        assert_eq!(s.shown_preview(), Notice::HeldTooBriefly.message());
        assert_eq!(s.state, State::Idle);
    }
    #[test]
    fn paused_previews_resume_once_the_tail_is_affordable_again() {
        let mut s = Session::new(true, Duration::from_millis(200));
        start(&mut s);
        let now = Instant::now();
        s.defer_previews(now + Duration::from_millis(200));
        assert!(!s.previewing());
        assert_eq!(s.shown_preview(), Notice::PreviewPaused.message());
        assert!(
            s.tick_due(now + Duration::from_millis(200)),
            "a paused preview must re-check, not stop"
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
        s.tick_finished(id, preview("much longer cosmetic text", 0), Instant::now());
        s.tick_finished(id, None, Instant::now());
        assert_eq!(
            s.shown_preview(),
            Notice::MemoryCap(RecordingStatus::NotRecorded).message()
        );
        assert!(!s.previewing());
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(30)));
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
        s.event(Event::Cancel);
        s.tick_finished(id, None, Instant::now());
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(2)));
        start(&mut s);
        assert!(s.should_warn_tick_failure(), "a new capture warns again");
    }
}
