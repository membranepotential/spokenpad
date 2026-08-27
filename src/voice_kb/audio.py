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

import threading

import numpy as np
import numpy.typing as npt
import sounddevice as sd

from voice_kb.config import AudioConfig

type MonoAudio = npt.NDArray[np.float32]

MAX_UTTERANCE_SECONDS = 600.0
"""Hard ceiling on a single capture. Guards against unbounded memory growth
if the hotkey's key-up is ever lost (stuck key, dropped event, wedged
device) -- without this, `_chunks` would grow for as long as the key stays
down. At 16 kHz float32 mono this is ~38 MB, negligible to hold and easy to
notice in the overlay if it is ever actually hit."""


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
    serialises start/stop calls with respect to each other. It is always
    safe to call :meth:`current_level` from any thread at any time.
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

        self._stream: sd.InputStream | None = None
        if self._ring is not None:
            self._stream = self._open_stream()

    def _open_stream(self) -> sd.InputStream:
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
        return stream

    def _callback(
        self,
        indata: MonoAudio,
        frames: int,
        _time_info: object,
        _status: sd.CallbackFlags,
    ) -> None:
        mono = indata[:, 0]
        self._level = float(np.abs(mono).max()) if frames else 0.0

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
