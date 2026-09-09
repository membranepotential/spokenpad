"""Microphone capture: one input stream, a pre-roll ring buffer, nothing else.

There is exactly one :class:`sounddevice.InputStream` per :class:`AudioCapture`
instance, mono, ``float32``, at :attr:`AudioConfig.sample_rate`. When
``preroll_ms`` is nonzero the stream is opened once (at construction) and
left running so a fixed-size ring buffer can be kept warm; a capture then
starts with that buffer's contents as its head, so the word spoken in the
instant between "finger touches key" and "keydown event reaches this
process" is not clipped. When ``preroll_ms`` is ``0`` there is nothing to
keep warm, so the stream is opened lazily in :meth:`AudioCapture.start_capture`
and closed again in :meth:`AudioCapture.stop_capture` instead of idling for
the life of the process.

The stream's callback runs on PortAudio's realtime thread. It does the
minimum required to stay off the audio thread's bad side: no logging, no
exceptions raised, no unbounded allocation. It copies at most one
callback's worth of samples per call (unavoidable -- PortAudio reuses its
input buffer immediately after the callback returns) and otherwise only
writes into a buffer sized up front. The one thing it hands outside this
module is that copy, pushed onto :class:`~spokenpad.recorder.CaptureRecorder`'s
queue for a writer thread to put on disk -- a queue push, never the write
itself, because a blocking write here is an xrun.
"""

from __future__ import annotations

import logging
import threading
import time
from typing import TYPE_CHECKING

import numpy as np
import numpy.typing as npt
import sounddevice as sd

from spokenpad.config import AudioConfig

if TYPE_CHECKING:
    from spokenpad.recorder import CaptureRecorder, RecordingStatus

type MonoAudio = npt.NDArray[np.float32]

log = logging.getLogger("spokenpad.audio")

MAX_UTTERANCE_SECONDS = 3600.0
"""Hard ceiling on how much of a capture is held **in memory**.

Its only job is to bound `_chunks` if the hotkey's key-up is ever lost (stuck
key, dropped event, wedged device), where the buffer would otherwise grow for
as long as the key stays down. At 16 kHz float32 mono an hour is ~230 MB.

It used to be 600s and it used to be a data-loss bug. On 2026-09-08 a hold of
821.3s hit it and the samples past the ceiling were dropped inside the
callback below -- the only copy in existence, because everything else sits
downstream of it. The docstring at the time claimed the cap was "easy to
notice in the overlay", which was false: it fired with no log line and no
indicator at all, and 3m41s of dictation was simply gone.

Both halves of that are fixed. Every callback now reaches
:class:`~spokenpad.recorder.CaptureRecorder` *before* this ceiling is
consulted, so what is on disk is never bounded by it, and hitting it is
reported at WARNING and on the indicators (see :meth:`AudioCapture.take_cap_notice`).
The number is generous because it no longer protects anything the user cares
about -- only the daemon's resident memory.
"""

FIRST_CALLBACK_TIMEOUT = 0.5
"""How long a newly opened stream gets to deliver its first callback.

A stream can be born dead -- opened without error, reporting ``active``, and
never calling back. The watchdog below only notices once enough time has
passed, which costs the user their first dictation. Proving liveness at open
time closes that gap.
"""

STALE_STREAM_SECONDS = 1.0
"""If no callback has fired in this long, treat the stream as dead.

PortAudio streams on Linux do die: PipeWire can suspend the device, the
default source can change, the server can restart. When that happens the
callback simply stops and nothing raises -- capture then returns only the
pre-roll ring, byte-identical every time, and the model dutifully decodes
0.25s of room noise into nothing. Observed in the wild; hence the watchdog.
"""


class AudioCaptureError(RuntimeError):
    """A PortAudio operation failed (device missing, busy, disconnected, ...)."""


class _RingBuffer:
    """Fixed-capacity float32 ring buffer, written from the realtime callback.

    Every write is either a plain slice assignment or two of them (on wrap),
    so it never allocates.
    """

    __slots__ = ("_buf", "_capacity", "_filled", "_write_pos")

    def __init__(self, capacity: int) -> None:
        self._capacity = capacity
        self._buf: MonoAudio = np.zeros(capacity, dtype=np.float32)
        self._write_pos = 0
        self._filled = 0

    def write(self, chunk: MonoAudio) -> None:
        n = len(chunk)
        if n >= self._capacity:
            self._buf[:] = chunk[-self._capacity :]
            self._write_pos = 0
            self._filled = self._capacity
            return
        end = self._write_pos + n
        if end <= self._capacity:
            self._buf[self._write_pos : end] = chunk
        else:
            first = self._capacity - self._write_pos
            self._buf[self._write_pos :] = chunk[:first]
            self._buf[: end - self._capacity] = chunk[first:]
        self._write_pos = end % self._capacity
        self._filled = min(self._capacity, self._filled + n)

    def snapshot(self) -> MonoAudio:
        """The buffered samples in chronological order, copied out."""
        if self._filled < self._capacity:
            return self._buf[: self._filled].copy()
        if self._write_pos == 0:
            return self._buf.copy()
        return np.concatenate((self._buf[self._write_pos :], self._buf[: self._write_pos]))


