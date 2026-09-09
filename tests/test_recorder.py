"""Regression coverage for the safety net added after the 2026-09-08 loss.

Every test here answers one question: *is the audio still there?* The failure
this module exists to prevent was silent -- a realtime callback dropped 3m41s
of speech and nothing anywhere said so -- so the properties worth pinning down
are that a recording holds what was spoken, that it survives the things that
go wrong around it, and that pruning never eats the file being written.
"""

from __future__ import annotations

import itertools
import os
import threading
import time
import wave
from pathlib import Path
from typing import Any

import numpy as np
import pytest

from spokenpad import recorder as recorder_module
from spokenpad.audio import MonoAudio
from spokenpad.config import RecordingConfig
from spokenpad.recorder import (
    CaptureRecorder,
    NotRecorded,
    Recorded,
    RecordingError,
    Truncated,
    prune_recordings,
    read_capture,
    secure_recordings,
)

_RATE = 16000


def _config(tmp_path: Path, **overrides: Any) -> RecordingConfig:
    return RecordingConfig(dir=tmp_path / "audio", **overrides)


def _path(recorder: CaptureRecorder) -> Path:
    """Where the last capture went, whole or not -- for tests that only need
    the filename. Tests about the *promise* assert on ``status()`` itself."""
    status = recorder.status()
    assert isinstance(status, Recorded | Truncated)
    return status.path


def _ramp(n: int) -> MonoAudio:
    """A signal where every sample differs, so a dropped or reordered buffer
    shows up as a mismatch rather than blending into its neighbours."""
    return np.linspace(-0.9, 0.9, n, dtype=np.float32)


def _write(recorder: CaptureRecorder, samples: MonoAudio, chunk: int = 512) -> None:
    for start in range(0, samples.size, chunk):
        recorder.write(samples[start : start + chunk].copy())


def test_a_recording_round_trips_to_the_samples_that_were_captured(tmp_path: Path) -> None:
    """The point of the file: what comes back out is what went in.

    Within half a quantisation step -- the wav is 16-bit -- because the whole
    value of a recovered capture is that decoding it gives the transcript the
    live decode would have produced.
    """
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    samples = _ramp(5000)

    recorder.start()
    _write(recorder, samples)
    recorder.stop()

    read_back, rate = read_capture(_path(recorder))
    assert rate == _RATE
    assert read_back.size == samples.size
    assert np.allclose(read_back, samples, atol=2e-5)


def test_stop_does_not_return_until_the_file_is_complete(tmp_path: Path) -> None:
    """``stop_capture`` hands the buffer straight to a decode, so a recording
    that is still being flushed when that happens is not a safety net. The
    header's frame count must be right the moment ``stop`` returns."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)

    recorder.start()
    _write(recorder, _ramp(20_000))
    recorder.stop()

    with wave.open(str(_path(recorder)), "rb") as wf:
        assert wf.getnframes() == 20_000
        assert wf.getnchannels() == 1
        assert wf.getsampwidth() == 2


def test_the_recording_and_its_directory_are_private(tmp_path: Path) -> None:
    """A capture holds whatever was dictated -- the same reason the log is 0600."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    recorder.stop()

    recording = _path(recorder)
    assert os.stat(recording).st_mode & 0o777 == 0o600
    assert os.stat(recording.parent).st_mode & 0o777 == 0o700


def test_the_recording_is_never_briefly_world_readable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """0600 from the instant the file exists, not 0644 with a chmod after it.

    The mode is observed from inside ``wave.open``, which is the first thing
    that runs after the file is created -- the window a chmod-afterwards
    implementation would leave open.
    """
    seen: list[int] = []
    real_open = wave.open

    def peek(handle: Any, mode: str) -> Any:
        seen.append(os.stat(handle.name).st_mode & 0o777)
        return real_open(handle, mode)

    monkeypatch.setattr(wave, "open", peek)
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    recorder.stop()

    assert seen == [0o600]


