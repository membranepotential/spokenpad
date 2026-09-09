"""Hardware-free stand-ins for ``spokenpad``'s I/O boundaries.

Every fake here implements just enough of the real class's surface for
``spokenpad.app.Daemon`` to run against it -- no microphone, no model load,
no subprocess, no X server. See ``conftest.py`` for how they get wired in.
"""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime
from pathlib import Path

import numpy as np

from spokenpad.asr import TranscriptionResult
from spokenpad.audio import MonoAudio
from spokenpad.config import AsrConfig, AudioConfig, NvimConfig
from spokenpad.geometry import Output, Rect
from spokenpad.nvim import Appended, AppendResult
from spokenpad.recorder import NotRecorded, RecordingStatus
from spokenpad.state import Phase


class FakeAudioCapture:
    """Stands in for :class:`spokenpad.audio.AudioCapture`.

    ``stop_capture`` always returns :attr:`next_samples`, regardless of what
    ``start_capture`` calls happened in between -- tests that care about the
    actual samples passed to a decode set it directly before dispatching the
    key-up that triggers ``Decode``.

    ``snapshot_capture`` is likewise decoupled from ``next_samples``: a
    tick reads its own :attr:`preview_samples`, so a test can tell what a
    tick committed from what the release decode did.
    """

    def __init__(self, config: AudioConfig, recorder: object) -> None:
        self.config = config
        #: The real ``AudioCapture`` hands every callback to this before its
        #: own in-memory ceiling; nothing here calls back, so the fake only
        #: has to accept it. ``recording`` below is what a test reads.
        self.recorder = recorder
        self.capped = False
        self.recording: RecordingStatus = NotRecorded()
        self.start_calls = 0
        self.stop_calls = 0
        self.snapshot_calls: list[int] = []
        self.closed = False
        self.next_samples: MonoAudio = np.zeros(1600, dtype=np.float32)
        self.preview_samples: MonoAudio = np.zeros(800, dtype=np.float32)
        self.dead = False
        self.recover_calls = 0
        self.seconds_since_callback_value = 0.0

    def start_capture(self) -> None:
        self.start_calls += 1

    def stop_capture(self) -> MonoAudio:
        self.stop_calls += 1
        return self.next_samples

    def snapshot_capture(self, since_frame: int = 0) -> MonoAudio:
        """Mirrors ``AudioCapture.snapshot_capture``: the audio from
        ``since_frame`` on, non-destructive, never disturbing what
        ``stop_capture`` will hand back."""
        self.snapshot_calls.append(since_frame)
        return self.preview_samples[since_frame:]

    def take_cap_notice(self) -> bool:
        """Mirrors AudioCapture.take_cap_notice. Set ``capped`` to make the
        next poll report the ceiling; it self-clears, as the real one does
        once it has been reported for this capture."""
        if not self.capped:
            return False
        self.capped = False
        return True

    def recording_status(self) -> RecordingStatus:
        """Mirrors AudioCapture.recording_status: where this capture is on disk,
        and whether the file is whole."""
        return self.recording

    def take_stream_status(self) -> str | None:
        """Mirrors AudioCapture.take_stream_status; the fake never sees flags."""
        return None

    def seconds_since_callback(self) -> float:
        return self.seconds_since_callback_value

    def recover_if_dead(self) -> bool:
        """Mirrors AudioCapture.recover_if_dead. Set ``dead`` to make the next
        poll report a recovery; it self-clears, as a real reopen would."""
        self.recover_calls += 1
        if not self.dead:
            return False
        self.dead = False
        return True

    def current_level(self) -> float:
        return 0.0

    def close(self) -> None:
        self.closed = True