class AudioCapture:
    """Owns the microphone stream and the in-flight utterance buffer.

    Not safe to call from multiple threads concurrently; the caller (the
    session state machine, via a single hotkey watcher thread) already
    serialises start/stop/snapshot calls with respect to each other. It is
    always safe to call :meth:`current_level` from any thread at any time.

    :meth:`snapshot_capture` is safe *against the realtime callback* -- it
    reads under the same lock -- and is non-destructive, so it can be called
    mid-capture without disturbing what :meth:`stop_capture` will return.

    A :class:`~spokenpad.recorder.CaptureRecorder` is required rather than
    optional: "not recording" is a state the recorder itself expresses
    (``RecordingConfig.enabled``), and making it a ``None`` here would put a
    branch on the realtime path and leave every caller to invent what "no
    recorder" means.
    """

    def __init__(self, config: AudioConfig, recorder: CaptureRecorder) -> None:
        self._config = config
        self._recorder = recorder
        self._max_frames = int(MAX_UTTERANCE_SECONDS * config.sample_rate)

        self._ring: _RingBuffer | None = None
        if config.preroll_frames > 0:
            self._ring = _RingBuffer(config.preroll_frames)

        self._lock = threading.Lock()
        self._capturing = False
        self._chunks: list[MonoAudio] = []
        self._frames_captured = 0
        self._level = 0.0
        self._last_callback_at = 0.0
        self._last_status: str | None = None
        self._callback_count = 0
        #: Whether the ceiling was reached, and whether that has been reported
        #: yet. Two facts, not one: the capture can end in the same 33ms tick
        #: that crosses the ceiling, and the news must survive that.
        self._capped = False
        self._cap_reported = False

        self._stream: sd.InputStream | None = None
        if self._ring is not None:
            self._stream = self._open_stream()

    def _open_stream(self) -> sd.InputStream:
        # Sampled before start(): a stream can deliver its first buffer during
        # start() itself, and that callback must count toward liveness.
        count_before = self._callback_count
        try:
            stream = sd.InputStream(
                samplerate=self._config.sample_rate,
                channels=1,
                dtype="float32",
                device=self._config.device,
                callback=self._callback,
            )
            stream.start()
        except sd.PortAudioError as e:
            raise AudioCaptureError(f"failed to open input stream: {e}") from e
        # A brand new stream has not called back yet; without this the watchdog
        # would judge it stale on the very first capture and reopen it, throwing
        # away the pre-roll it was about to use.
        self._last_callback_at = time.monotonic()
        if not self._wait_for_first_callback(count_before):
            log.error(
                "input stream opened but delivered no audio within %.0fms. "
                "The device may be suspended or held by another client; "
                "check `pactl list short sources`.",
                FIRST_CALLBACK_TIMEOUT * 1000,
            )
        return stream

    def _wait_for_first_callback(self, count_before: int) -> bool:
        """Block briefly until the new stream proves it is delivering audio.

        Liveness is observed through the callback rather than the stream handle,
        because a dead stream still reports ``active``.
        """
        start_count = count_before
        deadline = time.monotonic() + FIRST_CALLBACK_TIMEOUT
        while time.monotonic() < deadline:
            if self._callback_count > start_count:
                self._last_callback_at = time.monotonic()
                return True
            time.sleep(0.01)
        return False

    def _callback(
        self,
        indata: MonoAudio,
        frames: int,
        _time_info: object,
        _status: sd.CallbackFlags,
    ) -> None:
        mono = indata[:, 0]
        self._level = float(np.abs(mono).max()) if frames else 0.0
        self._last_callback_at = time.monotonic()
        self._callback_count += 1
        if _status:
            # Cannot log from a realtime thread; stash it for the next caller.
            self._last_status = str(_status)

        with self._lock:
            if self._capturing:
                # One copy, shared by both sinks: PortAudio reuses `indata`
                # the moment this returns, and neither sink mutates it.
                captured = mono.copy()
                # Disk first, and deliberately *above* the ceiling below.
                # Everything that discarded audio on 2026-09-08 sat downstream
                # of that `return`; nothing that decides how much to keep in
                # memory may ever again decide what reaches the file.
                self._recorder.write(captured)
                remaining = self._max_frames - self._frames_captured
                if remaining <= 0:
                    return
                chunk = captured[:remaining] if frames > remaining else captured
                self._chunks.append(chunk)
                self._frames_captured += len(chunk)
            elif self._ring is not None:
                self._ring.write(mono)

    def start_capture(self) -> None:
        """Begin accumulating audio, seeded with the pre-roll buffer if any."""
        if self._config.preroll_frames == 0:
            self._stream = self._open_stream()
        else:
            self._ensure_stream_alive()

        self._capped = False
        self._cap_reported = False
        self._recorder.start()

        with self._lock:
            # Snapshotting the ring buffer must happen under the same lock
            # the callback uses for `_RingBuffer.write` (the `_capturing is
            # False` branch below). Taken outside the lock, a concurrent
            # write could be mid-splice when `snapshot()` reads `_write_pos`,
            # returning the pre-roll reordered; taken outside entirely, a
            # write landing between the snapshot and the `_capturing = True`
            # flip would be silently dropped instead of ending up in
            # `_chunks`. Holding the lock across both closes both gaps.
            preroll = self._ring.snapshot() if self._ring is not None else None
            self._chunks = [preroll] if preroll is not None and preroll.size else []
            self._frames_captured = preroll.size if preroll is not None else 0
            # The pre-roll is the run-up to the first word, so it belongs in
            # the recording too -- and it has to go in here, under the lock and
            # before the flag flips, or the first callback of the capture could
            # reach the file ahead of the audio that precedes it.
            if preroll is not None and preroll.size:
                self._recorder.write(preroll)
            self._capturing = True

    def stop_capture(self) -> MonoAudio:
        """Stop accumulating and return everything captured, oldest first."""
        with self._lock:
            chunks = self._chunks
            # Latched here, from the final frame count, because the ceiling can
            # be crossed inside the last 33ms of a hold -- after the last poll
            # and before this. Deriving it only while capturing meant a release
            # that quick reported nothing at all: a silent cap in miniature,
            # which is the exact failure this whole change is about.
            self._capped = self._capped or self._frames_captured >= self._max_frames
            self._chunks = []
            self._frames_captured = 0
            self._capturing = False

        # After the flag is down, so no further callback can queue a buffer,
        # and before returning, so the wav is closed and complete by the time
        # the caller starts decoding what it just got back.
        self._recorder.stop()

        if self._config.preroll_frames == 0 and self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None

        if not chunks:
            return np.zeros(0, dtype=np.float32)
        return chunks[0] if len(chunks) == 1 else np.concatenate(chunks)

    def snapshot_capture(self, since_frame: int = 0) -> MonoAudio:
        """The audio captured from ``since_frame`` on, without ending the capture.

        ``since_frame`` counts from the start of the capture, pre-roll
        included, so a caller that has committed everything before it asks
        only for the rest. The result begins exactly there, or is empty if the
        capture is not that long yet (or is over).

        Copies out under the same lock the realtime callback uses, so it never
        observes a partially appended chunk, and walks only the trailing chunks
        it needs -- never the whole buffer -- so the cost is bounded by what is
        still *uncommitted*, not by the length of the utterance. That bound is
        what keeps the progressive-commit tick flat over an hour-long latched
        passage (``docs/progressive-commit.md``).

        This is deliberately non-destructive. It never touches ``_chunks``,
        ``_frames_captured`` or ``_capturing``, so the release decode still
        sees the complete buffer via :meth:`stop_capture`.
        """
        if since_frame < 0:
            raise ValueError(f"since_frame must not be negative: {since_frame}")

        with self._lock:
            if not self._capturing:
                return np.zeros(0, dtype=np.float32)
            needed = self._frames_captured - since_frame
            if needed <= 0:
                return np.zeros(0, dtype=np.float32)
            tail = []
            frames = 0
            for chunk in reversed(self._chunks):
                tail.append(chunk)
                frames += len(chunk)
                if frames >= needed:
                    break
            tail.reverse()

        # Concatenation happens outside the lock on purpose: every chunk in
        # `tail` is already an immutable-by-convention copy made by the
        # callback, so nothing can mutate them, and the realtime thread must
        # not be made to wait on an allocation it does not need to.
        audio = np.concatenate(tail) if len(tail) > 1 else tail[0].copy()
        if audio.size > needed:
            audio = audio[-needed:]
        return audio

    def _ensure_stream_alive(self) -> None:
        """Reopen the input stream if its callback has gone quiet.

        Called on the capture path rather than from a timer so the cost is paid
        once per utterance, and so a device that died while idle is repaired
        before it can swallow a dictation.
        """
        if self._stream is None:
            self._stream = self._open_stream()
            self._last_callback_at = time.monotonic()
            return
        idle = time.monotonic() - self._last_callback_at
        if getattr(self._stream, "active", True) and idle < STALE_STREAM_SECONDS:
            return
        log.warning(
            "input stream looks dead (active=%s, no callback for %.1fs); reopening",
            self._stream.active, idle,
        )
        try:
            self._stream.stop()
            self._stream.close()
        except sd.PortAudioError as e:
            log.debug("closing the dead stream failed, continuing: %s", e)
        self._stream = None
        if self._ring is not None:
            self._ring = _RingBuffer(self._config.preroll_frames)
        self._stream = self._open_stream()
        self._last_callback_at = time.monotonic()

    def seconds_since_callback(self) -> float:
        """How long since the realtime callback last delivered audio."""
        return time.monotonic() - self._last_callback_at

    def recover_if_dead(self) -> bool:
        """Reopen the stream if it has gone quiet, *keeping* the capture going.

        :meth:`_ensure_stream_alive` only runs when a capture starts, so a
        stream that dies mid-utterance goes unnoticed until the user releases
        the key and presses it again. Observed in the wild: a stream died 0.2s
        into a hold, the user spoke for 29.8s, and 0.4s of audio came back with
        nothing in the log until the *next* keypress reported "no callback for
        51.3s". This is the same watchdog, running while it can still save the
        utterance instead of after it is lost.

        Unlike :meth:`start_capture` this preserves ``_chunks`` and the
        capturing flag, so audio recorded before the stream died is kept and
        audio after the reopen appends to it. The dead interval is gone either
        way; the rest of the sentence does not have to be.

        Returns whether a reopen actually happened.
        """
        if self._stream is None or not self._capturing:
            return False
        if self.seconds_since_callback() < STALE_STREAM_SECONDS:
            return False
        log.error(
            "input stream stopped delivering audio %.1fs ago, mid-capture -- "
            "reopening now so the rest of this utterance survives. Everything "
            "spoken during the gap is lost.",
            self.seconds_since_callback(),
        )
        try:
            self._stream.stop()
            self._stream.close()
        except sd.PortAudioError as e:
            log.debug("closing the dead stream failed, continuing: %s", e)
        self._stream = None
        # The ring is only written while idle, so it is stale by definition
        # here; a capture is in progress and the callback appends to _chunks.
        if self._ring is not None:
            self._ring = _RingBuffer(self._config.preroll_frames)
        self._stream = self._open_stream()
        return True

    def take_cap_notice(self) -> bool:
        """Whether :data:`MAX_UTTERANCE_SECONDS` has bitten, once per capture.

        Derived from the frame count rather than latched in the callback, so
        the realtime thread gains no work at all from being watched, and
        cleared by :meth:`start_capture` rather than by reading -- the caller
        polls this at 30Hz for the whole of a recording and must be told once,
        not thirty times a second. :meth:`stop_capture` latches the same test
        over the final count, so a key released in the tick that crossed the
        ceiling is still reported rather than falling between two polls.

        The cap is no longer a data-loss bug (the audio is on disk regardless,
        see :data:`MAX_UTTERANCE_SECONDS`), but it does mean the transcript
        will stop short of what was said, and that is worth saying out loud
        while the user can still act on it.
        """
        if self._cap_reported:
            return False
        with self._lock:
            capped = self._capped or self._frames_captured >= self._max_frames
        self._cap_reported = capped
        return capped

    def recording_status(self) -> RecordingStatus:
        """Where the current or most recent capture went, and whether it is whole."""
        return self._recorder.status()

    def take_stream_status(self) -> str | None:
        """Any PortAudio status flags seen since the last call (overflows etc.)."""
        status, self._last_status = self._last_status, None
        return status

    def current_level(self) -> float:
        """Cheap peak level of the most recent callback, 0.0-1.0, for the level meter."""
        return self._level

    def close(self) -> None:
        """Release the stream, and any recording still open.

        Shutting down mid-capture is exactly when a recording matters most, so
        the wav is flushed and closed here as well as in :meth:`stop_capture`.
        """
        self._recorder.stop()
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None

    def __enter__(self) -> AudioCapture:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()
