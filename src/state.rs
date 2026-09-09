//! Pure session transitions. Repeat filtering belongs to the input adapter.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording { started: Instant, latched: bool },
    Transcribing { started: Instant, released: Instant },
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Down { at: Instant, latch: bool },
    Up { at: Instant },
    Finished,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    None,
    Start,
    Decode,
    Discard,
    Abort,
}

impl State {
    pub fn phase(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording { .. } => "recording",
            Self::Transcribing { .. } => "transcribing",
        }
    }
    pub fn recording(self) -> bool {
        matches!(self, Self::Recording { .. })
    }
    pub fn latched(self) -> bool {
        matches!(self, Self::Recording { latched: true, .. })
    }
}

pub fn step(state: State, event: Event) -> (State, Command) {
    use Command::*;
    use Event::*;
    use State::*;
    match (state, event) {
        (Idle | Transcribing { .. }, Down { at, latch }) => (
            Recording {
                started: at,
                latched: latch,
            },
            Start,
        ),
        (
            Recording {
                started,
                latched: true,
            },
            Down { at, .. },
        )
        | (
            Recording {
                started,
                latched: false,
            },
            Up { at },
        ) => {
            if at.saturating_duration_since(started) < Duration::from_millis(120) {
                (Idle, Discard)
            } else {
                (
                    Transcribing {
                        started,
                        released: at,
                    },
                    Decode,
                )
            }
        }
        (Recording { .. }, Cancel) => (Idle, Discard),
        (Transcribing { .. }, Cancel) => (Idle, Abort),
        (Transcribing { .. }, Finished) => (Idle, None),
        _ => (state, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_state_event_pairs() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        let states = [
            State::Idle,
            State::Recording {
                started: t,
                latched: false,
            },
            State::Recording {
                started: t,
                latched: true,
            },
            State::Transcribing {
                started: t,
                released: later,
            },
        ];
        let events = [
            Event::Down {
                at: later,
                latch: false,
            },
            Event::Up { at: later },
            Event::Finished,
            Event::Cancel,
        ];
        let expected = [
            [Command::Start, Command::None, Command::None, Command::None],
            [
                Command::None,
                Command::Decode,
                Command::None,
                Command::Discard,
            ],
            [
                Command::Decode,
                Command::None,
                Command::None,
                Command::Discard,
            ],
            [Command::Start, Command::None, Command::None, Command::Abort],
        ];
        for (i, state) in states.into_iter().enumerate() {
            for (j, event) in events.into_iter().enumerate() {
                assert_eq!(step(state, event).1, expected[i][j]);
            }
        }
    }
    #[test]
    fn taps_and_latch_boundary() {
        let t = Instant::now();
        for latched in [false, true] {
            for ms in [0, 119, 120, 121] {
                let s = State::Recording {
                    started: t,
                    latched,
                };
                let at = t + Duration::from_millis(ms);
                let e = if latched {
                    Event::Down { at, latch: false }
                } else {
                    Event::Up { at }
                };
                assert_eq!(
                    step(s, e).1,
                    if ms < 120 {
                        Command::Discard
                    } else {
                        Command::Decode
                    }
                );
            }
        }
    }
}
