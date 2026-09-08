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
from pathlib import Path
from typing import Any

import numpy as np
import numpy.typing as npt
import pytest
import sounddevice as sd

from voice_kb import audio as audio_module
from voice_kb.audio import STALE_STREAM_SECONDS, AudioCapture, _RingBuffer
from voice_kb.config import AudioConfig, RecordingConfig
from voice_kb.recorder import CaptureRecorder, NotRecorded, Recorded, read_capture

_NO_STATUS = sd.CallbackFlags()
"""An empty PortAudio status: no overflow, no underflow."""


def _no_recording(sample_rate: int = 16000) -> CaptureRecorder:
    """A recorder that writes nothing, for the tests that predate recording.

    ``AudioCapture`` requires one; "not recording" is the recorder's own state
    rather than a missing object, so this is how a test says it.
    """
    return CaptureRecorder(RecordingConfig(enabled=False), sample_rate)


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
    capture = AudioCapture(config, _no_recording())
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
    cap = AudioCapture(AudioConfig(preroll_ms=250), _no_recording())
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
    cap = AudioCapture(AudioConfig(preroll_ms=250), _no_recording())
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
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0), _no_recording())
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0])

    snapshot = cap.snapshot_capture()

    assert snapshot.tolist() == [1.0] * 4 + [2.0] * 4 + [3.0] * 4
    cap.stop_capture()
    cap.close()


def test_snapshot_begins_exactly_at_since_frame() -> None:
    """A tick asks for the audio past what is committed, and the result has
    to start on that frame -- one frame off either way and the next chunk is
    decoded with a sliver missing or a sliver twice."""
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0), _no_recording())
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0, 4.0, 5.0])

    snapshot = cap.snapshot_capture(since_frame=14)

    assert snapshot.size == 6
    assert snapshot.tolist() == [4.0] * 2 + [5.0] * 4
    assert cap.snapshot_capture(since_frame=20).size == 0, "nothing past the end yet"
    assert cap.snapshot_capture(since_frame=25).size == 0
    cap.stop_capture()
    cap.close()


