"""Every capture goes to disk while it is being spoken, whatever else happens.

On 2026-09-08 a user held the hotkey for 821.3s and got 600.0s of audio back:
the in-memory utterance ceiling was enforced inside the realtime callback,
which simply stopped copying samples once the buffer was full. Those samples
were the only copy in existence -- the preview, the decode, the log line and
the nvim window all sit downstream of that ``return`` -- so 3m41s of dictation
was gone before anything could notice. Nothing was logged, because a realtime
callback cannot log.

This module removes the category. The wav is written *as the audio arrives*
and independently of decoding, so the ceiling now bounds only what is held in
RAM, and a decode that fails, is cancelled, or never happens leaves a file
that ``voice-kb transcribe`` turns back into text.

Threading
---------
The realtime callback must never touch the filesystem: a blocking write on
PortAudio's thread is an xrun, which is audible as a gap in the recording --
the very thing being protected. So :meth:`CaptureRecorder.write` only pushes
onto a queue, and a dedicated writer thread drains it and does the I/O. This
is sounddevice's own ``rec_unlimited.py`` shape, for the same reason.

The writer thread owns the file completely once it is open: it is the only
thread that writes to it and the only one that closes it. That is what makes
abandoning a stalled writer safe -- nothing else is holding a half-written
wav, so if the thread ever unblocks it still finalises the file it has.

Failure policy
--------------
**A sick disk may cost you the recording, never the dictation.** Every
failure here -- an unwritable directory, a full disk, a mount that has stopped
answering, a write that raises or one that simply never returns -- is logged
at ERROR and swallowed; capture and decode carry on exactly as if recording
had been turned off. Two specific promises follow from that, because both
have an obvious implementation that breaks them:

* :meth:`CaptureRecorder.stop` waits for the writer, but only for
  :data:`_WRITER_JOIN_SECONDS`. It is reached from the key-up that starts a
  decode, on the Qt thread, so an unbounded join on a hung write would wedge
  the whole daemon: no transcript, no hotkey, no window;
* the queue is bounded in *seconds of audio*. An unbounded one grows at
  ~64 KB/s behind a stalled writer, past the very memory ceiling this module
  exists to make safe, and ends in an OOM that loses the in-memory capture
  too.

In both cases the recording is truncated and says so at ERROR. A truncated
wav must never be mistaken for a complete one.
"""

from __future__ import annotations

import logging
import os
import queue
import threading
import wave
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import BinaryIO

import numpy as np

from voice_kb.audio import MonoAudio
from voice_kb.config import RecordingConfig

log = logging.getLogger("voice-kb.recorder")

_DIR_MODE = 0o700
_FILE_MODE = 0o600
"""A recording holds whatever was dictated, so it is user-private -- the same
reasoning that keeps the log file at 0600."""

_CAPTURE_PREFIX = "capture-"
_CAPTURE_SUFFIX = ".wav"
_CAPTURE_GLOB = f"{_CAPTURE_PREFIX}*{_CAPTURE_SUFFIX}"
"""How a recording is named, and the *only* thing pruning will ever delete.

Derived from one pair of constants so the two cannot drift: whatever
:func:`_open_capture` creates is exactly what :func:`prune_recordings`
considers. ``recording.dir`` is configurable and a user may well point it at a
directory that already holds something else; deleting a stranger's files to
make room would be a data-loss bug inside a data-loss fix.
"""

_MAX_SAME_SECOND = 100
"""Distinct recordings the same timestamp can hold before we give up."""

_SAMPLE_WIDTH = 2
"""16-bit PCM. The stdlib ``wave`` module writes nothing else usefully, and
FLAC would mean a new dependency to halve a file that is already pruned."""

_FULL_SCALE = 32767.0
"""Scale for the float32 <-> int16 conversion, used in *both* directions.

Deliberately 32767 rather than the usual asymmetric 32768/32767 pair: it makes
:func:`read_capture` the exact inverse of what :class:`CaptureRecorder` writes,
to within the half-LSB the rounding costs. A recovered capture should be the
samples that were captured, not a version of them shifted by a hair.
"""

