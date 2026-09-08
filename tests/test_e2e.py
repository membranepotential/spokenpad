"""End-to-end regression tests for the seams between ``voice_kb``'s modules.

72 unit tests were green while, in production: a cancelled decode still got
injected, a missing model silently produced nothing, and holding the hotkey
spawned ~60 subprocesses/second and stalled the whole event loop. Unit tests
on ``state.py``/``text.py``/etc. in isolation cannot see any of that -- these
tests drive the real :class:`~voice_kb.app.Daemon` (state machine, ``_apply``,
``_sync_indicators``, ``_on_decoded``, the utterance id) against fakes
for every hardware/subprocess boundary, so a regression in how those pieces
are wired together shows up here even when every module still passes in
isolation.

Hardware-free by construction: ``conftest.py`` sets
``QT_QPA_PLATFORM=offscreen`` before PySide6 is ever imported, and every
``Daemon`` built by the ``make_daemon`` fixture has a fake ``AudioCapture``,
a fake ``NvimSession``, and faked ``voice_kb.x11`` queries. No test here
starts ``HotkeyWatcher`` or ``Daemon.start()`` (that would open real
``/dev/input`` nodes and touch a real X server), and no test spawns a real
terminal or writes to a real nvim -- the whole point of a fake
``NvimSession`` is that a regression in the append path can be caught
without ever risking that again. (The real ``NvimSession``, against a real
headless nvim, is exercised separately in ``test_nvim.py``.)

Driving the worker and the nvim bridge: ``Daemon``'s worker ``QThread`` and
its nvim bridge ``QThread`` are both never started (see ``make_daemon`` and
``fake_nvim`` in ``conftest.py`` for why). Instead, wherever production code
would cross to one of those threads via a queued signal
(``_decode_requested`` -> ``_Worker.run_decode``, ``_preview_requested`` ->
``_Worker.run_preview``, ``_nvim_append_requested`` -> ``_NvimBridge.append``,
...), tests call the real method directly on the main thread. Because the
*receiving* end of the result signals (``_Worker.decoded``/``decode_failed``,
``_NvimBridge.appended``/``append_failed``) lives on the main thread and the
call happens from the main thread, Qt resolves that connection to a direct
(synchronous) call -- so the real utterance check in ``_on_decoded`` (and the
real success/failure handling in ``_on_appended``/``_on_append_failed``)
still run for real; only the thread hop itself is skipped.
"""

from __future__ import annotations

import inspect
import logging
import sys
from collections.abc import Callable
from pathlib import Path
from typing import cast

import numpy as np
import pytest
from fakes import FakeAudioCapture, FakeNvimSession, FakeTranscriber, FakeX11
from PySide6.QtWidgets import QApplication

from voice_kb import app as app_module
from voice_kb import x11
from voice_kb.app import Daemon
from voice_kb.asr import ModelMissingError, Transcriber, TranscriptionResult
from voice_kb.audio import AudioCapture, MonoAudio
from voice_kb.config import AsrConfig, Config, HotkeyConfig, RecordingConfig
from voice_kb.hotkey import HotkeyWatcher
from voice_kb.nvim import AppendFailed, NvimSession
from voice_kb.recorder import CaptureRecorder, NotRecorded, Recorded, Truncated
from voice_kb.state import (
    MIN_HOLD_SECONDS,
    Cancelled,
    Idle,
    KeyDown,
    KeyUp,
    Phase,
    Recording,
    SessionState,
    Transcribing,
)
from voice_kb.vad import Segment, SpeechSegmenter

pytestmark = pytest.mark.usefixtures("qapp")

type DaemonFactory = Callable[[Config | None], Daemon]


def _append_spy(daemon: Daemon) -> list[str]:
    """Records every string the daemon asked to have appended to the
    dictation buffer, by connecting straight to ``_nvim_append_requested``.

    A plain function connected to a Qt signal is always invoked directly
    (Qt has no thread affinity to queue against for a bare callable), so
    this fires synchronously the moment ``_on_segment_decoded`` emits it --
    no bridge thread required to observe it.

    The ``continued`` flag is dropped here; :func:`_paragraph_spy` keeps it
    for the tests that are actually about paragraph shape.
    """
    calls: list[str] = []
    daemon._nvim_append_requested.connect(lambda text, _continued: calls.append(text))
    return calls


def _paragraph_spy(daemon: Daemon) -> list[tuple[str, bool]]:
    """Every append with its ``continued`` flag: ``False`` opens a paragraph,
    ``True`` extends the one the previous segment of the same utterance
    started."""
    calls: list[tuple[str, bool]] = []
    daemon._nvim_append_requested.connect(lambda text, continued: calls.append((text, continued)))
    return calls


def _nvim_preview_spy(daemon: Daemon) -> list[str]:
    """Records every string the daemon pushed toward the nvim indicator's
    preview field, the same way :func:`_append_spy` observes appends. This is
    the "sink" a preview must never actually reach in committed form -- see
    the invariants in the module docstring of ``voice_kb.app``.
    """
    calls: list[str] = []
    daemon._nvim_preview.connect(calls.append)
    return calls


def _decode_spy(daemon: Daemon) -> list[int]:
    """Records every utterance a decode was requested for."""
    calls: list[int] = []
    daemon._decode_requested.connect(lambda samples, utterance: calls.append(utterance))
    return calls


def _preview_spy(daemon: Daemon) -> list[tuple[MonoAudio, int, int]]:
    """Records every ``(samples, start, utterance)`` a tick was requested for.

    As :func:`_decode_spy`: the worker thread is never started, so tests take
    what the daemon asked for here and hand it to ``_Worker.run_preview``
    themselves, exactly where Qt would have made the thread hop.
    """
    calls: list[tuple[MonoAudio, int, int]] = []
    daemon._preview_requested.connect(
        lambda samples, start, utterance: calls.append((samples, start, utterance))
    )
    return calls


def _transcriber(daemon: Daemon) -> FakeTranscriber:
    """``daemon._worker._transcriber`` is statically ``Transcriber | None``;
    ``make_daemon`` always sets it to a :class:`FakeTranscriber` (a
    duck-typed stand-in, not a subclass -- ``Transcriber`` loads a real
    model in its own ``__init__``, which is exactly what these tests must
    not do), so an ``isinstance`` check would be structurally impossible
    per mypy and flagged unreachable. ``cast`` documents that fact instead.
    """
    return cast(FakeTranscriber, daemon._worker._transcriber)


def _audio(daemon: Daemon) -> FakeAudioCapture:
    """As :func:`_transcriber`, for ``daemon._audio`` (statically
    ``AudioCapture``, actually a :class:`FakeAudioCapture` set by the
    ``AudioCapture`` monkeypatch in ``make_daemon``)."""
    return cast(FakeAudioCapture, daemon._audio)


def _nvim_session(daemon: Daemon) -> FakeNvimSession:
    """As :func:`_transcriber`, for ``daemon._nvim._session`` (statically
    ``NvimSession``, actually the single :class:`FakeNvimSession` the
    ``fake_nvim`` fixture substitutes for every ``Daemon`` in a test)."""
    return cast(FakeNvimSession, daemon._nvim._session)


def _state(daemon: Daemon) -> SessionState:
    """A fresh read of ``daemon._state``, routed through a function call so
    mypy doesn't carry an ``isinstance`` narrowing of one variant (e.g.
    ``Transcribing``) across an intervening ``_dispatch`` call and then flag
    a later ``isinstance(..., Recording)`` check on the same attribute
    expression as unreachable -- the state genuinely does change underneath
    between checks, even though the attribute expression looks the same."""
    return daemon._state


