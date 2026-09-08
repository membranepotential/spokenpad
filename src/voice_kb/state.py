"""Session state machine.

Pure. Events go in, ``(next_state, command)`` comes out; nothing here touches a
device, the model, or X. The imperative shell in :mod:`voice_kb.app` interprets
the commands.

The state is a closed union, so "recording without a capture start time" or
"transcribing with no audio" cannot be represented.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum, auto
from typing import Final, assert_never


class Phase(Enum):
    """Coarse phase, for the indicator. Derived from the state, never stored."""

    IDLE = auto()
    RECORDING = auto()
    TRANSCRIBING = auto()


# --------------------------------------------------------------------------- states


@dataclass(frozen=True, slots=True)
class Idle:
    """Nothing in flight. The audio stream may still be open for pre-roll."""


@dataclass(frozen=True, slots=True)
class Recording:
    """Audio is accumulating. Either the key is held, or recording is latched."""

    started_at: float
    """``time.monotonic()`` when the key went down."""

    latched: bool = False
    """Whether this recording outlives the key release.

    ``False`` is push-to-talk: the release ends it. ``True`` is a latched
    recording, started with the modifier held, which ignores the release and
    runs until the key is pressed again. The distinction has to be *in the
    state* rather than read off the event that ends it, because the ending
    event is a different one in each mode -- a ``KeyUp`` for push-to-talk, a
    ``KeyDown`` for a latch.
    """


@dataclass(frozen=True, slots=True)
class Transcribing:
    """The key was released; a decode is running off the main thread."""

    started_at: float
    """``time.monotonic()`` when the key went down (not when decoding began)."""

    released_at: float
    """``time.monotonic()`` when the key came up."""

    @property
    def spoken_seconds(self) -> float:
        return self.released_at - self.started_at


type SessionState = Idle | Recording | Transcribing


def phase_of(state: SessionState) -> Phase:
    match state:
        case Idle():
            return Phase.IDLE
        case Recording():
            return Phase.RECORDING
        case Transcribing():
            return Phase.TRANSCRIBING
        case _:
            assert_never(state)


# --------------------------------------------------------------------------- events


@dataclass(frozen=True, slots=True)
class KeyDown:
    at: float

    latch: bool = False
    """The latch modifier was held when the key went down.

    Only meaningful for a press that *starts* a recording. The press that
    ends a latched recording stops it whether or not the modifier was held,
    so the user never has to remember which hand they started with.
    """


@dataclass(frozen=True, slots=True)
class KeyUp:
    at: float


@dataclass(frozen=True, slots=True)
class DecodeFinished:
    """The decode completed, successfully or not."""

    at: float


@dataclass(frozen=True, slots=True)
class Cancelled:
    """User abort (cancel key), or a decode that failed hard."""

    at: float


type Event = KeyDown | KeyUp | DecodeFinished | Cancelled


# ------------------------------------------------------------------------- commands


@dataclass(frozen=True, slots=True)
class Nothing:
    """The event is not meaningful in this state; ignore it.

    This is the common case for key auto-repeat, which X11 delivers as a stream
    of down events while a key is held.
    """


@dataclass(frozen=True, slots=True)
class StartCapture:
    """Begin accumulating audio, seeded with the pre-roll ring buffer."""


@dataclass(frozen=True, slots=True)
class Decode:
    """Stop accumulating and decode what was captured."""

    spoken_seconds: float


@dataclass(frozen=True, slots=True)
class DiscardCapture:
    """Stop accumulating and throw the audio away."""


@dataclass(frozen=True, slots=True)
class AbortDecode:
    """Invalidate the in-flight decode so its result is never injected.

    Without this the shell has no way to distinguish "the decode the user is
    still waiting for" from "the decode the user cancelled two seconds ago",
    and stale text lands in whatever window happens to be focused by the time
    it finishes.
    """


type Command = Nothing | StartCapture | Decode | DiscardCapture | AbortDecode


# ----------------------------------------------------------------------- transition


MIN_HOLD_SECONDS: Final = 0.12
"""Below this, a keypress is a stray tap rather than dictation.

The hotkey is deliberately not grabbed, so the key stays live for everything
else on the system and accidental presses are expected. Without this floor a
30ms brush of the key decodes a fraction of a second of room noise and pastes
whatever the model hallucinates into the focused window.
"""


def step(
    state: SessionState,
    event: Event,
    *,
    min_hold_seconds: float = MIN_HOLD_SECONDS,
) -> tuple[SessionState, Command]:
    """Total transition function.

    Every (state, event) pair is handled. Events that are meaningless in the
    current state return the state unchanged with :class:`Nothing` rather than
    raising -- key auto-repeat and late decode callbacks both rely on this.
    """
    match state, event:
        # -- starting
        case Idle(), KeyDown(at=at, latch=latch):
            return Recording(started_at=at, latched=latch), StartCapture()

        # A press arriving while a previous decode is still running starts a new
        # utterance rather than being dropped. The decode runs on its own thread
        # and is unaffected; its result still gets appended when it lands.
        case Transcribing(), KeyDown(at=at, latch=latch):
            return Recording(started_at=at, latched=latch), StartCapture()

        # -- finishing a latched recording: the *press* ends it, not the release
        #
        # This case has to precede the KeyDown cases above in intent, but it
        # cannot conflict with them: those match Idle and Transcribing, and a
        # latched recording is neither.
        case Recording(started_at=started, latched=True), KeyDown(at=at) if (
            at - started < min_hold_seconds
        ):
            return Idle(), DiscardCapture()

        case Recording(started_at=started, latched=True), KeyDown(at=at):
            return (
                Transcribing(started_at=started, released_at=at),
                Decode(spoken_seconds=at - started),
            )

        # A latched recording ignores the release that started it, and every
        # release after. Without this the very first key-up would end it.
        case Recording(latched=True), KeyUp():
            return state, Nothing()

        # -- finishing push-to-talk
        case Recording(started_at=started), KeyUp(at=at) if at - started < min_hold_seconds:
            return Idle(), DiscardCapture()

        case Recording(started_at=started), KeyUp(at=at):
            return (
                Transcribing(started_at=started, released_at=at),
                Decode(spoken_seconds=at - started),
            )

        # -- aborting
        case Recording(), Cancelled():
            return Idle(), DiscardCapture()

        case Transcribing(), Cancelled():
            return Idle(), AbortDecode()

        # -- settling
        case Transcribing(), DecodeFinished():
            return Idle(), Nothing()

        # -- everything else is noise: auto-repeat, duplicate ups, stale callbacks
        case _:
            return state, Nothing()
