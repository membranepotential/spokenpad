//! Pure session transitions over the four control requests and the clock.
//!
//! The requests come from key bindings in the user's window manager, so the
//! daemon never sees the key itself. Two consequences shape this table:
//!
//! - **Auto-repeat.** A held key can re-fire its binding every ~40 ms: a
//!   stream of `start`s (i3, which asks X11 for detectable auto-repeat), or
//!   `stop`, `start` pairs (other X11 clients), and the two processes of a
//!   pair may reach the daemon in either order. So a `stop` of a held capture
//!   does not end it at once: it opens a [`REPEAT_WINDOW`], and a `start`
//!   inside it means the key never came up. A `start` while held is ignored.
//! - **Modifiers.** Shift and the key come up in either order, so the `stop`
//!   that follows a latching `toggle` may or may not arrive. A latched
//!   capture ignores every `stop`; the next `start` or `toggle` ends it.
use crate::core::control::{Received, Request};
use std::time::{Duration, Instant};

/// Below this a press is a stray tap, not dictation. Deliberately not
/// configurable: it is a property of how a key feels, not of a deployment.
pub const MINIMUM_HOLD: Duration = Duration::from_millis(120);

/// How long after a `stop` a `start` still continues the same held capture,
/// and how close two presses of a latched capture must be to be one key's
/// auto-repeat rather than a second press.
///
/// X11's default repeat rate is 25 Hz (40 ms apart) and common settings go
/// up to 50 Hz; the continuing `start` arrives within one interval even when
/// the pair's processes are reordered. 150 ms covers three default intervals
/// plus process-spawn jitter, and is still shorter than the default
/// post-roll (250 ms), which delays the decode anyway, so waiting it out
/// costs nothing. A deliberate release and re-press inside 150 ms simply
/// keeps recording, which loses nothing.
pub const REPEAT_WINDOW: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording { started: Instant, hold: Hold },
    Transcribing { started: Instant, released: Instant },
}

/// What is keeping a capture running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// The push-to-talk key is down; a `stop` begins [`Hold::Releasing`].
    Held,
    /// A `stop` arrived at `at`. A `start` by `at + REPEAT_WINDOW` was the
    /// key's auto-repeat and continues the capture; the clock passing that
    /// point ends it, released at `at`. Audio is still captured meanwhile.
    Releasing { at: Instant },
    /// Begun by `toggle`, or latched by a `toggle` while held. `last_press`
    /// is the latest `start` or `toggle`, so a press within
    /// [`REPEAT_WINDOW`] of it is auto-repeat, not a second press.
    Latched { last_press: Instant },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Request(Received),
    /// The time now. Closes a [`Hold::Releasing`] window that has passed.
    /// The shell delivers it before every request, at that request's own
    /// stamp, and on every pass of its loop.
    Clock {
        now: Instant,
    },
    Finished,
}

/// Why a capture was thrown away. The user is told about both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardReason {
    /// Held for less than [`MINIMUM_HOLD`].
    TooShort,
    /// `spokenpad cancel`.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Nothing,
    Start,
    /// End the capture and decode it. `released` is when the key came up:
    /// the post-roll runs from there, not from when the window closed.
    Decode {
        released: Instant,
    },
    Discard(DiscardReason),
}

/// What phase the editor's indicator should show. Owned by the core because
/// it is a pure function of [`State`]; the shell's `IndicatorState` only
/// carries it to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorPhase {
    Idle,
    Recording,
    Transcribing,
}

impl IndicatorPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Transcribing => "transcribing",
        }
    }
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
        matches!(
            self,
            Self::Recording {
                hold: Hold::Latched { .. },
                ..
            }
        )
    }
}

/// A capture ends: decoded, unless it was only a tap.
fn end(started: Instant, released: Instant) -> (State, Command) {
    if released.saturating_duration_since(started) < MINIMUM_HOLD {
        (State::Idle, Command::Discard(DiscardReason::TooShort))
    } else {
        (
            State::Transcribing { started, released },
            Command::Decode { released },
        )
    }
}

