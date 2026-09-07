"""Hardware-free stand-ins for ``voice_kb``'s I/O boundaries.

Every fake here implements just enough of the real class's surface for
``voice_kb.app.Daemon`` to run against it -- no microphone, no model load,
no subprocess, no X server. See ``conftest.py`` for how they get wired in.
"""

from __future__ import annotations

from collections.abc import Callable
from datetime import datetime
from pathlib import Path

import numpy as np

from voice_kb.asr import TranscriptionResult
from voice_kb.audio import MonoAudio
from voice_kb.config import AsrConfig, AudioConfig, NvimConfig
from voice_kb.geometry import Output, Rect
from voice_kb.nvim import Appended, AppendResult
from voice_kb.state import Phase


class FakeAudioCapture:
    """Stands in for :class:`voice_kb.audio.AudioCapture`.

    ``stop_capture`` always returns :attr:`next_samples`, regardless of what
    ``start_capture`` calls happened in between -- tests that care about the
    actual samples passed to a decode set it directly before dispatching the
    key-up that triggers ``Decode``.

    ``snapshot_capture`` is likewise decoupled from ``next_samples``: a
    preview reads its own :attr:`preview_samples`, so a test can prove the
    injected text came from the *committed* decode and not from anything a
    preview saw.
    """

    def __init__(self, config: AudioConfig) -> None:
        self.config = config
        self.start_calls = 0
        self.stop_calls = 0
        self.snapshot_calls: list[int | None] = []
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

    def snapshot_capture(self, max_frames: int | None = None) -> MonoAudio:
        """Mirrors ``AudioCapture.snapshot_capture``: non-destructive, and
        never disturbs what ``stop_capture`` will hand back."""
        self.snapshot_calls.append(max_frames)
        if max_frames is None:
            return self.preview_samples
        return self.preview_samples[-max_frames:]

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
    """Stands in for :class:`voice_kb.asr.Transcriber`.

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
    """Stands in for :class:`voice_kb.nvim.NvimSession`.

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
        self.raise_calls = 0
        self.close_calls = 0
        self.place_calls: list[Rect] = []
        self.ensure_result = True
        self.append_result: AppendResult = Appended(line=1, elapsed_ms=1.0)
        self._path = Path("/tmp/voice-kb-test/dictation-fake.md")

    @property
    def path(self) -> Path | None:
        return self._path

    @property
    def connected(self) -> bool:
        return self.ensure_calls > 0 and self.ensure_result

    def ensure(self) -> bool:
        self.ensure_calls += 1
        return self.ensure_result

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

    def raise_window(self) -> None:
        self.raise_calls += 1

    def place_window(self, rect: Rect) -> None:
        self.place_calls.append(rect)

    def close(self) -> None:
        self.close_calls += 1


class FakeX11:
    """Stands in for the module-level functions in :mod:`voice_kb.x11`.

    Never shells out to ``xrandr``/``xdotool``; call counts are what the
    subprocess-storm regression test asserts a bound on.
    """

    def __init__(self) -> None:
        self.outputs_calls = 0
        self.focused_calls = 0
        self.outputs_result: list[Output] = [
            Output(name="fake-1", rect=Rect(x=0, y=0, width=1920, height=1080), primary=True)
        ]
        self.focused_result: Rect | None = None

    def outputs(self) -> list[Output]:
        self.outputs_calls += 1
        return self.outputs_result

    def focused_window_rect(self) -> Rect | None:
        self.focused_calls += 1
        return self.focused_result
