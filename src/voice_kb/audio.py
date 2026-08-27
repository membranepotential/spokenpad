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
writes into a buffer sized up front.
"""

from __future__ import annotations

import logging
import threading
import time

import numpy as np
import numpy.typing as npt
import sounddevice as sd

from voice_kb.config import AudioConfig

type MonoAudio = npt.NDArray[np.float32]

log = logging.getLogger("voice-kb.audio")

MAX_UTTERANCE_SECONDS = 600.0
"""Hard ceiling on a single capture. Guards against unbounded memory growth
if the hotkey's key-up is ever lost (stuck key, dropped event, wedged
device) -- without this, `_chunks` would grow for as long as the key stays
down. At 16 kHz float32 mono this is ~38 MB, negligible to hold and easy to
notice in the overlay if it is ever actually hit."""

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
    """

    def __init__(self, config: AudioConfig) -> None:
        self._config = config
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
                remaining = self._max_frames - self._frames_captured
                if remaining <= 0:
                    return
                chunk = mono[:remaining] if frames > remaining else mono
                self._chunks.append(chunk.copy())
                self._frames_captured += len(chunk)
            elif self._ring is not None:
                self._ring.write(mono)

    def start_capture(self) -> None:
        """Begin accumulating audio, seeded with the pre-roll buffer if any."""
        if self._config.preroll_frames == 0:
            self._stream = self._open_stream()
        else:
            self._ensure_stream_alive()

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
            self._capturing = True

    def stop_capture(self) -> MonoAudio:
        """Stop accumulating and return everything captured, oldest first."""
        with self._lock:
            chunks = self._chunks
            self._chunks = []
            self._frames_captured = 0
            self._capturing = False

        if self._config.preroll_frames == 0 and self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None

        if not chunks:
            return np.zeros(0, dtype=np.float32)
        return chunks[0] if len(chunks) == 1 else np.concatenate(chunks)

    def snapshot_capture(self, max_frames: int | None = None) -> MonoAudio:
        """The audio captured so far, without ending the capture.

        Returns at most the trailing ``max_frames`` samples. Copies out under
        the same lock the realtime callback uses, so it never observes a
        partially appended chunk.

        Only the trailing chunks needed to satisfy ``max_frames`` are walked
        and concatenated -- never the whole buffer -- so the cost of a
        snapshot is bounded by ``max_frames`` and does *not* grow with the
        length of the utterance. That bound is what makes the overlay's live
        preview safe: a five-minute dictation costs the same per preview as a
        ten-second one (``docs/constraints.md``).

        This is deliberately non-destructive. It never touches ``_chunks``,
        ``_frames_captured`` or ``_capturing``, so the committed decode at key
        release still sees the complete buffer, exactly once, via
        :meth:`stop_capture`.
        """
        if max_frames is not None and max_frames <= 0:
            return np.zeros(0, dtype=np.float32)

        with self._lock:
            if not self._capturing:
                return np.zeros(0, dtype=np.float32)
            if max_frames is None:
                tail = list(self._chunks)
            else:
                tail = []
                frames = 0
                for chunk in reversed(self._chunks):
                    tail.append(chunk)
                    frames += len(chunk)
                    if frames >= max_frames:
                        break
                tail.reverse()

        # Concatenation happens outside the lock on purpose: every chunk in
        # `tail` is already an immutable-by-convention copy made by the
        # callback, so nothing can mutate them, and the realtime thread must
        # not be made to wait on an allocation it does not need to.
        if not tail:
            return np.zeros(0, dtype=np.float32)
        audio = np.concatenate(tail) if len(tail) > 1 else tail[0].copy()
        if max_frames is not None and audio.size > max_frames:
            audio = audio[-max_frames:]
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

    def take_stream_status(self) -> str | None:
        """Any PortAudio status flags seen since the last call (overflows etc.)."""
        status, self._last_status = self._last_status, None
        return status

    def current_level(self) -> float:
        """Cheap peak level of the most recent callback, 0.0-1.0, for the overlay meter."""
        return self._level

    def close(self) -> None:
        """Release the stream. Only meaningful when ``preroll_ms > 0`` kept it open."""
        if self._stream is not None:
            self._stream.stop()
            self._stream.close()
            self._stream = None

    def __enter__(self) -> AudioCapture:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()
