"""The imperative shell: wires the hotkey, audio, model, and overlay together.

All policy lives in :func:`voice_kb.state.step`, which is pure. This module's
only job is to interpret the commands that function returns, and to get work
onto the right thread.

Threading
---------
Three threads, deliberately:

* the **evdev watcher**, which must never block or the hotkey lags;
* the **Qt main thread**, which owns the overlay and the state machine;
* one **decode worker**, because a 20-second utterance takes seconds of CPU and
  running it on the Qt thread would freeze the overlay mid-transcription.

Hotkey callbacks arrive on the watcher thread and are marshalled onto the Qt
thread by Signal emission -- Qt queues cross-thread signals automatically. The
state machine is therefore only ever touched from the Qt thread, so it needs no
lock. :class:`~voice_kb.asr.Transcriber` is not thread-safe and is used only
from the single decode worker.
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
from voice_kb.asr import ModelMissingError, Transcriber
from voice_kb.audio import AudioCapture, MonoAudio
from voice_kb.config import Config
from voice_kb.geometry import Rect, overlay_rect, pick_output
from voice_kb.hotkey import HotkeyPermissionError, HotkeyWatcher
from voice_kb.inject import Injected, InjectionFailed, inject_text
from voice_kb.overlay import Overlay
from voice_kb.state import (
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
)
from voice_kb.text import postprocess

log = logging.getLogger("voice-kb")

_LEVEL_POLL_MS = 33  # matches the overlay's own frame interval


class _DecodeWorker(QObject):
    """Owns the recogniser and runs on its own QThread."""

    done = Signal(str, float)
    failed = Signal(str)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._transcriber: Transcriber | None = None

    def prepare(self) -> None:
        """Load the model, then decode a moment of silence.

        The first inference allocates onnxruntime's arenas and pages the
        weights in. Doing it here means the cost lands at daemon start rather
        than on the user's first dictation.
        """
        self._transcriber = Transcriber(self._config.asr)
        self._transcriber.transcribe(
            np.zeros(self._config.audio.sample_rate, dtype=np.float32),
            self._config.audio.sample_rate,
        )
        log.info("model ready")

    def run_decode(self, samples: MonoAudio) -> None:
        if self._transcriber is None:
            self.failed.emit("model not loaded")
            return
        try:
            result = self._transcriber.transcribe(samples, self._config.audio.sample_rate)
        except Exception as e:
            log.exception("decode failed")
            self.failed.emit(str(e))
            return
        self.done.emit(result.text, result.elapsed_seconds)


class Daemon(QObject):
    """Holds the session state and interprets the state machine's commands."""

    # Emitted from the evdev thread; queued onto the Qt thread by Qt.
    key_down = Signal(float)
    key_up = Signal(float)
    cancelled = Signal(float)
    _decode_requested = Signal(object)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._state: SessionState = Idle()

        self._audio = AudioCapture(config.audio)
        self._overlay = Overlay(config.overlay) if config.overlay.enabled else None

        self._worker = _DecodeWorker(config)
        self._thread = QThread()
        self._worker.moveToThread(self._thread)
        self._thread.started.connect(self._worker.prepare)
        self._decode_requested.connect(self._worker.run_decode)
        self._worker.done.connect(self._on_decoded)
        self._worker.failed.connect(self._on_decode_failed)
        self._thread.start()

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
        self._watcher.start()
        log.info("listening for key code %d", self._config.hotkey.key_code)

    def stop(self) -> None:
        self._watcher.stop()
        self._level_timer.stop()
        self._audio.close()
        self._thread.quit()
        self._thread.wait(5000)

    # -------------------------------------------------------------- state machine

    def _dispatch(self, event: Event) -> None:
        from voice_kb.state import step

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
                self._decode_requested.emit(samples)
            case DiscardCapture():
                self._level_timer.stop()
                self._audio.stop_capture()
                log.info("cancelled")
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
        target = pick_output(screens, window) if window else screens[0]
        return overlay_rect(target.rect, self._config.overlay)

    def _poll_level(self) -> None:
        if self._overlay is not None:
            self._overlay.push_level(self._audio.current_level())

    # -------------------------------------------------------------------- results

    def _on_decoded(self, text: str, elapsed: float) -> None:
        cleaned = postprocess(text, self._config.text)
        log.info("decoded in %.2fs: %r", elapsed, cleaned[:80])
        if cleaned.strip():
            match inject_text(cleaned, self._config.paste):
                case Injected(elapsed_ms=ms):
                    log.info("injected in %.0fms", ms)
                case InjectionFailed(reason=reason):
                    log.error("injection failed: %s", reason)
        self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_decode_failed(self, reason: str) -> None:
        log.error("decode failed: %s", reason)
        self._dispatch(DecodeFinished(at=time.monotonic()))


def main() -> int:
    parser = argparse.ArgumentParser(prog="voice-kb", description=__doc__)
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

    app.aboutToQuit.connect(daemon.stop)
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