# -- 1. cancelled decode is never appended ------------------------------------


def test_cancelled_decode_is_never_appended(make_daemon: DaemonFactory) -> None:
    """Regression: a decode that finishes after ``Cancelled`` must not reach
    the dictation buffer. This is exactly the bug where a stale in-flight
    decode landed in whatever window happened to be focused seconds later
    (now: got appended to the buffer regardless of the cancellation).
    """
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)
    in_flight_utterance = daemon._utterance

    daemon._dispatch(Cancelled(at=0.6))
    assert isinstance(_state(daemon), Idle)
    assert in_flight_utterance in daemon._aborted

    _transcriber(daemon).next_result = TranscriptionResult(
        text="late result", elapsed_seconds=0.1
    )

    # The decode that was already running when Cancelled arrived completes
    # now, off-band, with the utterance it was started under.
    daemon._worker.run_decode(_audio(daemon).next_samples, in_flight_utterance)

    assert append_calls == []
    assert isinstance(_state(daemon), Idle)


def test_uncancelled_decode_is_appended(make_daemon: DaemonFactory) -> None:
    """Control for the above: without a cancellation, the same shape of
    decode-completing-later *does* reach the append boundary."""
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    utterance = daemon._utterance

    _transcriber(daemon).next_result = TranscriptionResult(
        text="on time result", elapsed_seconds=0.1
    )
    daemon._worker.run_decode(_audio(daemon).next_samples, utterance)

    assert append_calls == ["on time result"]
    assert isinstance(_state(daemon), Idle)


# -- 2. holding the key does not cause an X11 subprocess storm ---------------


def test_holding_key_does_not_flood_the_indicator(make_daemon: DaemonFactory) -> None:
    """Regression: holding the hotkey used to reposition the (since deleted)
    Qt overlay, shelling out to xrandr/xdotool on every dispatched event --
    ~60 subprocesses a second, enough to delay the key-up by 22s. What is
    left on the per-event path is the indicator push, and it must happen on
    an actual phase change only.
    """
    daemon = make_daemon(None)
    phases: list[Phase] = []
    daemon._nvim_phase_changed.connect(phases.append)

    daemon._dispatch(KeyDown(at=0.0))
    # A 2-second hold's worth of events reaching the daemon without ever
    # leaving the Recording phase -- what a held key looked like before
    # auto-repeat was dropped at the hotkey layer.
    for i in range(60):
        daemon._dispatch(KeyDown(at=0.01 * (i + 1)))
    daemon._dispatch(Cancelled(at=2.0))

    assert phases == [Phase.RECORDING, Phase.IDLE]


# -- 3. auto-repeat does not reach the daemon at all --------------------------


def test_auto_repeat_never_invokes_any_callback() -> None:
    """Regression: evdev delivers ~30 auto-repeat events/second while a key
    is held (value=2). These must be dropped at the hotkey layer -- forwarding
    them (even as no-ops in the state machine) is what saturated the Qt
    thread. No ``Daemon``/``QApplication`` involved: ``HotkeyWatcher`` is pure
    evdev/threading plumbing.
    """
    calls: list[tuple[str, float]] = []
    watcher = HotkeyWatcher(
        HotkeyConfig(key_code=186, cancel_key_code=1),
        on_key_down=lambda at, _latch: calls.append(("down", at)),
        on_key_up=lambda at: calls.append(("up", at)),
        on_cancel=lambda at: calls.append(("cancel", at)),
    )

    watcher._handle_key(186, 2)  # value=2: auto-repeat

    assert calls == []


# -- 4. a stray tap is discarded, not decoded ---------------------------------


def test_stray_tap_is_discarded_not_decoded(make_daemon: DaemonFactory) -> None:
    daemon = make_daemon(None)
    decode_calls = _decode_spy(daemon)
    append_calls = _append_spy(daemon)

    held = MIN_HOLD_SECONDS / 2
    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=held))

    assert decode_calls == []
    assert append_calls == []
    assert isinstance(_state(daemon), Idle)
    assert _audio(daemon).start_calls == 1
    assert _audio(daemon).stop_calls == 1


# -- 5. full happy path -------------------------------------------------------


def test_happy_path_key_down_to_appended_postprocessed_text(
    make_daemon: DaemonFactory,
) -> None:
    """key down -> audio captured -> decode -> postprocess -> appended text
    equals the expected post-processed string, proving filler stripping
    happened end to end (not just in ``test_text.py`` isolation), all the
    way down to the real ``_NvimBridge.append`` -> ``NvimSession.append``
    seam."""
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    assert _audio(daemon).start_calls == 1
    daemon._dispatch(KeyUp(at=0.5))
    utterance = daemon._utterance

    _transcriber(daemon).next_result = TranscriptionResult(
        text="um hello world", elapsed_seconds=0.05
    )
    daemon._worker.run_decode(_audio(daemon).next_samples, utterance)

    assert append_calls == ["hello world"]
    assert isinstance(_state(daemon), Idle)

    # Drive the actual append boundary too, with the exact text the daemon
    # asked for, so the real seam between the bridge and the dictation
    # window (faked here) is exercised as well.
    daemon._nvim.append(append_calls[0], False)

    assert _nvim_session(daemon).appended == ["hello world"]


# -- 6. a press during an in-flight decode starts a new capture --------------


def test_press_during_inflight_decode_starts_new_capture_and_both_results_land(
    make_daemon: DaemonFactory,
) -> None:
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)
    first_utterance = daemon._utterance
    first_samples = _audio(daemon).next_samples.copy()

    # A press arrives while the first decode is still "in flight" (we have
    # not yet resolved it) -- must start a new capture, not be dropped.
    daemon._dispatch(KeyDown(at=0.6))
    assert isinstance(_state(daemon), Recording)
    assert _audio(daemon).start_calls == 2

    # The earlier decode now completes; its result must still be appended.
    transcriber.next_result = TranscriptionResult(text="first result", elapsed_seconds=0.1)
    daemon._worker.run_decode(first_samples, first_utterance)
    assert append_calls[-1] == "first result"
    # The daemon is still mid-recording the second utterance.
    assert isinstance(_state(daemon), Recording)

    daemon._dispatch(KeyUp(at=1.0))
    second_utterance = daemon._utterance
    assert second_utterance == first_utterance + 1
    transcriber.next_result = TranscriptionResult(text="second result", elapsed_seconds=0.1)
    daemon._worker.run_decode(_audio(daemon).next_samples, second_utterance)

    assert append_calls == ["first result", "second result"]
    assert isinstance(_state(daemon), Idle)


# -- 7. missing model fails loudly -------------------------------------------


def test_missing_model_raises_model_missing_error(tmp_path: Path, qapp: QApplication) -> None:
    """Constructing a ``Daemon`` against a model directory with no model
    files must raise synchronously, on the caller's thread -- not bury the
    failure in a background traceback while the daemon looks healthy."""
    del qapp
    config = Config(asr=AsrConfig(model_dir=tmp_path / "no-such-model"))

    with pytest.raises(ModelMissingError):
        Daemon(config)


# -- 8. a decode that raises does not kill the daemon -------------------------


def test_decode_that_raises_does_not_kill_daemon_and_returns_to_idle(
    make_daemon: DaemonFactory,
) -> None:
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    utterance = daemon._utterance

    _transcriber(daemon).next_result = RuntimeError("decode blew up")
    daemon._worker.run_decode(_audio(daemon).next_samples, utterance)

    assert append_calls == []
    assert isinstance(_state(daemon), Idle)

    # The daemon is still alive and responsive to new events.
    daemon._dispatch(KeyDown(at=2.0))
    assert isinstance(_state(daemon), Recording)