def test_an_existing_directory_and_recording_have_their_modes_corrected(
    tmp_path: Path,
) -> None:
    """``mkdir(mode=...)`` only applies to a directory it actually creates, so
    an upgrade from a version that left things 0755/0644 would keep them --
    and a recording of what you said should not stay world-readable because of
    when it happened to be written."""
    audio_dir = tmp_path / "audio"
    audio_dir.mkdir(parents=True)
    audio_dir.chmod(0o755)
    stale = audio_dir / "capture-2026-09-07-101500.wav"
    stale.write_bytes(b"stale")
    stale.chmod(0o644)

    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    _write(recorder, _ramp(1000))
    recorder.stop()

    assert os.stat(audio_dir).st_mode & 0o777 == 0o700
    assert os.stat(stale).st_mode & 0o777 == 0o600


def test_two_captures_in_the_same_second_get_their_own_files(tmp_path: Path) -> None:
    """The filename is stamped to the second and a double tap fits inside one.
    The second capture must not open the first one's file."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)

    recorder.start()
    _write(recorder, _ramp(1000))
    first = _path(recorder)
    recorder.stop()

    recorder.start()
    _write(recorder, _ramp(2000))
    second = _path(recorder)
    recorder.stop()

    assert first != second
    assert read_capture(first)[0].size == 1000
    assert read_capture(second)[0].size == 2000


def test_disabled_recording_writes_nothing_and_still_accepts_audio(tmp_path: Path) -> None:
    """Turning it off must be inert, not broken: the callback still calls
    ``write`` on every buffer and must not care."""
    recorder = CaptureRecorder(_config(tmp_path, enabled=False), _RATE)

    recorder.start()
    _write(recorder, _ramp(1000))
    recorder.stop()

    assert recorder.status() == NotRecorded()
    assert not (tmp_path / "audio").exists()


def test_a_directory_that_cannot_be_created_does_not_raise(tmp_path: Path) -> None:
    """A recorder that breaks dictation is worse than no recorder.

    Here the configured directory is a *file*, so ``mkdir`` fails. The capture
    must carry on with nothing on disk rather than take the daemon down at the
    first keypress.
    """
    blocked = tmp_path / "audio"
    blocked.write_text("not a directory")
    recorder = CaptureRecorder(RecordingConfig(dir=blocked), _RATE)

    recorder.start()
    _write(recorder, _ramp(1000))
    recorder.stop()

    assert recorder.status() == NotRecorded()


def test_a_write_that_fails_mid_capture_is_reported_and_swallowed(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """A full disk mid-utterance must cost the recording, not the dictation.

    It must also be *loud*: the whole reason 3m41s went missing unnoticed is
    that the code that dropped it could not log.
    """
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()

    def explode(samples: MonoAudio) -> bytes:
        raise ValueError("no space left on device")

    recording = recorder._current
    assert recording is not None
    monkeypatch.setattr(recorder_module, "to_pcm16", explode)
    with caplog.at_level("ERROR", logger="spokenpad.recorder"):
        _write(recorder, _ramp(4096))
        recorder.stop()

    assert "the rest of this capture is not on disk" in caplog.text
    # And the writer is not left with a queue nobody drains: `write` is a
    # no-op from here, so a stuck key cannot grow the memory this failed.
    assert recording.broken.is_set()


def test_read_capture_refuses_a_format_it_would_have_to_guess_at(tmp_path: Path) -> None:
    """Misreading stereo as mono produces noise that decodes to plausible
    nonsense. A refusal is worth more than a transcript nobody can trust."""
    path = tmp_path / "stereo.wav"
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(_RATE)
        wf.writeframes(b"\x00\x00\x00\x00")

    with pytest.raises(RecordingError, match="2 channels"):
        read_capture(path)


# -- pruning -------------------------------------------------------------------


def _recording(directory: Path, name: str, size: int, age_seconds: float) -> Path:
    """A file of ``size`` bytes, aged ``age_seconds``. Names matter here: only
    ``capture-*.wav`` is a recording as far as pruning is concerned."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_bytes(b"\0" * size)
    stamp = time.time() - age_seconds
    os.utime(path, (stamp, stamp))
    return path


