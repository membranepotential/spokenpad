"""The imperative shell: wires the hotkey, audio, model, and overlay together.

All policy lives in :func:`voice_kb.state.step`, which is pure. This module's
only job is to interpret the commands that function returns, and to get work
onto the right thread.

Threading
---------
Three threads, deliberately:

* the **evdev watcher**, which must never block or the hotkey lags;
* the **Qt main thread**, which owns the overlay and the state machine;
* one **worker**, which owns the recogniser and also performs injection --
  both call out to slow things (seconds of CPU; ``xdotool``/``xclip``
  subprocesses) and neither may stall the overlay or delay the next keypress.

Hotkey callbacks arrive on the watcher thread and are marshalled onto the Qt
thread by Signal emission -- Qt queues cross-thread signals automatically. The
state machine is therefore only ever touched from the Qt thread, so it needs no
lock. :class:`~voice_kb.asr.Transcriber` is not thread-safe and is used only
from the worker.

Nothing is ever dropped silently. Every path that discards audio or a decode
result says so in the log, because silent loss is the failure this project
exists to eliminate.
"""

from __future__ import annotations

import argparse
import logging
import sys
import time
from pathlib import Path
from typing import assert_never

import numpy as np
from PySide6.QtCore import QObject, QThread, QTimer, Signal
from PySide6.QtWidgets import QApplication

from voice_kb import x11
from voice_kb.asr import ModelMissingError, Transcriber, ensure_model_files
from voice_kb.audio import AudioCapture, MonoAudio
from voice_kb.config import Config, PasteConfig
from voice_kb.geometry import Output, Rect, overlay_rect, pick_output
from voice_kb.hotkey import HotkeyPermissionError, HotkeyWatcher
from voice_kb.inject import Injected, InjectionFailed, inject_text
from voice_kb.overlay import Overlay
from voice_kb.state import (
    AbortDecode,
    Cancelled,
    Command,
    Decode,
    DecodeFinished,
    DiscardCapture,
    Event,
    Idle,
    KeyDown,
    KeyUp,
    Nothing,
    SessionState,
    StartCapture,
    phase_of,
    step,
)
from voice_kb.text import postprocess

log = logging.getLogger("voice-kb")

_LEVEL_POLL_MS = 33  # matches the overlay's own frame interval


class _Worker(QObject):
    """Owns the recogniser. Lives on its own QThread; never touched from Qt."""

    ready = Signal()
    prepare_failed = Signal(str)
    decoded = Signal(str, float, int)  # text, elapsed, generation
    decode_failed = Signal(str, int)
    injected = Signal(float, bool)
    inject_failed = Signal(str)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._transcriber: Transcriber | None = None

    def prepare(self) -> None:
        """Load the model, then decode a moment of silence.

        The first inference allocates onnxruntime's arenas and pages the weights
        in, so doing it here puts the cost at daemon start rather than on the
        user's first dictation.
        """
        try:
            self._transcriber = Transcriber(self._config.asr)
            self._transcriber.transcribe(
                np.zeros(self._config.audio.sample_rate, dtype=np.float32),
                self._config.audio.sample_rate,
            )
        except Exception as e:
            self.prepare_failed.emit(str(e))
            return
        self.ready.emit()

    def run_decode(self, samples: MonoAudio, generation: int) -> None:
        if self._transcriber is None:
            self.decode_failed.emit("model not loaded", generation)
            return
        try:
            result = self._transcriber.transcribe(samples, self._config.audio.sample_rate)
        except Exception as e:
            log.exception("decode raised")
            self.decode_failed.emit(str(e), generation)
            return
        self.decoded.emit(result.text, result.elapsed_seconds, generation)

    def run_inject(self, text: str, paste: PasteConfig) -> None:
        match inject_text(text, paste):
            case Injected(elapsed_ms=ms, confirmed=confirmed):
                self.injected.emit(ms, confirmed)
            case InjectionFailed(reason=reason):
                self.inject_failed.emit(reason)