def test_snapshot_walks_only_the_chunks_the_window_needs(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A tick's cost is bounded by what is uncommitted, not by the utterance.
    If the remainder were taken by concatenating the whole buffer and slicing,
    an hour-long latched passage would copy 230 MB every second.
    """
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0), _no_recording())
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
    snapshot = cap.snapshot_capture(since_frame=1992)

    assert snapshot.tolist() == [498.0] * 4 + [499.0] * 4
    # Two 4-frame chunks cover the 8-frame window; the other 498 are untouched.
    assert joined == [2]


def test_snapshot_does_not_disturb_a_subsequent_stop_capture() -> None:
    """The committed decode still sees the complete buffer: previewing must
    not consume, truncate or reorder a single sample of it."""
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0), _no_recording())
    cap.start_capture()
    _feed(cap, [1.0, 2.0, 3.0])

    cap.snapshot_capture(since_frame=8)
    cap.snapshot_capture()
    _feed(cap, [4.0])
    cap.snapshot_capture(since_frame=10)

    full = cap.stop_capture()

    assert full.tolist() == [1.0] * 4 + [2.0] * 4 + [3.0] * 4 + [4.0] * 4
    cap.close()


def test_snapshot_while_idle_returns_an_empty_array() -> None:
    cap = AudioCapture(AudioConfig(sample_rate=1000, preroll_ms=0), _no_recording())

    snapshot = cap.snapshot_capture(since_frame=0)

    assert snapshot.size == 0
    assert snapshot.dtype == np.float32
    cap.close()


def test_stream_dying_mid_capture_is_recovered_without_losing_what_was_recorded(
    _fake_sounddevice: None,
) -> None:
    """The 29.8s failure: the stream died 0.2s into a hold and nothing noticed
    until the *next* keypress, because ``_ensure_stream_alive`` only runs at
    ``start_capture``.

    Recovery mid-capture has to preserve the audio recorded before the stream
    died -- reopening is worth nothing if it silently discards the first half
    of the sentence along with the dead interval.
    """
    cap = AudioCapture(AudioConfig(preroll_ms=250), _no_recording())
    first = cap._stream
    assert first is not None

    cap.start_capture()
    before = np.full((800, 1), 0.5, dtype=np.float32)
    cap._callback(before, 800, None, _NO_STATUS)

    # Alive and recently heard from: nothing to do.
    assert cap.recover_if_dead() is False
    assert cap._stream is first

    # Now the callback stops, mid-utterance.
    cap._last_callback_at = time.monotonic() - (STALE_STREAM_SECONDS + 0.5)
    assert cap.recover_if_dead() is True
    assert cap._stream is not first, "a stream that died mid-capture must be reopened"

    after = np.full((400, 1), 0.25, dtype=np.float32)
    cap._callback(after, 400, None, _NO_STATUS)

    samples = cap.stop_capture()
    # Pre-roll + 800 recorded before the death + whatever the reopened stream's
    # own start() delivered + 400 after. The two real chunks must both survive.
    assert np.count_nonzero(samples == 0.5) == 800
    assert np.count_nonzero(samples == 0.25) == 400
    cap.close()


def test_recover_if_dead_leaves_an_idle_stream_alone(_fake_sounddevice: None) -> None:
    """Scoped to an in-flight capture on purpose: an idle stream is handled at
    the next ``start_capture``, and reopening it here would throw away the warm
    pre-roll ring for nothing."""
    cap = AudioCapture(AudioConfig(preroll_ms=250), _no_recording())
    first = cap._stream
    cap._last_callback_at = time.monotonic() - (STALE_STREAM_SECONDS + 0.5)

    assert cap.recover_if_dead() is False
    assert cap._stream is first
    cap.close()


# -- the in-memory ceiling, and the disk underneath it -------------------------


def _capped_capture(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> AudioCapture:
    """A capture whose in-memory ceiling is 50 frames, recording to ``tmp_path``.

    ``preroll_ms=0`` so nothing seeds the buffer and the sample counts below
    are exactly what the callbacks delivered.
    """
    monkeypatch.setattr(audio_module, "MAX_UTTERANCE_SECONDS", 0.05)
    config = AudioConfig(sample_rate=1000, preroll_ms=0, device=None)
    recorder = CaptureRecorder(RecordingConfig(dir=tmp_path / "audio"), config.sample_rate)
    return AudioCapture(config, recorder)


def _speak(capture: AudioCapture, chunks: int, frames: int = 10) -> None:
    for i in range(chunks):
        value = (i + 1) / 100.0
        capture._callback(np.full((frames, 1), value, dtype=np.float32), frames, None, _NO_STATUS)


def test_audio_past_the_ceiling_still_reaches_the_recording(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _fake_sounddevice: None
) -> None:
    """The 2026-09-08 loss, made impossible.

    A hold of 821.3s returned 600.0s of audio because the ceiling was enforced
    with a ``return`` at the top of the callback, above every other consumer --
    so the samples past it existed nowhere at all. The recorder now sits
    *above* that ``return``: the ceiling may bound what the decode sees, and it
    may never again bound what is on disk.
    """
    capture = _capped_capture(tmp_path, monkeypatch)
    capture.start_capture()
    _speak(capture, chunks=10)  # 100 frames against a 50-frame ceiling

    samples = capture.stop_capture()

    assert samples.size == 50, "the ceiling still bounds memory"
    status = capture.recording_status()
    assert isinstance(status, Recorded)
    written, rate = read_capture(status.path)
    assert rate == 1000
    assert written.size == 100, "everything spoken reached the file"
    assert np.allclose(written[:50], samples, atol=2e-5)
    capture.close()


def test_the_ceiling_is_reported_once_per_capture_not_once_per_callback(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _fake_sounddevice: None
) -> None:
    """The old cap fired silently in every channel at once. This one is polled
    at 30Hz for the whole of a recording, so it has to say so exactly once --
    and again for the next capture, which is a new thing to warn about."""
    capture = _capped_capture(tmp_path, monkeypatch)

    capture.start_capture()
    _speak(capture, chunks=3)
    assert capture.take_cap_notice() is False, "30 frames is under the ceiling"

    _speak(capture, chunks=4)
    assert capture.take_cap_notice() is True
    assert capture.take_cap_notice() is False, "already reported for this capture"
    capture.stop_capture()

    capture.start_capture()
    _speak(capture, chunks=7)
    assert capture.take_cap_notice() is True, "a new capture warns again"
    capture.stop_capture()
    capture.close()


def test_a_recorder_that_cannot_write_never_costs_the_capture(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _fake_sounddevice: None
) -> None:
    """The safety net must never become the hazard: the configured directory is
    a file here, so every recorder call fails, and the dictation is unaffected."""
    monkeypatch.setattr(audio_module, "MAX_UTTERANCE_SECONDS", 60.0)
    blocked = tmp_path / "audio"
    blocked.write_text("not a directory")
    config = AudioConfig(sample_rate=1000, preroll_ms=0, device=None)
    capture = AudioCapture(config, CaptureRecorder(RecordingConfig(dir=blocked), 1000))

    capture.start_capture()
    _speak(capture, chunks=10)
    samples = capture.stop_capture()

    assert samples.size == 100
    assert capture.recording_status() == NotRecorded()
    capture.close()


def test_the_recording_includes_the_preroll_the_decode_sees(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _fake_sounddevice: None
) -> None:
    """The recording has to be the capture, not a suffix of it.

    The pre-roll is the run-up to the first word -- the reason the ring buffer
    exists -- so a recording that started at the keypress would clip exactly
    what a recovered transcript most needs.
    """
    monkeypatch.setattr(audio_module, "MAX_UTTERANCE_SECONDS", 60.0)
    config = AudioConfig(sample_rate=1000, preroll_ms=50, device=None)
    recorder = CaptureRecorder(RecordingConfig(dir=tmp_path / "audio"), config.sample_rate)
    capture = AudioCapture(config, recorder)
    # Fill the ring while idle, as the always-open stream does between hotkeys.
    capture._callback(np.full((50, 1), 0.5, dtype=np.float32), 50, None, _NO_STATUS)

    capture.start_capture()
    _speak(capture, chunks=2, frames=10)
    samples = capture.stop_capture()

    status = capture.recording_status()
    assert isinstance(status, Recorded)
    written, _ = read_capture(status.path)
    assert written.size == samples.size
    assert np.allclose(written, samples, atol=2e-5)
    assert np.count_nonzero(np.isclose(written, 0.5, atol=2e-5)) == 50, "the pre-roll is in it"
    capture.close()


def test_the_ceiling_is_reported_when_the_key_comes_up_in_the_same_tick(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, _fake_sounddevice: None
) -> None:
    """A hold that crosses the ceiling and ends before the next 30Hz poll.

    Nothing observes ``_frames_captured`` in that window, and ``stop_capture``
    zeroes it two lines later, so deriving the notice only from the live count
    reported nothing at all -- the transcript would stop short in silence,
    which is the failure this whole change exists to end. Hence the latch.
    """
    capture = _capped_capture(tmp_path, monkeypatch)
    capture.start_capture()
    _speak(capture, chunks=10)  # 100 frames against a 50-frame ceiling

    capture.stop_capture()  # no poll between crossing the ceiling and here

    assert capture.take_cap_notice() is True
    assert capture.take_cap_notice() is False, "still exactly once per capture"
    capture.close()
