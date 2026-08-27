"""End-to-end regression tests for the seams between ``voice_kb``'s modules.

72 unit tests were green while, in production: a cancelled decode still got
injected, a missing model silently produced nothing, and holding the hotkey
spawned ~60 subprocesses/second and stalled the whole event loop. Unit tests
on ``state.py``/``text.py``/etc. in isolation cannot see any of that -- these
tests drive the real :class:`~voice_kb.app.Daemon` (state machine, ``_apply``,
``_sync_overlay``, ``_on_decoded``, the generation counter) against fakes for
every hardware/subprocess boundary, so a regression in how those pieces are
wired together shows up here even when every module still passes in
isolation.

Hardware-free by construction: ``conftest.py`` sets
``QT_QPA_PLATFORM=offscreen`` before PySide6 is ever imported, and every
``Daemon`` built by the ``make_daemon`` fixture has a fake ``AudioCapture``,
a fake ``inject_text``, and faked ``voice_kb.x11`` queries. No test here
starts ``HotkeyWatcher`` or ``Daemon.start()`` (that would open real
``/dev/input`` nodes and touch a real X server) and no test pastes into a
real window -- the whole point of a fake ``inject_text`` is that a bug can be
caught without ever risking that again.

Driving the worker: ``Daemon``'s worker thread is never started (see
``make_daemon`` in ``conftest.py`` for why). Instead, wherever production
code would cross to the worker thread via a queued signal
(``_decode_requested`` -> ``_Worker.run_decode``, ``_inject_requested`` ->
``_Worker.run_inject``, ``_preview_requested`` -> ``_Worker.run_preview``),
tests call the real worker method directly on the main thread. Because the
*receiving* end of ``_worker.decoded``/
``decode_failed`` (``Daemon._on_decoded``/``_on_decode_failed``) lives on the
main thread and the call happens from the main thread, Qt resolves that
connection to a direct (synchronous) call -- so the real generation check in
``_on_decoded`` still runs for real; only the thread hop itself is skipped.
"""

from __future__ import annotations

import inspect
import logging
from collections.abc import Callable
from pathlib import Path
from typing import cast

import numpy as np
import pytest
from fakes import FakeAudioCapture, FakeInjector, FakeTranscriber, FakeX11
from PySide6.QtWidgets import QApplication

from voice_kb import x11
from voice_kb.app import Daemon
from voice_kb.asr import ModelMissingError, Transcriber, TranscriptionResult
from voice_kb.audio import AudioCapture, MonoAudio
from voice_kb.config import AsrConfig, Config, HotkeyConfig, PasteConfig
from voice_kb.geometry import Rect
from voice_kb.hotkey import HotkeyWatcher
from voice_kb.inject import inject_text
from voice_kb.overlay import Overlay
from voice_kb.state import (
    MIN_HOLD_SECONDS,
    Cancelled,
    Idle,
    KeyDown,
    KeyUp,
    Recording,
    SessionState,
    Transcribing,
)

pytestmark = pytest.mark.usefixtures("qapp")

type DaemonFactory = Callable[[Config | None], Daemon]


def _inject_spy(daemon: Daemon) -> list[tuple[str, PasteConfig]]:
    """Records every ``(text, paste_config)`` the daemon asked to have
    injected, by connecting straight to the ``_inject_requested`` signal.

    A plain function connected to a Qt signal is always invoked directly
    (Qt has no thread affinity to queue against for a bare callable), so
    this fires synchronously the moment ``_on_decoded`` emits it -- no
    worker thread required to observe it.
    """
    calls: list[tuple[str, PasteConfig]] = []
    daemon._inject_requested.connect(lambda text, paste: calls.append((text, paste)))
    return calls


def _decode_spy(daemon: Daemon) -> list[int]:
    """Records every generation a decode was requested for."""
    calls: list[int] = []
    daemon._decode_requested.connect(lambda samples, generation: calls.append(generation))
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


