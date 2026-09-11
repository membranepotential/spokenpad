"""Pure reference decode pipeline behavior."""

from dataclasses import dataclass, field

import numpy as np

from spokenpad.asr import TranscriptionResult
from spokenpad.audio import MonoAudio
from spokenpad.decode import decode_capture
from spokenpad.vad import Segment


@dataclass
class FakeTranscriber:
    texts: list[str]
    calls: list[MonoAudio] = field(default_factory=list)

    def transcribe(self, samples: MonoAudio, sample_rate: int) -> TranscriptionResult:
        assert sample_rate == 16000
        self.calls.append(samples)
        return TranscriptionResult(self.texts.pop(0), 0.01)


@dataclass
class FakeSegmenter:
    segments: list[Segment]

    def split(self, samples: MonoAudio) -> list[Segment]:
        del samples
        return self.segments


def _samples(size: int = 12) -> MonoAudio:
    return np.arange(size, dtype=np.float32)


def _segment(start: int, end: int, *, settled: bool = False) -> Segment:
    return Segment(_samples()[start:end], start / 16000, end, settled)


def test_segments_decode_once_and_join_nonempty_text() -> None:
    transcriber = FakeTranscriber(["first", "", "second"])
    landed: list[str] = []
    text = decode_capture(
        _samples(),
        transcriber=transcriber,
        segmenter=FakeSegmenter([_segment(0, 4), _segment(4, 8), _segment(8, 12)]),
        sample_rate=16000,
        on_segment=landed.append,
    )
    assert text == "first second"
    assert landed == ["first", "second"]
    assert len(transcriber.calls) == 3


def test_all_empty_segments_retry_the_whole_buffer_once() -> None:
    transcriber = FakeTranscriber(["", "", "recovered"])
    text = decode_capture(
        _samples(),
        transcriber=transcriber,
        segmenter=FakeSegmenter([_segment(0, 6), _segment(6, 12)]),
        sample_rate=16000,
    )
    assert text == "recovered"
    assert len(transcriber.calls) == 3


def test_no_segments_means_no_decode_and_no_retry() -> None:
    """Silence is not decoded, and the recovery retry does not rescue it.

    The retry exists for chunks that decoded to nothing; a capture the VAD
    found no speech in has no chunks, and asking the model for a transcript of
    silence is how "Thank you." got into the file.
    """
    transcriber = FakeTranscriber(["never asked for"])
    text = decode_capture(
        _samples(),
        transcriber=transcriber,
        segmenter=FakeSegmenter([]),
        sample_rate=16000,
    )
    assert text == ""
    assert transcriber.calls == []


def test_abandoned_decode_never_retries() -> None:
    transcriber = FakeTranscriber(["unused"])
    text = decode_capture(
        _samples(),
        transcriber=transcriber,
        segmenter=FakeSegmenter([_segment(0, 6), _segment(6, 12)]),
        sample_rate=16000,
        abandoned=lambda: True,
    )
    assert text == ""
    assert transcriber.calls == []


def test_missing_segmenter_decodes_the_whole_buffer_once() -> None:
    samples = _samples()
    transcriber = FakeTranscriber(["whole"])
    assert (
        decode_capture(samples, transcriber=transcriber, segmenter=None, sample_rate=16000)
        == "whole"
    )
    assert len(transcriber.calls) == 1
    assert transcriber.calls[0] is samples