# -- 9. the open-tail preview is cosmetic ------------------------------------
#
# A tick decodes the open tail once and shows it; that is an extra decode of
# audio that is still growing, so these pin the invariants that keep it on the
# right side of docs/constraints.md: it never reaches the buffer, it is
# bounded, and it can never make the user wait.


def test_preview_mid_recording_reaches_the_nvim_indicator(make_daemon: DaemonFactory) -> None:
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()  # what the preview QTimer does, once a second

    assert len(requests) == 1
    # From the committed offset -- nothing committed yet, so from the start.
    assert _audio(daemon).snapshot_calls == [0]

    samples, start, utterance = requests[0]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="um hello world", elapsed_seconds=0.02
    )
    daemon._worker.run_preview(samples, start, utterance)

    # Post-processed on the way to the indicator, like the committed transcript.
    assert previews[-1] == "hello world"


def test_preview_from_a_previous_utterance_is_dropped(make_daemon: DaemonFactory) -> None:
    """A tick requested before a cancellation must not land in the
    indicator of the *next* utterance -- the same staleness rule the
    committed chunks obey."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    stale = requests[0]

    # Key up, then cancel the decode.
    daemon._dispatch(KeyUp(at=0.5))
    daemon._dispatch(Cancelled(at=0.6))
    assert stale[2] in daemon._aborted

    # A new utterance starts, which re-arms ticks and bumps the id.
    daemon._dispatch(KeyDown(at=1.0))
    assert isinstance(_state(daemon), Recording)
    assert daemon._utterance != stale[2]

    _transcriber(daemon).next_result = TranscriptionResult(
        text="stale preview", elapsed_seconds=0.02
    )
    daemon._worker.run_preview(*stale)

    assert "stale preview" not in previews
    assert previews[-1] == ""


def test_preview_arriving_after_key_up_is_dropped(make_daemon: DaemonFactory) -> None:
    """Once the key is up the daemon is transcribing, and a preview that
    resolves late must not overwrite (or resurrect) the indicator text."""
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    utterance = daemon._utterance
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)

    # Straight from the worker's signal, bypassing the worker-side abandon
    # check, so this pins the *receiving* guard on its own.
    daemon._worker.previewed.emit("late preview", utterance, 0)

    assert "late preview" not in previews
    assert previews[-1] == ""


def test_abandon_flag_skips_a_queued_preview_entirely(make_daemon: DaemonFactory) -> None:
    """Invariant 3, the one that protects decode latency: when the key comes
    up, a preview already queued on the worker must be *skipped*, not decoded
    ahead of the committed decode. The recogniser is single-threaded, so a
    preview that ran here would be pure added latency on the text the user is
    waiting for -- which is how the tool this replaced felt slow.
    """
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    queued = requests[0]

    daemon._dispatch(KeyUp(at=0.5))

    transcriber = _transcriber(daemon)
    decodes_before = len(transcriber.calls)
    transcriber.next_result = TranscriptionResult(text="never decoded", elapsed_seconds=0.02)

    # The queued tick finally reaches the worker, after the key-up.
    daemon._worker.run_preview(*queued)

    assert len(transcriber.calls) == decodes_before, "the preview was decoded anyway"
    assert "never decoded" not in previews
    assert previews[-1] == ""


def test_appended_text_comes_only_from_the_committed_decode(
    make_daemon: DaemonFactory,
) -> None:
    """Previews are cosmetic. Without a segmenter nothing ever settles, so
    even with a preview that decoded to something completely different, the
    appended text is produced by exactly one decode of the complete captured
    buffer at key release.
    """
    daemon = make_daemon(None)
    append_calls = _append_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.preview_samples = np.full(800, 0.5, dtype=np.float32)
    audio.next_samples = np.full(1600, 0.25, dtype=np.float32)
    requests = _preview_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.next_result = TranscriptionResult(text="preview only", elapsed_seconds=0.02)
    daemon._worker.run_preview(*requests[0])
    assert previews[-1] == "preview only"

    daemon._dispatch(KeyUp(at=0.5))
    transcriber.next_result = TranscriptionResult(text="committed text", elapsed_seconds=0.1)
    daemon._worker.run_decode(audio.next_samples, daemon._utterance)

    assert append_calls == ["committed text"]
    # Exactly one decode saw the full buffer, and the preview's window never
    # reached the append boundary in any form.
    full_buffer_decodes = [c for c in transcriber.calls if c.size == audio.next_samples.size]
    assert len(full_buffer_decodes) == 1
    assert np.array_equal(full_buffer_decodes[0], audio.next_samples)
    assert "preview only" not in append_calls


# -- 10. the fakes still match the real interfaces ----------------------------


def _params(member: object) -> list[str]:
    return [p for p in inspect.signature(member).parameters if p != "self"]  # type: ignore[arg-type]


def _assert_stands_in_for(fake: type, real: type) -> None:
    for name, member in inspect.getmembers(real, callable):
        if name.startswith("_"):
            continue
        assert hasattr(fake, name), f"{fake.__name__} is missing {real.__name__}.{name}"
        assert _params(getattr(fake, name)) == _params(member), (
            f"{fake.__name__}.{name} no longer matches {real.__name__}.{name}"
        )
    # `signature` of the class itself is the constructor's, minus `self`.
    assert _params(fake) == _params(real), f"{fake.__name__}() no longer matches {real.__name__}()"


def test_a_stream_that_dies_mid_capture_is_caught_while_the_key_is_still_held(
    make_daemon: DaemonFactory,
) -> None:
    """The failure this was written against: the input stream died 0.2s into a
    hold, the user spoke for 29.8s, and 0.4s of audio came back -- with nothing
    in the log until the *next* keypress reported "no callback for 51.3s".

    ``_ensure_stream_alive`` only runs at ``start_capture``, so the level timer
    is the only thing ticking during an utterance and therefore the only place
    that can notice in time to save it.
    """
    daemon = make_daemon(None)
    audio = _audio(daemon)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    assert audio.recover_calls == 0

    daemon._poll_level()  # healthy: polls, finds nothing wrong, says nothing
    assert audio.recover_calls == 1
    assert previews[-1] == ""

    audio.dead = True
    daemon._poll_level()

    # Recovered, and the user is told on the nvim indicator -- silence in the
    # meter is ambiguous (a quiet room looks identical to a dead device), so
    # it has to say which one it is while they can still act on it. This is
    # pushed unconditionally, not gated on the overlay (off by default).
    assert "no audio" in previews[-1]


def test_the_level_poll_does_not_touch_the_stream_when_idle(
    make_daemon: DaemonFactory,
) -> None:
    """Recovery is scoped to an in-flight capture: reopening a stream that is
    merely idle would throw away the warm pre-roll ring for no reason."""
    daemon = make_daemon(None)
    audio = _audio(daemon)
    audio.dead = True

    daemon._poll_level()

    # The fake reports the reopen it was told to report; the real
    # AudioCapture.recover_if_dead returns False when not capturing. What is
    # pinned here is that the daemon asks rather than deciding for itself.
    assert audio.recover_calls == 1


def test_capture_far_shorter_than_the_hold_is_reported_as_an_error(
    make_daemon: DaemonFactory,
    caplog: pytest.LogCaptureFixture,
) -> None:
    """The old check was ``len(samples) <= preroll``, which this real failure
    slipped past: 5705 samples against a 4000-sample pre-roll, from a 29.8s
    hold. It was logged as an ordinary capture that happened to decode to
    nothing. Held-versus-captured is the comparison that actually detects it.
    """
    daemon = make_daemon(None)
    rate = daemon._config.audio.sample_rate

    with caplog.at_level(logging.ERROR, logger="voice-kb"):
        daemon._log_capture(29.8, np.full(5705, 0.01, dtype=np.float32))
    assert "stopped delivering" in caplog.text

    caplog.clear()
    with caplog.at_level(logging.ERROR, logger="voice-kb"):
        daemon._log_capture(2.0, np.full(2 * rate, 0.01, dtype=np.float32))
    assert caplog.text == ""


def test_previews_stop_past_the_cap_without_clearing_what_is_on_screen(
    make_daemon: DaemonFactory,
) -> None:
    """Without a segmenter nothing settles, every preview decodes the whole
    capture, and cost grows with the utterance -- so it has to stop somewhere.

    Stopping must not look like the failure it was introduced to fix: the text
    already shown stays exactly where it is. Silence from the previewer is not
    the same as a cleared indicator.
    """
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    rate = daemon._config.audio.sample_rate
    cap = daemon._config.preview.max_seconds

    daemon._dispatch(KeyDown(at=0.0))
    audio.preview_samples = np.zeros(int((cap - 1) * rate), dtype=np.float32)
    daemon._request_preview()
    assert len(requests) == 1

    samples, start, utterance = requests[0]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="everything said so far", elapsed_seconds=0.5
    )
    daemon._worker.run_preview(samples, start, utterance)
    assert previews[-1] == "everything said so far"

    # Past the cap: no new request, and critically the old text is untouched.
    audio.preview_samples = np.zeros(int((cap + 5) * rate), dtype=np.float32)
    daemon._request_preview()
    assert len(requests) == 1, "no preview should be issued past the cap"
    assert previews[-1] == "everything said so far"


def test_preview_cadence_backs_off_so_the_worker_stays_half_idle(
    make_daemon: DaemonFactory,
) -> None:
    """A preview inside ``decode_stream`` when the key comes up cannot be
    cancelled, so the fraction of time one is running is the fraction of
    releases that wait for it. The gap is ``max(interval - decode, decode)``,
    making the period ``max(interval, 2 * decode)`` -- a 50% ceiling on the
    duty cycle regardless of how long the utterance gets."""
    daemon = make_daemon(None)
    timer = daemon._preview_timer
    assert timer is not None
    assert timer.isSingleShot(), "a repeating timer cannot express an adaptive gap"

    interval = daemon._config.preview.interval_ms / 1000.0
    daemon._dispatch(KeyDown(at=0.0))

    # Cheap decode: keep the configured cadence exactly.
    daemon._rearm_previews(0.2)
    assert timer.interval() == pytest.approx(int((interval - 0.2) * 1000), abs=2)

    # Expensive decode: back off, so decode/period stays at 50%.
    daemon._rearm_previews(2.0)
    assert timer.interval() == pytest.approx(2000, abs=2)


def test_a_preview_that_shrank_never_replaces_a_longer_one(
    make_daemon: DaemonFactory,
) -> None:
    """Each preview sees strictly more audio than the last, so the transcript
    should only grow -- but the recogniser does not guarantee it. On a real
    sample, decoding 4.4s returned "Okay." where 3.3s had returned "Okay, we
    are now at the new model.". Rendering that verbatim is the disappearing
    text this whole change exists to fix, so a shorter preview is treated as
    instability and the previous text stands."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    daemon._dispatch(KeyDown(at=0.0))

    daemon._request_preview()
    samples, start, utterance = requests[-1]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="Okay, we are now at the new model.", elapsed_seconds=0.3
    )
    daemon._worker.run_preview(samples, start, utterance)
    assert previews[-1] == "Okay, we are now at the new model."

    daemon._request_preview()
    samples, start, utterance = requests[-1]
    _transcriber(daemon).next_result = TranscriptionResult(text="Okay.", elapsed_seconds=0.4)
    daemon._worker.run_preview(samples, start, utterance)
    assert previews[-1] == "Okay, we are now at the new model."

    # Growth still lands, so this is not a freeze.
    daemon._request_preview()
    samples, start, utterance = requests[-1]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="Okay, we are now at the new model. The overlay is there.", elapsed_seconds=0.5
    )
    daemon._worker.run_preview(samples, start, utterance)
    assert previews[-1].endswith("The overlay is there.")