def test_pruning_deletes_oldest_first_until_the_budget_is_met(tmp_path: Path) -> None:
    """Oldest-first and size-driven only. Age is not a criterion: a recording
    is pruned because something newer needs the space, never because it is old
    -- so the newest survivors are exactly the ones still worth recovering."""
    oldest = _recording(tmp_path, "capture-a.wav", 100, age_seconds=300)
    middle = _recording(tmp_path, "capture-b.wav", 100, age_seconds=200)
    newest = _recording(tmp_path, "capture-c.wav", 100, age_seconds=100)

    prune_recordings(tmp_path, max_total_bytes=150)

    assert not oldest.exists()
    assert not middle.exists()
    assert newest.exists()


def test_pruning_stops_as_soon_as_the_directory_fits(tmp_path: Path) -> None:
    oldest = _recording(tmp_path, "capture-a.wav", 100, age_seconds=300)
    middle = _recording(tmp_path, "capture-b.wav", 100, age_seconds=200)
    newest = _recording(tmp_path, "capture-c.wav", 100, age_seconds=100)

    prune_recordings(tmp_path, max_total_bytes=250)

    assert not oldest.exists()
    assert middle.exists() and newest.exists()


def test_pruning_never_deletes_the_capture_being_written(tmp_path: Path) -> None:
    """The in-flight file is the one recording that cannot be reproduced --
    deleting it would be this module recreating the bug it exists to prevent.
    It is the oldest file here, so nothing but the guard spares it."""
    in_flight = _recording(tmp_path, "capture-a.wav", 100, age_seconds=300)
    newer = _recording(tmp_path, "capture-b.wav", 100, age_seconds=100)

    prune_recordings(tmp_path, max_total_bytes=50, keep={in_flight}.__contains__)

    assert in_flight.exists()
    assert not newer.exists()


def test_pruning_never_touches_a_file_it_did_not_write(tmp_path: Path) -> None:
    """``recording.dir`` is configurable, so it may well be pointed at a
    directory that already holds something. Deleting a stranger's files to
    make room would be a data-loss bug inside a data-loss fix -- so only the
    ``capture-*.wav`` names this module creates are ever candidates."""
    thesis = _recording(tmp_path, "thesis.odt", 10_000, age_seconds=10_000)
    notes = _recording(tmp_path, "capture-notes.txt", 10_000, age_seconds=9_000)
    recording = _recording(tmp_path, "capture-old.wav", 100, age_seconds=8_000)

    prune_recordings(tmp_path, max_total_bytes=50)

    assert thesis.exists() and notes.exists(), "not ours, not ours to delete"
    assert not recording.exists()


def test_the_keep_check_is_asked_at_the_moment_of_deletion(tmp_path: Path) -> None:
    """Not snapshotted when the prune was requested.

    This runs on a writer thread and the listing above it can stall on a sick
    mount for longer than the abandonment timeout, so by the time it answers,
    the *next* capture may hold a file the snapshot never heard of. Asking at
    unlink time is the difference between sparing that file and deleting the
    recording someone is speaking into. Modelled here by a predicate that
    starts protecting ``later`` only once the listing has already been taken.
    """
    older = _recording(tmp_path, "capture-a.wav", 100, age_seconds=300)
    later = _recording(tmp_path, "capture-b.wav", 100, age_seconds=100)

    def keep(path: Path) -> bool:
        # `later` becomes an open file only once this prune has already begun
        # deleting -- the window an answer computed up front cannot see, and
        # the one where a real capture is opened while a stalled listing
        # catches up.
        return path == later and not older.exists()

    prune_recordings(tmp_path, max_total_bytes=50, keep=keep)

    assert not older.exists()
    assert later.exists(), "opened mid-prune, and still spared"


