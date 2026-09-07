"""The imperative shell: wires the hotkey, audio, model, and overlay together.

All policy lives in :func:`voice_kb.state.step`, which is pure. This module's
only job is to interpret the commands that function returns, and to get work
onto the right thread.

Threading
---------
Four threads, deliberately:

* the **evdev watcher**, which must never block or the hotkey lags;
* the **Qt main thread**, which owns the overlay and the state machine;
* one **worker**, which owns the recogniser -- seconds of CPU per decode,
  which must not stall the UI or delay the next keypress;
* one **nvim bridge**, which owns the RPC connection to the dictation window.
  It is separate from the worker because the two block on different things
  and must not queue behind each other: opening the window takes ~1s
  (measured), and it happens *while* the first utterance is still being
  recorded, so the window is up by the time there is text for it.

Hotkey callbacks arrive on the watcher thread and are marshalled onto the Qt
thread by Signal emission -- Qt queues cross-thread signals automatically. The
state machine is therefore only ever touched from the Qt thread, so it needs no
lock. :class:`~voice_kb.asr.Transcriber` is not thread-safe and is used only
from the worker; :class:`~voice_kb.nvim.NvimSession` likewise belongs to the
bridge.

Nothing is ever dropped silently. Every path that discards audio or a decode
result says so in the log, because silent loss is the failure this project
exists to eliminate.

The sink
--------
The committed transcript is appended to a floating neovim over its RPC socket
(:mod:`voice_kb.nvim`). Nothing is pasted anywhere and no window but that one
is ever written to, so dictating never depends on -- or disturbs -- whatever
happens to have focus.

Live preview
------------
While the key is held, the indicators show a rolling preview of the
transcript. It is cosmetic and strictly subordinate to the committed decode
(``docs/constraints.md``, "One-shot committed decode"):

1. the text that reaches the buffer is still produced by *exactly one*
   decode of the complete captured buffer at key release -- preview output is
   never committed, never merged into it, and never influences it. In nvim it
   is not even buffer content: it is an extmark's virtual text, which cannot
   be saved, yanked or undone into the file;
2. a preview's cost is bounded -- by an adaptive cadence that keeps the
   worker idle at least half the time, and by a hard stop at
   ``PreviewConfig.max_seconds``, so it is bounded and
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
from voice_kb.config import Config, xdg_state_home
from voice_kb.geometry import Output, Rect, overlay_rect, pick_output
from voice_kb.hotkey import HotkeyPermissionError, HotkeyWatcher
from voice_kb.nvim import Appended, AppendFailed, NvimSession
from voice_kb.overlay import Overlay, screen_rect
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
    Recording,
    SessionState,
    StartCapture,
    phase_of,
    step,
)
from voice_kb.text import postprocess
from voice_kb.vad import Segment, SpeechSegmenter, load_segmenter

log = logging.getLogger("voice-kb")

_LEVEL_POLL_MS = 33  # matches the overlay's own frame interval
_NVIM_LEVEL_EVERY = 3  # so nvim's meter updates at ~10Hz, not 30
_THREAD_SHUTDOWN_MS = 5000
_OUTPUTS_TTL_S = 5.0


class _Worker(QObject):
    """Owns the recogniser. Lives on its own QThread; never touched from Qt."""

    ready = Signal()
    prepare_failed = Signal(str)
    decoded = Signal(str, float, int)  # full text, elapsed, generation
    segment_decoded = Signal(str, int)  # one segment's text, generation
    decode_failed = Signal(str, int)
    previewed = Signal(str, int)  # text, generation

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._transcriber: Transcriber | None = None
        self._segmenter: SpeechSegmenter | None = None
        #: Set from the Qt thread, read on this worker thread. The *only*
        #: mutable state shared between the two -- everything else crosses by
        #: queued signal. It exists so a preview that is already queued when a
        #: committed decode becomes due is dropped instead of run, which is
        #: what keeps a cosmetic decode off the user's latency path.
        self._abandon_previews = threading.Event()
        #: Set from the Qt thread when an in-flight decode is cancelled. Read
        #: between segments so a cancelled long decode stops appending rather
        #: than running to the end of a passage the user has abandoned.
        self._abandon_decode = threading.Event()
        #: Preview text for chunks of this utterance that are already closed,
        #: and how much audio it accounts for. Worker-thread-only, and reset
        #: per utterance. This is what makes a preview cost the same at ten
        #: seconds as at ten minutes: only the open tail is ever re-decoded.
        #:
        #: It cannot reach the committed transcript. `run_decode` never reads
        #: it, and decodes the capture from zero -- the invariant in
        #: docs/constraints.md that preview output is never merged into the
        #: final text holds exactly as before.
        self._preview_prefix = ""
        self._preview_from = 0

    @property
    def abandon_previews(self) -> threading.Event:
        """The abandon flag: ``set()`` to drop pending previews, ``clear()`` to re-arm.

        Exposed as the whole ``Event`` rather than as a pair of methods so it
        is obvious at the call site that this is the one shared object.
        """
        return self._abandon_previews

    @property
    def abandon_decode(self) -> threading.Event:
        """Set to stop a running decode after its current segment."""
        return self._abandon_decode

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
        # After the recogniser, and never fatal: a missing VAD model costs
        # segmentation, not the daemon.
        self._segmenter = load_segmenter(self._config.vad, self._config.audio.sample_rate)
        self.ready.emit()

    def run_decode(self, samples: MonoAudio, generation: int) -> None:
        """Decode the capture segment by segment, emitting each as it lands.

        Every sample is decoded exactly once, in one pass, after the key came
        up -- no region is re-decoded as more audio arrives, which is the
        property ``docs/constraints.md`` protects. Splitting first is a
        correctness fix (see ``voice_kb.vad``) that happens to also let text
        start appearing in a few hundred milliseconds instead of at the end.
        """
        if self._transcriber is None:
            self.decode_failed.emit("model not loaded", generation)
            return
        rate = self._config.audio.sample_rate
        start = time.perf_counter()
        try:
            segments = (
                self._segmenter.split(samples)
                if self._segmenter is not None
                else [Segment(samples=samples, start_seconds=0.0)]
            )
            texts: list[str] = []
            cancelled = False
            for index, segment in enumerate(segments):
                if self._abandon_decode.is_set():
                    log.info("decode cancelled after %d of %d chunks", index, len(segments))
                    cancelled = True
                    break
                text = self._transcriber.transcribe(segment.samples, rate).text
                # Every chunk, with where it came from, at DEBUG. Words going
                # missing from the front of a dictation is the one report that
                # cannot be diagnosed after the fact without this: it says
                # whether the first chunk covered the start of the capture and
                # what it decoded to, which separates "the audio was not
                # there" from "the recogniser returned nothing for it".
                log.debug(
                    "chunk %d/%d at %.1fs, %.1fs long -> %d chars: %r",
                    index + 1,
                    len(segments),
                    segment.start_seconds,
                    segment.samples.size / rate,
                    len(text),
                    text[:60],
                )
                if not text.strip():
                    continue
                texts.append(text)
                self.segment_decoded.emit(text, generation)

            # Chunking must never lose a whole utterance. It has: a 2.8s
            # capture at peak 0.28 -- unmistakably speech -- came back empty
            # from every chunk, and the words were simply gone. Whatever the
            # detector did there, the audio is still in hand, so decode it the
            # old way rather than report nothing.
            #
            # Costs one extra decode, and only in a case that was already a
            # total failure. The floor this buys is worth stating plainly:
            # segmentation can now never do worse than not segmenting.
            if not texts and not cancelled and len(segments) != 1:
                log.warning(
                    "all %d chunks decoded to nothing; retrying the whole %.1fs buffer",
                    len(segments),
                    samples.size / rate,
                )
                text = self._transcriber.transcribe(samples, rate).text
                if text.strip():
                    log.info("the whole-buffer retry recovered the utterance")
                    texts.append(text)
                    self.segment_decoded.emit(text, generation)
        except Exception as e:
            log.exception("decode raised")
            self.decode_failed.emit(str(e), generation)
            return
        self._reset_preview()
        self.decoded.emit(" ".join(texts), time.perf_counter() - start, generation)

    def run_preview(self, samples: MonoAudio, generation: int) -> None:
        """Decode the in-flight capture, for the indicators only.

        Deliberately toothless: it returns without decoding at all if previews
        have been abandoned, and it emits nothing if they were abandoned while
        it was decoding. Anything it raises is swallowed at DEBUG -- a failed
        preview is cosmetic, and must never surface as an error, abort the
        session, or touch the committed decode.
        """
        if self._abandon_previews.is_set() or self._transcriber is None or samples.size == 0:
            return
        # The capture restarting is how this thread learns a new utterance
        # began: it is the one signal that arrives without a round trip to the
        # Qt thread, and a preview carrying the last utterance's words would
        # be worse than no preview at all.
        if samples.size < self._preview_from:
            self._reset_preview()
        try:
            text = self._preview_text(samples)
        except Exception as e:
            log.debug("preview decode failed, ignoring: %s", e)
            return
        if self._abandon_previews.is_set():
            return
        self.previewed.emit(text, generation)

    def _reset_preview(self) -> None:
        self._preview_prefix = ""
        self._preview_from = 0

    def _preview_text(self, samples: MonoAudio) -> str:
        """The utterance so far: settled chunks remembered, open tail decoded.

        Previews used to decode the whole utterance every time, which made
        each one cost more than the last -- ~4s by the one-minute mark -- and
        is why they had to stop being issued at all past a time limit. Neither
        is acceptable in the window someone is watching while they speak.

        Once the detector closes a chunk, that audio is final: no later speech
        changes it, so its text is kept and its samples are never looked at
        again. Only the open tail is re-decoded, which bounds a preview by the
        chunk rather than by the utterance -- the same cost at ten minutes as
        at ten seconds.
        """
        assert self._transcriber is not None
        rate = self._config.audio.sample_rate
        if self._segmenter is None:
            return self._transcriber.transcribe(samples, rate).text

        chunks = self._segmenter.split(samples[self._preview_from :])
        # The last chunk is still open -- the user may be mid-sentence in it --
        # so only the ones before it are settled.
        for chunk in chunks[:-1]:
            settled = self._transcriber.transcribe(chunk.samples, rate).text
            if settled.strip():
                self._preview_prefix = f"{self._preview_prefix} {settled}".strip()
            self._preview_from += chunk.samples.size
            if self._abandon_previews.is_set():
                # Mid-fold, but the prefix and the offset moved together, so
                # what is kept is consistent and the next preview resumes here.
                return self._preview_prefix

        tail = self._transcriber.transcribe(chunks[-1].samples, rate).text
        return f"{self._preview_prefix} {tail}".strip()

class _NvimBridge(QObject):
    """Owns the connection to the dictation window. Lives on its own QThread.

    Every slot here is entered from a queued signal, so calls arrive in the
    order the Qt thread emitted them -- which is what makes "open the window,
    then append to it" correct without any explicit handshake.

    Nothing in here raises: :class:`~voice_kb.nvim.NvimSession` reports
    failure as a value or a log line, and this class turns that into a signal.
    A dictation window that cannot be opened must not take down a daemon that
    is otherwise recording and decoding perfectly well.
    """

    opened = Signal(str)  # path of the dictation file
    open_failed = Signal()
    appended = Signal(int, float)  # line count, elapsed ms
    append_failed = Signal(str)

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._session = NvimSession(config.nvim)

    def warm_up(self) -> None:
        """Get the first, slow X query out of the way before it is needed.

        Runs when the bridge thread starts, so the ~1.9s one-off cost of this
        process's first subprocess lands at daemon start rather than on the
        first dictation of the session.
        """
        self._session.warm_up()

    def open(self) -> None:
        """Ensure a dictation window exists, opening one if it does not.

        Called on every key-down rather than once at startup: the window is
        the user's to close, and reopening it on the next dictation is the
        behaviour that needs no explanation.
        """
        if not self._session.ensure():
            self.open_failed.emit()
            return
        # Nothing moves the window after it opens. It used to be pulled to the
        # current workspace on every key-down, which meant it followed the user
        # between screens -- reported as wrong, and it is: where the window
        # sits is the user's decision from the moment it exists. Placement is a
        # starting position, not a policy to keep enforcing.
        #
        # A window left on another workspace therefore stays there, and the
        # transcript still lands in it and on disk. Closing it is how you get a
        # new one where you are.
        # ensure() succeeded, so there is a path; str(None) would be a lie
        # in the log rather than an error, which is worse.
        self.opened.emit(str(self._session.path or "?"))

    def append(self, text: str, continued: bool) -> None:
        match self._session.append(text, continued=continued):
            case Appended(line=line, elapsed_ms=ms):
                self.appended.emit(line, ms)
            case AppendFailed(reason=reason):
                self.append_failed.emit(reason)

    def set_phase(self, phase: object) -> None:
        assert isinstance(phase, Phase)
        self._session.set_state(phase=phase)

    def set_level(self, level: float) -> None:
        self._session.set_state(level=level)

    def set_preview(self, text: str) -> None:
        self._session.set_state(preview=text)

    def set_latched(self, latched: bool) -> None:
        self._session.set_state(latched=latched)

    def set_previewing(self, previewing: bool) -> None:
        self._session.set_state(previewing=previewing)

    def shutdown(self) -> None:
        self._session.close()



class Daemon(QObject):
    """Holds the session state and interprets the state machine's commands."""

    # Emitted from the evdev thread; queued onto the Qt thread by Qt.
    key_down = Signal(float, bool)  # at, latch modifier held
    key_up = Signal(float)
    cancelled = Signal(float)

    _decode_requested = Signal(object, int)
    _preview_requested = Signal(object, int)

    _nvim_open_requested = Signal()
    _nvim_append_requested = Signal(str, bool)
    _nvim_phase_changed = Signal(object)
    _nvim_level = Signal(float)
    _nvim_preview = Signal(str)
    _nvim_latched = Signal(bool)
    _nvim_previewing = Signal(bool)
    _nvim_shutdown_requested = Signal()

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
        self._level_tick = 0
        #: Whether the utterance being decoded has already appended a
        #: paragraph. The *second* and later segments extend it rather than
        #: starting a new one, so an utterance stays one paragraph however
        #: many pieces it arrives in. Qt-thread-only, reset per decode.
        self._utterance_started = False
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False
        self._preview_text = ""

        # Fail fast, on this thread, while the error can still reach main().
        # Constructing the Transcriber on the worker would bury ModelMissingError
        # in a background traceback and leave a daemon that looks healthy but
        # silently produces nothing.
        ensure_model_files(config.asr)

        self._audio = AudioCapture(config.audio)
        self._overlay = (
            Overlay(config.overlay, preview_band=config.preview.enabled)
            if config.overlay.enabled
            else None
        )

        self._worker = _Worker(config)
        self._thread = QThread()
        self._worker.moveToThread(self._thread)
        self._thread.started.connect(self._worker.prepare)
        self._decode_requested.connect(self._worker.run_decode)
        self._preview_requested.connect(self._worker.run_preview)
        self._worker.ready.connect(lambda: log.info("model ready"))
        self._worker.prepare_failed.connect(self._on_prepare_failed)
        self._worker.decoded.connect(self._on_decoded)
        self._worker.segment_decoded.connect(self._on_segment_decoded)
        self._worker.decode_failed.connect(self._on_decode_failed)
        self._worker.previewed.connect(self._on_previewed)

        # The bridge gets its own thread, not the worker's: opening a terminal
        # blocks for a few hundred milliseconds and must not sit in front of a
        # decode, nor a decode in front of it.
        self._nvim = _NvimBridge(config)
        self._nvim_thread = QThread()
        self._nvim.moveToThread(self._nvim_thread)
        self._nvim_thread.started.connect(self._nvim.warm_up)
        self._nvim_open_requested.connect(self._nvim.open)
        self._nvim_append_requested.connect(self._nvim.append)
        self._nvim_phase_changed.connect(self._nvim.set_phase)
        self._nvim_level.connect(self._nvim.set_level)
        self._nvim_preview.connect(self._nvim.set_preview)
        self._nvim_latched.connect(self._nvim.set_latched)
        self._nvim_previewing.connect(self._nvim.set_previewing)
        self._nvim_shutdown_requested.connect(self._nvim.shutdown)
        self._nvim.opened.connect(lambda path: log.info("dictation window ready: %s", path))
        self._nvim.open_failed.connect(self._on_nvim_unavailable)
        self._nvim.appended.connect(self._on_appended)
        self._nvim.append_failed.connect(self._on_append_failed)

        self.key_down.connect(lambda at, latch: self._dispatch(KeyDown(at=at, latch=latch)))
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

        # Previews turned off means there is nothing to schedule, so the timer
        # is not created at all rather than created and left stopped. It is no
        # longer conditional on the overlay: the nvim indicator shows previews
        # too, and it is the one that is on by default.
        self._preview_timer: QTimer | None = None
        if config.preview.enabled:
            self._preview_timer = QTimer(self)
            # Single-shot and re-armed after each result: the gap between
            # previews depends on how long the last one took. See
            # _rearm_previews.
            self._preview_timer.setSingleShot(True)
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
            self._nvim_thread.start()
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
        # Ask the bridge to detach before the thread that owns the connection
        # goes away. The editor itself is left running -- see
        # NvimSession.close. Deliberately *not* a blocking invocation: the
        # bridge may be part-way through opening a window, and an editor that
        # wedges after answering its readiness probe would otherwise hang
        # Ctrl-C forever. The queued call runs ahead of quit() if the thread
        # is idle, and if it never runs, process exit closes the socket.
        if self._nvim_thread.isRunning():
            self._nvim_shutdown_requested.emit()
        for name, thread in (("worker", self._thread), ("nvim bridge", self._nvim_thread)):
            if not thread.isRunning():
                continue
            thread.quit()
            if not thread.wait(_THREAD_SHUTDOWN_MS):
                log.warning("the %s thread did not stop in time; exiting anyway", name)
        self._started = False

    # -------------------------------------------------------------- state machine

    def _dispatch(self, event: Event) -> None:
        self._state, command = step(self._state, event)
        self._apply(command)
        self._sync_indicators()

    def _apply(self, command: Command) -> None:
        match command:
            case Nothing():
                pass
            case StartCapture():
                self._audio.start_capture()
                self._level_timer.start()
                # Asked for on every key-down, and answered on the bridge
                # thread while this utterance is still being spoken -- so the
                # few hundred milliseconds a terminal takes to appear are paid
                # in parallel with the recording, not added to it.
                self._nvim_open_requested.emit()
                self._arm_previews()
            case Decode(spoken_seconds=spoken):
                self._level_timer.stop()
                self._utterance_started = False
                self._worker.abandon_decode.clear()
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
                # Stop the decode between segments as well as discarding its
                # result. With text landing progressively, a cancellation can
                # arrive after some of it is already in the buffer -- so it
                # now means "stop adding", not "none of this happened". What
                # is already written stays; it is the user's file.
                self._worker.abandon_decode.set()
                self._generation += 1
                log.info("cancelled; in-flight decode will not be injected")
            case _:
                assert_never(command)

    # ------------------------------------------------------------------- overlay

    def _sync_indicators(self) -> None:
        """Push a real phase change to the nvim winbar and the overlay.

        This runs from every dispatched event, and overlay positioning shells
        out to xrandr and xdotool. Doing that per event is what previously
        stalled the Qt thread; the indicators only change when the phase does,
        so anything that leaves the phase unchanged must cost nothing.

        The phase-change test deliberately sits *above* the overlay check --
        the overlay is off by default now, and gating the whole thing on it
        would leave the nvim indicator frozen.
        """
        phase = phase_of(self._state)
        if phase == self._last_phase:
            return
        self._last_phase = phase
        self._nvim_phase_changed.emit(phase)
        self._nvim_latched.emit(isinstance(self._state, Recording) and self._state.latched)
        if self._overlay is None:
            return
        if phase is not Phase.IDLE:
            pos = self._overlay_position()
            if pos is not None:
                self._overlay.show_at(pos.x, pos.y)
        self._overlay.set_phase(phase)

    def _overlay_position(self) -> Rect | None:
        """Where to put the overlay, in the coordinate space Qt's ``move()`` uses.

        Two coordinate systems meet here and must not be confused. Choosing the
        *output* is a question about the physical desktop, so it is answered
        with device pixels from ``xrandr`` and ``xdotool``. Placing the overlay
        *on* that output is a question for Qt, which works in logical units
        whenever the device pixel ratio is not 1 -- so the placement maths runs
        against the matching ``QScreen``'s own rect, never the xrandr one.

        Mixing them is silently wrong: it put the overlay 1776px below the
        bottom of the screen, mapped and painted and invisible.
        """
        screens = self._cached_outputs()
        if not screens:
            return None
        window = x11.focused_window_rect() if self._config.overlay.follow_focus else None
        target = pick_output(screens, window) if window else _primary(screens)
        # Fall back to the xrandr rect only if Qt does not know this screen by
        # name; on a device pixel ratio of 1 the two are identical anyway.
        qt_rect = screen_rect(target.name)
        if qt_rect is None:
            log.warning(
                "Qt does not know a screen named %r; placing the overlay from "
                "xrandr geometry, which is wrong under HiDPI scaling",
                target.name,
            )
            qt_rect = target.rect
        return overlay_rect(
            qt_rect, self._config.overlay, preview_band=self._config.preview.enabled
        )

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
        level = self._audio.current_level()
        if self._overlay is not None:
            self._overlay.push_level(level)
        # The overlay redraws locally at 30fps; nvim is across a socket, so it
        # gets every third sample. A 24-cell text meter has nothing to gain
        # from the other twenty, and the winbar redraw is nvim's main loop.
        self._level_tick += 1
        if self._level_tick % _NVIM_LEVEL_EVERY == 0:
            self._nvim_level.emit(level)
        if self._audio.recover_if_dead():
            self._warn_no_audio()

    def _warn_no_audio(self) -> None:
        """Tell the user, where they are looking, that the microphone went away.

        Both indicators, not just the overlay: with the overlay off by default
        this warning would otherwise exist only in the log, which is precisely
        where a user mid-utterance is not looking.
        """
        message = "no audio from the microphone -- reconnected, keep talking"
        if self._overlay is not None:
            self._overlay.set_preview_text(message)
        self._nvim_preview.emit(message)

    # -------------------------------------------------------------------- preview

    def _arm_previews(self) -> None:
        """Start previewing a new utterance, from a blank slate."""
        self._worker.abandon_previews.clear()
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False
        self._preview_text = ""
        if self._overlay is not None:
            self._overlay.set_preview_text("")
        self._nvim_preview.emit("")
        self._nvim_previewing.emit(self._config.preview.enabled)
        if self._preview_timer is not None:
            self._preview_timer.start(self._config.preview.interval_ms)

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
        """Ask the worker to preview the whole utterance so far.

        From the beginning, not a trailing window. A trailing window was tried
        first and it was wrong for the one job a preview has: it physically
        discarded the start of the sentence, so words the user had already
        watched appear would vanish in chunks while they were still speaking.
        Decoding from the start means what is on screen only ever grows.

        Snapshotted non-destructively, so the capture keeps accumulating and
        the committed decode at key release still sees everything.
        """
        if phase_of(self._state) is not Phase.RECORDING:
            return
        samples = self._audio.snapshot_capture()
        seconds = samples.size / self._config.audio.sample_rate
        if seconds > self._config.preview.max_seconds:
            # Cost grows with the utterance, so it has to stop somewhere. The
            # text stays exactly as it is -- this stops updating it, it does
            # not clear it -- and by now there is far more of it than the
            # overlay can show anyway.
            log.debug(
                "utterance past %.0fs; no more previews for it (the last one stays on screen)",
                self._config.preview.max_seconds,
            )
            self._nvim_previewing.emit(False)
            return
        if samples.size == 0:
            self._rearm_previews(0.0)
            return
        self._preview_requested_at = time.monotonic()
        self._preview_seconds = seconds
        self._preview_requested.emit(samples, self._generation)

    def _rearm_previews(self, last_decode_seconds: float) -> None:
        """Schedule the next preview, keeping the worker idle at least half the time.

        The gap is ``max(interval - decode, decode)``, so the *period* is
        ``max(interval, 2 * decode)``. Short utterances keep the configured
        cadence exactly; long ones slow down on their own rather than pinning
        the worker. The duty cycle is what matters: a preview already inside
        ``decode_stream`` when the key comes up cannot be cancelled, so the
        fraction of time one is running is the fraction of releases that pay
        for it.

        A repeating timer cannot express this, hence single-shot plus re-arm.
        """
        if self._preview_timer is None or phase_of(self._state) is not Phase.RECORDING:
            return
        interval = self._config.preview.interval_ms / 1000.0
        gap = max(interval - last_decode_seconds, last_decode_seconds)
        self._preview_timer.start(int(gap * 1000))

    def _on_previewed(self, text: str, generation: int) -> None:
        """Show a preview, unless it has been overtaken by events.

        Everything here is DEBUG: a preview is cosmetic, and previews arrive
        about once a second, so anything louder would drown the log that
        matters.
        """
        if generation != self._generation or phase_of(self._state) is not Phase.RECORDING:
            log.debug("dropping a preview that no longer applies (gen %d)", generation)
            return
        round_trip = (
            time.monotonic() - self._preview_requested_at if self._preview_requested_at else 0.0
        )
        self._rearm_previews(round_trip)
        if not self._preview_timed and self._preview_requested_at:
            # Once per utterance, so preview cost stays visible in the log file
            # without flooding it. Round trip, so it includes the queue hop the
            # user would actually feel if a preview ever delayed a decode.
            self._preview_timed = True
            log.debug(
                "preview round trip %.0fms for %.1fs of audio",
                round_trip * 1000,
                self._preview_seconds,
            )
        cleaned = postprocess(text, self._config.text)
        # Each preview sees strictly more audio than the last, so the
        # transcript should only ever grow -- but the recogniser does not
        # guarantee that. Decoding 4.4s of a real sample returned "Okay."
        # where 3.3s of the same sample had returned "Okay, we are now at the
        # new model.". Showing that verbatim is the exact thing being fixed
        # here: words the user watched appear, disappearing again.
        #
        # So a preview that came back shorter is treated as instability rather
        # than as news, and the previous text stands. Worst case is a slightly
        # stale preview for one cycle; the committed decode is unaffected
        # either way, since nothing here can reach the clipboard.
        if len(cleaned) < len(self._preview_text):
            log.debug("ignoring a preview that shrank: %r", cleaned[:40])
            return
        self._preview_text = cleaned
        if self._overlay is not None:
            self._overlay.set_preview_text(cleaned)
        self._nvim_preview.emit(cleaned)

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
        self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_segment_decoded(self, text: str, generation: int) -> None:
        """Put one segment's text in the buffer, without waiting for the rest.

        The first segment of an utterance opens a paragraph and the rest
        extend it, so what the user sees is a paragraph growing rather than
        several appearing. Nothing here can run ahead of itself: appends cross
        to the bridge thread on a queued signal, so they land in the order
        this method emitted them.
        """
        if generation != self._generation:
            log.debug("dropping a segment from a cancelled session")
            return
        cleaned = postprocess(text, self._config.text)
        if not cleaned.strip():
            return
        self._nvim_append_requested.emit(cleaned, self._utterance_started)
        self._utterance_started = True

    def _on_decode_failed(self, reason: str, generation: int) -> None:
        if generation == self._generation:
            log.error("decode failed: %s", reason)
        self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_appended(self, line: int, elapsed_ms: float) -> None:
        """The text is in the buffer *and* on disk -- nvim confirmed the line
        count after writing the file, so this is evidence, not optimism."""
        log.info("appended to the dictation buffer in %.0fms (line %d)", elapsed_ms, line)

    def _on_append_failed(self, reason: str) -> None:
        # The decode succeeded and the text exists; it just could not be
        # delivered. It is already in the log at INFO from _on_decoded, which
        # is the difference between a transcription that is recoverable and
        # one that is gone.
        log.error("could not append to the dictation buffer: %s", reason)

    def _on_nvim_unavailable(self) -> None:
        log.error(
            "no dictation window: check that %r is installed and that "
            "`%s` runs from a terminal",
            self._config.nvim.editor[0],
            " ".join(self._config.nvim.terminal[:1] or self._config.nvim.editor[:1]),
        )

    def _on_prepare_failed(self, reason: str) -> None:
        # Without a model there is nothing this daemon can do; staying up would
        # mean every dictation vanishes with only a log line to show for it.
        log.error("could not load the model: %s", reason)
        QApplication.instance().quit()  # type: ignore[union-attr]


DEFAULT_LOG_FILE = xdg_state_home() / "voice-kb" / "voice-kb.log"


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