def test_each_utterance_starts_its_preview_from_nothing(
    make_daemon: DaemonFactory,
) -> None:
    """The monotonic guard is per utterance. Without a reset, a long first
    dictation would suppress every shorter preview of the next one."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    previews = _nvim_preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    samples, start, utterance = requests[-1]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="a considerably longer first dictation than the second", elapsed_seconds=0.3
    )
    daemon._worker.run_preview(samples, start, utterance)
    daemon._dispatch(KeyUp(at=5.0))

    daemon._dispatch(KeyDown(at=6.0))
    daemon._request_preview()
    samples, start, utterance = requests[-1]
    _transcriber(daemon).next_result = TranscriptionResult(text="short", elapsed_seconds=0.2)
    daemon._worker.run_preview(samples, start, utterance)
    assert previews[-1] == "short"


def test_every_fake_still_matches_the_interface_it_stands_in_for() -> None:
    """Regression: the last two features each shipped a new method on a real
    boundary class without adding it to the fake, and the whole e2e suite went
    red on an ``AttributeError`` that had nothing to do with the feature. This
    fails on the *fake* instead, pointing straight at the fix.
    """
    _assert_stands_in_for(FakeAudioCapture, AudioCapture)
    _assert_stands_in_for(FakeTranscriber, Transcriber)
    _assert_stands_in_for(FakeNvimSession, NvimSession)
    assert _params(FakeX11.outputs) == _params(x11.outputs)


# -- 11. the nvim seam: opening, appending, and phase changes -----------------


def test_key_down_opens_the_dictation_window(make_daemon: DaemonFactory) -> None:
    """Asked for on every key-down, not just once at startup: the window is
    the user's to close, so the next dictation must reopen it. Requested while
    the utterance is still being spoken, in parallel with the recording."""
    daemon = make_daemon(None)
    open_calls: list[None] = []
    daemon._nvim_open_requested.connect(lambda: open_calls.append(None))

    daemon._dispatch(KeyDown(at=0.0))

    assert len(open_calls) == 1


def test_failed_append_is_reported_and_does_not_take_the_daemon_down(
    make_daemon: DaemonFactory,
    caplog: pytest.LogCaptureFixture,
) -> None:
    """The decode succeeded and the text exists; it just could not be
    delivered to the dictation buffer. That must be visible in the log and
    must not take the daemon down -- the text is recoverable (it is already
    logged at INFO from ``_on_decoded``), and a broken editor connection is
    not a reason to stop listening for the next dictation.
    """
    daemon = make_daemon(None)
    _nvim_session(daemon).append_result = AppendFailed(reason="nvim went away")

    with caplog.at_level(logging.ERROR, logger="voice-kb"):
        daemon._nvim.append("some text", False)

    assert "could not append" in caplog.text
    assert "nvim went away" in caplog.text

    # Still alive and responsive to the next dictation.
    daemon._dispatch(KeyDown(at=0.0))
    assert isinstance(_state(daemon), Recording)


def test_phase_changes_reach_the_nvim_bridge(make_daemon: DaemonFactory) -> None:
    daemon = make_daemon(None)
    phases: list[Phase] = []
    daemon._nvim_phase_changed.connect(phases.append)

    daemon._dispatch(KeyDown(at=0.0))

    assert phases == [Phase.RECORDING]


# ------------------------------------------------ incremental (segmented) decode


class _FakeSegmenter:
    """Splits a capture into ``count`` equal pieces, like a VAD that found
    ``count`` runs of speech. Stands in for ``voice_kb.vad.SpeechSegmenter``
    so these tests need no model and no real audio.

    Every piece but the last is ``settled`` -- the rule the real segmenter
    applies to chunks before the last -- and the last is settled only with
    ``settle_last``, standing in for a closed chunk followed by a second of
    silence. ``end_frame`` is where each piece ends in the input, so the
    worker's committed offset advances exactly as it would on real audio.

    ``counts`` scripts a different number per call, which is what ticks
    need: the real segmenter sees a shorter remainder each time as settled
    chunks are committed, and so returns fewer chunks, while a fixed
    ``count`` would keep splitting whatever remains into the same number of
    pieces forever.
    """

    def __init__(
        self, count: int, counts: list[int] | None = None, *, settle_last: bool = False
    ) -> None:
        self.count = count
        self.counts = counts or []
        self.settle_last = settle_last
        self.calls: list[MonoAudio] = []

    def split(self, samples: MonoAudio) -> list[Segment]:
        self.calls.append(samples)
        n = self.counts.pop(0) if self.counts else self.count
        step = max(1, samples.size // n)
        return [
            Segment(
                samples=samples[i * step : (i + 1) * step],
                start_seconds=float(i),
                end_frame=(i + 1) * step,
                settled=i < n - 1 or self.settle_last,
            )
            for i in range(n)
        ]


def test_each_segment_lands_as_it_decodes_and_the_utterance_stays_one_paragraph(
    make_daemon: DaemonFactory,
) -> None:
    """The point of segmenting: text appears while the rest is still decoding.

    Three segments means three appends, not one at the end -- and the first
    opens a paragraph while the rest extend it, so the user sees a paragraph
    growing rather than three of them appearing.
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    _transcriber(daemon).results = [
        TranscriptionResult(text=text, elapsed_seconds=0.1)
        for text in ("first piece", "second piece", "third piece")
    ]

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert paragraphs == [
        ("first piece", False),
        ("second piece", True),
        ("third piece", True),
    ]