_WRITER_JOIN_SECONDS = 3.0
"""How long :meth:`CaptureRecorder.stop` waits for the writer to finish.

``stop`` runs on the Qt thread, reached from the key-up that requests the
decode. A write to a stalled mount can block indefinitely without ever
raising, so a plain ``join()`` here would hang the daemon for as long as the
filesystem felt like it -- the recorder breaking dictation, which is the one
thing it may not do. Three seconds is far beyond a healthy local write of a
few hundred kilobytes and far short of anything a user would sit through.
"""

_QUEUE_SECONDS = 60.0
"""Audio the writer is allowed to fall behind by before buffers are dropped.

The bound is in seconds rather than in buffers because that is the quantity
that matters: PortAudio's block size varies by device and is not known here.
A minute is orders of magnitude more slack than a healthy disk ever needs
(~1 MB of float32 at 16 kHz), and the tradeoff past it is not close -- if the
writer is that far behind, the audio is not reaching disk anyway, so dropping
it loudly beats growing until the daemon is killed and the in-memory capture
dies with it.
"""


class RecordingError(RuntimeError):
    """A recording could not be read back. Only :func:`read_capture` raises."""


@dataclass(frozen=True, slots=True)
class Recorded:
    """The capture is on disk in full, at ``path``."""

    path: Path


@dataclass(frozen=True, slots=True)
class Truncated:
    """A recording was started at ``path`` but given up on part-way.

    A separate answer from :class:`Recorded` because the difference is the
    whole promise: "recover the rest with ``voice-kb transcribe``" is a lie
    about a file that stops in the middle, and a lie of exactly the kind this
    module exists to stop telling.
    """

    path: Path


@dataclass(frozen=True, slots=True)
class NotRecorded:
    """No file: recording is off, or none could be opened for this capture."""


type RecordingStatus = Recorded | Truncated | NotRecorded


def to_pcm16(samples: MonoAudio) -> bytes:
    """Float32 in ``[-1, 1]`` as little-endian 16-bit PCM frames.

    Clipped rather than scaled: a sample outside the range is a hot input, and
    quietening the whole recording to accommodate it would be a worse lie than
    the clipping the soundcard already did.
    """
    scaled = np.rint(np.clip(samples, -1.0, 1.0) * _FULL_SCALE)
    return scaled.astype("<i2").tobytes()


def read_capture(path: Path) -> tuple[MonoAudio, int]:
    """A recording as ``(samples, sample_rate)``, the inverse of what we write.

    Strict about the format rather than accommodating: the only files this is
    asked to read are the ones :class:`CaptureRecorder` wrote, and silently
    misreading a stereo or 24-bit file as mono 16-bit produces noise that
    decodes to plausible nonsense. A clear refusal is worth more.
    """
    try:
        with wave.open(str(path), "rb") as wf:
            channels = wf.getnchannels()
            width = wf.getsampwidth()
            rate = wf.getframerate()
            raw = wf.readframes(wf.getnframes())
    except (OSError, wave.Error) as e:
        raise RecordingError(f"{path}: not a readable wav: {e}") from e
    if channels != 1:
        raise RecordingError(f"{path}: expected mono audio, got {channels} channels")
    if width != _SAMPLE_WIDTH:
        raise RecordingError(f"{path}: expected 16-bit PCM, got {width * 8}-bit")
    pcm16 = np.frombuffer(raw, dtype="<i2")
    samples: MonoAudio = (pcm16.astype(np.float32) / _FULL_SCALE).clip(-1.0, 1.0)
    return samples, rate


def _keeps_nothing(path: Path) -> bool:
    del path
    return False


