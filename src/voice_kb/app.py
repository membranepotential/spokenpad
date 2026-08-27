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

Live preview
------------
While the key is held, the overlay shows a rolling preview of the transcript.
It is cosmetic and strictly subordinate to the committed decode
(``docs/constraints.md``, "One-shot committed decode"):

1. the text that gets injected is still produced by *exactly one* decode of
   the complete captured buffer at key release -- preview output is never
   injected, never merged into it, and never influences it;
2. a preview decodes only a fixed-length trailing window
   (``OverlayConfig.preview_window_s``), so its cost is bounded and
   independent of how long the user has been speaking;
3. a preview can never make the user wait: previews are *abandoned* the
   instant a committed decode (or a cancellation) is due, so a preview
   already queued on the worker is dropped rather than run ahead of it.

Preview decodes run on the same single worker as the committed decode, which
is what keeps the non-thread-safe recogniser serialized.
"""

from __future__ import annotations

import argparse
import logging
import logging.handlers
import os
import signal
import sys
import threading
import time
import wave
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
    Phase,
    SessionState,
    StartCapture,
    phase_of,
    step,
)
from voice_kb.text import postprocess

log = logging.getLogger("voice-kb")

_LEVEL_POLL_MS = 33  # matches the overlay's own frame interval
_OUTPUTS_TTL_S = 5.0


class _Worker(QObject):
    """Owns the recogniser. Lives on its own QThread; never touched from Qt."""

    ready = Signal()
    prepare_failed = Signal(str)
    decoded = Signal(str, float, int)  # text, elapsed, generation
    decode_failed = Signal(str, int)
    injected = Signal(float, bool)
    inject_failed = Signal(str)
    previewed = Signal(str, int)  # text, generation

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._transcriber: Transcriber | None = None
        #: Set from the Qt thread, read on this worker thread. The *only*
        #: mutable state shared between the two -- everything else crosses by
        #: queued signal. It exists so a preview that is already queued when a
        #: committed decode becomes due is dropped instead of run, which is
        #: what keeps a cosmetic decode off the user's latency path.
        self._abandon_previews = threading.Event()

    @property
    def abandon_previews(self) -> threading.Event:
        """The abandon flag: ``set()`` to drop pending previews, ``clear()`` to re-arm.

        Exposed as the whole ``Event`` rather than as a pair of methods so it
        is obvious at the call site that this is the one shared object.
        """
        return self._abandon_previews

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

    def run_preview(self, samples: MonoAudio, generation: int) -> None:
        """Decode a trailing slice of the in-flight capture, for the overlay only.

        Deliberately toothless: it returns without decoding at all if previews
        have been abandoned, and it emits nothing if they were abandoned while
        it was decoding. Anything it raises is swallowed at DEBUG -- a failed
        preview is cosmetic, and must never surface as an error, abort the
        session, or touch the committed decode.
        """
        if self._abandon_previews.is_set() or self._transcriber is None or samples.size == 0:
            return
        try:
            result = self._transcriber.transcribe(samples, self._config.audio.sample_rate)
        except Exception as e:
            log.debug("preview decode failed, ignoring: %s", e)
            return
        if self._abandon_previews.is_set():
            return
        self.previewed.emit(result.text, generation)

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
    _preview_requested = Signal(object, int)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._state: SessionState = Idle()
        self._started = False
        self._dump_dir: Path | None = None
        self._last_phase = Phase.IDLE
        self._outputs_cache: list[Output] | None = None
        self._outputs_read_at = 0.0

        #: Bumped whenever an in-flight decode is invalidated. A result whose
        #: generation no longer matches is discarded instead of injected.
        #: Previews never touch it: a preview is a shell concern, not a
        #: session transition.
        self._generation = 0

        #: When the current utterance's most recent preview was requested, and
        #: whether its cost has already been logged. Qt-thread-only.
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False

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
        self._preview_requested.connect(self._worker.run_preview)
        self._worker.ready.connect(lambda: log.info("model ready"))
        self._worker.prepare_failed.connect(self._on_prepare_failed)
        self._worker.decoded.connect(self._on_decoded)
        self._worker.decode_failed.connect(self._on_decode_failed)
        self._worker.injected.connect(self._on_injected)
        self._worker.previewed.connect(self._on_previewed)
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

        # No overlay (or previews turned off) means there is nothing a preview
        # could be *for*, so the timer is not created at all rather than
        # created and left stopped.
        self._preview_timer: QTimer | None = None
        if self._overlay is not None and config.overlay.live_preview:
            self._preview_timer = QTimer(self)
            self._preview_timer.setInterval(config.overlay.preview_interval_ms)
            self._preview_timer.timeout.connect(self._request_preview)

    # ------------------------------------------------------------------ lifecycle

    def set_dump_dir(self, path: Path) -> None:
        """Write every captured utterance to ``path`` as a wav, for diagnosis."""
        self._dump_dir = path
        log.info("dumping captured audio to %s", path)

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
        self._abandon_previews()
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
                self._arm_previews()
            case Decode(spoken_seconds=spoken):
                self._level_timer.stop()
                # Before the decode is even requested: a preview already queued
                # on the worker must be dropped, not run ahead of it.
                self._abandon_previews()
                samples = self._audio.stop_capture()
                self._log_capture(spoken, samples)
                self._decode_requested.emit(samples, self._generation)
            case DiscardCapture():
                self._level_timer.stop()
                self._abandon_previews()
                self._audio.stop_capture()
                log.info("discarded capture (too short, or cancelled)")
            case AbortDecode():
                self._abandon_previews()
                self._generation += 1
                log.info("cancelled; in-flight decode will not be injected")
            case _:
                assert_never(command)

    # ------------------------------------------------------------------- overlay

    def _sync_overlay(self) -> None:
        """Reposition only on a real phase change.

        This runs from every dispatched event, and positioning shells out to
        xrandr and xdotool. Doing that per event is what previously stalled the
        Qt thread; the overlay only needs moving when it appears, so anything
        that leaves the phase unchanged must cost nothing.
        """
        if self._overlay is None:
            return
        phase = phase_of(self._state)
        if phase == self._last_phase:
            return
        self._last_phase = phase
        if phase is not Phase.IDLE:
            pos = self._overlay_position()
            if pos is not None:
                self._overlay.show_at(pos.x, pos.y)
        self._overlay.set_phase(phase)

    def _overlay_position(self) -> Rect | None:
        screens = self._cached_outputs()
        if not screens:
            return None
        window = x11.focused_window_rect() if self._config.overlay.follow_focus else None
        target = pick_output(screens, window) if window else _primary(screens)
        return overlay_rect(target.rect, self._config.overlay)

    def _cached_outputs(self) -> list[Output]:
        """Monitor layout, re-read at most every few seconds.

        xrandr is a subprocess and the layout almost never changes; re-reading
        it on the hot path is pure latency.
        """
        now = time.monotonic()
        if self._outputs_cache is None or now - self._outputs_read_at > _OUTPUTS_TTL_S:
            self._outputs_cache = x11.outputs()
            self._outputs_read_at = now
        return self._outputs_cache

    def _log_capture(self, held_seconds: float, samples: MonoAudio) -> None:
        """Report what was actually captured, not just how long the key was held.

        These are different numbers and conflating them hides the failure where
        the key is held for seconds but the buffer comes back empty or silent.
        """
        rate = self._config.audio.sample_rate
        audio_seconds = len(samples) / rate if rate else 0.0
        rms = float(np.sqrt(np.mean(np.square(samples)))) if len(samples) else 0.0
        peak = float(np.max(np.abs(samples))) if len(samples) else 0.0
        log.info(
            "captured %.1fs held -> %.1fs audio (%d samples), rms=%.4f peak=%.4f",
            held_seconds, audio_seconds, len(samples), rms, peak,
        )
        status = self._audio.take_stream_status()
        if status:
            log.warning("PortAudio reported: %s", status)

        # A dead input stream shows up as a mismatch between how long the key
        # was held and how much audio arrived. Testing only for "nothing beyond
        # the pre-roll" was too narrow and missed the real thing: a stream that
        # died 0.2s into a 29.8s hold delivered 5705 samples against a 4000
        # sample pre-roll, cleared that test, and was reported as a normal
        # capture that merely decoded to nothing.
        if held_seconds > 1.0 and audio_seconds < held_seconds * 0.5:
            log.error(
                "captured only %.1fs of audio while the key was held for %.1fs -- "
                "the input stream stopped delivering. It has been reopened; if "
                "this repeats, check `pactl list short sources`.",
                audio_seconds, held_seconds,
            )
        elif peak < 0.01:
            log.warning(
                "captured audio is near-silent (peak %.4f). Check the input device "
                "and its gain -- the model will hallucinate on silence.", peak
            )
        if self._dump_dir is not None:
            self._dump_capture(samples, rate)

    def _dump_capture(self, samples: MonoAudio, rate: int) -> None:
        assert self._dump_dir is not None
        path = self._dump_dir / f"capture-{int(time.time())}.wav"
        try:
            with wave.open(str(path), "wb") as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(rate)
                w.writeframes((np.clip(samples, -1.0, 1.0) * 32767).astype(np.int16).tobytes())
        except OSError as e:
            log.warning("could not dump capture: %s", e)
            return
        log.info("dumped capture to %s", path)

    def _poll_level(self) -> None:
        """Feed the meter, and notice a microphone that has stopped talking.

        This timer already ticks every 33ms for the whole of a recording, so it
        is the one place that can catch a stream dying *during* an utterance
        rather than on the next keypress. Silence in the overlay is ambiguous
        -- a quiet room looks the same as a dead device -- so say which it is,
        in the log and on the overlay, while the user is still holding the key
        and can do something about it.
        """
        if self._overlay is not None:
            self._overlay.push_level(self._audio.current_level())
        if self._audio.recover_if_dead():
            self._warn_no_audio()

    def _warn_no_audio(self) -> None:
        """Tell the user, on the overlay, that the microphone went away."""
        if self._overlay is not None:
            self._overlay.set_preview_text(
                "no audio from the microphone -- reconnected, keep talking"
            )

    # -------------------------------------------------------------------- preview

    def _arm_previews(self) -> None:
        """Start previewing a new utterance, from a blank slate."""
        self._worker.abandon_previews.clear()
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False
        if self._overlay is not None:
            self._overlay.set_preview_text("")
        if self._preview_timer is not None:
            self._preview_timer.start()

    def _abandon_previews(self) -> None:
        """Stop previewing, and drop whatever is already queued on the worker.

        This is invariant 3 of the live preview (see the module docstring): a
        preview must never make the user wait. Setting the flag *before* the
        committed decode is requested is what guarantees a queued preview is
        skipped entirely rather than decoded ahead of the real thing.
        """
        self._worker.abandon_previews.set()
        if self._preview_timer is not None:
            self._preview_timer.stop()

    def _request_preview(self) -> None:
        """Ask the worker for a preview of the last ``preview_window_s`` of audio.

        Fixed-length window, snapshotted non-destructively: the capture keeps
        accumulating and the committed decode at key release still sees the
        whole buffer.
        """
        if phase_of(self._state) is not Phase.RECORDING:
            return
        overlay_cfg = self._config.overlay
        frames = int(overlay_cfg.preview_window_s * self._config.audio.sample_rate)
        samples = self._audio.snapshot_capture(frames)
        if samples.size == 0:
            return
        self._preview_requested_at = time.monotonic()
        self._preview_seconds = samples.size / self._config.audio.sample_rate
        self._preview_requested.emit(samples, self._generation)

    def _on_previewed(self, text: str, generation: int) -> None:
        """Show a preview, unless it has been overtaken by events.

        Everything here is DEBUG: a preview is cosmetic, and previews arrive
        about once a second, so anything louder would drown the log that
        matters.
        """
        if generation != self._generation or phase_of(self._state) is not Phase.RECORDING:
            log.debug("dropping a preview that no longer applies (gen %d)", generation)
            return
        if not self._preview_timed and self._preview_requested_at:
            # Once per utterance, so preview cost stays visible in the log file
            # without flooding it. Round trip, so it includes the queue hop the
            # user would actually feel if a preview ever delayed a decode.
            self._preview_timed = True
            log.debug(
                "preview round trip %.0fms for %.1fs of audio (window %.1fs)",
                (time.monotonic() - self._preview_requested_at) * 1000,
                self._preview_seconds,
                self._config.overlay.preview_window_s,
            )
        if self._overlay is not None:
            self._overlay.set_preview_text(postprocess(text, self._config.text))

    # -------------------------------------------------------------------- results

    def _on_decoded(self, text: str, elapsed: float, generation: int) -> None:
        if generation != self._generation:
            log.info("dropping decode from a cancelled session (%.2fs)", elapsed)
            return
        cleaned = postprocess(text, self._config.text)
        log.info("decoded in %.2fs (%d chars): %r", elapsed, len(cleaned), cleaned[:80])
        if cleaned != text:
            log.debug("raw model output: %r", text)
        log.debug("full transcript: %r", cleaned)
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


DEFAULT_LOG_FILE = (
    Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state"))
    / "voice-kb"
    / "voice-kb.log"
)


def _configure_logging(*, verbose: bool, log_file: Path | None) -> None:
    """Console plus a rotating file.

    The file is always at DEBUG regardless of ``-v``: a dictation problem is
    usually only noticed after the fact, and re-running to reproduce it is not
    always possible. Rotation keeps it bounded without any maintenance.

    Note the file records transcribed text, so it holds whatever was dictated.
    It stays on this machine, mode 0600, and `--log-file none` turns it off.
    """
    root = logging.getLogger("voice-kb")
    root.setLevel(logging.DEBUG)
    root.propagate = False

    console = logging.StreamHandler()
    console.setLevel(logging.DEBUG if verbose else logging.INFO)
    console.setFormatter(logging.Formatter("%(asctime)s %(levelname)-7s %(message)s", "%H:%M:%S"))
    root.addHandler(console)

    if log_file is not None and str(log_file).lower() == "none":
        return
    target = log_file or DEFAULT_LOG_FILE
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        handler = logging.handlers.RotatingFileHandler(
            target, maxBytes=1_000_000, backupCount=3, encoding="utf-8"
        )
        os.chmod(target, 0o600)
    except OSError as e:
        root.warning("could not open log file %s: %s", target, e)
        return
    handler.setLevel(logging.DEBUG)
    handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)-7s %(name)s: %(message)s")
    )
    root.addHandler(handler)
    root.info("logging to %s", target)


def _primary(screens: list[Output]) -> Output:
    """The primary output, else the first. xrandr order is not primary order."""
    return next((s for s in screens if s.primary), screens[0])


def main() -> int:
    parser = argparse.ArgumentParser(prog="voice-kb", description="Push-to-talk dictation.")
    parser.add_argument("-c", "--config", type=Path, default=None)
    parser.add_argument("--model-dir", type=Path, default=None)
    parser.add_argument("-v", "--verbose", action="store_true")
    parser.add_argument(
        "--dump-audio",
        type=Path,
        default=None,
        metavar="DIR",
        help="write every captured utterance to DIR as a wav, for diagnosis",
    )
    parser.add_argument(
        "--log-file",
        type=Path,
        default=None,
        help=f"defaults to {DEFAULT_LOG_FILE}; pass 'none' to disable",
    )
    args = parser.parse_args()

    _configure_logging(verbose=args.verbose, log_file=args.log_file)

    config = Config.load(args.config)
    if args.model_dir is not None:
        config = config.with_model_dir(args.model_dir)

    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)  # the overlay hides; that must not exit

    try:
        daemon = Daemon(config)
        if args.dump_audio is not None:
            args.dump_audio.mkdir(parents=True, exist_ok=True)
            daemon.set_dump_dir(args.dump_audio)
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

    # Qt's event loop blocks in C, so Python never runs its SIGINT handler and
    # Ctrl-C is swallowed. Ask Qt to surface briefly so the handler can fire.
    signal.signal(signal.SIGINT, lambda *_: app.quit())
    signal.signal(signal.SIGTERM, lambda *_: app.quit())
    wakeup = QTimer()
    wakeup.timeout.connect(lambda: None)
    wakeup.start(200)

    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
