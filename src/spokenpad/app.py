"""The imperative shell: wires the hotkey, audio, model and dictation window together.

All policy lives in :func:`spokenpad.state.step`, which is pure. This module's
only job is to interpret the commands that function returns, and to get work
onto the right thread.

Threading
---------
Four threads, deliberately:

* the **evdev watcher**, which must never block or the hotkey lags;
* the **Qt main thread**, which owns the state machine and the timers;
* one **worker**, which owns the recogniser -- seconds of CPU per decode,
  which must not stall the UI or delay the next keypress -- and with it the
  *committed offset*, the one fact about a capture that has to be decided
  on the thread that serialises decodes;
* one **nvim bridge**, which owns the RPC connection to the dictation window.
  It is separate from the worker because the two block on different things
  and must not queue behind each other: opening the window takes ~1s
  (measured), and it happens *while* the first utterance is still being
  recorded, so the window is up by the time there is text for it.

Hotkey callbacks arrive on the watcher thread and are marshalled onto the Qt
thread by Signal emission -- Qt queues cross-thread signals automatically. The
state machine is therefore only ever touched from the Qt thread, so it needs no
lock. :class:`~spokenpad.asr.Transcriber` is not thread-safe and is used only
from the worker; :class:`~spokenpad.nvim.NvimSession` likewise belongs to the
bridge.

Nothing is ever dropped silently. Every path that discards audio or a decode
result says so in the log, because silent loss is the failure this project
exists to eliminate.

The sink
--------
The transcript is appended to a floating neovim over its RPC socket
(:mod:`spokenpad.nvim`). Nothing is pasted anywhere and no window but that one
is ever written to, so dictating never depends on -- or disturbs -- whatever
happens to have focus.

Progressive commit
------------------
Text lands *while the user is still speaking* (``docs/progressive-commit.md``).
Every ``preview.interval_ms`` while recording, the worker splits the audio
since its committed offset at silence, decodes each chunk that has settled
exactly once and appends it, and decodes the open tail for the preview. At
key release only the remainder past the committed offset is decoded, so the
wait is bounded by one chunk however long the passage was.

Three things keep that on the right side of ``docs/constraints.md``:

1. every committed sample is decoded exactly once -- when its chunk settles
   or at release, never both. The offset only grows within an utterance and
   is owned by the worker;
2. the preview is the open tail only: virtual text in nvim, bounded by one
   chunk, replaced every tick, never merged into anything;
3. a tick can never make the user wait: one still queued when the key comes
   up is skipped, one already running stops after its current chunk (which,
   if settled, it has just committed rather than wasted). Ticks are allowed
   per utterance id, so re-arming for the next utterance cannot revive one.

Every message between the two threads carries the **utterance id**, bumped on
every capture. It is what lets a stale tick, a cancelled decode and a slow
decode from the previous utterance each be told apart from the current one.
"""

from __future__ import annotations

import argparse
import logging
import logging.handlers
import os
import signal
import sys
import time
import wave
from pathlib import Path
from typing import assert_never

import numpy as np
from PySide6.QtCore import QObject, QThread, QTimer, Signal
from PySide6.QtWidgets import QApplication

