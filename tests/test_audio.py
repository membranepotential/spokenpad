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

from voice_kb.audio import STALE_STREAM_SECONDS, AudioCapture, _RingBuffer
from voice_kb.config import AudioConfig

_NO_STATUS = sd.CallbackFlags()
"""An empty PortAudio status: no overflow, no underflow."""


class _FakeInputStream:
    """Stands in for ``sounddevice.InputStream``: records the callback,
    never touches real audio hardware."""

    def __init__(self, **kwargs: Any) -> None:
        self.callback = kwargs["callback"]
        # Mirrors sounddevice.InputStream.active, which AudioCapture's stale
        # stream watchdog reads. A fake that omits it would make the watchdog
        # untestable here.
        self.active = False

    def start(self) -> None:
        self.active = True
        # A real InputStream begins delivering immediately; AudioCapture proves
        # liveness at open time by waiting for the first callback, so a fake
        # that never calls back would just burn that timeout on every open.
        self.callback(np.zeros((256, 1), dtype=np.float32), 256, None, _NO_STATUS)

    def stop(self) -> None:
        self.active = False

    def close(self) -> None:
        self.active = False


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


def test_dead_stream_is_detected_and_reopened(_fake_sounddevice: None) -> None:
    """A stream whose callback has gone silent must be replaced, not trusted.

    Regression: PipeWire suspended the device mid-session. The callback simply
    stopped, nothing raised, and every subsequent capture returned the same
    frozen 250ms pre-roll ring -- byte-identical, decoded to nothing, over and
    over. Silent failure is the one mode this project refuses.
    """
    cap = AudioCapture(AudioConfig(preroll_ms=250))
    first = cap._stream
    assert first is not None

    # The stream is alive and calling back: no reopen.
    cap._callback(np.zeros((512, 1), dtype=np.float32), 512, None, _NO_STATUS)
    cap.start_capture()
    assert cap._stream is first
    cap.stop_capture()

    # Now it goes quiet for longer than the watchdog tolerates.
    cap._last_callback_at = time.monotonic() - (STALE_STREAM_SECONDS + 0.5)
    cap.start_capture()
    assert cap._stream is not first, "a silent stream should have been reopened"
    cap.stop_capture()
    cap.close()


def test_inactive_stream_is_reopened_even_if_recently_called_back(
    _fake_sounddevice: None,
) -> None:
    """`active` going False is enough on its own; do not wait for the timeout."""
    cap = AudioCapture(AudioConfig(preroll_ms=250))
    first = cap._stream
    assert first is not None
    cap._callback(np.zeros((512, 1), dtype=np.float32), 512, None, _NO_STATUS)
    first.active = False

    cap.start_capture()
    assert cap._stream is not first
    cap.stop_capture()
    cap.close()


# -- non-destructive snapshots for the overlay's live preview -----------------


def _feed(capture: AudioCapture, values: list[float], frames: int = 4) -> None:
    """Push `frames`-sample callbacks, one constant value each, as PortAudio would."""
    for value in values:
        capture._callback(np.full((frames, 1), value, dtype=np.float32), frames, None, _NO_STATUS)


def test_snapshot_returns_what_has_arrived_so_far_mid_capture() -> None:
    """The preview needs the in-flight audio without ending the capture."""
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0))
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0])

    snapshot = cap.snapshot_capture()

    assert snapshot.tolist() == [1.0] * 4 + [2.0] * 4 + [3.0] * 4
    cap.stop_capture()
    cap.close()


def test_snapshot_respects_max_frames_and_returns_the_tail() -> None:
    """The window is trailing, not leading: the newest audio is what a preview
    must decode, and its cost must not grow with the utterance."""
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0))
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0, 4.0, 5.0])

    snapshot = cap.snapshot_capture(6)

    assert snapshot.size == 6
    assert snapshot.tolist() == [4.0] * 2 + [5.0] * 4


def test_snapshot_walks_only_the_chunks_the_window_needs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Invariant 2: a preview's cost is independent of utterance length. If
    the tail were taken by concatenating the whole buffer and slicing, a long
    dictation would make every preview steadily more expensive than the last
    -- which is exactly the growing-buffer cost this project exists to avoid.
    """
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0))
    cap.start_capture()
    _feed(cap, [float(i) for i in range(500)])
    assert len(cap._chunks) == 500

    joined: list[int] = []
    real_concatenate = np.concatenate

    def counting_concatenate(chunks: Any, *args: Any, **kwargs: Any) -> Any:
        joined.append(len(chunks))
        return real_concatenate(chunks, *args, **kwargs)

    # `audio.py` does `import numpy as np`, so it resolves `np.concatenate`
    # against this very module object at call time.
    monkeypatch.setattr(np, "concatenate", counting_concatenate)
    snapshot = cap.snapshot_capture(8)

    assert snapshot.tolist() == [498.0] * 4 + [499.0] * 4
    # Two 4-frame chunks cover the 8-frame window; the other 498 are untouched.
    assert joined == [2]


def test_snapshot_does_not_disturb_a_subsequent_stop_capture() -> None:
    """The committed decode still sees the complete buffer: previewing must
    not consume, truncate or reorder a single sample of it."""
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0))
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0])

    cap.snapshot_capture(4)
    cap.snapshot_capture()
    _feed(cap, [4.0])
    cap.snapshot_capture(2)

    full = cap.stop_capture()

    assert full.tolist() == [1.0] * 4 + [2.0] * 4 + [3.0] * 4 + [4.0] * 4
    cap.close()


def test_snapshot_while_idle_returns_an_empty_array() -> None:
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0))

    snapshot = cap.snapshot_capture(1000)

    assert snapshot.size == 0
    assert snapshot.dtype == np.float32
    cap.close()