class Daemon(QObject):
    """Holds the session state and interprets the state machine's commands."""

    # Emitted from the evdev thread; queued onto the Qt thread by Qt.
    key_down = Signal(float)
    key_up = Signal(float)
    cancelled = Signal(float)

    _decode_requested = Signal(object, int)
    _inject_requested = Signal(str, object)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._state: SessionState = Idle()
        self._started = False

        #: Bumped whenever an in-flight decode is invalidated. A result whose
        #: generation no longer matches is discarded instead of injected.
        self._generation = 0

        # Fail fast, on this thread, while the error can still reach main().
        # Constructing the Transcriber on the worker would bury ModelMissingError
        # in a background traceback and leave a daemon that looks healthy but
        # silently produces nothing.
        ensure_model_files(config.asr)

        self._audio = AudioCapture(config.audio)
        self._overlay = Overlay(config.overlay) if config.overlay.enabled else None

        self._worker = _Worker(config)
        self._thread = QThread()
        self._worker.moveToThread(self._thread)
        self._thread.started.connect(self._worker.prepare)
        self._decode_requested.connect(self._worker.run_decode)
        self._inject_requested.connect(self._worker.run_inject)
        self._worker.ready.connect(lambda: log.info("model ready"))
        self._worker.prepare_failed.connect(self._on_prepare_failed)
        self._worker.decoded.connect(self._on_decoded)
        self._worker.decode_failed.connect(self._on_decode_failed)
        self._worker.injected.connect(self._on_injected)
        self._worker.inject_failed.connect(lambda r: log.error("injection failed: %s", r))

        self.key_down.connect(lambda at: self._dispatch(KeyDown(at=at)))
        self.key_up.connect(lambda at: self._dispatch(KeyUp(at=at)))
        self.cancelled.connect(lambda at: self._dispatch(Cancelled(at=at)))

        self._watcher = HotkeyWatcher(
            config.hotkey,
            on_key_down=self.key_down.emit,
            on_key_up=self.key_up.emit,
            on_cancel=self.cancelled.emit,
        )

        self._level_timer = QTimer(self)
        self._level_timer.setInterval(_LEVEL_POLL_MS)
        self._level_timer.timeout.connect(self._poll_level)

    # ------------------------------------------------------------------ lifecycle

    def start(self) -> None:
        """Arm the daemon. On failure, tears down whatever did start.

        The QThread must not be left running when this raises: destroying a
        running QThread aborts the process, which would replace a useful error
        message ("add yourself to the input group") with SIGABRT.
        """
        try:
            self._thread.start()
            self._watcher.start()
        except Exception:
            self.stop()
            raise
        self._started = True
        log.info("listening for key code %d", self._config.hotkey.key_code)

    def stop(self) -> None:
        self._watcher.stop()
        self._level_timer.stop()
        self._audio.close()
        if self._thread.isRunning():
            self._thread.quit()
            self._thread.wait(5000)
        self._started = False

    # -------------------------------------------------------------- state machine

    def _dispatch(self, event: Event) -> None:
        self._state, command = step(self._state, event)
        self._apply(command)
        self._sync_overlay()

    def _apply(self, command: Command) -> None:
        match command:
            case Nothing():
                pass
            case StartCapture():
                self._audio.start_capture()
                self._level_timer.start()
            case Decode(spoken_seconds=spoken):
                self._level_timer.stop()
                samples = self._audio.stop_capture()
                log.info("captured %.1fs", spoken)
                self._decode_requested.emit(samples, self._generation)
            case DiscardCapture():
                self._level_timer.stop()
                self._audio.stop_capture()
                log.info("discarded capture (too short, or cancelled)")
            case AbortDecode():
                self._generation += 1
                log.info("cancelled; in-flight decode will not be injected")
            case _:
                assert_never(command)

    # ------------------------------------------------------------------- overlay

    def _sync_overlay(self) -> None:
        if self._overlay is None:
            return
        phase = phase_of(self._state)
        if isinstance(self._state, Idle):
            self._overlay.set_phase(phase)
            return
        pos = self._overlay_position()
        if pos is not None:
            self._overlay.show_at(pos.x, pos.y)
        self._overlay.set_phase(phase)

    def _overlay_position(self) -> Rect | None:
        screens = x11.outputs()
        if not screens:
            return None
        window = x11.focused_window_rect() if self._config.overlay.follow_focus else None
        target = pick_output(screens, window) if window else _primary(screens)
        return overlay_rect(target.rect, self._config.overlay)

    def _poll_level(self) -> None:
        if self._overlay is not None:
            self._overlay.push_level(self._audio.current_level())

    # -------------------------------------------------------------------- results

    def _on_decoded(self, text: str, elapsed: float, generation: int) -> None:
        if generation != self._generation:
            log.info("dropping decode from a cancelled session (%.2fs)", elapsed)
            return
        cleaned = postprocess(text, self._config.text)
        log.info("decoded in %.2fs: %r", elapsed, cleaned[:80])
        if not cleaned.strip():
            log.warning("decode produced no text")
        else:
            self._inject_requested.emit(cleaned, self._config.paste)
        self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_decode_failed(self, reason: str, generation: int) -> None:
        if generation == self._generation:
            log.error("decode failed: %s", reason)
        self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_injected(self, elapsed_ms: float, confirmed: bool) -> None:
        if confirmed:
            log.info("injected in %.0fms", elapsed_ms)
        else:
            # The clipboard was restored on a timeout rather than on evidence the
            # paste was served. The text may not have landed -- say so, because a
            # transcription disappearing without a trace is the whole reason this
            # project exists.
            log.warning(
                "injected in %.0fms but the paste was never confirmed; "
                "the text may not have landed in the target window",
                elapsed_ms,
            )

    def _on_prepare_failed(self, reason: str) -> None:
        # Without a model there is nothing this daemon can do; staying up would
        # mean every dictation vanishes with only a log line to show for it.
        log.error("could not load the model: %s", reason)
        QApplication.instance().quit()  # type: ignore[union-attr]


def _primary(screens: list[Output]) -> Output:
    """The primary output, else the first. xrandr order is not primary order."""
    return next((s for s in screens if s.primary), screens[0])


def main() -> int:
    parser = argparse.ArgumentParser(prog="voice-kb", description="Push-to-talk dictation.")
    parser.add_argument("-c", "--config", type=Path, default=None)
    parser.add_argument("--model-dir", type=Path, default=None)
    parser.add_argument("-v", "--verbose", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)-7s %(message)s",
        datefmt="%H:%M:%S",
    )

    config = Config.load(args.config)
    if args.model_dir is not None:
        config = config.with_model_dir(args.model_dir)

    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)  # the overlay hides; that must not exit

    try:
        daemon = Daemon(config)
        daemon.start()
    except ModelMissingError as e:
        log.error("%s", e)
        return 2
    except HotkeyPermissionError as e:
        log.error("%s", e)
        return 3

    # Only worth connecting once we are actually going to reach the event loop;
    # both failure paths above have already torn themselves down.
    app.aboutToQuit.connect(daemon.stop)
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
