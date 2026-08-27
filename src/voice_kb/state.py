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
from typing import assert_never


class Phase(Enum):
    """Coarse phase, for the overlay. Derived from the state, never stored."""

    IDLE = auto()
    RECORDING = auto()
    TRANSCRIBING = auto()


# --------------------------------------------------------------------------- states


@dataclass(frozen=True, slots=True)
class Idle:
    """Nothing in flight. The audio stream may still be open for pre-roll."""


@dataclass(frozen=True, slots=True)
class Recording:
    """The hotkey is held down and audio is accumulating."""

    started_at: float
    """``time.monotonic()`` when the key went down."""


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


type Command = Nothing | StartCapture | Decode | DiscardCapture


# ----------------------------------------------------------------------- transition


def step(state: SessionState, event: Event) -> tuple[SessionState, Command]:
    """Total transition function.

    Every (state, event) pair is handled. Events that are meaningless in the
    current state return the state unchanged with :class:`Nothing` rather than
    raising -- key auto-repeat and late decode callbacks both rely on this.
    """
    match state, event:
        # -- starting
        case Idle(), KeyDown(at=at):
            return Recording(started_at=at), StartCapture()

        # -- finishing
        case Recording(started_at=started), KeyUp(at=at):
            # A key held for a few milliseconds is a stray tap, not dictation.
            return (
                Transcribing(started_at=started, released_at=at),
                Decode(spoken_seconds=at - started),
            )

        # -- aborting
        case Recording(), Cancelled():
            return Idle(), DiscardCapture()
        case Transcribing(), Cancelled():
            # The decode thread checks for this and drops its result.
            return Idle(), Nothing()

        # -- settling
        case Transcribing(), DecodeFinished():
            return Idle(), Nothing()

        # -- everything else is noise: auto-repeat, duplicate ups, stale callbacks
        case _:
            return state, Nothing()
