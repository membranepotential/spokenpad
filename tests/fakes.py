"""Hardware-free stand-ins for ``voice_kb``'s I/O boundaries.

Every fake here implements just enough of the real class's surface for
``voice_kb.app.Daemon`` to run against it -- no microphone, no model load,
no subprocess, no X server. See ``conftest.py`` for how they get wired in.
"""

from __future__ import annotations

import numpy as np

from voice_kb.asr import TranscriptionResult
from voice_kb.audio import MonoAudio
from voice_kb.config import AsrConfig, AudioConfig, PasteConfig
from voice_kb.geometry import Output, Rect
from voice_kb.inject import Injected, InjectResult


class FakeAudioCapture:
    """Stands in for :class:`voice_kb.audio.AudioCapture`.

    ``stop_capture`` always returns :attr:`next_samples`, regardless of what
    ``start_capture`` calls happened in between -- tests that care about the
    actual samples passed to a decode set it directly before dispatching the
    key-up that triggers ``Decode``.
    """

    def __init__(self, config: AudioConfig) -> None:
        self.config = config
        self.start_calls = 0
        self.stop_calls = 0
        self.closed = False
        self.next_samples: MonoAudio = np.zeros(1600, dtype=np.float32)

    def start_capture(self) -> None:
        self.start_calls += 1

    def stop_capture(self) -> MonoAudio:
        self.stop_calls += 1
        return self.next_samples

    def current_level(self) -> float:
        return 0.0

    def close(self) -> None:
        self.closed = True


class FakeTranscriber:
    """Stands in for :class:`voice_kb.asr.Transcriber`.

    ``next_result`` is mutated by the test right before the call it wants to
    control; a :class:`BaseException` instance there makes ``transcribe``
    raise it instead of returning, for exercising the decode-failure path.
    """

    def __init__(self, config: AsrConfig | None = None) -> None:
        self.config = config
        self.calls: list[MonoAudio] = []
        self.next_result: TranscriptionResult | BaseException = TranscriptionResult(
            text="", elapsed_seconds=0.0
        )

    def transcribe(self, samples: MonoAudio, sample_rate: int) -> TranscriptionResult:
        del sample_rate
        self.calls.append(samples)
        if isinstance(self.next_result, BaseException):
            raise self.next_result
        return self.next_result


class FakeInjector:
    """Stands in for :func:`voice_kb.inject.inject_text`.

    Recorded via ``calls`` so a test can assert exactly what text (and paste
    config) reached the injection boundary.
    """

    def __init__(self) -> None:
        self.calls: list[tuple[str, PasteConfig]] = []
        self.result: InjectResult = Injected(elapsed_ms=1.0, confirmed=True)

    def __call__(self, text: str, config: PasteConfig) -> InjectResult:
        self.calls.append((text, config))
        return self.result


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
