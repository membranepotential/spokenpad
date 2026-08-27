"""``voice_kb.state.step`` is a total function: every (state, event) pair is
covered here, not just the happy path."""

from __future__ import annotations

from voice_kb.state import (
    Cancelled,
    Decode,
    DecodeFinished,
    DiscardCapture,
    Idle,
    KeyDown,
    KeyUp,
    Nothing,
    Recording,
    StartCapture,
    Transcribing,
    step,
)

# -- happy path ---------------------------------------------------------


def test_idle_key_down_starts_recording() -> None:
    state, command = step(Idle(), KeyDown(at=1.0))
    assert state == Recording(started_at=1.0)
    assert command == StartCapture()


def test_recording_key_up_starts_transcribing_and_decodes() -> None:
    state, command = step(Recording(started_at=1.0), KeyUp(at=1.5))
    assert state == Transcribing(started_at=1.0, released_at=1.5)
    assert command == Decode(spoken_seconds=0.5)


def test_transcribing_decode_finished_returns_to_idle() -> None:
    state, command = step(Transcribing(started_at=1.0, released_at=1.5), DecodeFinished(at=2.0))
    assert state == Idle()
    assert command == Nothing()


# -- cancellation ---------------------------------------------------------


def test_recording_cancelled_discards_and_returns_to_idle() -> None:
    state, command = step(Recording(started_at=1.0), Cancelled(at=1.2))
    assert state == Idle()
    assert command == DiscardCapture()


def test_transcribing_cancelled_returns_to_idle_without_discard() -> None:
    # The decode is already running off-thread; there's nothing left to
    # discard here, the decode thread itself checks for this and drops its
    # result once it finishes.
    state, command = step(Transcribing(started_at=1.0, released_at=1.5), Cancelled(at=1.6))
    assert state == Idle()
    assert command == Nothing()


def test_idle_cancelled_is_a_noop() -> None:
    state, command = step(Idle(), Cancelled(at=1.0))
    assert state == Idle()
    assert command == Nothing()


# -- auto-repeat and other noise ---------------------------------------------


def test_recording_key_down_auto_repeat_does_not_restart_capture() -> None:
    """X11 delivers a stream of KeyDown while a key is held. A repeat must
    not reset ``started_at`` or re-issue ``StartCapture``."""
    original = Recording(started_at=1.0)
    state, command = step(original, KeyDown(at=1.3))
    assert state == original
    assert state.started_at == 1.0
    assert command == Nothing()


def test_idle_key_up_is_a_noop() -> None:
    state, command = step(Idle(), KeyUp(at=1.0))
    assert state == Idle()
    assert command == Nothing()


def test_idle_decode_finished_is_a_noop() -> None:
    state, command = step(Idle(), DecodeFinished(at=1.0))
    assert state == Idle()
    assert command == Nothing()


def test_recording_decode_finished_is_ignored() -> None:
    """A stale callback from a previous decode arriving mid-recording changes nothing."""
    original = Recording(started_at=1.0)
    state, command = step(original, DecodeFinished(at=1.0))
    assert state == original
    assert command == Nothing()


def test_transcribing_key_down_is_ignored() -> None:
    original = Transcribing(started_at=1.0, released_at=1.5)
    state, command = step(original, KeyDown(at=1.6))
    assert state == original
    assert command == Nothing()


def test_transcribing_key_up_is_ignored() -> None:
    original = Transcribing(started_at=1.0, released_at=1.5)
    state, command = step(original, KeyUp(at=1.6))
    assert state == original
    assert command == Nothing()