class FakeTranscriber:
    """Stands in for :class:`spokenpad.asr.Transcriber`.

    ``next_result`` is mutated by the test right before the call it wants to
    control; a :class:`BaseException` instance there makes ``transcribe``
    raise it instead of returning, for exercising the decode-failure path.

    ``results`` queues one result per call, for a decode that is split into
    several segments and so calls ``transcribe`` more than once. It is
    consumed in order and falls back to ``next_result`` when empty, so every
    existing single-decode test is unaffected.
    """

    def __init__(self, config: AsrConfig | None = None) -> None:
        self.config = config
        self.calls: list[MonoAudio] = []
        self.next_result: TranscriptionResult | BaseException = TranscriptionResult(
            text="", elapsed_seconds=0.0
        )
        self.results: list[TranscriptionResult | BaseException] = []

    def transcribe(self, samples: MonoAudio, sample_rate: int) -> TranscriptionResult:
        del sample_rate
        self.calls.append(samples)
        result = self.results.pop(0) if self.results else self.next_result
        if isinstance(result, BaseException):
            raise result
        return result


class FakeNvimSession:
    """Stands in for :class:`spokenpad.nvim.NvimSession`.

    Duck-typed, like the other fakes here -- ``_NvimBridge`` never learns
    whether it is holding a real session or this one, so none of the
    daemon's threading or signal wiring needs to change to be testable.

    Every call is recorded so a test can assert on it exactly the way it
    would assert on the real thing having happened: ``appended`` is the list
    of every string ``append`` was called with (the "nothing vanishes"
    guarantee, one level up from ``test_nvim.py``'s on-disk check), and
    ``states`` is every ``dict`` a ``set_state`` call actually changed, in
    order, so a test can find the last preview or phase pushed toward the
    indicator.

    ``ensure_result``/``append_result`` are settable *before* the call they
    should affect, exactly like ``FakeTranscriber.next_result`` -- so the
    failure paths (a window that cannot be opened, an append that cannot
    land) are as easy to exercise as the happy path, which is the whole
    reason ``NvimSession`` reports those as values instead of raising.
    """

    def __init__(
        self, config: NvimConfig | None = None, *, clock: Callable[[], datetime] | None = None
    ) -> None:
        self.config = config
        self.clock = clock
        self.appended: list[str] = []
        #: ``(text, continued)`` for every append, so a test can assert that
        #: an utterance arriving in several segments still forms one
        #: paragraph -- which ``appended`` alone cannot show.
        self.append_calls: list[tuple[str, bool]] = []
        self.states: list[dict[str, Phase | float | str]] = []
        self.ensure_calls = 0
        self.warm_up_calls = 0
        self.close_calls = 0
        self.place_calls: list[Rect] = []
        self.ensure_result = True
        self.append_result: AppendResult = Appended(line=1, elapsed_ms=1.0)
        self._path = Path("/tmp/spokenpad-test/dictation-fake.md")

    @property
    def path(self) -> Path | None:
        return self._path

    @property
    def connected(self) -> bool:
        return self.ensure_calls > 0 and self.ensure_result

    def ensure(self) -> bool:
        self.ensure_calls += 1
        return self.ensure_result

    def warm_up(self) -> None:
        self.warm_up_calls += 1

    def append(self, text: str, *, continued: bool = False) -> AppendResult:
        self.appended.append(text)
        self.append_calls.append((text, continued))
        return self.append_result

    def set_state(
        self,
        *,
        phase: Phase | None = None,
        level: float | None = None,
        preview: str | None = None,
        latched: bool | None = None,
        previewing: bool | None = None,
    ) -> None:
        update: dict[str, Phase | float | str | bool] = {}
        if phase is not None:
            update["phase"] = phase
        if latched is not None:
            update["latched"] = latched
        if previewing is not None:
            update["previewing"] = previewing
        if level is not None:
            update["level"] = level
        if preview is not None:
            update["preview"] = preview
        if update:
            self.states.append(update)

    def place_window(self, rect: Rect) -> None:
        self.place_calls.append(rect)

    def close(self) -> None:
        self.close_calls += 1


class FakeX11:
    """Stands in for the module-level functions in :mod:`spokenpad.x11`.

    Never shells out to ``xrandr``.
    """

    def __init__(self) -> None:
        self.outputs_calls = 0
        self.outputs_result: list[Output] = [
            Output(name="fake-1", rect=Rect(x=0, y=0, width=1920, height=1080), primary=True)
        ]

    def outputs(self) -> list[Output]:
        self.outputs_calls += 1
        return self.outputs_result