def _state(daemon: Daemon) -> SessionState:
    """A fresh read of ``daemon._state``, routed through a function call so
    mypy doesn't carry an ``isinstance`` narrowing of one variant (e.g.
    ``Transcribing``) across an intervening ``_dispatch`` call and then flag
    a later ``isinstance(..., Recording)`` check on the same attribute
    expression as unreachable -- the state genuinely does change underneath
    between checks, even though the attribute expression looks the same."""
    return daemon._state


# -- 1. cancelled decode is never injected -----------------------------------


def test_cancelled_decode_is_never_injected(make_daemon: DaemonFactory) -> None:
    """Regression: a decode that finishes after ``Cancelled`` must not reach
    injection. This is exactly the bug where a stale in-flight decode landed
    in whatever window happened to be focused seconds later.
    """
    daemon = make_daemon(None)
    inject_calls = _inject_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)
    in_flight_generation = daemon._generation

    daemon._dispatch(Cancelled(at=0.6))
    assert isinstance(_state(daemon), Idle)
    assert daemon._generation == in_flight_generation + 1

    _transcriber(daemon).next_result = TranscriptionResult(
        text="late result", elapsed_seconds=0.1
    )

    # The decode that was already running when Cancelled arrived completes
    # now, off-band, with the generation it was started under.
    daemon._worker.run_decode(_audio(daemon).next_samples, in_flight_generation)

    assert inject_calls == []
    assert isinstance(_state(daemon), Idle)


def test_uncancelled_decode_is_injected(make_daemon: DaemonFactory) -> None:
    """Control for the above: without a cancellation, the same shape of
    decode-completing-later *does* reach injection."""
    daemon = make_daemon(None)
    inject_calls = _inject_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    generation = daemon._generation

    _transcriber(daemon).next_result = TranscriptionResult(
        text="on time result", elapsed_seconds=0.1
    )
    daemon._worker.run_decode(_audio(daemon).next_samples, generation)

    assert inject_calls == [("on time result", daemon._config.paste)]
    assert isinstance(_state(daemon), Idle)


# -- 2. holding the key does not cause an X11 subprocess storm ---------------


def test_holding_key_does_not_cause_x11_subprocess_storm(
    make_daemon: DaemonFactory, fake_x11: FakeX11
) -> None:
    """Regression: holding the hotkey used to reposition the overlay (and so
    shell out to xrandr/xdotool) on every single dispatched event -- ~2 calls
    per event, ~122 calls for a 2s hold. Repositioning must only happen on an
    actual phase change.
    """
    daemon = make_daemon(None)

    daemon._dispatch(KeyDown(at=0.0))
    # Simulate a 2-second hold's worth of events reaching the daemon without
    # ever leaving the Recording phase -- what a held key looked like before
    # auto-repeat was dropped at the hotkey layer.
    for i in range(60):
        daemon._dispatch(KeyDown(at=0.01 * (i + 1)))
    daemon._dispatch(Cancelled(at=2.0))

    assert fake_x11.outputs_calls <= 5
    assert fake_x11.focused_calls <= 5


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
        on_key_down=lambda at: calls.append(("down", at)),
        on_key_up=lambda at: calls.append(("up", at)),
        on_cancel=lambda at: calls.append(("cancel", at)),
    )

    watcher._handle_key(186, 2)  # value=2: auto-repeat

    assert calls == []


# -- 4. a stray tap is discarded, not decoded ---------------------------------


def test_stray_tap_is_discarded_not_decoded(make_daemon: DaemonFactory) -> None:
    daemon = make_daemon(None)
    decode_calls = _decode_spy(daemon)
    inject_calls = _inject_spy(daemon)

    held = MIN_HOLD_SECONDS / 2
    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=held))

    assert decode_calls == []
    assert inject_calls == []
    assert isinstance(_state(daemon), Idle)
    assert _audio(daemon).start_calls == 1
    assert _audio(daemon).stop_calls == 1


# -- 5. full happy path -------------------------------------------------------