from spokenpad.asr import ModelMissingError, Transcriber, ensure_model_files
from spokenpad.audio import MAX_UTTERANCE_SECONDS, AudioCapture, MonoAudio
from spokenpad.config import Config, xdg_state_home
from spokenpad.decode import decode_capture
from spokenpad.hotkey import HotkeyPermissionError, HotkeyWatcher
from spokenpad.nvim import Appended, AppendFailed, NvimSession
from spokenpad.recorder import (
    CaptureRecorder,
    NotRecorded,
    Recorded,
    RecordingError,
    Truncated,
    read_capture,
)
from spokenpad.state import (
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
from spokenpad.text import postprocess
from spokenpad.vad import SpeechSegmenter, load_segmenter

log = logging.getLogger("spokenpad")

_LEVEL_POLL_MS = 33
_NVIM_LEVEL_EVERY = 3  # so nvim's meter updates at ~10Hz, not 30
_THREAD_SHUTDOWN_MS = 5000

#: No utterance. The worker starts here and returns here after each release
#: decode, so the first message of any real utterance always resets it.
_NO_UTTERANCE = -1


class _Worker(QObject):
    """Owns the recogniser and the committed offset. Lives on its own QThread.

    The offset -- how much of the current capture has been committed -- lives
    here and nowhere else, because this is the one thread on which decodes
    are serialised. A tick that commits a chunk and a release decode that
    starts after it cannot race each other, so no sample is ever decoded
    twice and none is skipped. The daemon keeps a lagging copy for sizing
    snapshots (:attr:`Daemon._committed_hint`) and never acts on it.
    """

    ready = Signal()
    prepare_failed = Signal(str)
    committed = Signal(str, int, int)  # text, utterance, frames committed through
    previewed = Signal(str, int, int)  # open-tail text, utterance, frames committed through
    tick_failed = Signal(str, int)  # reason, utterance
    decoded = Signal(str, float, float, int)  # utterance text, elapsed, tail seconds, utterance
    decode_failed = Signal(str, int)  # reason, utterance

    def __init__(self, config: Config) -> None:
        super().__init__()
        self._config = config
        self._transcriber: Transcriber | None = None
        self._segmenter: SpeechSegmenter | None = None
        #: The one utterance ticks are allowed for, or none. Written from the
        #: Qt thread (an int store is atomic under the GIL), read here between
        #: chunks. It is an id rather than a flag so that a tick still queued
        #: when the key comes up is dropped instead of run -- which keeps the
        #: cosmetic half of a tick off the user's latency path -- *and* so
        #: that re-arming for the next utterance cannot revive one for the
        #: last, which a shared ``Event`` did.
        self._ticking = _NO_UTTERANCE
        #: Utterances whose decode was cancelled: ``add`` from the Qt thread,
        #: ``in`` from here, both atomic under the GIL. A set rather than the
        #: latest id so a second cancel cannot un-cancel the first.
        self._abandoned: set[int] = set()
        #: Worker-thread-only, reset whenever the utterance id changes.
        self._utterance = _NO_UTTERANCE
        self._committed_from = 0
        self._committed_text: list[str] = []

    def tick_for(self, utterance: int) -> None:
        """Allow ticks for ``utterance`` and no other. Qt-thread caller."""
        self._ticking = utterance

    def stop_ticks(self) -> None:
        """Drop any queued tick and stop a running one after its current chunk."""
        self._ticking = _NO_UTTERANCE

    def abandon(self, utterance: int) -> None:
        """Stop decoding ``utterance`` after its current chunk. Qt-thread caller."""
        self._abandoned.add(utterance)

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

    # ------------------------------------------------------------ the tick

    def run_preview(self, samples: MonoAudio, start: int, utterance: int) -> None:
        """One tick: commit what has settled since the offset, preview the rest.

        ``samples`` begins at frame ``start`` of the capture, which is the
        daemon's lagging copy of the offset and so never *past* the real one;
        the remainder is sliced from the real one here. Deliberately
        toothless about failure: anything it raises is swallowed at DEBUG. A
        chunk it committed before failing is committed -- the offset moved
        with it -- and the release decode carries on from there.
        """
        if self._ticking != utterance or utterance in self._abandoned or self._transcriber is None:
            return
        self._begin(utterance)
        remainder = samples[max(0, self._committed_from - start) :]
        if remainder.size == 0:
            return
        try:
            tail = self._tick(remainder, utterance)
        except Exception as e:
            # Reported rather than swallowed: the daemon re-arms the timer off
            # this, and without it one bad decode would end ticking -- and
            # with it progressive commit -- for the rest of the utterance.
            self.tick_failed.emit(str(e), utterance)
            return
        if tail is None or self._ticking != utterance:
            return
        self.previewed.emit(tail, utterance, self._committed_from)

    def _tick(self, remainder: MonoAudio, utterance: int) -> str | None:
        """Commit the settled chunks of ``remainder``; return the open tail's text.

        ``None`` means the tick was stopped part-way and nothing should be
        shown for it. Without a segmenter nothing ever settles, so the whole
        remainder is the tail -- the pre-2026-09-08 preview, and the reason
        ``PreviewConfig.max_seconds`` still exists.
        """
        assert self._transcriber is not None
        rate = self._config.audio.sample_rate
        if self._segmenter is None:
            return self._transcriber.transcribe(remainder, rate).text

        base = self._committed_from
        for chunk in self._segmenter.split(remainder):
            if self._ticking != utterance:
                # Whatever was committed above stands; the release decode
                # resumes from the offset it left.
                return None
            text = self._transcriber.transcribe(chunk.samples, rate).text
            if not chunk.settled:
                # Only the last chunk can be unsettled (spokenpad.vad), so
                # this is the tail and there is nothing after it.
                return text
            self._commit(text, utterance, base + chunk.end_frame)
        # Everything settled, and the buffer ends in silence: nothing to show.
        return ""

    def _commit(self, text: str, utterance: int, through: int) -> None:
        """One chunk decoded, once: advance the offset and hand the text over.

        Emitted even when empty, because the *offset* is news the daemon
        needs for its next snapshot whether or not there were words in it.
        """
        self._committed_from = through
        if text.strip():
            self._committed_text.append(text)
        self.committed.emit(text, utterance, through)

    # --------------------------------------------------------- the release

    def run_decode(self, samples: MonoAudio, utterance: int) -> None:
        """Decode the remainder past the committed offset, emitting each chunk.

        The pipeline itself is :func:`~spokenpad.decode.decode_capture`, shared
        with ``spokenpad transcribe``, so a transcript recovered from a
        recording is produced exactly the way the live one would have been.
        All this adds is the thread boundary and the offset: turn each chunk
        into a signal, turn a failure into one too, and start where the ticks
        left off.
        """
        if self._transcriber is None:
            self.decode_failed.emit("model not loaded", utterance)
            return
        self._begin(utterance)
        remainder = samples[self._committed_from :]
        tail_seconds = remainder.size / self._config.audio.sample_rate
        start = time.perf_counter()
        try:
            decode_capture(
                remainder,
                transcriber=self._transcriber,
                segmenter=self._segmenter,
                sample_rate=self._config.audio.sample_rate,
                # `through` is the whole capture for every release chunk:
                # the offset only matters while snapshots are still taken,
                # and none are after release.
                on_segment=lambda text: self._commit(text, utterance, samples.size),
                abandoned=lambda: utterance in self._abandoned,
            )
        except Exception as e:
            log.exception("decode raised")
            self._reset()
            self.decode_failed.emit(str(e), utterance)
            return
        text = " ".join(self._committed_text)
        self._reset()
        self.decoded.emit(text, time.perf_counter() - start, tail_seconds, utterance)

    # ------------------------------------------------------------ bookkeeping

    def _begin(self, utterance: int) -> None:
        """Start from nothing if this is a new utterance.

        Explicit, by id, rather than by noticing that the capture got shorter:
        a stale tick from the previous utterance can only ever reset state
        for *that* utterance, and the next message for this one resets it
        again. The one thing it can never do is carry an offset across.
        """
        if utterance != self._utterance:
            self._reset()
            self._utterance = utterance

    def _reset(self) -> None:
        self._utterance = _NO_UTTERANCE
        self._committed_from = 0
        self._committed_text = []


class _NvimBridge(QObject):
    """Owns the connection to the dictation window. Lives on its own QThread.

    Every slot here is entered from a queued signal, so calls arrive in the
    order the Qt thread emitted them -- which is what makes "open the window,
    then append to it" correct without any explicit handshake.

    Nothing in here raises: :class:`~spokenpad.nvim.NvimSession` reports
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

    _decode_requested = Signal(object, int)  # samples, utterance
    _preview_requested = Signal(object, int, int)  # samples, start frame, utterance

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

        #: The current capture's id, bumped on every StartCapture and carried
        #: on every message to and from the worker. Qt-thread-only.
        self._utterance = 0
        #: Utterances whose decode was cancelled. A chunk arriving for one of
        #: them is dropped; the audio stays on disk regardless. Grows by one
        #: int per cancel, which is nothing.
        self._aborted: set[int] = set()
        #: Lagging copy of the worker's committed offset, fed by `committed`,
        #: used only to ask the capture for the audio past it. Never acted
        #: on: the worker's own offset decides what is decoded.
        self._committed_hint = 0
        #: Which utterance the last paragraph in the buffer belongs to, so a
        #: chunk extends it only if it is from the same one. A slow decode
        #: from the previous utterance therefore opens its own paragraph
        #: rather than gluing itself onto the one being dictated now.
        self._paragraph_of: int | None = None

        self._level_tick = 0
        #: The open-tail preview currently shown, and the committed offset it
        #: belongs to. The "previews only grow" guard compares within one
        #: offset: when a chunk commits, the tail legitimately starts over.
        self._preview_text = ""
        self._preview_epoch = 0
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False
        self._tick_failures = 0
        #: Whether this capture has hit the in-memory ceiling. Latched for the
        #: rest of the capture and cleared only by the next one.
        #:
        #: **Once capped, no preview may write over the notice.** It is a
        #: state, not a message, because the first two attempts smuggled the
        #: warning through the *preview* field and lost it: a longer late
        #: preview beat it on the "previews only grow" rule. Nothing cosmetic
        #: may outrank a warning that the transcript is about to stop short.
        #:
        #: A *warning* still may, and only one does: a dead microphone
        #: (:meth:`_warn_no_audio`) invalidates what the notice promises, so
        #: it replaces it rather than queueing behind it.
        self._capped = False

        # Fail fast, on this thread, while the error can still reach main().
        # Constructing the Transcriber on the worker would bury ModelMissingError
        # in a background traceback and leave a daemon that looks healthy but
        # silently produces nothing.
        ensure_model_files(config.asr)

        # The recorder is built here, in the shell, and handed to the capture:
        # `audio.py` writes to it from the realtime callback but has no business
        # knowing where recordings live or how many of them are kept.
        self._audio = AudioCapture(
            config.audio, CaptureRecorder(config.recording, config.audio.sample_rate)
        )

        self._worker = _Worker(config)
        self._thread = QThread()
        self._worker.moveToThread(self._thread)
        self._thread.started.connect(self._worker.prepare)
        self._decode_requested.connect(self._worker.run_decode)
        self._preview_requested.connect(self._worker.run_preview)
        self._worker.ready.connect(lambda: log.info("model ready"))
        self._worker.prepare_failed.connect(self._on_prepare_failed)
        self._worker.committed.connect(self._on_committed)
        self._worker.previewed.connect(self._on_previewed)
        self._worker.tick_failed.connect(self._on_tick_failed)
        self._worker.decoded.connect(self._on_decoded)
        self._worker.decode_failed.connect(self._on_decode_failed)

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

        # Ticks turned off means there is nothing to schedule, so the timer is
        # not created at all rather than created and left stopped.
        self._preview_timer: QTimer | None = None
        if config.preview.enabled:
            self._preview_timer = QTimer(self)
            # Single-shot and re-armed after each result: the gap between
            # ticks depends on how long the last one took. See _rearm_previews.
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
                self._utterance += 1
                self._committed_hint = 0
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
                # Before the decode is even requested: a tick already queued
                # on the worker must be dropped, not run ahead of it.
                self._abandon_previews()
                samples = self._audio.stop_capture()
                self._log_capture(spoken, samples)
                self._decode_requested.emit(samples, self._utterance)
            case DiscardCapture():
                self._level_timer.stop()
                self._abandon_capture()
                self._audio.stop_capture()
                log.info("discarded capture (too short, or cancelled)")
            case AbortDecode():
                self._abandon_capture()
                log.info("cancelled; the rest of this decode will not be appended")
            case _:
                assert_never(command)

    def _abandon_capture(self) -> None:
        """Cancel the current utterance: nothing further lands, nothing lands twice.

        With text committed progressively a cancel can arrive after some of it
        is already in the buffer -- during recording as well as during the
        tail decode -- so it means "stop adding", not "none of this happened".
        What is already written stays; it is the user's file, and the wav on
        disk is untouched either way.
        """
        self._abandon_previews()
        self._aborted.add(self._utterance)
        self._worker.abandon(self._utterance)

    # ---------------------------------------------------------------- indicators

    def _sync_indicators(self) -> None:
        """Push a real phase change to the nvim winbar.

        This runs from every dispatched event; the indicator only changes when
        the phase does, so anything that leaves the phase unchanged must cost
        nothing. (Evdev auto-repeat used to reach here ~30 times a second.)
        """
        phase = phase_of(self._state)
        if phase == self._last_phase:
            return
        self._last_phase = phase
        self._nvim_phase_changed.emit(phase)
        self._nvim_latched.emit(isinstance(self._state, Recording) and self._state.latched)

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
        # Next to the capture line, and honest about which of the two it is:
        # "recorded to X" next to a file that stops in the middle is the same
        # false reassurance the old cap gave.
        match self._audio.recording_status():
            case Recorded(path=path):
                log.info("recorded to %s", path)
            case Truncated(path=path):
                log.error(
                    "the recording %s was given up on part-way, so it does NOT "
                    "hold this whole capture -- see the error above for why",
                    path,
                )
            case NotRecorded():
                pass

        # The 30Hz poll cannot see a ceiling crossed in the last tick before
        # the key came up, and "the transcript stops short and nothing said
        # so" is the failure being fixed, not a variant of it to leave in.
        if self._audio.take_cap_notice():
            self._warn_capture_capped()

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
        rather than on the next keypress. Silence in the meter is ambiguous --
        a quiet room looks the same as a dead device -- so say which it is, in
        the log and on the indicator, while the user is still holding the key
        and can do something about it.
        """
        level = self._audio.current_level()
        # nvim is across a socket, so it gets every third sample. A 24-cell
        # text meter has nothing to gain from the other twenty, and the winbar
        # redraw is nvim's main loop.
        self._level_tick += 1
        if self._level_tick % _NVIM_LEVEL_EVERY == 0:
            self._nvim_level.emit(level)
        if self._audio.take_cap_notice():
            self._warn_capture_capped()
        if self._audio.recover_if_dead():
            self._warn_no_audio()

    def _warn_no_audio(self) -> None:
        """Tell the user, where they are looking, that the microphone went away.

        This *does* overwrite the capped notice, and it is the one thing that
        may. The two messages are not competing for the same slot: a dead
        microphone is news that **invalidates** what the capped notice says.
        Past the ceiling that notice promises the rest of the audio is on disk
        and recoverable with ``spokenpad transcribe`` -- but the recorder is
        still writing, so if the input stream then dies (a live bug here,
        three occurrences in STATUS.md) what lands in the wav is silence, and
        the promise is false. Saying so, in the capped wording below, is worth
        more than keeping a message that has just stopped being true.
        """
        message = "no audio from the microphone -- reconnected, keep talking"
        if self._capped:
            # Past the ceiling the wav is the only copy, so what to say about
            # the gap depends on whether there is a wav at all. Branching on
            # `_capped` alone asserted a recording exists: with `[recording]`
            # off it replaced the correct "audio from here is being discarded"
            # with a promise of a silent gap in a file that was never opened,
            # which is the same lie in the other direction.
            match self._audio.recording_status():
                case Recorded():
                    message = (
                        "no audio from the microphone -- reconnected, but the "
                        "recording past the limit has a silent gap in it"
                    )
                case Truncated():
                    message = (
                        "no audio from the microphone -- reconnected, and the "
                        "recording had already been given up on"
                    )
                case NotRecorded():
                    message = (
                        "no audio from the microphone -- reconnected, but past "
                        "the limit and not recording: this is being discarded"
                    )
        self._nvim_preview.emit(message)

    def _warn_capture_capped(self) -> None:
        """Say, once, that the in-memory ceiling has been reached, and stop ticking.

        The old cap was silent in every channel at once -- no log line, no
        indicator -- which is how 3m41s went missing on 2026-09-08 without
        anyone able to say afterwards what had happened. It is now said in the
        log *and* where the user is looking while they are still speaking, and
        it says what to do about it, because the audio is no longer lost: it
        is on disk, and the whole thing can be decoded again.

        Ticking ends here for the rest of the capture, and ``_capped`` latches
        so that no preview -- not even one already in flight -- can write over
        the notice afterwards. That is a state rather than a stronger message
        because the two attempts before it were messages, and both were erased
        within seconds: a capped notice that survives one tick is barely
        better than the silent cap that lost 3m41s.

        Stopping the tick is not only about the screen. Past the ceiling the
        buffer is frozen, so every further tick would split byte-identical
        audio -- pure cost, no news. What was committed before the ceiling
        stands, and the release decode picks up from there.
        """
        self._capped = True
        self._abandon_previews()
        self._nvim_previewing.emit(False)
        minutes = MAX_UTTERANCE_SECONDS / 60
        match self._audio.recording_status():
            case Recorded(path=path):
                log.warning(
                    "this capture has passed the %.0f-minute in-memory ceiling, so the "
                    "transcript will stop there -- but the audio is still being recorded "
                    "in full. Recover the rest with `spokenpad transcribe %s`.",
                    minutes, path,
                )
                message = (
                    f"past the {minutes:.0f}min limit -- the rest is on disk, "
                    "recover it with spokenpad transcribe"
                )
            case Truncated(path=path):
                # The one case where neither half is working: the transcript
                # stops at the ceiling and the file stops wherever the disk
                # gave up. Promising `spokenpad transcribe` here would be worse
                # than saying nothing.
                log.warning(
                    "this capture has passed the %.0f-minute in-memory ceiling AND its "
                    "recording %s was given up on part-way, so what was said past the "
                    "ceiling is not anywhere. Stop and start a new recording.",
                    minutes, path,
                )
                message = (
                    f"past the {minutes:.0f}min limit and the recording failed -- "
                    "stop and start a new one"
                )
            case NotRecorded():
                log.warning(
                    "this capture has passed the %.0f-minute in-memory ceiling and is NOT "
                    "being recorded, so everything spoken from here is being discarded. "
                    "Stop and start a new recording, and turn [recording] back on.",
                    minutes,
                )
                message = (
                    f"past the {minutes:.0f}min limit and not recording -- "
                    "audio from here is being discarded"
                )
        self._nvim_preview.emit(message)

    # ------------------------------------------------------------------- the tick

    def _arm_previews(self) -> None:
        """Start ticking for a new utterance, from a blank slate."""
        self._worker.tick_for(self._utterance)
        self._capped = False
        self._preview_requested_at = 0.0
        self._preview_seconds = 0.0
        self._preview_timed = False
        self._tick_failures = 0
        self._preview_text = ""
        self._preview_epoch = 0
        self._nvim_preview.emit("")
        self._nvim_previewing.emit(self._config.preview.enabled)
        if self._preview_timer is not None:
            self._preview_timer.start(self._config.preview.interval_ms)

    def _abandon_previews(self) -> None:
        """Stop ticking, and drop whatever tick is already queued on the worker.

        Telling the worker *before* the release decode is requested is what
        guarantees a queued tick is skipped entirely rather than run ahead of
        the text the user is waiting for. One already running stops after its
        current chunk, which -- if it was settled -- it has just committed.
        """
        self._worker.stop_ticks()
        if self._preview_timer is not None:
            self._preview_timer.stop()

    def _request_preview(self) -> None:
        """Ask the worker for a tick over the audio past the committed offset.

        From the *hint*, which lags the worker's real offset by at most the
        chunks a tick in flight has committed: the worker slices the rest off
        itself. Snapshotted non-destructively, so the capture keeps
        accumulating and the release decode still sees everything.
        """
        if phase_of(self._state) is not Phase.RECORDING:
            return
        samples = self._audio.snapshot_capture(since_frame=self._committed_hint)
        seconds = samples.size / self._config.audio.sample_rate
        if seconds > self._config.preview.max_seconds:
            # Only reachable without a VAD model, where nothing ever settles
            # and the tail is the whole capture. The text stays exactly as it
            # is -- this stops updating it, it does not clear it.
            log.debug(
                "uncommitted audio past %.0fs; no more previews for this utterance "
                "(the last one stays on screen)",
                self._config.preview.max_seconds,
            )
            self._nvim_previewing.emit(False)
            return
        if samples.size == 0:
            self._rearm_previews(0.0)
            return
        self._preview_requested_at = time.monotonic()
        self._preview_seconds = seconds
        self._preview_requested.emit(samples, self._committed_hint, self._utterance)

    def _rearm_previews(self, last_decode_seconds: float) -> None:
        """Schedule the next tick, keeping the worker idle at least half the time.

        The gap is ``max(interval - decode, decode)``, so the *period* is
        ``max(interval, 2 * decode)``. Short tails keep the configured cadence
        exactly; a long one slows down on its own rather than pinning the
        worker. The duty cycle is what matters: a tick already inside a decode
        when the key comes up cannot be cancelled, so the fraction of time one
        is running is the fraction of releases that pay for it.

        A repeating timer cannot express this, hence single-shot plus re-arm.
        """
        if self._preview_timer is None or phase_of(self._state) is not Phase.RECORDING:
            return
        interval = self._config.preview.interval_ms / 1000.0
        gap = max(interval - last_decode_seconds, last_decode_seconds)
        self._preview_timer.start(int(gap * 1000))

    # -------------------------------------------------------------------- results

    def _on_committed(self, text: str, utterance: int, through: int) -> None:
        """One chunk, decoded once, into the buffer -- during recording or after.

        The first chunk of an utterance opens a paragraph and the rest extend
        it, so what the user sees is a paragraph growing rather than several
        appearing. Nothing here can run ahead of itself: appends cross to the
        bridge thread on a queued signal, so they land in the order this
        method emitted them.
        """
        if utterance in self._aborted:
            log.debug("dropping a chunk from a cancelled utterance")
            return
        if utterance == self._utterance:
            self._committed_hint = max(self._committed_hint, through)
        cleaned = postprocess(text, self._config.text)
        if not cleaned.strip():
            return
        self._nvim_append_requested.emit(cleaned, self._paragraph_of == utterance)
        self._paragraph_of = utterance

    def _on_previewed(self, text: str, utterance: int, through: int) -> None:
        """Show the open tail, unless it has been overtaken by events.

        Everything here is DEBUG: a preview is cosmetic, and previews arrive
        about once a second, so anything louder would drown the log that
        matters.
        """
        if self._capped:
            # Before the re-arm, not just before the write: re-arming would
            # resurrect the timer `_warn_capture_capped` just stopped, and the
            # preview after that would take the notice off the screen. There
            # is no legitimate preview past the ceiling -- the buffer is
            # frozen, so it would say nothing new even if it were free.
            log.debug("dropping a preview: this capture is capped")
            return
        if utterance != self._utterance or phase_of(self._state) is not Phase.RECORDING:
            log.debug("dropping a preview that no longer applies (utterance %d)", utterance)
            return
        round_trip = (
            time.monotonic() - self._preview_requested_at if self._preview_requested_at else 0.0
        )
        self._rearm_previews(round_trip)
        if not self._preview_timed and self._preview_requested_at:
            # Once per utterance, so tick cost stays visible in the log file
            # without flooding it. Round trip, so it includes the queue hop the
            # user would actually feel if a tick ever delayed a decode.
            self._preview_timed = True
            log.debug(
                "tick round trip %.0fms for %.1fs of audio",
                round_trip * 1000,
                self._preview_seconds,
            )
        if through != self._preview_epoch:
            # A chunk landed above: the tail starts over, and a shorter one
            # is the truth rather than instability.
            self._preview_epoch = through
            self._preview_text = ""
        cleaned = postprocess(text, self._config.text)
        # Within one offset each preview sees strictly more audio than the
        # last, so the tail should only ever grow -- but the recogniser does
        # not guarantee that. Decoding 4.4s of a real sample returned "Okay."
        # where 3.3s of the same sample had returned "Okay, we are now at the
        # new model.". Showing that verbatim is words the user watched appear,
        # disappearing again; so a preview that came back shorter is treated
        # as instability and the previous text stands.
        if len(cleaned) < len(self._preview_text):
            log.debug("ignoring a preview that shrank: %r", cleaned[:40])
            return
        self._preview_text = cleaned
        self._nvim_preview.emit(cleaned)

    def _on_tick_failed(self, reason: str, utterance: int) -> None:
        """A tick raised. Say so, and keep ticking: the next one may well succeed,
        and stopping here would quietly turn the rest of a long passage back
        into one big decode at release."""
        if utterance != self._utterance or phase_of(self._state) is not Phase.RECORDING:
            return
        self._tick_failures += 1
        # Once at WARNING per utterance; a persistent failure at 1Hz belongs
        # in the DEBUG file, not on the console.
        level = logging.WARNING if self._tick_failures == 1 else logging.DEBUG
        log.log(level, "a tick failed (%s); still recording, will keep trying", reason)
        round_trip = (
            time.monotonic() - self._preview_requested_at if self._preview_requested_at else 0.0
        )
        self._rearm_previews(round_trip)

    def _on_decoded(self, text: str, elapsed: float, tail_seconds: float, utterance: int) -> None:
        if utterance in self._aborted:
            log.info("dropping the rest of a cancelled decode (%.2fs)", elapsed)
            return
        cleaned = postprocess(text, self._config.text)
        log.info(
            "decoded the last %.1fs in %.2fs; utterance %d chars: %r",
            tail_seconds, elapsed, len(cleaned), cleaned[:80],
        )
        if cleaned != text:
            log.debug("raw model output: %r", text)
        log.debug("full transcript: %r", cleaned)
        if not cleaned.strip():
            log.warning("decode produced no text")
        if utterance == self._utterance:
            # A decode from the utterance *before* this one finishing late is
            # not this one finishing; it must not settle a Transcribing state
            # that belongs to a newer capture.
            self._dispatch(DecodeFinished(at=time.monotonic()))

    def _on_decode_failed(self, reason: str, utterance: int) -> None:
        if utterance not in self._aborted:
            log.error("decode failed: %s", reason)
        if utterance == self._utterance:
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


DEFAULT_LOG_FILE = xdg_state_home() / "spokenpad" / "spokenpad.log"


def _configure_logging(*, verbose: bool, log_file: Path | None) -> None:
    """Console plus a rotating file.

    The file is always at DEBUG regardless of ``-v``: a dictation problem is
    usually only noticed after the fact, and re-running to reproduce it is not
    always possible. Rotation keeps it bounded without any maintenance.

    Note the file records transcribed text, so it holds whatever was dictated.
    It stays on this machine, mode 0600, and `--log-file none` turns it off.
    """
    root = logging.getLogger("spokenpad")
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


EXIT_MODEL_MISSING = 2
EXIT_HOTKEY_PERMISSION = 3
EXIT_UNREADABLE_RECORDING = 4


def transcribe_recording(wav: Path, out: Path | None, config: Config) -> int:
    """Decode a recorded capture and write the transcript out. The recovery path.

    This is what makes a recording worth having. It runs the *same* pipeline
    as live dictation -- :func:`~spokenpad.decode.decode_capture`, the same VAD
    segmentation, the same model, the same post-processing -- so what comes
    back is what the daemon would have produced at the time, not an
    approximation of it produced by a second implementation.

    No Qt, no hotkey, no microphone: this runs in a terminal, over a file,
    long after the fact.
    """
    try:
        samples, rate = read_capture(wav)
    except RecordingError as e:
        log.error("%s", e)
        return EXIT_UNREADABLE_RECORDING
    if rate != config.audio.sample_rate:
        log.error(
            "%s is %d Hz but the model expects %d Hz. Decoding it anyway would "
            "produce plausible nonsense rather than an error, so it is refused.",
            wav, rate, config.audio.sample_rate,
        )
        return EXIT_UNREADABLE_RECORDING

    try:
        transcriber = Transcriber(config.asr)
    except ModelMissingError as e:
        log.error("%s", e)
        return EXIT_MODEL_MISSING
    log.info("decoding %.1fs of audio from %s", samples.size / rate, wav)
    text = postprocess(
        decode_capture(
            samples,
            transcriber=transcriber,
            segmenter=load_segmenter(config.vad, rate),
            sample_rate=rate,
        ),
        config.text,
    )
    if not text.strip():
        log.warning("%s decoded to nothing", wav)
    if out is None:
        # The log goes to stderr, so stdout carries the transcript alone and
        # this composes with a pipe.
        print(text)
    else:
        out.write_text(text + "\n", encoding="utf-8")
        log.info("wrote %d chars to %s", len(text), out)
    return 0


def run_daemon(config: Config, dump_audio: Path | None) -> int:
    """The daemon: hotkey, microphone, model, dictation window. Blocks."""
    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)  # there are no Qt windows; closing none must not exit

    try:
        daemon = Daemon(config)
        if dump_audio is not None:
            dump_audio.mkdir(parents=True, exist_ok=True)
            daemon.set_dump_dir(dump_audio)
        daemon.start()
    except ModelMissingError as e:
        log.error("%s", e)
        return EXIT_MODEL_MISSING
    except HotkeyPermissionError as e:
        log.error("%s", e)
        return EXIT_HOTKEY_PERMISSION

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


