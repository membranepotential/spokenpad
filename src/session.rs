//! Main-thread policy, without devices or threads. The bridge owns paragraph order.
use crate::{
    decode::{Commit, Tick, Utterance, UtteranceId},
    state::{self, Command, Event, State},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Session {
    pub state: State,
    pub current: Option<Arc<Utterance>>,
    pub committed_hint: usize,
    pub preview: String,
    pub notice: Option<String>,
    pub previewing: bool,
    preview_epoch: usize,
    next_id: u64,
    due: Option<Instant>,
    pending: Option<(UtteranceId, Instant)>,
    interval: Duration,
    enabled: bool,
}
impl Session {
    pub fn new(enabled: bool, interval: Duration) -> Self {
        Self {
            state: State::Idle,
            current: None,
            committed_hint: 0,
            preview: String::new(),
            notice: None,
            previewing: enabled,
            preview_epoch: 0,
            next_id: 0,
            due: None,
            pending: None,
            interval,
            enabled,
        }
    }
    pub fn event(&mut self, event: Event) -> Command {
        let (state, command) = state::step(self.state, event);
        self.state = state;
        match command {
            Command::Start => {
                self.next_id += 1;
                self.current = Some(Utterance::new(self.next_id));
                self.committed_hint = 0;
                self.preview_epoch = 0;
                self.preview.clear();
                self.notice = None;
                self.previewing = self.enabled;
                self.pending = None;
                self.due = self.enabled.then(|| Instant::now() + self.interval);
            }
            Command::Decode => {
                if let Some(u) = &self.current {
                    u.release();
                }
                self.due = None;
            }
            Command::Discard | Command::Abort => {
                if let Some(u) = &self.current {
                    u.cancel();
                }
                self.due = None;
                self.preview.clear();
                self.notice = None;
            }
            Command::None => {}
        }
        command
    }
    pub fn is_current(&self, id: UtteranceId) -> bool {
        self.current.as_ref().is_some_and(|u| u.id == id)
    }
    pub fn accept_commit(&mut self, c: &Commit) -> bool {
        if c.utterance.cancelled() {
            return false;
        }
        if self.is_current(c.utterance.id) {
            self.committed_hint = self.committed_hint.max(c.through);
            if c.through > self.preview_epoch {
                self.preview_epoch = c.through;
                self.preview.clear();
            }
        }
        true
    }
    pub fn finish(&mut self, id: UtteranceId) {
        if self.is_current(id) {
            self.event(Event::Finished);
            if self.state == State::Idle {
                self.preview.clear();
                self.notice = None;
            }
        }
    }
    pub fn tick_due(&self, now: Instant) -> bool {
        self.state.recording()
            && self.previewing
            && self.pending.is_none()
            && self.due.is_some_and(|d| now >= d)
    }
    pub fn requested(&mut self, now: Instant) {
        if let Some(u) = &self.current {
            self.pending = Some((u.id, now));
            self.due = None;
        }
    }
    pub fn tick_finished(&mut self, id: UtteranceId, tick: Option<Tick>, now: Instant) {
        if !self.is_current(id) || !self.state.recording() {
            return;
        }
        let elapsed = self
            .pending
            .filter(|(i, _)| *i == id)
            .map(|(_, at)| now.saturating_duration_since(at))
            .unwrap_or_default();
        self.pending = None;
        if self.previewing {
            self.due = Some(now + self.interval.saturating_sub(elapsed).max(elapsed));
        }
        if self.notice.is_some() {
            return;
        }
        if let Some(Tick::Preview { text, through }) = tick {
            if through < self.committed_hint {
                return;
            }
            if through != self.preview_epoch {
                self.preview_epoch = through;
                self.preview.clear();
            }
            if text.chars().count() >= self.preview.chars().count() {
                self.preview = text;
            }
        }
    }
    pub fn pause_previews(&mut self) {
        self.due = None;
        self.previewing = false;
    }
    pub fn warn(&mut self, message: String) {
        self.notice = Some(message);
    }
    pub fn cap(&mut self, message: String) {
        self.pause_previews();
        if let Some(u) = &self.current {
            u.release();
        }
        self.warn(message);
    }
    pub fn shown_preview(&self) -> &str {
        self.notice.as_deref().unwrap_or(&self.preview)
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
            Some(Tick::Preview {
                text: "first live preview".into(),
                through: 0,
            }),
            due + Duration::from_millis(100),
        );
        assert_eq!(s.committed_hint, 0);
        assert_eq!(s.shown_preview(), "first live preview");
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
        assert!(s.accept_commit(&Commit {
            utterance: old,
            text: "older".into(),
            through: 999
        }));
        assert_eq!(s.committed_hint, 0);
    }
    #[test]
    fn cancel_is_sticky_across_new_capture() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let u = s.current.clone().unwrap();
        s.event(Event::Cancel);
        start(&mut s);
        assert!(!s.accept_commit(&Commit {
            utterance: u,
            text: "late".into(),
            through: 4
        }));
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
            s.tick_finished(
                id,
                Some(Tick::Preview {
                    text: text.into(),
                    through,
                }),
                Instant::now(),
            );
            assert_eq!(s.preview, expected);
        }
    }
    #[test]
    fn cap_survives_late_preview_and_failure_rearm() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        s.cap("capped".into());
        s.tick_finished(
            id,
            Some(Tick::Preview {
                text: "much longer cosmetic text".into(),
                through: 0,
            }),
            Instant::now(),
        );
        s.tick_finished(id, None, Instant::now());
        assert_eq!(s.shown_preview(), "capped");
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(30)));
    }
    #[test]
    fn tick_failure_rearms_and_stale_tick_cannot_rearm() {
        let mut s = Session::new(true, Duration::from_secs(1));
        start(&mut s);
        let id = s.current.as_ref().unwrap().id;
        s.requested(Instant::now());
        s.tick_finished(id, None, Instant::now());
        assert!(s.tick_due(Instant::now() + Duration::from_secs(2)));
        s.event(Event::Cancel);
        s.tick_finished(id, None, Instant::now());
        assert!(!s.tick_due(Instant::now() + Duration::from_secs(2)));
    }
}