def test_happy_path_key_down_to_injected_postprocessed_text(
    make_daemon: DaemonFactory, fake_injector: FakeInjector
) -> None:
    """key down -> audio captured -> decode -> postprocess -> injected text
    equals the expected post-processed string, proving filler stripping
    happened end to end (not just in ``test_text.py`` isolation), all the
    way down to the real ``_Worker.run_inject`` -> ``inject_text`` seam."""
    daemon = make_daemon(None)
    inject_calls = _inject_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    assert _audio(daemon).start_calls == 1
    daemon._dispatch(KeyUp(at=0.5))
    generation = daemon._generation

    _transcriber(daemon).next_result = TranscriptionResult(
        text="um hello world", elapsed_seconds=0.05
    )
    daemon._worker.run_decode(_audio(daemon).next_samples, generation)

    assert inject_calls == [("hello world", daemon._config.paste)]
    assert isinstance(_state(daemon), Idle)

    # Drive the actual injection boundary too, with the exact text/paste
    # config the daemon asked for, so the real seam between the worker and
    # the outside world (faked here) is exercised as well.
    text, paste = inject_calls[0]
    daemon._worker.run_inject(text, paste)

    assert fake_injector.calls == [("hello world", daemon._config.paste)]


# -- 6. a press during an in-flight decode starts a new capture --------------


def test_press_during_inflight_decode_starts_new_capture_and_both_results_land(
    make_daemon: DaemonFactory,
) -> None:
    daemon = make_daemon(None)
    inject_calls = _inject_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)
    first_generation = daemon._generation
    first_samples = _audio(daemon).next_samples.copy()

    # A press arrives while the first decode is still "in flight" (we have
    # not yet resolved it) -- must start a new capture, not be dropped.
    daemon._dispatch(KeyDown(at=0.6))
    assert isinstance(_state(daemon), Recording)
    assert _audio(daemon).start_calls == 2

    # The earlier decode now completes; its result must still be injected.
    transcriber.next_result = TranscriptionResult(text="first result", elapsed_seconds=0.1)
    daemon._worker.run_decode(first_samples, first_generation)
    assert inject_calls[-1] == ("first result", daemon._config.paste)
    # The daemon is still mid-recording the second utterance.
    assert isinstance(_state(daemon), Recording)

    daemon._dispatch(KeyUp(at=1.0))
    second_generation = daemon._generation
    assert second_generation == first_generation  # nothing cancelled, so unchanged
    transcriber.next_result = TranscriptionResult(text="second result", elapsed_seconds=0.1)
    daemon._worker.run_decode(_audio(daemon).next_samples, second_generation)

    assert inject_calls == [
        ("first result", daemon._config.paste),
        ("second result", daemon._config.paste),
    ]
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
    inject_calls = _inject_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._dispatch(KeyUp(at=0.5))
    generation = daemon._generation

    _transcriber(daemon).next_result = RuntimeError("decode blew up")
    daemon._worker.run_decode(_audio(daemon).next_samples, generation)

    assert inject_calls == []
    assert isinstance(_state(daemon), Idle)

    # The daemon is still alive and responsive to new events.
    daemon._dispatch(KeyDown(at=2.0))
    assert isinstance(_state(daemon), Recording)


# -- 9. the live preview never touches the committed decode -------------------
#
# The preview relaxes constraint 3 ("one-shot decode") in exactly one
# direction: extra *cosmetic* decodes are allowed, the *committed* decode is
# still one shot over the whole buffer. These tests pin the three invariants
# that keep the relaxation from turning back into the failure it came from --
# a streaming decode that made the user wait and silently dropped audio.


def _preview_spy(daemon: Daemon) -> list[tuple[MonoAudio, int]]:
    """Records every ``(samples, generation)`` a preview was requested for.

    As :func:`_decode_spy`: the worker thread is never started, so tests take
    what the daemon asked for here and hand it to ``_Worker.run_preview``
    themselves, exactly where Qt would have made the thread hop.
    """
    calls: list[tuple[MonoAudio, int]] = []
    daemon._preview_requested.connect(
        lambda samples, generation: calls.append((samples, generation))
    )
    return calls


def _overlay(daemon: Daemon) -> Overlay:
    """``daemon._overlay`` is statically ``Overlay | None``; every daemon these
    tests build has ``overlay.enabled`` on, so it is always present."""
    assert daemon._overlay is not None
    return daemon._overlay