def test_a_segment_that_decodes_to_nothing_does_not_open_a_paragraph(
    make_daemon: DaemonFactory,
) -> None:
    """An empty segment must not consume the "first" slot.

    If it did, the next real segment would arrive with ``continued=True`` and
    be glued onto whatever paragraph happened to be above it -- the previous
    utterance's.
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    _transcriber(daemon).results = [
        TranscriptionResult(text=text, elapsed_seconds=0.1)
        for text in ("   ", "real text", "more text")
    ]

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert paragraphs == [("real text", False), ("more text", True)]


def test_a_new_utterance_starts_a_new_paragraph(make_daemon: DaemonFactory) -> None:
    """``continued`` is per utterance, not per daemon.

    Without it, every dictation after the first would be glued onto the
    previous one -- exactly the "appending to the old dictation" behaviour the
    one-file-per-window work removed.
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(2))

    for text in ("first utterance", "second utterance"):
        daemon._dispatch(KeyDown(at=0.0))
        daemon._dispatch(KeyUp(at=0.5))
        _transcriber(daemon).results = [
            TranscriptionResult(text=text, elapsed_seconds=0.1),
            TranscriptionResult(text="tail", elapsed_seconds=0.1),
        ]
        daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert [continued for _, continued in paragraphs] == [False, True, False, True]


def test_cancelling_stops_the_decode_between_segments(make_daemon: DaemonFactory) -> None:
    """A cancelled long decode stops adding text rather than running to the end.

    What already landed stays -- it is the user's file, and with progressive
    delivery a cancellation can arrive after some of it is written. The
    guarantee is "stop adding", not "none of this happened".
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(4))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    utterance = daemon._utterance
    daemon._worker.abandon(utterance)

    daemon._worker.run_decode(_audio(daemon).next_samples, utterance)

    assert paragraphs == []


def test_without_a_segmenter_the_whole_buffer_is_decoded_exactly_as_before(
    make_daemon: DaemonFactory,
) -> None:
    """A missing VAD model is a lost improvement, not a broken daemon."""
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    assert daemon._worker._segmenter is None

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    _transcriber(daemon).next_result = TranscriptionResult(
        text="um the whole thing", elapsed_seconds=0.1
    )

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert paragraphs == [("the whole thing", False)]
    assert len(_transcriber(daemon).calls) == 1


# ------------------------------------------------- the chunking safety net


def test_an_utterance_is_never_lost_to_chunking(make_daemon: DaemonFactory) -> None:
    """If every chunk decodes to nothing, the whole buffer is decoded instead.

    This is not hypothetical: a 2.8s capture at peak 0.28 -- unmistakably
    speech -- came back empty from every chunk in real use, and the words were
    simply gone. The floor this buys is that segmentation can never do worse
    than not segmenting.
    """
    daemon = make_daemon(None)
    appended = _append_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    _transcriber(daemon).results = [
        TranscriptionResult(text="", elapsed_seconds=0.1),
        TranscriptionResult(text="", elapsed_seconds=0.1),
        TranscriptionResult(text="", elapsed_seconds=0.1),
        TranscriptionResult(text="the whole thing after all", elapsed_seconds=0.3),
    ]

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert appended == ["the whole thing after all"]
    assert len(_transcriber(daemon).calls) == 4, "three chunks, then the retry"


def test_the_retry_does_not_fire_when_a_chunk_produced_text(
    make_daemon: DaemonFactory,
) -> None:
    """A partly-empty decode is a normal decode -- pauses produce empty chunks
    all the time. Retrying then would decode everything twice."""
    daemon = make_daemon(None)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    _transcriber(daemon).results = [
        TranscriptionResult(text="", elapsed_seconds=0.1),
        TranscriptionResult(text="something", elapsed_seconds=0.1),
        TranscriptionResult(text="", elapsed_seconds=0.1),
    ]

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert len(_transcriber(daemon).calls) == 3, "no retry"


def test_a_cancelled_decode_is_not_retried(make_daemon: DaemonFactory) -> None:
    """Cancelling means stop working, not fall back to the slow path."""
    daemon = make_daemon(None)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))
    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    daemon._worker.abandon(daemon._utterance)

    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert _transcriber(daemon).calls == []


# --------------------------------------------------------- progressive commit
#
# docs/progressive-commit.md. A tick commits the chunks that have settled and
# previews the open tail; the release decodes only what is past the committed
# offset. The offset lives on the worker; the daemon holds a lagging hint.


def _result(text: str) -> TranscriptionResult:
    return TranscriptionResult(text=text, elapsed_seconds=0.1)


def test_a_settled_chunk_lands_while_still_recording(make_daemon: DaemonFactory) -> None:
    """The point of the change: text is in the buffer before the key comes up.

    Three chunks on the tick. The first two are settled and are appended as
    one growing paragraph; the third is the open tail and is only previewed.
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    requests = _preview_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    _transcriber(daemon).results = [
        _result(t) for t in ("first settled", "second settled", "open tail")
    ]
    daemon._worker.run_preview(*requests[-1])

    assert isinstance(_state(daemon), Recording)
    assert paragraphs == [("first settled", False), ("second settled", True)]
    assert previews[-1] == "open tail"