def prune_recordings(
    directory: Path,
    max_total_bytes: int,
    *,
    keep: Callable[[Path], bool] = _keeps_nothing,
) -> None:
    """Delete the oldest recordings until the directory fits in its budget.

    Only files matching :data:`_CAPTURE_GLOB` are ever considered, let alone
    deleted. ``recording.dir`` is a configurable path and pointing it at a
    directory that already holds something else must cost the user nothing.

    Oldest-first and size-driven only: age is not a criterion, because a
    recording is worth keeping for exactly as long as nothing newer needs the
    space.

    ``keep`` answers "is this file still open for writing?" and is asked at
    the moment of deletion, never earlier. A set snapshotted when the prune
    was *requested* is not good enough: this runs on a writer thread, the
    listing below can stall on a sick mount for longer than
    :data:`_WRITER_JOIN_SECONDS`, and by the time it answers, the next capture
    may have opened a file the snapshot has never heard of -- which this would
    then unlink out from under the writer holding it, leaving "recorded to
    <path>" in the log for a file that no longer exists. Pruning the recording
    the user is speaking into would be this module recreating the bug it
    exists to prevent.

    Never raises: a directory that cannot be pruned is a disk that fills
    slowly, which is not a reason to interrupt a dictation.
    """
    if not directory.is_dir():
        return  # nothing recorded yet; the first capture creates it
    try:
        entries = sorted(
            (path.stat().st_mtime, path.stat().st_size, path)
            for path in directory.glob(_CAPTURE_GLOB)
            if path.is_file()
        )
    except OSError as e:
        log.error("could not list %s to prune it: %s", directory, e)
        return

    total = sum(size for _, size, _ in entries)
    for _, size, path in entries:
        if total <= max_total_bytes:
            return
        if keep(path):
            continue
        try:
            path.unlink()
        except OSError as e:
            log.error("could not prune %s: %s", path, e)
            continue
        total -= size
        log.info("pruned %s (%.1f MB) to stay under %.1f GB of recordings",
                 path.name, size / 1e6, max_total_bytes / 1e9)


def secure_recordings(directory: Path) -> None:
    """Narrow any recording whose mode is wider than 0600.

    New files are created 0600 by :func:`_open_capture`, so this only ever has
    work to do for files an older version left behind -- but a recording of
    what you said is exactly the kind of thing that should not stay
    world-readable because of when it happened to be written. Never raises.

    Symlinks are skipped. ``chmod`` follows them and Linux cannot chmod the
    link itself, so ``capture-x.wav -> notes.txt`` would have this narrowing a
    file it never wrote -- outside the "only what :func:`_open_capture`
    created" rule the rest of this module keeps. (``unlink`` does not follow,
    so pruning was never exposed to it.)
    """
    for path in directory.glob(_CAPTURE_GLOB):
        try:
            if path.is_symlink():
                continue
            if path.is_file() and path.stat().st_mode & 0o777 != _FILE_MODE:
                path.chmod(_FILE_MODE)
                log.info("narrowed the permissions on %s", path.name)
        except OSError as e:
            log.error("could not narrow the permissions on %s: %s", path, e)


@dataclass(slots=True)
class _Recording:
    """Everything one writer thread owns, and nothing it shares.

    This is the fix for a bug the first version of this module had and the
    review caught: a writer abandoned at the join timeout is still *running*,
    and if it kept reading ``self._broken``, the last path and the queued
    frame counter off the recorder, it would reach into the next capture. It
    did: a zombie whose write finally raised set the live capture's broken
    flag and silently muted half of it, under an ERROR naming the wrong file.

    So a writer is handed a record and never touches the recorder again.
    Nothing a zombie can do is visible to the capture that followed it: the
    flag it sets is its own, the counter it decrements is its own, and the
    path it names in a log line is the file that actually failed.
    """

    path: Path
    wav: wave.Wave_write
    handle: BinaryIO
    queue: queue.SimpleQueue[MonoAudio | None]
    max_queued: int
    broken: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)
    """Guards ``queued`` and ``dropped``. Only ever held around an integer
    update -- never across I/O, so the realtime thread cannot wait on a disk."""
    queued: int = 0
    dropped: int = 0