def test_preview_mid_recording_reaches_the_overlay(make_daemon: DaemonFactory) -> None:
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()  # what the preview QTimer does, once a second

    assert len(requests) == 1
    # A fixed-length trailing window, not the whole buffer: preview cost must
    # not grow with the utterance (invariant 2).
    expected_frames = int(
        daemon._config.overlay.preview_window_s * daemon._config.audio.sample_rate
    )
    assert _audio(daemon).snapshot_calls == [expected_frames]

    samples, generation = requests[0]
    _transcriber(daemon).next_result = TranscriptionResult(
        text="um hello world", elapsed_seconds=0.02
    )
    daemon._worker.run_preview(samples, generation)

    # Post-processed on the way to the overlay, like the committed transcript.
    assert _overlay(daemon)._preview == "hello world"


def test_preview_from_a_previous_generation_is_dropped(make_daemon: DaemonFactory) -> None:
    """A preview requested before a cancellation must not land in the overlay
    of the *next* utterance -- the same staleness rule the committed decode
    already obeys."""
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    stale_samples, stale_generation = requests[0]

    # Key up, then cancel the decode: this is what bumps the generation.
    daemon._dispatch(KeyUp(at=0.5))
    daemon._dispatch(Cancelled(at=0.6))
    assert daemon._generation != stale_generation

    # A new utterance starts, which re-arms previews.
    daemon._dispatch(KeyDown(at=1.0))
    assert isinstance(_state(daemon), Recording)

    _transcriber(daemon).next_result = TranscriptionResult(
        text="stale preview", elapsed_seconds=0.02
    )
    daemon._worker.run_preview(stale_samples, stale_generation)

    assert _overlay(daemon)._preview == ""


def test_preview_arriving_after_key_up_is_dropped(make_daemon: DaemonFactory) -> None:
    """Once the key is up the daemon is transcribing, and a preview that
    resolves late must not overwrite (or resurrect) the overlay text."""
    daemon = make_daemon(None)

    daemon._dispatch(KeyDown(at=0.0))
    generation = daemon._generation
    daemon._dispatch(KeyUp(at=0.5))
    assert isinstance(_state(daemon), Transcribing)

    # Straight from the worker's signal, bypassing the worker-side abandon
    # check, so this pins the *receiving* guard on its own.
    daemon._worker.previewed.emit("late preview", generation)

    assert _overlay(daemon)._preview == ""


