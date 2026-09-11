//! Pure session transitions. Repeat filtering belongs to the input adapter.
use crate::nvim::IndicatorPhase;
use std::time::{Duration, Instant};

/// Below this a press is a stray tap, not dictation. Deliberately not
/// configurable: it is a property of how a key feels, not of a deployment.
pub const MINIMUM_HOLD: Duration = Duration::from_millis(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording { started: Instant, latched: bool },
    Transcribing { started: Instant, released: Instant },
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    Down {
        at: Instant,
        latch: bool,
    },
    Up {
        at: Instant,
    },
    /// No watched keyboard can report the hotkey any more — the one holding it
    /// disappeared, or the last one that advertises it did. A recording must
    /// never outlive the key that would end it.
    HotkeyLost {
        at: Instant,
    },
    Finished,
    Cancel,
}

/// Why a capture was thrown away. The user is told about both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardReason {
    /// Held for less than [`MINIMUM_HOLD`].
    TooShort,
    /// The cancel key.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Nothing,
    Start,
    Decode,
    Discard(DiscardReason),
}

impl State {
    pub fn indicator_phase(self) -> IndicatorPhase {
        match self {
            Self::Idle => IndicatorPhase::Idle,
            Self::Recording { .. } => IndicatorPhase::Recording,
            Self::Transcribing { .. } => IndicatorPhase::Transcribing,
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
            if at.saturating_duration_since(started) < MINIMUM_HOLD {
                (Idle, Discard(DiscardReason::TooShort))
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
        // Not a tap, however short: the user was speaking and the key they
        // would have released is gone. A latched recording has no other way
        // out at all — its key is already up, and the keyboard that carried
        // the cancel key left with it — so what was said is decoded.
        (Recording { started, .. }, HotkeyLost { at }) => (
            Transcribing {
                started,
                released: at,
            },
            Decode,
        ),
        (Recording { .. }, Cancel) => (Idle, Discard(DiscardReason::Cancelled)),
        // The audio is already captured and the release decode is about to
        // land. Escape is read from every keyboard regardless of focus, so
        // honouring it here would destroy a finished dictation.
        (Transcribing { .. }, Cancel) => (state, Nothing),
        (Transcribing { .. }, Finished) => (Idle, Nothing),
        _ => (state, Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiscardReason::*;

    #[test]
    fn all_state_event_pairs() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        let recording = |latched| State::Recording {
            started: t,
            latched,
        };
        let transcribing = State::Transcribing {
            started: t,
            released: later,
        };
        let started = State::Recording {
            started: later,
            latched: false,
        };
        let states = [State::Idle, recording(false), recording(true), transcribing];
        let events = [
            Event::Down {
                at: later,
                latch: false,
            },
            Event::Up { at: later },
            Event::HotkeyLost { at: later },
            Event::Finished,
            Event::Cancel,
        ];
        let decoded = State::Transcribing {
            started: t,
            released: later,
        };
        let expected = [
            [
                (started, Command::Start),
                (State::Idle, Command::Nothing),
                (State::Idle, Command::Nothing),
                (State::Idle, Command::Nothing),
                (State::Idle, Command::Nothing),
            ],
            [
                (recording(false), Command::Nothing),
                (decoded, Command::Decode),
                (decoded, Command::Decode),
                (recording(false), Command::Nothing),
                (State::Idle, Command::Discard(Cancelled)),
            ],
            [
                (decoded, Command::Decode),
                (recording(true), Command::Nothing),
                // The only way out of a latched recording whose keyboard left.
                (decoded, Command::Decode),
                (recording(true), Command::Nothing),
                (State::Idle, Command::Discard(Cancelled)),
            ],
            [
                (started, Command::Start),
                (transcribing, Command::Nothing),
                (transcribing, Command::Nothing),
                (State::Idle, Command::Nothing),
                // A cancel cannot destroy a dictation that is already captured.
                (transcribing, Command::Nothing),
            ],
        ];
        for (i, state) in states.into_iter().enumerate() {
            for (j, event) in events.into_iter().enumerate() {
                assert_eq!(step(state, event), expected[i][j], "state {i}, event {j}");
            }
        }
    }

    /// A device loss is not a tap: it ends the recording by decoding it, at
    /// any hold length and whether or not the key was latched.
    #[test]
    fn a_lost_hotkey_decodes_whatever_was_said() {
        let t = Instant::now();
        for latched in [false, true] {
            for ms in [0, 119, 500] {
                let at = t + Duration::from_millis(ms);
                assert_eq!(
                    step(
                        State::Recording {
                            started: t,
                            latched
                        },
                        Event::HotkeyLost { at }
                    ),
                    (
                        State::Transcribing {
                            started: t,
                            released: at
                        },
                        Command::Decode
                    ),
                    "latched {latched}, held {ms}ms"
                );
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
                let (state, command) = step(s, e);
                if ms < 120 {
                    assert_eq!(command, Command::Discard(TooShort));
                    assert_eq!(state, State::Idle);
                } else {
                    assert_eq!(command, Command::Decode);
                    assert_eq!(
                        state,
                        State::Transcribing {
                            started: t,
                            released: at
                        }
                    );
                }
            }
        }
    }
}