def test_the_release_decodes_only_the_audio_past_the_committed_offset(
    make_daemon: DaemonFactory,
) -> None:
    """Every committed sample is decoded exactly once: what a tick committed is
    never handed to the recogniser again, at release or ever."""
    daemon = make_daemon(None)
    appended = _append_spy(daemon)
    requests = _preview_spy(daemon)
    audio = _audio(daemon)
    audio.preview_samples = np.arange(900, dtype=np.float32)
    audio.next_samples = np.arange(1500, dtype=np.float32)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.results = [_result(t) for t in ("a", "b", "tail")]
    daemon._worker.run_preview(*requests[-1])
    assert daemon._worker._committed_from == 600
    assert daemon._committed_hint == 600

    daemon._dispatch(KeyUp(at=5.0))
    release_from = len(transcriber.calls)
    transcriber.results = [_result(t) for t in ("c", "d", "e")]
    daemon._worker.run_decode(audio.next_samples, daemon._utterance)

    assert appended == ["a", "b", "c", "d", "e"]
    # The release split frames 600..1500 and never looked at 0..600.
    assert all(call.min() >= 600 for call in transcriber.calls[release_from:])
    assert sum(call.size for call in transcriber.calls[release_from:]) == 900
    assert daemon._worker._committed_from == 0, "reset for the next utterance"
    assert isinstance(_state(daemon), Idle)


def test_the_next_tick_is_asked_for_the_audio_past_the_hint(make_daemon: DaemonFactory) -> None:
    """The snapshot starts at the daemon's copy of the offset, so a tick's cost
    is bounded by what is uncommitted rather than by the utterance -- and the
    worker slices from its own offset, so a hint that lags is harmless."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    audio = _audio(daemon)
    audio.preview_samples = np.arange(900, dtype=np.float32)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3, counts=[3, 1]))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    assert audio.snapshot_calls == [0]
    transcriber.results = [_result(t) for t in ("a", "b", "tail")]
    daemon._worker.run_preview(*requests[-1])

    daemon._request_preview()
    assert audio.snapshot_calls == [0, 600]
    samples, start, utterance = requests[-1]
    assert (start, samples.size) == (600, 300)

    # A lagging hint: the worker is at 600, the daemon asked from 300.
    stale = audio.preview_samples[300:]
    calls_before = len(transcriber.calls)
    transcriber.results = [_result("longer tail")]
    daemon._worker.run_preview(stale, 300, utterance)

    assert len(transcriber.calls) == calls_before + 1
    assert transcriber.calls[-1].min() >= 600, "nothing below the real offset was decoded"


def test_a_chunk_landing_resets_the_shrink_guard(make_daemon: DaemonFactory) -> None:
    """Within one offset a shorter preview is instability and is ignored; once
    a chunk commits, the tail starts over and a shorter one is the truth."""
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    requests = _preview_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(1, counts=[1, 1, 2]))
    transcriber = _transcriber(daemon)
    daemon._dispatch(KeyDown(at=0.0))

    daemon._request_preview()
    transcriber.results = [_result("a long open tail so far")]
    daemon._worker.run_preview(*requests[-1])
    assert previews[-1] == "a long open tail so far"

    daemon._request_preview()
    transcriber.results = [_result("a long")]
    daemon._worker.run_preview(*requests[-1])
    assert previews[-1] == "a long open tail so far", "a shorter tail at the same offset is noise"

    daemon._request_preview()
    transcriber.results = [_result("a long open tail so far."), _result("so")]
    daemon._worker.run_preview(*requests[-1])
    assert paragraphs == [("a long open tail so far.", False)]
    assert previews[-1] == "so", "the tail restarted below the chunk that landed"


def test_a_cancel_during_recording_stops_further_commits(make_daemon: DaemonFactory) -> None:
    """Cancel means stop adding. A chunk from a tick that was already mid-decode
    when the cancel landed is dropped; what landed before it stays."""
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    requests = _preview_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(2))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.results = [_result("landed"), _result("tail")]
    daemon._worker.run_preview(*requests[-1])
    assert paragraphs == [("landed", False)]

    cancelled = daemon._utterance
    daemon._dispatch(Cancelled(at=1.0))
    assert isinstance(_state(daemon), Idle)
    # Straight from the worker's signal: a chunk a tick had already decoded.
    daemon._worker.committed.emit("late chunk", cancelled, 800)
    assert paragraphs == [("landed", False)]

    # The next utterance re-arms ticks, but only for itself: a tick for the
    # cancelled utterance is refused by id, not by a flag the re-arm cleared.
    daemon._dispatch(KeyDown(at=2.0))
    calls_before = len(transcriber.calls)
    daemon._worker.run_preview(_audio(daemon).preview_samples, 0, cancelled)
    assert len(transcriber.calls) == calls_before


def test_a_slow_decode_from_the_previous_utterance_opens_its_own_paragraph(
    make_daemon: DaemonFactory,
) -> None:
    """A chunk extends the last paragraph only if it is from the same utterance.

    The old per-decode boolean was reset by the *next* utterance's decode, so
    a late chunk of the previous one could open a fresh paragraph mid-passage
    or glue itself onto the new one. It also must not settle the newer
    capture's state when it finishes.
    """
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(2))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    first = daemon._utterance
    # The next utterance starts, and its first chunk lands, before the first
    # utterance's decode has finished.
    daemon._dispatch(KeyDown(at=1.0))
    daemon._worker.committed.emit("second utterance", daemon._utterance, 400)
    transcriber.results = [_result("first a"), _result("first b")]
    daemon._worker.run_decode(_audio(daemon).next_samples, first)

    assert paragraphs == [
        ("second utterance", False),
        ("first a", False),
        ("first b", True),
    ]
    assert isinstance(_state(daemon), Recording), "the old decode must not settle the new capture"


def test_the_offset_never_carries_into_the_next_utterance(make_daemon: DaemonFactory) -> None:
    """Explicitly, by utterance id -- not by noticing the capture got shorter."""
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    requests = _preview_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(2))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.results = [_result("old settled"), _result("old tail")]
    daemon._worker.run_preview(*requests[-1])
    daemon._dispatch(KeyUp(at=0.5))
    transcriber.results = [_result("old c"), _result("old d")]
    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    daemon._dispatch(KeyDown(at=1.0))
    daemon._request_preview()
    assert requests[-1][1] == 0, "the hint is reset with the capture"
    transcriber.results = [_result("new settled"), _result("new tail")]
    daemon._worker.run_preview(*requests[-1])

    assert paragraphs == [
        ("old settled", False),
        ("old c", True),
        ("old d", True),
        ("new settled", False),
    ]
    assert previews[-1] == "new tail"


def test_without_a_segmenter_nothing_settles_before_release(make_daemon: DaemonFactory) -> None:
    """A missing VAD model is a lost improvement, not a broken daemon: the
    tick previews the whole capture and the release decodes all of it."""
    daemon = make_daemon(None)
    assert daemon._worker._segmenter is None
    paragraphs = _paragraph_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    requests = _preview_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.next_result = _result("everything so far")
    daemon._worker.run_preview(*requests[-1])
    assert paragraphs == []
    assert previews[-1] == "everything so far"

    daemon._dispatch(KeyUp(at=0.5))
    transcriber.next_result = _result("the whole thing")
    daemon._worker.run_decode(_audio(daemon).next_samples, daemon._utterance)

    assert paragraphs == [("the whole thing", False)]
    assert transcriber.calls[-1].size == _audio(daemon).next_samples.size


def test_the_whole_buffer_retry_covers_only_the_remainder(make_daemon: DaemonFactory) -> None:
    """The chunking safety net still fires at release, over the audio the
    release is responsible for -- not over chunks a tick already committed."""
    daemon = make_daemon(None)
    appended = _append_spy(daemon)
    requests = _preview_spy(daemon)
    audio = _audio(daemon)
    audio.preview_samples = np.arange(900, dtype=np.float32)
    audio.next_samples = np.arange(1500, dtype=np.float32)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(3))
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.results = [_result(t) for t in ("a", "b", "tail")]
    daemon._worker.run_preview(*requests[-1])

    daemon._dispatch(KeyUp(at=5.0))
    transcriber.results = [_result(""), _result(""), _result(""), _result("recovered")]
    daemon._worker.run_decode(audio.next_samples, daemon._utterance)

    assert appended == ["a", "b", "recovered"]
    retry = transcriber.calls[-1]
    assert retry.size == 900 and retry.min() >= 600, "the retry decoded the remainder, once"


def test_a_tick_that_raises_keeps_the_utterance_ticking(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """One bad decode must not end progressive commit for the rest of a
    passage: the timer is re-armed off the failure, and it is said once."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    paragraphs = _paragraph_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(2))
    transcriber = _transcriber(daemon)
    timer = daemon._preview_timer
    assert timer is not None

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    timer.stop()  # as it is while a tick is in flight
    transcriber.next_result = RuntimeError("onnx hiccup")
    with caplog.at_level(logging.WARNING, logger="voice-kb"):
        daemon._worker.run_preview(*requests[-1])

    assert "a tick failed" in caplog.text
    assert timer.isActive(), "the next tick is scheduled despite the failure"

    daemon._request_preview()
    transcriber.next_result = _result("recovered")
    daemon._worker.run_preview(*requests[-1])
    assert paragraphs == [("recovered", False)]


