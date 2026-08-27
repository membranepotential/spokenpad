"""Regression coverage for the pre-roll snapshot race in ``AudioCapture``.

Drives the real :class:`~voice_kb.audio.AudioCapture` and its ``_callback``
directly, with :class:`sounddevice.InputStream` replaced by a fake so no
real soundcard is needed. The callback is invoked from a background thread
exactly as PortAudio's realtime thread would, concurrently with
``start_capture()`` on the "main" thread -- the same concurrency
``audio.py``'s docstring describes.
"""

from __future__ import annotations

import itertools
import threading
import time
from typing import Any

import numpy as np
import numpy.typing as npt
import pytest
import sounddevice as sd

from voice_kb.audio import AudioCapture, _RingBuffer
from voice_kb.config import AudioConfig


class _FakeInputStream:
    """Stands in for ``sounddevice.InputStream``: records the callback,
    never touches real audio hardware."""

    def __init__(self, **kwargs: Any) -> None:
        self.callback = kwargs["callback"]

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def close(self) -> None:
        pass


class _SlowRingBuffer(_RingBuffer):
    """``_RingBuffer`` with an artificial pause between mutating ``_buf``
    and publishing the new ``_write_pos``/``_filled``.

    A real write's buffer mutation is fast enough that hitting the race in
    ``start_capture`` depends on unpredictable thread scheduling. The pause
    widens that window to something a test can hit deterministically,
    without changing the write's actual logic -- it is only ever used to
    make this regression test reliable, never shipped in the real capture
    path.
    """

    def write(self, chunk: npt.NDArray[np.float32]) -> None:
        n = len(chunk)
        if n >= self._capacity:
            self._buf[:] = chunk[-self._capacity :]
            time.sleep(0.001)
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
        time.sleep(0.001)
        self._write_pos = end % self._capacity
        self._filled = min(self._capacity, self._filled + n)


@pytest.fixture(autouse=True)
def _fake_sounddevice(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sd, "InputStream", _FakeInputStream)


def test_preroll_snapshot_is_never_reordered_under_concurrent_writes() -> None:
    """Regression for the pre-roll race: without holding the same lock across
    the ring-buffer snapshot and the ``_capturing`` flip, a write landing
    mid-``snapshot()`` can splice the ring at a stale write position and
    hand back samples out of chronological order.

    Each callback chunk is filled with one value from a strictly increasing
    counter, so a correctly ordered pre-roll snapshot is always
    non-decreasing left to right; any decrease is a reordering. The ring is
    swapped for :class:`_SlowRingBuffer` so the write's critical section
    takes long enough that a racing, unlocked ``snapshot()`` reliably lands
    inside it.
    """
    config = AudioConfig(sample_rate=1000, preroll_ms=50, device=None)
    capture = AudioCapture(config)
    assert capture._ring is not None
    capture._ring = _SlowRingBuffer(config.preroll_frames)

    counter = itertools.count()
    stop_writing = threading.Event()

    def writer() -> None:
        while not stop_writing.is_set():
            value = float(next(counter))
            chunk = np.full((7, 1), value, dtype=np.float32)
            capture._callback(chunk, 7, None, None)

    writer_thread = threading.Thread(target=writer, daemon=True)
    writer_thread.start()
    time.sleep(0.02)  # let the ring buffer fill and start wrapping

    try:
        reordered = 0
        iterations = 200
        for _ in range(iterations):
            capture.start_capture()
            snapshot = capture.stop_capture()
            if snapshot.size > 1 and np.any(np.diff(snapshot) < 0):
                reordered += 1
    finally:
        stop_writing.set()
        writer_thread.join(timeout=2)
        capture.close()

    assert reordered == 0, f"{reordered}/{iterations} pre-roll snapshots were reordered"