class CaptureRecorder:
    """Writes the in-flight capture to a wav, one file per capture.

    Lifecycle mirrors :class:`~voice_kb.audio.AudioCapture`: :meth:`start` on
    ``start_capture``, :meth:`write` from the realtime callback, :meth:`stop`
    on ``stop_capture``. The directory's size budget is enforced twice: once
    when the daemon starts (here), so a disk under pressure is relieved even
    by a session that never records, and once per capture on the writer
    thread, so a daemon running for weeks stays bounded without ever putting
    a directory scan on the keypress path.

    Every capture gets its own :class:`_Recording`; the recorder itself holds
    only which one is current, which threads are still winding down, and where
    the last capture went.
    """

    def __init__(self, config: RecordingConfig, sample_rate: int) -> None:
        self._config = config
        self._sample_rate = sample_rate
        #: The last capture's recording, for the log line naming it. Written
        #: by :meth:`start` and read by :meth:`status`, both on the caller's
        #: thread. Its ``broken`` flag is set by the writer that owns it, and
        #: that is the point: the status has to know.
        self._last: _Recording | None = None
        self._current: _Recording | None = None
        self._writer: threading.Thread | None = None
        #: Every capture file currently open for writing -- the one being
        #: recorded, plus any an abandoned writer may still be finishing. The
        #: prune consults it at the moment it is about to unlink, through
        #: :meth:`_is_open`, which is the only thing any writer thread reads
        #: off this object. Guarded by its own lock precisely because it is
        #: shared on purpose, unlike the per-writer state in `_Recording`.
        self._open_lock = threading.Lock()
        self._open_paths: set[Path] = set()
        if config.enabled:
            prune_recordings(config.dir, config.max_total_bytes)

    def status(self) -> RecordingStatus:
        """Where the current or most recent capture went, and whether it is whole.

        Kept after :meth:`stop` so the capture's log line can name the file it
        landed in, which is what makes a lost transcript recoverable: the log
        says which recording holds the audio it is missing.

        The whole/short distinction is not decoration. Callers use this to
        tell the user "recover the rest with ``voice-kb transcribe``", and a
        recording that was given up on at 200s does not support that promise.
        Returning a bare path made every caller assume the happy case; three
        variants make the unhappy one impossible to miss.
        """
        recording = self._last
        if recording is None:
            return NotRecorded()
        if recording.broken.is_set():
            return Truncated(path=recording.path)
        return Recorded(path=recording.path)

    def start(self) -> None:
        """Open the next recording and the thread that writes it.

        The file is created here, on the caller's thread, rather than on the
        writer: a directory that cannot be written to should be reported at
        the first keypress, not silently once per capture forever.
        """
        if not self._config.enabled:
            return
        self.stop()  # a previous capture that was never stopped must not leak
        try:
            self._config.dir.mkdir(parents=True, exist_ok=True, mode=_DIR_MODE)
            _narrow_directory(self._config.dir)
            path, handle = _open_capture(self._config.dir)
        except OSError as e:
            log.error(
                "could not open a recording in %s, so this capture exists only "
                "in memory: %s", self._config.dir, e
            )
            self._last = None
            return
        try:
            # Not a context manager: the file stays open across `write` and
            # `stop`, which is the whole point -- the audio is written as it
            # arrives, not gathered up and dumped at the end. The writer
            # thread closes both, including if it is abandoned and finishes
            # later.
            wav = wave.open(handle, "wb")  # noqa: SIM115
            wav.setnchannels(1)
            wav.setsampwidth(_SAMPLE_WIDTH)
            wav.setframerate(self._sample_rate)
        except (OSError, wave.Error) as e:
            log.error("could not start the recording %s: %s", path, e)
            handle.close()
            self._last = None
            return
        recording = _Recording(
            path=path,
            wav=wav,
            handle=handle,
            queue=queue.SimpleQueue(),
            max_queued=int(_QUEUE_SECONDS * self._sample_rate),
        )
        with self._open_lock:
            self._open_paths.add(path)
        self._last = recording
        self._current = recording
        self._writer = threading.Thread(
            target=self._drain, args=(recording,), name="voice-kb-recorder", daemon=True
        )
        self._writer.start()
        log.debug("recording this capture to %s", path)

    def write(self, samples: MonoAudio) -> None:
        """Hand one callback's audio to the writer thread. Realtime-safe.

        Called from PortAudio's thread, so it does the least it can: take an
        uncontended lock around two integers, then put a reference on a queue.
        No I/O, no logging, no blocking -- in particular the put is on an
        unbounded ``SimpleQueue`` and the *bound* is enforced by the frame
        counter above it, so a full queue drops a buffer instead of stalling
        the audio thread.

        Everything it consults belongs to the *current* recording, read once
        into a local. A writer abandoned by an earlier :meth:`stop` has no way
        to reach any of it, which is what keeps a zombie from muting a capture
        that is going perfectly well.

        A drop is reported by :meth:`stop`, not from here, for the same reason
        the ceiling in ``audio.py`` is: this thread cannot log.

        ``samples`` must already be a copy the caller owns; PortAudio reuses
        its input buffer the moment the callback returns.
        """
        recording = self._current
        if recording is None or recording.broken.is_set():
            return
        with recording.lock:
            if recording.queued + samples.size > recording.max_queued:
                recording.dropped += samples.size
                return
            recording.queued += samples.size
        recording.queue.put(samples)

    def stop(self) -> None:
        """Flush what is queued, then let the writer close the file.

        Blocks until the writer has drained the queue and closed the wav, so
        the file is complete and its header's frame count is correct before
        the caller goes on to decode. That wait is the point: a recording that
        is only *mostly* written when the decode fails is not the safety net
        this is meant to be.

        But it is a *bounded* wait. A write to a stalled mount can block
        forever without raising, and this runs on the Qt thread on the way to
        the decode, so past :data:`_WRITER_JOIN_SECONDS` the writer is
        abandoned -- it is a daemon thread, so it cannot hold up exit either --
        and the dictation carries on with a truncated recording that has said
        so at ERROR. An abandoned writer keeps its own recording, and its file
        stays in the open-path registry until it finally closes, so no later
        prune can delete the file out from under it.
        """
        writer, recording = self._writer, self._current
        self._writer, self._current = None, None
        if writer is None or recording is None:
            return
        recording.queue.put(None)
        writer.join(_WRITER_JOIN_SECONDS)
        if writer.is_alive():
            recording.broken.set()
            log.error(
                "the recording %s is still being written after %.0fs -- the disk "
                "is not answering. Carrying on without it; the file may be "
                "truncated or unreadable. The dictation itself is unaffected.",
                recording.path, _WRITER_JOIN_SECONDS,
            )
        with recording.lock:
            dropped = recording.dropped
        if dropped:
            # A hole in the middle is exactly as unrecoverable as a file cut
            # short, so it has to reach `status()` the same way. Logging
            # "INCOMPLETE" while `status()` still answered `Recorded` let
            # `_warn_capture_capped` offer `voice-kb transcribe` for a file
            # that cannot support the promise -- the false reassurance the
            # `Truncated` variant exists to make unrepresentable.
            recording.broken.set()
            log.error(
                "dropped %.1fs of audio that the writer could not keep up with: "
                "%s is INCOMPLETE and does not hold the whole capture.",
                dropped / self._sample_rate, recording.path,
            )

    def _is_open(self, path: Path) -> bool:
        """Whether some writer still holds ``path``. Asked by a prune, mid-delete."""
        with self._open_lock:
            return path in self._open_paths

    def _drain(self, recording: _Recording) -> None:
        """Writer thread: pull buffers off the queue and write them.

        Touches ``recording`` and nothing else on the recorder -- see
        :class:`_Recording` for the bug that rule exists to prevent.

        Gives up on the whole recording at the first failure rather than
        writing a wav with holes in it, and says so at ERROR -- a recording
        that quietly stopped part-way is the failure mode this module was
        written to end. Closing the file is this thread's job in every exit
        path, including the one where :meth:`stop` has already given up on it:
        a wav is only readable once its header has been patched with the frame
        count, so an abandoned writer that eventually unblocks still leaves a
        file worth having.
        """
        try:
            # Pruning here rather than at ``start`` keeps a directory scan off
            # both the realtime thread and the keypress path, and it is what
            # bounds a daemon that has been running for weeks -- the
            # constructor's prune ran once, before any of this session's
            # captures.
            prune_recordings(
                self._config.dir, self._config.max_total_bytes, keep=self._is_open
            )
            secure_recordings(self._config.dir)
            while True:
                samples = recording.queue.get()
                if samples is None:
                    return
                with recording.lock:
                    recording.queued -= samples.size
                try:
                    recording.wav.writeframes(to_pcm16(samples))
                except (OSError, wave.Error, ValueError) as e:
                    log.error(
                        "recording to %s failed after %.1fs of audio; the rest of "
                        "this capture is not on disk: %s",
                        recording.path,
                        recording.wav.getnframes() / self._sample_rate,
                        e,
                    )
                    recording.broken.set()
                    return
        finally:
            _close_capture(recording.wav, recording.handle, recording.path)
            with self._open_lock:
                self._open_paths.discard(recording.path)