def test_rearming_for_the_next_utterance_does_not_revive_a_queued_tick(
    make_daemon: DaemonFactory,
) -> None:
    """A tick queued for utterance N when its key came up, reaching the worker
    after utterance N+1 has re-armed ticking, must still be skipped: it would
    otherwise run ahead of N's release decode."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    queued = requests[0]
    daemon._dispatch(KeyUp(at=0.5))
    daemon._dispatch(KeyDown(at=0.6))  # re-arms, for the new utterance only

    calls_before = len(transcriber.calls)
    daemon._worker.run_preview(*queued)

    assert len(transcriber.calls) == calls_before


def test_a_settled_last_chunk_is_committed_too(make_daemon: DaemonFactory) -> None:
    """When the buffer ends in a second of silence after a closed chunk, the
    segmenter marks the last chunk settled and there is no tail to show."""
    daemon = make_daemon(None)
    paragraphs = _paragraph_spy(daemon)
    previews = _nvim_preview_spy(daemon)
    requests = _preview_spy(daemon)
    daemon._worker._segmenter = cast(SpeechSegmenter, _FakeSegmenter(1, settle_last=True))

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    _transcriber(daemon).results = [_result("all of it, settled")]
    daemon._worker.run_preview(*requests[-1])

    assert paragraphs == [("all of it, settled", False)]
    assert previews[-1] == ""
    assert daemon._committed_hint == _audio(daemon).preview_samples.size


# -- 12. the recording, and recovering a transcript from it -------------------


def test_the_capture_log_names_the_recording_it_landed_in(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """The two lines are only useful together: one says how much audio there
    was, the other says which file holds it. That pairing is what makes a lost
    transcript recoverable after the fact rather than merely regrettable."""
    daemon = make_daemon(None)
    _audio(daemon).recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )

    daemon._dispatch(KeyDown(at=0.0))
    with caplog.at_level(logging.INFO, logger="voice-kb"):
        daemon._dispatch(KeyUp(at=2.0))

    assert "captured 2.0s held" in caplog.text
    assert "recorded to /state/voice-kb/audio/capture-2026-09-08-141530.wav" in caplog.text


def test_hitting_the_ceiling_is_loud_in_the_log_and_on_the_indicator(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """What the 2026-09-08 loss did not do.

    The cap fired with no log line and no indicator, and the docstring at the
    time claimed it was "easy to notice in the overlay". It now says so at
    WARNING *and* where the user is looking mid-utterance, and it says what to
    do about it -- the audio is on disk, so this is recoverable news rather
    than a loss.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    with caplog.at_level(logging.WARNING, logger="voice-kb"):
        daemon._poll_level()

    assert "in-memory ceiling" in caplog.text
    assert "voice-kb transcribe /state/voice-kb/audio/capture-2026-09-08-141530.wav" in caplog.text
    assert "voice-kb transcribe" in previews[-1]


def test_the_cap_warning_survives_the_preview_that_would_have_erased_it(
    make_daemon: DaemonFactory,
) -> None:
    """The warning has to still be there a minute later, not for one cycle.

    Previews share the indicator's preview field, so the next one due ~1.1s
    after the cap bit would have overwritten the notice and left the user
    looking at a stale transcript for the rest of a 70-minute hold -- a cap
    that is visible for one second is barely better than the silent one that
    lost 3m41s. Past the ceiling the buffer is frozen anyway, so every further
    preview is byte-identical work.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    daemon._poll_level()
    warning = previews[-1]

    # The preview timer is stopped for the rest of the capture...
    assert daemon._preview_timer is not None
    assert not daemon._preview_timer.isActive()
    # ...and a preview that was already in flight cannot land afterwards.
    # Deliberately *longer* than the notice: the "previews only grow" rule
    # that protects a preview from a shorter one would happily let this one
    # through, so this only passes because being capped is a state the preview
    # path cannot write through, not because of the length comparison.
    late = "the words of a preview that was already decoding when the cap bit " * 6
    assert len(late) > len(warning)
    daemon._on_previewed(late, daemon._utterance, 0)

    assert previews[-1] == warning
    assert not daemon._preview_timer.isActive(), "and it did not re-arm the timer"
    assert "voice-kb transcribe" in warning


def test_a_dead_microphone_replaces_the_cap_notice_it_has_invalidated(
    make_daemon: DaemonFactory,
) -> None:
    """The one message allowed to overwrite the capped notice.

    The notice promises the rest of the audio is on disk and recoverable with
    ``voice-kb transcribe``. The recorder keeps writing past the ceiling, so
    when the input stream dies -- a live bug here, three occurrences in
    STATUS.md -- what lands in the wav from then on is silence and the promise
    is false. This is not a staler message losing a ranking contest; it is
    news that invalidates what is on screen, so it replaces it and says what
    is actually true about the recording.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    daemon._poll_level()
    assert "voice-kb transcribe" in previews[-1]

    audio.dead = True  # the microphone drops out after the cap
    daemon._poll_level()

    assert "no audio from the microphone" in previews[-1]
    assert "silent gap" in previews[-1], "and it does not repeat the false promise"
    assert "voice-kb transcribe" not in previews[-1]


