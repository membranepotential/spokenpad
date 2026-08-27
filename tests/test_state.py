"""``voice_kb.state.step`` is a total function: every (state, event) pair is
covered here, not just the happy path."""

from __future__ import annotations

from voice_kb.state import (
    MIN_HOLD_SECONDS,
    AbortDecode,
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


def test_transcribing_cancelled_aborts_the_decode() -> None:
    """Cancelling mid-decode must invalidate the result, not merely go Idle.

    Regression: this previously returned Nothing(), so a cancelled decode still
    landed in whatever window was focused ~2s later. The comment claimed the
    decode thread dropped the result; no such code existed.
    """
    state, command = step(Transcribing(started_at=1.0, released_at=1.5), Cancelled(at=1.6))
    assert state == Idle()
    assert command == AbortDecode()


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


def test_transcribing_key_down_starts_a_new_utterance() -> None:
    """Speaking again while a decode runs must not be silently dropped.

    The decode is on its own thread and unaffected; its result still arrives.
    """
    state, command = step(Transcribing(started_at=1.0, released_at=1.5), KeyDown(at=1.6))
    assert state == Recording(started_at=1.6)
    assert command == StartCapture()


def test_transcribing_key_up_is_ignored() -> None:
    original = Transcribing(started_at=1.0, released_at=1.5)
    state, command = step(original, KeyUp(at=1.6))
    assert state == original
    assert command == Nothing()


def test_stray_tap_is_discarded_not_decoded() -> None:
    """A key brushed for a few milliseconds must not paste hallucinated noise.

    The hotkey is deliberately not grabbed, so accidental presses reach us.
    """
    held = MIN_HOLD_SECONDS / 2
    state, command = step(Recording(started_at=1.0), KeyUp(at=1.0 + held))
    assert state == Idle()
    assert command == DiscardCapture()


def test_hold_just_over_the_floor_is_decoded() -> None:
    held = MIN_HOLD_SECONDS * 1.5
    state, command = step(Recording(started_at=1.0), KeyUp(at=1.0 + held))
    assert isinstance(state, Transcribing)
    assert isinstance(command, Decode)


def test_min_hold_is_injectable_for_tests() -> None:
    """The floor is a parameter so callers can tune it without patching."""
    _, command = step(Recording(started_at=1.0), KeyUp(at=1.05), min_hold_seconds=0.0)
    assert isinstance(command, Decode)