def test_pruning_an_absent_directory_is_silent(tmp_path: Path) -> None:
    """Before the first capture there is nothing to prune, and a daemon start
    is not the place for an error about it."""
    prune_recordings(tmp_path / "never-recorded", max_total_bytes=1)


def test_starting_a_capture_prunes_around_the_file_it_just_opened(tmp_path: Path) -> None:
    """The per-capture prune runs on the writer thread with the new file
    protected, so a daemon that has been running for weeks stays inside its
    budget without ever putting a directory scan on the keypress path."""
    recorder = CaptureRecorder(_config(tmp_path, max_total_bytes=1024), _RATE)
    # Written after construction, so it is the *per-capture* prune that has to
    # notice it and not the one the constructor already ran.
    stale = _recording(tmp_path / "audio", "capture-old.wav", 4096, age_seconds=10_000)

    recorder.start()
    _write(recorder, _ramp(1000))
    recorder.stop()

    assert not stale.exists()
    assert _path(recorder).exists()


# -- the realtime-thread contract ---------------------------------------------


def _stalling_writer(monkeypatch: pytest.MonkeyPatch, *, after: int = 0) -> threading.Event:
    """Make writes block until the returned event is set.

    A write to a stalled mount is not an exception -- it simply never returns,
    which is the case a plain ``join()`` and an unbounded queue both get
    wrong. Nothing here raises, so only the timeout and the queue bound can
    save the daemon.

    ``after`` buffers are written normally first, for the tests that need real
    bytes on disk before the stall -- a recording that is still 0 bytes is one
    the pruner has no reason to delete, so a test that stalls immediately can
    prove nothing about pruning.
    """
    release = threading.Event()
    healthy = recorder_module.to_pcm16
    written = itertools.count()

    def stall(samples: MonoAudio) -> bytes:
        if next(written) < after:
            return healthy(samples)
        release.wait(10.0)
        return b""

    monkeypatch.setattr(recorder_module, "to_pcm16", stall)
    return release


