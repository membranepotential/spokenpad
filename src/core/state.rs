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
//!
//! The clock also ends a capture nobody is ending: see [`MAX_CAPTURE`] and
//! the `silence` argument of [`step`]. And closing the dictation window
//! cancels the capture it was showing ([`Event::WindowClosed`]).
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

/// How long after the last press a latch counts as having no key down.
///
/// A latch made by holding Shift and the key still has a key down, and its
/// auto-repeat fires the *toggle* binding: every repeat lands inside
/// [`REPEAT_WINDOW`] and refreshes `last_press`, so such a capture is a held
/// key wearing a latch's state. The silence rule skips it while the presses
/// keep coming, because ending it would only let the next repeat start a new
/// capture — one capture and one WAV per timeout, forever. [`MAX_CAPTURE`]
/// bounds it instead, as it bounds a stuck plain key.
///
/// Well above the repeat interval (40 ms at X11's default 25 Hz, 20 ms at the
/// fastest common setting), so a slow repeat, a dropped one, or a moment of
/// scheduling jitter does not read as a release. A real release stops the
/// repeats, and one second later the capture is a forgotten latch again.
pub const KEY_SETTLED: Duration = Duration::from_secs(1);

/// The longest any capture may run, whatever is being said into it.
///
/// A capture holds only its uncommitted tail in memory, so its length is
/// bounded by nothing else; the recovery WAV is what needs the bound. `hound`
/// writes a 32-bit RIFF data length, which overflows at 4 GiB — about 37
/// hours of 16 kHz mono PCM16 — and `shell::recorder::read_capture` refuses
/// anything past this limit plus the pre-roll and post-roll around it, so a
/// capture that reached it is still recoverable.
///
/// Deliberately not configurable: it is what keeps a file readable, not a
/// preference. The silence timeout below is the one users tune.
pub const MAX_CAPTURE: Duration = Duration::from_secs(4 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording {
        started: Instant,
        /// The last moment the recognizer produced text for this capture, or
        /// `started` while it has produced none. A latched capture that has
        /// been quiet this long ends itself; see [`step`].
        last_speech: Instant,
        hold: Hold,
    },
    Transcribing {
        started: Instant,
        released: Instant,
    },
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
    /// The time now. Closes a [`Hold::Releasing`] window that has passed, and
    /// ends a capture that has run too long or been quiet too long. The shell
    /// delivers it before every request, at that request's own stamp, and on
    /// every pass of its loop.
    Clock {
        now: Instant,
    },
    /// The recognizer produced text for the running capture at `at`, so the
    /// user is still talking. It moves `last_speech` and nothing else: no
    /// command can follow it.
    Speech {
        at: Instant,
    },
    /// The capture may hold no more audio in memory. Only the shell can see
    /// this, which is why it is an event rather than a rule over the clock.
    Exhausted {
        at: Instant,
    },
    /// The user closed the dictation window at `at`: the pane's own close,
    /// or `:q` in it. A capture that was running then is cancelled as
    /// `spokenpad cancel` cancels it — the user is done with that window,
    /// and a recording going on into a window nobody looks at would only
    /// open another one with its next text. A capture that started after
    /// `at` is a new press, and goes on.
    WindowClosed {
        at: Instant,
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
    /// The user closed the dictation window ([`Event::WindowClosed`]).
    WindowClosed,
}

/// What ended a capture. Everything but [`Cause::KeyPress`] is spokenpad
/// ending one the user did not end, and the user is told which it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The key: a `stop` whose repeat window closed, or the press that ends
    /// a latch.
    KeyPress,
    /// A latched capture heard no speech for the silence timeout.
    Silence,
    /// The capture ran for [`MAX_CAPTURE`].
    Length,
    /// The in-memory ceiling ([`Event::Exhausted`]).
    Memory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Nothing,
    Start,
    /// End the capture and decode it. `released` is when the key came up:
    /// the post-roll runs from there, not from when the window closed. A
    /// capture that ended by itself is released at the moment it ended.
    Decode {
        released: Instant,
        cause: Cause,
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

/// A capture begins. Nothing has been heard yet, so the silence timeout runs
/// from the press.
fn begin(at: Instant, hold: Hold) -> (State, Command) {
    (
        State::Recording {
            started: at,
            last_speech: at,
            hold,
        },
        Command::Start,
    )
}

/// A capture ends: decoded, unless a key ended it inside [`MINIMUM_HOLD`],
/// which makes it a stray tap. Only a key can tap: the other causes need
/// minutes of capture to fire, and throwing away what they end would lose
/// dictation rather than a slip of the finger.
fn end(started: Instant, released: Instant, cause: Cause) -> (State, Command) {
    if cause == Cause::KeyPress && released.saturating_duration_since(started) < MINIMUM_HOLD {
        (State::Idle, Command::Discard(DiscardReason::TooShort))
    } else {
        (
            State::Transcribing { started, released },
            Command::Decode { released, cause },
        )
    }
}

/// One transition. `silence` is how long a latched capture may hear no speech
/// before it ends itself, or `None` where nothing can report speech: before
/// the speech model has loaded, a capture is only recorded and
/// [`Event::Speech`] never arrives.
pub fn step(state: State, event: Event, silence: Option<Duration>) -> (State, Command) {
    use Command::Nothing;
    use Hold::*;
    use Request::*;
    use State::*;
    match (state, event) {
        // A capture that already ended, still decoding, does not stop a new
        // one: a quick re-press is a new paragraph.
        (Idle | Transcribing { .. }, Event::Request(Received { request, at })) => match request {
            Start => begin(at, Held),
            Toggle => begin(at, Latched { last_press: at }),
            // The `stop` of the press that ended a latched capture, or of a
            // tap already discarded; and a cancel after the release, which
            // must not destroy a dictation that is already captured. A
            // capture that ended by itself sees the same strays, later.
            Stop | Cancel => (state, Nothing),
        },
        (
            Recording {
                started,
                last_speech,
                hold,
            },
            Event::Request(Received { request, at }),
        ) => {
            let holding = |hold| {
                (
                    Recording {
                        started,
                        last_speech,
                        hold,
                    },
                    Nothing,
                )
            };
            match (hold, request) {
                // Auto-repeat of the held key.
                (Held, Start) => (state, Nothing),
                (Held, Stop) => holding(Releasing { at }),
                // The key came back within the window: it never came up.
                // The shell closes a passed window with a `Clock` at this
                // request's stamp first, so here `at` is inside it.
                (Releasing { .. }, Start) => holding(Held),
                // The first release is the one that counts.
                (Releasing { .. }, Stop) => (state, Nothing),
                // Shift added to a held key latches it: its auto-repeat now
                // fires the toggle binding.
                (Held | Releasing { .. }, Toggle) => holding(Latched { last_press: at }),
                (Latched { last_press }, Start | Toggle) => {
                    if at.saturating_duration_since(last_press) <= REPEAT_WINDOW {
                        holding(Latched { last_press: at })
                    } else {
                        end(started, at, Cause::KeyPress)
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
                last_speech,
                hold,
            },
            Event::Clock { now },
        ) => match hold {
            // The key came up and its repeat window has closed: the release
            // is what ended this capture, at the moment the key came up.
            Releasing { at } if now.saturating_duration_since(at) > REPEAT_WINDOW => {
                end(started, at, Cause::KeyPress)
            }
            // Every capture is bounded, so its recovery WAV stays readable.
            _ if now.saturating_duration_since(started) >= MAX_CAPTURE => {
                end(started, now, Cause::Length)
            }
            // A latch with no key down, which is the forgotten capture this
            // rule is for. A key that is still being pressed — a held
            // push-to-talk key, or a held Shift+key whose auto-repeat fires
            // the toggle binding and keeps `last_press` fresh — is excluded:
            // ending its capture would only let the next repeat start the
            // one after it. See [`KEY_SETTLED`].
            Latched { last_press }
                if now.saturating_duration_since(last_press) >= KEY_SETTLED
                    && silence.is_some_and(|q| now.saturating_duration_since(last_speech) >= q) =>
            {
                end(started, now, Cause::Silence)
            }
            _ => (state, Nothing),
        },
        // A stamp from a decode that finished out of order must not move the
        // silence timeout backwards.
        (
            Recording {
                started,
                last_speech,
                hold,
            },
            Event::Speech { at },
        ) => (
            Recording {
                started,
                last_speech: last_speech.max(at),
                hold,
            },
            Nothing,
        ),
        (Recording { started, .. }, Event::Exhausted { at }) => end(started, at, Cause::Memory),
        // Like a cancel, whatever holds the capture: the window it was
        // dictating into is gone, and so is the user's interest in it.
        (Recording { started, .. }, Event::WindowClosed { at }) if at >= started => {
            (Idle, Command::Discard(DiscardReason::WindowClosed))
        }
        (Transcribing { .. }, Event::Finished) => (Idle, Nothing),
        (
            _,
            Event::Clock { .. }
            | Event::Speech { .. }
            | Event::Exhausted { .. }
            | Event::WindowClosed { .. }
            | Event::Finished,
        ) => (state, Nothing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DiscardReason::*;

    /// The silence timeout these tests run with. Long enough that nothing in
    /// a test measured in milliseconds reaches it, so every case below is
    /// also a check that the timeout leaves ordinary dictation alone.
    const QUIET: Duration = Duration::from_secs(300);

    fn request(request: Request, at: Instant) -> Event {
        Event::Request(Received { request, at })
    }

    /// The command a key release produces, which is most of them.
    fn decode(released: Instant) -> Command {
        Command::Decode {
            released,
            cause: Cause::KeyPress,
        }
    }

    /// Folds `events` from `state`, returning the final state and every
    /// command that was not `Nothing`.
    fn run(state: State, events: &[Event]) -> (State, Vec<Command>) {
        run_with(Some(QUIET), state, events)
    }

    fn run_with(
        silence: Option<Duration>,
        state: State,
        events: &[Event],
    ) -> (State, Vec<Command>) {
        events.iter().fold((state, Vec::new()), |(s, mut out), e| {
            let (s, c) = step(s, *e, silence);
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

    /// Every event the table can be handed, stamped at `at`, for the
    /// exhaustive sweep below. `Event` carries `Instant`s, so this cannot be
    /// a `const` array; the match instead makes a new variant a *compile*
    /// error here, pointing at the one place that has to list it.
    fn all_events(at: Instant) -> [Event; 9] {
        fn covered(event: Event) {
            match event {
                // Adding a variant breaks this match. Add it to the array
                // below and give every row of `expected` a column for it.
                Event::Request(_)
                | Event::Clock { .. }
                | Event::Speech { .. }
                | Event::Exhausted { .. }
                | Event::WindowClosed { .. }
                | Event::Finished => {}
            }
        }
        let events = [
            request(Request::Start, at),
            request(Request::Stop, at),
            request(Request::Toggle, at),
            request(Request::Cancel, at),
            Event::Clock { now: at },
            Event::Speech { at },
            Event::Exhausted { at },
            Event::WindowClosed { at },
            Event::Finished,
        ];
        events.into_iter().for_each(covered);
        events
    }

    #[test]
    fn all_state_event_pairs() {
        let t = Instant::now();
        let later = t + Duration::from_secs(1);
        let at = |hold| State::Recording {
            started: t,
            last_speech: t,
            hold,
        };
        // The same capture after `Event::Speech { at: later }`.
        let spoke = |hold| State::Recording {
            started: t,
            last_speech: later,
            hold,
        };
        let transcribing = State::Transcribing {
            started: t,
            released: later - Duration::from_millis(500),
        };
        let releasing = Hold::Releasing {
            at: later - REPEAT_WINDOW,
        };
        let states = [
            State::Idle,
            at(Hold::Held),
            // A window that is exactly at its edge at `later`.
            at(releasing),
            at(Hold::Latched { last_press: t }),
            transcribing,
        ];
        let events = all_events(later);
        let begun = |hold| State::Recording {
            started: later,
            last_speech: later,
            hold,
        };
        let held_new = begun(Hold::Held);
        let latched_new = begun(Hold::Latched { last_press: later });
        let latched_now = at(Hold::Latched { last_press: later });
        let cancelled = (State::Idle, Command::Discard(Cancelled));
        let closed = (State::Idle, Command::Discard(WindowClosed));
        let ended = (
            State::Transcribing {
                started: t,
                released: later,
            },
            decode(later),
        );
        let exhausted = (
            State::Transcribing {
                started: t,
                released: later,
            },
            Command::Decode {
                released: later,
                cause: Cause::Memory,
            },
        );
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
                nothing(spoke(Hold::Held)),
                exhausted,
                closed,
                nothing(at(Hold::Held)),
            ],
            // Releasing, window exactly at its edge: still open.
            [
                nothing(at(Hold::Held)),
                nothing(states[2]),
                nothing(latched_now),
                cancelled,
                nothing(states[2]),
                nothing(spoke(releasing)),
                exhausted,
                closed,
                nothing(states[2]),
            ],
            // Latched
            [
                ended,
                nothing(states[3]),
                ended,
                cancelled,
                nothing(states[3]),
                nothing(spoke(Hold::Latched { last_press: t })),
                exhausted,
                closed,
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
                nothing(transcribing),
                nothing(transcribing),
                // Nor can closing the window: the tail is decoded as for any
                // release, and lands in the passage it belongs to.
                nothing(transcribing),
                nothing(State::Idle),
            ],
        ];
        assert_eq!(
            events.len(),
            expected[0].len(),
            "every event in `all_events` needs a column in `expected`"
        );
        assert_eq!(
            states.len(),
            expected.len(),
            "every state needs a row in `expected`"
        );
        for (i, state) in states.into_iter().enumerate() {
            for (j, event) in events.into_iter().enumerate() {
                assert_eq!(
                    step(state, event, Some(QUIET)),
                    expected[i][j],
                    "state {i}, event {j}"
                );
            }
        }
    }

    /// Closing the window cancels the capture it was showing, held or
    /// latched, exactly as `spokenpad cancel` would; a capture pressed after
    /// the close is a new one and goes on, and one already released is
    /// decoded as usual.
    #[test]
    fn closing_the_window_cancels_the_capture_it_was_showing() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let closed = |at| Event::WindowClosed { at };
        for (latch, name) in [(Request::Toggle, "latched"), (Request::Start, "held")] {
            let (state, commands) = run(
                State::Idle,
                &[requests(&[(latch, t)]), vec![closed(ms(20_000))]].concat(),
            );
            assert_eq!(
                commands,
                [Command::Start, Command::Discard(WindowClosed)],
                "{name}"
            );
            assert_eq!(state, State::Idle, "{name}");
        }
        // The window closed, and the next press came before the close was
        // reported: that capture is not the one the window showed.
        let (state, commands) = run(
            State::Idle,
            &[
                requests(&[(Request::Toggle, ms(500))]),
                vec![closed(ms(400))],
            ]
            .concat(),
        );
        assert_eq!(commands, [Command::Start]);
        assert!(state.latched());
        // Released and decoding: the close changes nothing.
        let (state, commands) = run(
            State::Idle,
            &[
                requests(&[(Request::Toggle, t), (Request::Toggle, ms(1000))]),
                vec![closed(ms(1100))],
            ]
            .concat(),
        );
        assert_eq!(commands, [Command::Start, decode(ms(1000))]);
        assert!(matches!(state, State::Transcribing { .. }));
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
        assert_eq!(commands, [Command::Start, decode(ms(1000))]);
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
                Some(QUIET),
            );
            assert_eq!(commands, decode(released), "{order}");
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
                last_speech: t,
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
        assert_eq!(commands, [Command::Start, decode(ms(1000)), Command::Start]);
        assert_eq!(
            state,
            State::Recording {
                started: ms(1151),
                last_speech: ms(1151),
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
            assert_eq!(commands, [Command::Start, decode(ms(5000))], "{ending}");
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
        assert_eq!(commands, [decode(ms(2000))]);
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
                assert_eq!(commands, [Command::Start, decode(released)]);
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

    /// The capture the user forgot: latched, nothing said, no key down. It
    /// ends on the clock, as a release does, so the tail is decoded and kept.
    #[test]
    fn a_silent_latch_ends_itself_on_the_clock() {
        let t = Instant::now();
        let (state, commands) = run(State::Idle, &requests(&[(Request::Toggle, t)]));
        assert_eq!(commands, [Command::Start]);

        let (still, commands) = run(
            state,
            &[Event::Clock {
                now: t + QUIET - Duration::from_millis(1),
            }],
        );
        assert_eq!(commands, [], "one millisecond early");
        assert!(still.latched());

        let ended = t + QUIET;
        let (after, commands) = run(still, &[Event::Clock { now: ended }]);
        assert_eq!(
            commands,
            [Command::Decode {
                released: ended,
                cause: Cause::Silence
            }]
        );
        assert_eq!(
            after,
            State::Transcribing {
                started: t,
                released: ended
            }
        );
    }

    /// Text from the recognizer is the proof that the user is still there,
    /// and it restarts the timeout wherever it lands.
    #[test]
    fn speech_restarts_the_silence_timeout() {
        let t = Instant::now();
        let (state, _) = run(State::Idle, &requests(&[(Request::Toggle, t)]));
        let mut state = state;
        // Half the timeout apart, four times over: never quiet long enough.
        for i in 1..=4 {
            let at = t + QUIET / 2 * i;
            let (next, commands) = run(state, &[Event::Clock { now: at }, Event::Speech { at }]);
            assert_eq!(commands, [], "speech at {i} half-timeouts ended it");
            state = next;
        }
        let quiet_from = t + QUIET / 2 * 4;
        let (state, commands) = run(
            state,
            &[Event::Clock {
                now: quiet_from + QUIET,
            }],
        );
        assert_eq!(
            commands,
            [Command::Decode {
                released: quiet_from + QUIET,
                cause: Cause::Silence
            }],
            "the timeout runs from the last speech, not from the press"
        );
        assert!(!state.recording());
    }

    /// A stale stamp from a decode that finished out of order must not pull
    /// the timeout backwards into a stop.
    #[test]
    fn speech_stamped_in_the_past_does_not_shorten_the_timeout() {
        let t = Instant::now();
        let (state, _) = run(State::Idle, &requests(&[(Request::Toggle, t)]));
        let recent = t + QUIET;
        let (state, _) = run(state, &[Event::Speech { at: recent }]);
        let (state, commands) = run(
            state,
            &[
                Event::Speech { at: t },
                Event::Clock {
                    now: recent + QUIET - Duration::from_millis(1),
                },
            ],
        );
        assert_eq!(commands, []);
        assert!(state.latched());
    }

    /// A held key re-fires its binding every few tens of milliseconds, so
    /// ending its capture for silence would only start the next one. The
    /// length limit is what bounds it.
    #[test]
    fn silence_ends_a_latch_but_never_a_held_key() {
        let t = Instant::now();
        for (opening, latched) in [(Request::Start, false), (Request::Toggle, true)] {
            let (state, _) = run(State::Idle, &requests(&[(opening, t)]));
            let (state, commands) = run(state, &[Event::Clock { now: t + QUIET * 3 }]);
            if latched {
                assert!(matches!(
                    commands[..],
                    [Command::Decode {
                        cause: Cause::Silence,
                        ..
                    }]
                ));
            } else {
                assert_eq!(commands, [], "a held key is not ended by silence");
                assert!(state.recording());
            }
        }
    }

    /// With no VAD model, or with the progressive tick off, nothing ever
    /// reports speech: there is no silence to measure, and the rule is off.
    #[test]
    fn without_speech_reports_only_the_length_limit_applies() {
        let t = Instant::now();
        let (latched, _) = run_with(None, State::Idle, &requests(&[(Request::Toggle, t)]));
        let (state, commands) = run_with(
            None,
            latched,
            &[Event::Clock {
                now: t + MAX_CAPTURE - Duration::from_secs(1),
            }],
        );
        assert_eq!(commands, [], "silence alone cannot end it");
        assert!(state.latched());
        let limit = t + MAX_CAPTURE;
        let (state, commands) = run_with(None, state, &[Event::Clock { now: limit }]);
        assert_eq!(
            commands,
            [Command::Decode {
                released: limit,
                cause: Cause::Length
            }]
        );
        assert!(!state.recording());
    }

    /// Held or latched, quiet or not: no capture outlives the limit that
    /// keeps its recovery WAV readable.
    #[test]
    fn every_capture_ends_at_the_length_limit() {
        let t = Instant::now();
        let limit = t + MAX_CAPTURE;
        for opening in [Request::Start, Request::Toggle] {
            let (mut state, _) = run(State::Idle, &requests(&[(opening, t)]));
            // Talking throughout, so the silence rule never fires.
            let mut at = t;
            while at + QUIET / 2 < limit {
                at += QUIET / 2;
                let (next, commands) =
                    run(state, &[Event::Clock { now: at }, Event::Speech { at }]);
                assert_eq!(commands, [], "{opening} ended early at {:?}", at - t);
                state = next;
            }
            let (state, commands) = run(
                state,
                &[
                    Event::Clock {
                        now: limit - Duration::from_millis(1),
                    },
                    Event::Clock { now: limit },
                ],
            );
            assert_eq!(
                commands,
                [Command::Decode {
                    released: limit,
                    cause: Cause::Length
                }],
                "{opening}"
            );
            assert_eq!(
                state,
                State::Transcribing {
                    started: t,
                    released: limit
                }
            );
        }
    }

    /// The in-memory ceiling: the capture cannot hold more audio, so it ends
    /// and is decoded rather than recording on into a file nobody reads.
    #[test]
    fn the_memory_ceiling_ends_the_capture() {
        let t = Instant::now();
        let at = t + Duration::from_secs(3600);
        for opening in [Request::Start, Request::Toggle] {
            let (state, _) = run(State::Idle, &requests(&[(opening, t)]));
            let (state, commands) = run(state, &[Event::Exhausted { at }]);
            assert_eq!(
                commands,
                [Command::Decode {
                    released: at,
                    cause: Cause::Memory
                }]
            );
            assert_eq!(
                state,
                State::Transcribing {
                    started: t,
                    released: at
                }
            );
            // Reported again while the tail decodes: nothing more to end.
            let (state, commands) = run(
                state,
                &[Event::Exhausted {
                    at: at + Duration::from_secs(1),
                }],
            );
            assert_eq!(commands, []);
            assert!(!state.recording());
        }
    }

    /// Only a key can tap. A capture that spokenpad ends is never discarded
    /// as too short, however the stamps fall: what it ends is dictation.
    ///
    /// The ceiling is the only cause that can fire this early at all. A
    /// silence stop waits out [`KEY_SETTLED`] and the length limit waits out
    /// [`MAX_CAPTURE`], both of which are longer than [`MINIMUM_HOLD`], so
    /// neither can reach the tap rule even with a one-second timeout.
    #[test]
    fn an_auto_stop_is_never_read_as_a_tap() {
        assert!(KEY_SETTLED > MINIMUM_HOLD && MAX_CAPTURE > MINIMUM_HOLD);
        let t = Instant::now();
        let soon = t + MINIMUM_HOLD / 2;
        for opening in [Request::Start, Request::Toggle] {
            let (state, _) = run(State::Idle, &requests(&[(opening, t)]));
            let (state, commands) = run(state, &[Event::Exhausted { at: soon }]);
            assert_eq!(
                commands,
                [Command::Decode {
                    released: soon,
                    cause: Cause::Memory
                }],
                "{opening}"
            );
            assert_eq!(
                state,
                State::Transcribing {
                    started: t,
                    released: soon
                }
            );
        }
    }

    /// A latch made by holding Shift and the key: the key is still down, and
    /// its auto-repeat fires the toggle binding every 40 ms. Ending such a
    /// capture for silence would let the next repeat start the one after it,
    /// so the silence rule leaves it alone and `MAX_CAPTURE` bounds it — the
    /// same answer a stuck plain key gets. A real release stops the repeats,
    /// and the capture becomes a forgotten latch a second later.
    #[test]
    fn a_latch_whose_key_is_still_down_is_not_ended_by_silence() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let (mut state, commands) = run(State::Idle, &requests(&[(Request::Toggle, t)]));
        assert_eq!(commands, [Command::Start]);

        // Shift+key held for twice the timeout, repeating at 25 Hz. Nothing
        // is said into it: without the freshness rule the first 300 s would
        // end it and the repeat 40 ms later would open the next capture.
        let repeats = (QUIET.as_millis() as u64 * 2) / 40;
        for i in 1..=repeats {
            let at = ms(40 * i);
            let (next, commands) = run(
                state,
                &[Event::Clock { now: at }, request(Request::Toggle, at)],
            );
            assert_eq!(commands, [], "the repeat at {:?} broke the capture", at - t);
            state = next;
        }
        assert!(
            state.latched(),
            "still one capture, {repeats} repeats later"
        );

        // The key finally comes up, so the repeats stop. Nothing was ever
        // said into this capture, so it has been quiet since the press: the
        // freshness of the last press is the only thing still holding it,
        // and it ends as soon as that ages out.
        let up = ms(40 * repeats);
        let (state, commands) = run(
            state,
            &[Event::Clock {
                now: up + KEY_SETTLED - Duration::from_millis(1),
            }],
        );
        assert_eq!(commands, [], "the key has only just come up");
        let ended = up + KEY_SETTLED;
        let (state, commands) = run(state, &[Event::Clock { now: ended }]);
        assert_eq!(
            commands,
            [Command::Decode {
                released: ended,
                cause: Cause::Silence
            }],
            "a latch nobody is pressing any more still ends"
        );
        assert!(!state.recording());
    }

    /// The key the user finally presses after a capture ended itself. A
    /// `stop` or a `cancel` must change nothing, and a press must start a
    /// fresh capture rather than resurrect the old one.
    #[test]
    fn a_late_key_after_an_auto_stop_is_harmless() {
        let t = Instant::now();
        let ended = t + QUIET;
        let (state, _) = run(State::Idle, &requests(&[(Request::Toggle, t)]));
        let (transcribing, _) = run(state, &[Event::Clock { now: ended }]);
        let late = |state, from: Instant| {
            let (state, commands) = run(
                state,
                &requests(&[
                    (Request::Stop, from + Duration::from_secs(1)),
                    (Request::Cancel, from + Duration::from_secs(2)),
                    (Request::Stop, from + Duration::from_secs(3)),
                ]),
            );
            assert_eq!(commands, []);
            state
        };
        // While the tail is still decoding, and again once it has landed.
        let state = late(transcribing, ended);
        assert_eq!(state, transcribing);
        let (idle, commands) = run(state, &[Event::Finished]);
        assert_eq!(commands, []);
        assert_eq!(idle, State::Idle);
        assert_eq!(late(idle, ended + Duration::from_secs(10)), State::Idle);

        let again = ended + Duration::from_secs(60);
        let (state, commands) = run(idle, &requests(&[(Request::Toggle, again)]));
        assert_eq!(commands, [Command::Start]);
        assert_eq!(
            state,
            State::Recording {
                started: again,
                last_speech: again,
                hold: Hold::Latched { last_press: again }
            }
        );
    }
}