def test_the_dead_microphone_notice_does_not_invent_a_recording(
    make_daemon: DaemonFactory,
) -> None:
    """With ``[recording]`` off there is no wav to have a gap in.

    Branching on ``_capped`` alone made this message assert a recording
    exists: it replaced the correct "audio from here is being discarded" with
    a promise of a silent gap in a file that was never opened. That is the
    same lie as offering `voice-kb transcribe` for a truncated file, pointing
    the other way -- so it branches on the recording, not on the cap.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = NotRecorded()
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    daemon._poll_level()
    audio.dead = True
    daemon._poll_level()

    assert "no audio from the microphone" in previews[-1]
    assert "discarded" in previews[-1]
    assert "silent gap" not in previews[-1], "there is no file to have a gap in"


def test_a_cap_crossed_in_the_last_tick_is_still_reported(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """The 30Hz poll cannot see a ceiling crossed between the last tick and the
    key coming up. Left there, the transcript would stop short with nothing
    anywhere saying why -- the original failure in miniature."""
    daemon = make_daemon(None)
    audio = _audio(daemon)
    audio.recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )

    daemon._dispatch(KeyDown(at=0.0))
    audio.capped = True  # crossed after the last _poll_level of the hold
    with caplog.at_level(logging.WARNING, logger="voice-kb"):
        daemon._dispatch(KeyUp(at=2.0))

    assert "in-memory ceiling" in caplog.text


def test_the_ceiling_warning_says_when_nothing_is_being_recorded(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """With recording off there is no file to recover from, so the advice has
    to be the opposite one: stop now, because audio is being discarded."""
    daemon = make_daemon(None)
    _audio(daemon).capped = True

    daemon._dispatch(KeyDown(at=0.0))
    with caplog.at_level(logging.WARNING, logger="voice-kb"):
        daemon._poll_level()

    assert "being discarded" in caplog.text


def _recorded(tmp_path: Path, samples: MonoAudio, rate: int = 16000) -> Path:
    """A real recording, written by the real recorder -- so these tests decode
    the same bytes the daemon would have left behind."""
    recorder = CaptureRecorder(RecordingConfig(dir=tmp_path / "audio"), rate)
    recorder.start()
    recorder.write(samples)
    recorder.stop()
    status = recorder.status()
    assert isinstance(status, Recorded)
    return status.path


def _faked_pipeline(
    monkeypatch: pytest.MonkeyPatch, text: str
) -> FakeTranscriber:
    fake = FakeTranscriber()
    fake.next_result = TranscriptionResult(text=text, elapsed_seconds=0.1)
    monkeypatch.setattr(app_module, "Transcriber", lambda config: fake)
    monkeypatch.setattr(app_module, "load_segmenter", lambda config, rate: None)
    return fake


def test_transcribe_recovers_the_transcript_from_a_recording(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The recovery path end to end: a wav on disk in, the transcript out.

    It goes through the same ``decode_capture`` the daemon's worker runs, so
    what comes back is what the live decode would have produced -- that shared
    pipeline is the whole reason a recording is worth keeping.
    """
    samples = np.linspace(-0.5, 0.5, 8000, dtype=np.float32)
    wav = _recorded(tmp_path, samples)
    fake = _faked_pipeline(monkeypatch, "the words that were nearly lost")
    out = tmp_path / "recovered.txt"

    assert app_module.transcribe_recording(wav, out, Config()) == 0

    assert out.read_text(encoding="utf-8") == "the words that were nearly lost\n"
    assert len(fake.calls) == 1
    assert np.allclose(fake.calls[0], samples, atol=2e-5), "it decoded the recorded audio"


def test_transcribe_writes_to_stdout_when_no_out_is_given(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """Logging goes to stderr, so stdout carries the transcript alone and this
    composes with a pipe."""
    wav = _recorded(tmp_path, np.linspace(-0.5, 0.5, 1600, dtype=np.float32))
    _faked_pipeline(monkeypatch, "um, straight to stdout")

    assert app_module.transcribe_recording(wav, None, Config()) == 0

    # Post-processed exactly as the live path would have: fillers stripped.
    assert capsys.readouterr().out == "straight to stdout\n"


def test_transcribe_refuses_a_recording_at_the_wrong_sample_rate(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Decoding 8 kHz audio as if it were 16 kHz returns plausible nonsense
    rather than an error, which is worse than refusing."""
    wav = _recorded(tmp_path, np.zeros(800, dtype=np.float32), rate=8000)
    _faked_pipeline(monkeypatch, "never reached")

    assert app_module.transcribe_recording(wav, None, Config()) == 4


def test_a_bare_invocation_still_starts_the_daemon(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The subcommand is optional. ``voice-kb`` with no arguments is what the
    systemd unit runs and what every existing habit types."""
    started: list[Path | None] = []

    def fake_daemon(config: Config, dump_audio: Path | None) -> int:
        started.append(dump_audio)
        return 0

    monkeypatch.setattr(sys, "argv", ["voice-kb", "-c", str(tmp_path / "absent.toml")])
    monkeypatch.setattr(app_module, "_configure_logging", lambda **kwargs: None)
    monkeypatch.setattr(app_module, "run_daemon", fake_daemon)

    assert app_module.main() == 0
    assert started == [None]


def test_the_transcribe_subcommand_never_reaches_the_daemon(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No Qt, no hotkey, no microphone: recovery runs in a terminal over a
    file, possibly while the daemon is running in another one."""
    recovered: list[tuple[Path, Path | None]] = []

    def fake_recovery(wav: Path, out: Path | None, config: Config) -> int:
        recovered.append((wav, out))
        return 0

    monkeypatch.setattr(
        sys,
        "argv",
        [
            "voice-kb",
            "-c",
            str(tmp_path / "absent.toml"),
            "transcribe",
            "capture.wav",
            "--out",
            "t.txt",
        ],
    )
    monkeypatch.setattr(app_module, "_configure_logging", lambda **kwargs: None)
    monkeypatch.setattr(app_module, "run_daemon", lambda config, dump_audio: pytest.fail("daemon"))
    monkeypatch.setattr(app_module, "transcribe_recording", fake_recovery)

    assert app_module.main() == 0
    assert recovered == [(Path("capture.wav"), Path("t.txt"))]


def test_a_capped_capture_does_not_silence_the_one_after_it(
    make_daemon: DaemonFactory,
) -> None:
    """``_capped`` latches for a capture, not for the process.

    Left set, the daemon would go quiet for good after one long dictation:
    every later preview dropped before it reached either indicator, with the
    stale notice still on screen. That is the regression this latch is one
    mistake away from, so it is pinned here.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = Recorded(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    daemon._poll_level()
    notice = previews[-1]
    daemon._on_previewed("refused while capped", daemon._utterance, 0)
    assert previews[-1] == notice
    daemon._dispatch(KeyUp(at=2.0))

    daemon._dispatch(KeyDown(at=3.0))  # a new capture, a clean slate

    daemon._on_previewed("the next dictation", daemon._utterance, 0)
    assert previews[-1] == "the next dictation"


def test_a_truncated_recording_is_never_offered_as_a_recovery(
    make_daemon: DaemonFactory, caplog: pytest.LogCaptureFixture
) -> None:
    """Both callers of the recording status have to tell the truth.

    A recording given up on at 200s does not support "recover the rest with
    `voice-kb transcribe`" -- that is the same false reassurance the silent
    cap gave, dressed up as a fix. When the file is short, the cap notice says
    so and the capture line says so.
    """
    daemon = make_daemon(None)
    previews = _nvim_preview_spy(daemon)
    audio = _audio(daemon)
    audio.recording = Truncated(
        path=Path("/state/voice-kb/audio/capture-2026-09-08-141530.wav")
    )
    audio.capped = True

    daemon._dispatch(KeyDown(at=0.0))
    with caplog.at_level(logging.WARNING, logger="voice-kb"):
        daemon._poll_level()
        daemon._dispatch(KeyUp(at=2.0))

    assert "voice-kb transcribe" not in caplog.text
    assert "voice-kb transcribe" not in previews[-1]
    assert "recording failed" in previews[-1]
    assert "was given up on part-way" in caplog.text