def test_abandon_flag_skips_a_queued_preview_entirely(make_daemon: DaemonFactory) -> None:
    """Invariant 3, the one that protects decode latency: when the key comes
    up, a preview already queued on the worker must be *skipped*, not decoded
    ahead of the committed decode. The recogniser is single-threaded, so a
    preview that ran here would be pure added latency on the text the user is
    waiting for -- which is how the tool this replaced felt slow.
    """
    daemon = make_daemon(None)
    requests = _preview_spy(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    queued_samples, queued_generation = requests[0]

    daemon._dispatch(KeyUp(at=0.5))
    assert daemon._worker.abandon_previews.is_set()

    transcriber = _transcriber(daemon)
    decodes_before = len(transcriber.calls)
    transcriber.next_result = TranscriptionResult(text="never decoded", elapsed_seconds=0.02)

    # The queued preview finally reaches the worker, after the key-up.
    daemon._worker.run_preview(queued_samples, queued_generation)

    assert len(transcriber.calls) == decodes_before, "the preview was decoded anyway"
    assert _overlay(daemon)._preview == ""


def test_injected_text_comes_only_from_the_committed_decode(
    make_daemon: DaemonFactory,
) -> None:
    """Invariant 1: previews are cosmetic. Even with a preview that decoded to
    something completely different, the injected text is produced by exactly
    one decode of the complete captured buffer at key release.
    """
    daemon = make_daemon(None)
    inject_calls = _inject_spy(daemon)
    audio = _audio(daemon)
    audio.preview_samples = np.full(800, 0.5, dtype=np.float32)
    audio.next_samples = np.full(1600, 0.25, dtype=np.float32)
    requests = _preview_spy(daemon)
    transcriber = _transcriber(daemon)

    daemon._dispatch(KeyDown(at=0.0))
    daemon._request_preview()
    transcriber.next_result = TranscriptionResult(text="preview only", elapsed_seconds=0.02)
    daemon._worker.run_preview(*requests[0])
    assert _overlay(daemon)._preview == "preview only"

    daemon._dispatch(KeyUp(at=0.5))
    transcriber.next_result = TranscriptionResult(text="committed text", elapsed_seconds=0.1)
    daemon._worker.run_decode(audio.next_samples, daemon._generation)

    assert inject_calls == [("committed text", daemon._config.paste)]
    # Exactly one decode saw the full buffer, and the preview's window never
    # reached injection in any form.
    full_buffer_decodes = [c for c in transcriber.calls if c.size == audio.next_samples.size]
    assert len(full_buffer_decodes) == 1
    assert np.array_equal(full_buffer_decodes[0], audio.next_samples)


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

    daemon._dispatch(KeyDown(at=0.0))
    assert audio.recover_calls == 0

    daemon._poll_level()  # healthy: polls, finds nothing wrong, says nothing
    assert audio.recover_calls == 1
    assert _overlay(daemon)._preview == ""

    audio.dead = True
    daemon._poll_level()

    # Recovered, and the user is told on the overlay -- silence in the meter is
    # ambiguous (a quiet room looks identical to a dead device), so the overlay
    # has to say which one it is while they can still act on it.
    assert "no audio" in _overlay(daemon)._preview


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


def test_overlay_is_placed_from_qt_geometry_not_xrandr_geometry(
    make_daemon: DaemonFactory,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The overlay was invisible for the whole of the project's life.

    Placement was computed from xrandr, which reports device pixels, and handed
    to Qt's ``move()``, which takes logical units whenever the device pixel
    ratio is not 1. With ``Xft.dpi: 192`` (ratio 2.0) the overlay landed at
    y=3936 on a 2160px-tall screen -- mapped, viewable, painted, and 1776px
    below the bottom edge. Every offscreen render test passed throughout,
    because a render test never involves a screen.

    So: pick the output with xrandr, place on it with Qt's own rect. This pins
    that the *Qt* rect is what reaches the placement maths.
    """
    daemon = make_daemon(None)

    # A screen Qt describes very differently from xrandr: same panel, half the
    # logical size, exactly what a 2.0 device pixel ratio produces.
    monkeypatch.setattr(
        "voice_kb.app.screen_rect",
        lambda name: Rect(x=0, y=0, width=960, height=540),
    )
    position = daemon._overlay_position()

    assert position is not None
    # Centred and bottom-aligned within the QT rect (960x540), not the 1920x1080
    # one the fake xrandr reports.
    cfg = daemon._config.overlay
    assert position.x == (960 - cfg.width) // 2
    assert position.y == 540 - cfg.margin_px - cfg.total_height


def test_overlay_falls_back_to_xrandr_when_qt_does_not_know_the_screen(
    make_daemon: DaemonFactory,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """An unrecognised screen name must still put the overlay somewhere. At a
    device pixel ratio of 1 the two rects are identical anyway, so the fallback
    is only wrong on the setup where Qt would have known the name."""
    daemon = make_daemon(None)
    monkeypatch.setattr("voice_kb.app.screen_rect", lambda name: None)

    position = daemon._overlay_position()

    assert position is not None
    cfg = daemon._config.overlay
    assert position.y == 1080 - cfg.margin_px - cfg.total_height


def test_every_fake_still_matches_the_interface_it_stands_in_for() -> None:
    """Regression: the last two features each shipped a new method on a real
    boundary class without adding it to the fake, and the whole e2e suite went
    red on an ``AttributeError`` that had nothing to do with the feature. This
    fails on the *fake* instead, pointing straight at the fix.
    """
    _assert_stands_in_for(FakeAudioCapture, AudioCapture)
    _assert_stands_in_for(FakeTranscriber, Transcriber)
    for name in ("outputs", "focused_window_rect"):
        assert _params(getattr(FakeX11, name)) == _params(getattr(x11, name))
    assert _params(FakeInjector.__call__) == _params(inject_text)