def _narrow_directory(directory: Path) -> None:
    """Force the recording directory to 0700, creating it or not.

    ``mkdir(mode=...)`` only applies to a directory it actually creates, so an
    upgrade -- or a directory the user made themselves -- would otherwise keep
    whatever mode it had. Failure is a warning, never fatal: a directory we
    can write to but not chmod is still a working recording.
    """
    try:
        if directory.stat().st_mode & 0o777 != _DIR_MODE:
            directory.chmod(_DIR_MODE)
    except OSError as e:
        log.warning("could not set the permissions on %s: %s", directory, e)


def _open_capture(directory: Path) -> tuple[Path, BinaryIO]:
    """Create the next capture file, 0600 from the instant it exists.

    ``os.open`` with ``O_CREAT|O_EXCL`` and an explicit mode rather than
    ``wave.open(str(path))``: that creates the file 0644 and narrows it
    afterwards, leaving a window in which another local user can open a
    recording of whatever is being said. ``O_EXCL`` also settles the
    two-captures-in-the-same-second race (a double tap of the hotkey does it)
    by construction, instead of by an existence check that could be raced.
    """
    stamp = datetime.now().strftime("%Y-%m-%d-%H%M%S")
    for index in range(_MAX_SAME_SECOND):
        tail = "" if index == 0 else f"-{index}"
        path = directory / f"{_CAPTURE_PREFIX}{stamp}{tail}{_CAPTURE_SUFFIX}"
        try:
            fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_EXCL, _FILE_MODE)
        except FileExistsError:
            continue
        return path, os.fdopen(fd, "wb")
    raise OSError(f"{directory}: {_MAX_SAME_SECOND} recordings already stamped {stamp}")


def _close_capture(wav: wave.Wave_write, handle: BinaryIO, path: Path | None) -> None:
    """Patch the header, then close the file descriptor underneath it.

    ``wave`` does not close a file object it was handed, only one it opened
    itself, so both halves are needed -- and the header patch is what makes
    the difference between a readable recording and one that claims to hold
    zero frames.
    """
    try:
        wav.close()
    except (OSError, wave.Error) as e:
        log.error("could not finalise the recording %s: %s", path, e)
    try:
        handle.close()
    except OSError as e:
        log.error("could not close the recording %s: %s", path, e)