pub fn step(state: State, event: Event) -> (State, Command) {
    use Command::Nothing;
    use Hold::*;
    use Request::*;
    use State::*;
    let recording = |started, hold| (Recording { started, hold }, Nothing);
    match (state, event) {
        // A capture that already ended, still decoding, does not stop a new
        // one: a quick re-press is a new paragraph.
        (Idle | Transcribing { .. }, Event::Request(Received { request, at })) => match request {
            Start => (
                Recording {
                    started: at,
                    hold: Held,
                },
                Command::Start,
            ),
            Toggle => (
                Recording {
                    started: at,
                    hold: Latched { last_press: at },
                },
                Command::Start,
            ),
            // The `stop` of the press that ended a latched capture, or of a
            // tap already discarded; and a cancel after the release, which
            // must not destroy a dictation that is already captured.
            Stop | Cancel => (state, Nothing),
        },
        (Recording { started, hold }, Event::Request(Received { request, at })) => {
            match (hold, request) {
                // Auto-repeat of the held key.
                (Held, Start) => (state, Nothing),
                (Held, Stop) => recording(started, Releasing { at }),
                // The key came back within the window: it never came up.
                // The shell closes a passed window with a `Clock` at this
                // request's stamp first, so here `at` is inside it.
                (Releasing { .. }, Start) => recording(started, Held),
                // The first release is the one that counts.
                (Releasing { .. }, Stop) => (state, Nothing),
                // Shift added to a held key latches it: its auto-repeat now
                // fires the toggle binding.
                (Held | Releasing { .. }, Toggle) => recording(started, Latched { last_press: at }),
                (Latched { last_press }, Start | Toggle) => {
                    if at.saturating_duration_since(last_press) <= REPEAT_WINDOW {
                        recording(started, Latched { last_press: at })
                    } else {
                        end(started, at)
                    }
                }
                // Shift and the key come up in either order.
                (Latched { .. }, Stop) => (state, Nothing),
                (_, Cancel) => (Idle, Command::Discard(DiscardReason::Cancelled)),
            }
        }
        (
            Recording {
                started,
                hold: Releasing { at },
            },
            Event::Clock { now },
        ) if now.saturating_duration_since(at) > REPEAT_WINDOW => end(started, at),
        (Transcribing { .. }, Event::Finished) => (Idle, Nothing),
        (_, Event::Clock { .. } | Event::Finished) => (state, Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiscardReason::*;

    fn request(request: Request, at: Instant) -> Event {
        Event::Request(Received { request, at })
    }

    /// Folds `events` from `state`, returning the final state and every
    /// command that was not `Nothing`.
    fn run(state: State, events: &[Event]) -> (State, Vec<Command>) {
        events.iter().fold((state, Vec::new()), |(s, mut out), e| {
            let (s, c) = step(s, *e);
            if c != Command::Nothing {
                out.push(c);
            }
            (s, out)
        })
    }

    /// The shell's order: every request is preceded by the clock at its stamp.
    fn requests(pairs: &[(Request, Instant)]) -> Vec<Event> {
        pairs
            .iter()
            .flat_map(|&(r, at)| [Event::Clock { now: at }, request(r, at)])
            .collect()
    }

    #[test]
    fn all_state_event_pairs() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        let at = |hold| State::Recording { started: t, hold };
        let transcribing = State::Transcribing {
            started: t,
            released: later - Duration::from_millis(500),
        };
        let states = [
            State::Idle,
            at(Hold::Held),
            // A window that is exactly at its edge at `later`.
            at(Hold::Releasing {
                at: later - REPEAT_WINDOW,
            }),
            at(Hold::Latched { last_press: t }),
            transcribing,
        ];
        let events = [
            request(Request::Start, later),
            request(Request::Stop, later),
            request(Request::Toggle, later),
            request(Request::Cancel, later),
            Event::Clock { now: later },
            Event::Finished,
        ];
        let held_new = State::Recording {
            started: later,
            hold: Hold::Held,
        };
        let latched_new = State::Recording {
            started: later,
            hold: Hold::Latched { last_press: later },
        };
        let latched_now = at(Hold::Latched { last_press: later });
        let cancelled = (State::Idle, Command::Discard(Cancelled));
        let nothing = |s| (s, Command::Nothing);
        let expected = [
            // Idle
            [
                (held_new, Command::Start),
                nothing(State::Idle),
                (latched_new, Command::Start),
                nothing(State::Idle),
                nothing(State::Idle),
                nothing(State::Idle),
            ],
            // Held
            [
                nothing(at(Hold::Held)),
                nothing(at(Hold::Releasing { at: later })),
                nothing(latched_now),
                cancelled,
                nothing(at(Hold::Held)),
                nothing(at(Hold::Held)),
            ],
            // Releasing, window exactly at its edge: still open.
            [
                nothing(at(Hold::Held)),
                nothing(states[2]),
                nothing(latched_now),
                cancelled,
                nothing(states[2]),
                nothing(states[2]),
            ],
            // Latched
            [
                (
                    State::Transcribing {
                        started: t,
                        released: later,
                    },
                    Command::Decode { released: later },
                ),
                nothing(states[3]),
                (
                    State::Transcribing {
                        started: t,
                        released: later,
                    },
                    Command::Decode { released: later },
                ),
                cancelled,
                nothing(states[3]),
                nothing(states[3]),
            ],
            // Transcribing
            [
                (held_new, Command::Start),
                nothing(transcribing),
                (latched_new, Command::Start),
                // A cancel cannot destroy a dictation that is already captured.
                nothing(transcribing),
                nothing(transcribing),
                nothing(State::Idle),
            ],
        ];
        for (i, state) in states.into_iter().enumerate() {
            for (j, event) in events.into_iter().enumerate() {
                assert_eq!(step(state, event), expected[i][j], "state {i}, event {j}");
            }
        }
    }

    #[test]
    fn a_held_capture_ends_when_the_window_after_its_stop_closes() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let (state, commands) = run(
            State::Idle,
            &[
                requests(&[(Request::Start, t), (Request::Stop, ms(1000))]),
                vec![
                    Event::Clock { now: ms(1150) },
                    Event::Clock { now: ms(1151) },
                ],
            ]
            .concat(),
        );
        assert_eq!(
            commands,
            [Command::Start, Command::Decode { released: ms(1000) }]
        );
        assert_eq!(
            state,
            State::Transcribing {
                started: t,
                released: ms(1000)
            }
        );
    }

    /// X11 auto-repeat without detectable repeat: a stop/start pair every
    /// 40 ms while the key is held, each pair in either order. One capture
    /// throughout, until the real release.
    #[test]
    fn repeat_pairs_in_either_order_continue_one_capture() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        for order in ["stop first", "start first", "mixed"] {
            let reversed = |i: u64| match order {
                "stop first" => false,
                "start first" => true,
                _ => i % 3 == 1,
            };
            let mut pairs = vec![(Request::Start, t)];
            for i in 0..20u64 {
                let pair_at = 660 + 40 * i;
                let (first, second) = if reversed(i) {
                    (Request::Start, Request::Stop)
                } else {
                    (Request::Stop, Request::Start)
                };
                pairs.push((first, ms(pair_at)));
                pairs.push((second, ms(pair_at + 2)));
            }
            // The real release, 40 ms after the last pair. When that pair
            // arrived start-first, its stop is already the release: the real
            // one lands inside that stop's window and is ignored.
            pairs.push((Request::Stop, ms(1460)));
            let released = if reversed(19) { ms(1422) } else { ms(1460) };
            let mut events = requests(&pairs);
            // The loop's own clock ticks in between.
            events.push(Event::Clock {
                now: released + REPEAT_WINDOW,
            });
            let (state, commands) = run(State::Idle, &events);
            assert_eq!(commands, [Command::Start], "{order}");
            assert!(state.recording(), "still inside the last window");

            let (state, commands) = step(
                state,
                Event::Clock {
                    now: released + REPEAT_WINDOW + Duration::from_millis(1),
                },
            );
            assert_eq!(commands, Command::Decode { released }, "{order}");
            assert_eq!(
                state,
                State::Transcribing {
                    started: t,
                    released
                }
            );
        }
    }

    /// i3 turns on detectable auto-repeat: a held key re-sends only `start`.
    #[test]
    fn repeated_starts_while_held_are_ignored() {
        let t = Instant::now();
        let mut pairs: Vec<_> = (0..30u64)
            .map(|i| (Request::Start, t + Duration::from_millis(660 + 40 * i)))
            .collect();
        pairs.insert(0, (Request::Start, t));
        let (state, commands) = run(State::Idle, &requests(&pairs));
        assert_eq!(commands, [Command::Start]);
        assert_eq!(
            state,
            State::Recording {
                started: t,
                hold: Hold::Held
            }
        );
    }

    #[test]
    fn a_start_after_the_window_is_a_new_capture() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let (state, commands) = run(
            State::Idle,
            &requests(&[
                (Request::Start, t),
                (Request::Stop, ms(1000)),
                (Request::Start, ms(1151)),
            ]),
        );
        assert_eq!(
            commands,
            [
                Command::Start,
                Command::Decode { released: ms(1000) },
                Command::Start
            ]
        );
        assert_eq!(
            state,
            State::Recording {
                started: ms(1151),
                hold: Hold::Held
            }
        );
    }

    /// Latch with Shift+key, release in any order, stop with the key alone:
    /// neither the latching press's stop nor the ending press's stop may
    /// start or end anything.
    #[test]
    fn a_latched_capture_ignores_stray_stops_and_ends_on_the_next_press() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        for ending in [Request::Start, Request::Toggle] {
            let (state, commands) = run(
                State::Idle,
                &requests(&[
                    (Request::Toggle, t),
                    (Request::Stop, ms(90)),
                    (Request::Stop, ms(3000)),
                    (ending, ms(5000)),
                    (Request::Stop, ms(5080)),
                ]),
            );
            assert_eq!(
                commands,
                [Command::Start, Command::Decode { released: ms(5000) }],
                "{ending}"
            );
            assert_eq!(
                state,
                State::Transcribing {
                    started: t,
                    released: ms(5000)
                }
            );
        }
    }

    /// A latching key held down auto-repeats its toggle; every repeat after
    /// the first follows the previous one within the window.
    #[test]
    fn a_repeating_toggle_does_not_flap() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let (state, commands) = run(
            State::Idle,
            &requests(&[
                (Request::Toggle, t),
                (Request::Toggle, ms(40)),
                (Request::Toggle, ms(80)),
                (Request::Toggle, ms(200)),
                (Request::Toggle, ms(340)),
            ]),
        );
        assert_eq!(commands, [Command::Start]);
        assert!(state.latched());
    }

    #[test]
    fn toggle_while_held_latches_and_the_release_is_ignored() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let (state, commands) = run(
            State::Idle,
            &requests(&[
                (Request::Start, t),
                (Request::Toggle, ms(800)),
                (Request::Stop, ms(1200)),
            ]),
        );
        assert_eq!(commands, [Command::Start]);
        assert!(state.latched());
        let (_, commands) = run(
            state,
            &requests(&[(Request::Start, ms(2000)), (Request::Stop, ms(2100))]),
        );
        assert_eq!(commands, [Command::Decode { released: ms(2000) }]);
    }

    #[test]
    fn cancel_discards_only_while_recording() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        for opening in [Request::Start, Request::Toggle] {
            let (state, commands) = run(
                State::Idle,
                &requests(&[
                    (opening, t),
                    (Request::Cancel, ms(500)),
                    (Request::Stop, ms(600)),
                ]),
            );
            assert_eq!(commands, [Command::Start, Command::Discard(Cancelled)]);
            assert_eq!(state, State::Idle);
        }
        // Inside a release's window the capture is still recording.
        let (state, commands) = run(
            State::Idle,
            &requests(&[
                (Request::Start, t),
                (Request::Stop, ms(500)),
                (Request::Cancel, ms(550)),
            ]),
        );
        assert_eq!(commands, [Command::Start, Command::Discard(Cancelled)]);
        assert_eq!(state, State::Idle);
    }

    #[test]
    fn taps_are_measured_to_the_release() {
        let t = Instant::now();
        for ms in [0, 119, 120, 121] {
            let released = t + Duration::from_millis(ms);
            let (state, commands) = run(
                State::Idle,
                &[
                    requests(&[(Request::Start, t), (Request::Stop, released)]),
                    vec![Event::Clock {
                        now: released + REPEAT_WINDOW + Duration::from_millis(1),
                    }],
                ]
                .concat(),
            );
            if ms < 120 {
                assert_eq!(commands, [Command::Start, Command::Discard(TooShort)]);
                assert_eq!(state, State::Idle);
            } else {
                assert_eq!(commands, [Command::Start, Command::Decode { released }]);
                assert_eq!(
                    state,
                    State::Transcribing {
                        started: t,
                        released
                    }
                );
            }
        }
    }
}