def main() -> int:
    """``spokenpad`` runs the daemon; ``spokenpad transcribe`` recovers a wav.

    The subcommand is optional, so the bare invocation the systemd unit and
    every existing habit use is untouched. The options common to both stay on
    the top-level parser rather than being repeated per subcommand -- argparse
    lets a subparser's defaults overwrite what the top level already parsed,
    so ``-v`` in both places would silently mean *less* verbose, not more.
    """
    parser = argparse.ArgumentParser(prog="spokenpad", description="Push-to-talk dictation.")
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
    sub = parser.add_subparsers(dest="command")
    recover = sub.add_parser(
        "transcribe",
        help="decode a recorded capture (see [recording]) and print the transcript",
        description=(
            "Decode a wav through the same VAD and model the daemon uses. "
            "Global options go before the subcommand: spokenpad -v transcribe FILE."
        ),
    )
    recover.add_argument("wav", type=Path, help="a capture from the recording directory")
    recover.add_argument(
        "--out", type=Path, default=None, metavar="PATH", help="write here instead of stdout"
    )
    args = parser.parse_args()

    _configure_logging(verbose=args.verbose, log_file=args.log_file)

    config = Config.load(args.config)
    if args.model_dir is not None:
        config = config.with_model_dir(args.model_dir)

    if args.command == "transcribe":
        return transcribe_recording(args.wav, args.out, config)
    return run_daemon(config, args.dump_audio)


if __name__ == "__main__":
    raise SystemExit(main())