def test_stop_gives_up_on_a_writer_that_has_stopped_answering(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """``stop`` runs on the Qt thread, on the way to the decode. An unbounded
    join on a hung write would wedge the daemon completely: no transcript, no
    hotkey, no window. A sick disk may cost the recording, never the
    dictation."""
    release = _stalling_writer(monkeypatch)
    monkeypatch.setattr(recorder_module, "_WRITER_JOIN_SECONDS", 0.2)
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    _write(recorder, _ramp(2048))

    started = time.monotonic()
    with caplog.at_level("ERROR", logger="spokenpad.recorder"):
        recorder.stop()
    elapsed = time.monotonic() - started
    release.set()

    assert elapsed < 2.0, "stop must return promptly however sick the disk is"
    assert "is not answering" in caplog.text
    assert "dictation itself is unaffected" in caplog.text


def test_audio_the_writer_cannot_keep_up_with_is_dropped_not_hoarded(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """The queue is bounded in seconds of audio, and the bound holds.

    Unbounded, a stalled writer collects ~64 KB/s for as long as the key is
    held -- straight past the in-memory ceiling this module exists to make
    safe, ending in an OOM that loses the capture too. Dropping is the better
    trade only because it is loud: a truncated wav must never be mistaken for
    a complete one.
    """
    release = _stalling_writer(monkeypatch)
    monkeypatch.setattr(recorder_module, "_WRITER_JOIN_SECONDS", 0.2)
    monkeypatch.setattr(recorder_module, "_QUEUE_SECONDS", 0.1)
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()

    _write(recorder, _ramp(10 * _RATE))  # ten seconds against a 0.1s bound

    recording = recorder._current
    assert recording is not None
    assert recording.queued <= recording.max_queued, "the bound is the bound"
    with caplog.at_level("ERROR", logger="spokenpad.recorder"):
        recorder.stop()
    release.set()

    assert "is INCOMPLETE" in caplog.text


def test_a_capture_with_dropped_audio_reports_itself_truncated(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A hole in the middle counts as truncation, even when the writer recovers.

    The stall is released *before* ``stop``, so the join succeeds and the
    timeout path never runs -- the only thing that can mark this recording
    short is the drop itself. Without that, ``status()`` answered ``Recorded``
    for a file missing 9.9 of its 10 seconds, and ``_warn_capture_capped``
    offered `spokenpad transcribe` for audio that is not in the file. A hole is
    exactly as unrecoverable as a cut-off end.
    """
    release = _stalling_writer(monkeypatch, after=1)
    monkeypatch.setattr(recorder_module, "_QUEUE_SECONDS", 0.1)
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()

    _write(recorder, _ramp(10 * _RATE))  # ten seconds against a 0.1s bound
    release.set()  # the disk comes back: the writer drains and closes cleanly
    recorder.stop()

    assert isinstance(recorder.status(), Truncated)


def test_stop_is_idempotent(tmp_path: Path) -> None:
    """``stop_capture`` and ``close`` both call it, and a cancelled capture can
    reach it twice. The second call must be inert, not a second close."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    _write(recorder, _ramp(1000))

    recorder.stop()
    recorder.stop()

    assert read_capture(_path(recorder))[0].size == 1000


def test_starting_again_without_stopping_leaves_one_writer_and_one_good_file(
    tmp_path: Path,
) -> None:
    """A capture that was never stopped -- a cancelled session, a daemon
    restarting mid-hold -- must not leave a thread behind holding a wav whose
    header was never patched."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    _write(recorder, _ramp(1000))
    first = _path(recorder)

    recorder.start()
    _write(recorder, _ramp(2000))
    second = _path(recorder)
    writers = [t for t in threading.enumerate() if t.name == "spokenpad-recorder"]
    recorder.stop()

    assert len(writers) == 1, "the first writer was joined, not abandoned"
    assert first != second
    assert read_capture(first)[0].size == 1000, "the abandoned recording was still finalised"
    assert read_capture(second)[0].size == 2000


def test_a_buffer_arriving_while_stop_runs_never_corrupts_the_recording(
    tmp_path: Path,
) -> None:
    """``AudioCapture`` serialises this with its own lock, but the recorder is
    the thing the realtime thread touches and must not depend on that: a
    buffer landing mid-``stop`` is dropped, never half-written."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    hammering = threading.Event()
    finished = threading.Event()

    def hammer() -> None:
        # Bounded: an unbounded spin builds a backlog big enough that `stop`
        # abandons the writer, and an abandoned thread outlives the test.
        for _ in range(200):
            if finished.is_set():
                return
            recorder.write(np.full(64, 0.25, dtype=np.float32))
            hammering.set()

    thread = threading.Thread(target=hammer, daemon=True)
    thread.start()
    hammering.wait(5.0)

    recorder.stop()
    finished.set()
    thread.join()

    samples, _ = read_capture(_path(recorder))
    assert samples.size % 64 == 0, "no partial buffer was written"
    assert np.allclose(samples, 0.25, atol=2e-5)


def test_a_zombie_writer_cannot_reach_the_capture_that_follows_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """The regression the review caught, and the reason a writer owns a record.

    Reproduced exactly: stall writer 1 until ``stop`` abandons it, record a
    second capture onto a healthy disk, then let writer 1's write finally
    raise. Sharing ``self._broken`` meant the zombie's failure muted the live
    ``write``, half of capture 2 vanished, and the ERROR named capture 2's
    file for capture 1's failure -- silent audio loss, inside the change that
    exists to end silent audio loss.
    """
    healthy = recorder_module.to_pcm16
    release = threading.Event()

    def stall_then_fail(samples: MonoAudio) -> bytes:
        release.wait(10.0)
        raise ValueError("the mount came back angry")

    monkeypatch.setattr(recorder_module, "to_pcm16", stall_then_fail)
    monkeypatch.setattr(recorder_module, "_WRITER_JOIN_SECONDS", 0.2)
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    _write(recorder, _ramp(1000))
    zombie = recorder._writer
    assert zombie is not None
    recorder.stop()
    first = _path(recorder)

    monkeypatch.setattr(recorder_module, "to_pcm16", healthy)
    recorder.start()
    second = _path(recorder)
    _write(recorder, _ramp(16000))

    with caplog.at_level("ERROR", logger="spokenpad.recorder"):
        release.set()  # the zombie's write raises now, mid-capture-2
        zombie.join(5.0)
        _write(recorder, _ramp(16000))
        recorder.stop()

    assert first != second
    assert read_capture(second)[0].size == 32000, "the live capture kept every frame"
    failures = [r.message for r in caplog.records if "failed after" in r.message]
    assert len(failures) == 1
    assert str(first) in failures[0] and str(second) not in failures[0]


def test_a_prune_never_deletes_a_file_an_abandoned_writer_still_holds(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A zombie's file is still open and may still be finalised, so the prune
    the *next* capture runs must spare it exactly as it spares its own.

    The first buffer is written for real, so the abandoned recording is
    hundreds of kilobytes against a 1 KB budget: the prune genuinely wants to
    delete it, and only the open-file registry stops it.
    """
    release = _stalling_writer(monkeypatch, after=1)
    monkeypatch.setattr(recorder_module, "_WRITER_JOIN_SECONDS", 0.2)
    recorder = CaptureRecorder(_config(tmp_path, max_total_bytes=1024), _RATE)

    recorder.start()
    _write(recorder, _ramp(100_000), chunk=100_000)  # lands on disk
    _write(recorder, _ramp(512))  # stalls the writer
    zombie = recorder._writer
    recorder.stop()
    abandoned = _path(recorder)
    assert abandoned.stat().st_size > 1024, "the prune has a reason to delete it"

    recorder.start()  # this writer prunes, with the budget already exceeded
    current = _path(recorder)
    _write(recorder, _ramp(1000))
    live = recorder._writer
    recorder.stop()

    release.set()
    for thread in (zombie, live):
        assert thread is not None
        thread.join(5.0)

    assert abandoned.exists(), "still open, and the zombie may yet finalise it"
    assert current.exists()


def test_narrowing_permissions_never_follows_a_symlink(tmp_path: Path) -> None:
    """``chmod`` follows symlinks and Linux cannot chmod the link itself, so a
    ``capture-x.wav -> notes.txt`` link would have this narrowing a file the
    module never wrote -- outside the "only what we created" rule that keeps
    a configurable ``recording.dir`` safe. (``unlink`` does not follow, so
    pruning was never exposed to this.)"""
    audio_dir = tmp_path / "audio"
    audio_dir.mkdir()
    target = audio_dir / "notes.txt"
    target.write_text("not a recording")
    target.chmod(0o644)
    (audio_dir / "capture-2026-09-08-120000.wav").symlink_to(target)

    secure_recordings(audio_dir)

    assert os.stat(target).st_mode & 0o777 == 0o644


def test_a_recording_given_up_on_reports_itself_as_truncated(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A bare path let every caller assume the happy case, so the cap notice
    kept promising ``spokenpad transcribe`` for a file that stops in the
    middle. The status has to carry the difference."""
    recorder = CaptureRecorder(_config(tmp_path), _RATE)
    recorder.start()
    assert isinstance(recorder.status(), Recorded)

    def explode(samples: MonoAudio) -> bytes:
        raise ValueError("no space left on device")

    monkeypatch.setattr(recorder_module, "to_pcm16", explode)
    _write(recorder, _ramp(1000))
    recorder.stop()

    assert isinstance(recorder.status(), Truncated)
