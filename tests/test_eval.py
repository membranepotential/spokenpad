"""Evaluation metric and WAV-boundary regressions."""

import wave
from pathlib import Path

import numpy as np

from scripts.eval import (
    AggregateStats,
    EvalReport,
    SampleResult,
    character_error_rate,
    load_wav,
    normalize,
    word_error_rate,
)
from scripts.verify_rust import read_wav as read_reference_wav


def test_normalization_preserves_symbolized_underscores() -> None:
    assert normalize(" Test_file, RM-RF! ") == "test_file rm rf"


def test_error_rates_handle_edits_and_empty_references() -> None:
    assert word_error_rate("one two", "one too") == 0.5
    assert character_error_rate("ab", "ac") == 0.5
    assert word_error_rate("", "") == 0.0
    assert word_error_rate("", "word") == 1.0


def test_load_wav_reads_mono_pcm16(tmp_path: Path) -> None:
    path = tmp_path / "sample.wav"
    values = np.array([-32768, 0, 32767], dtype="<i2")
    with wave.open(str(path), "wb") as target:
        target.setnchannels(1)
        target.setsampwidth(2)
        target.setframerate(16000)
        target.writeframes(values.tobytes())
    samples, rate, duration = load_wav(path)
    assert rate == 16000
    assert duration == 3 / 16000
    assert np.allclose(samples, [-1.0, 0.0, 32767 / 32768])


def test_reference_reader_accepts_a_one_sample_pcm_payload(tmp_path: Path) -> None:
    path = tmp_path / "short.wav"
    with wave.open(str(path), "wb") as target:
        target.setnchannels(1)
        target.setsampwidth(2)
        target.setframerate(16000)
        target.writeframes(np.array([32767], dtype="<i2").tobytes())

    samples, rate = read_reference_wav(path)

    assert rate == 16000
    assert samples.dtype == np.float32
    assert samples.shape == (1,)
    assert samples[0] == np.float32(1.0)


def _report(*verified: bool) -> EvalReport:
    samples = tuple(
        SampleResult(
            file=f"{index}.wav",
            verified=flag,
            duration_s=1.0,
            reference="word",
            hypothesis="word",
            decode_seconds=0.1,
            rtf=10.0,
            wer=0.0,
            cer=0.0,
            handy_hypothesis=None,
            handy_wer=None,
            exercises=(),
        )
        for index, flag in enumerate(verified)
    )
    return EvalReport(
        vocabulary=(),
        hotwords_score=0.0,
        decoding="greedy_search",
        samples=samples,
        long_clip=None,
        aggregate=AggregateStats(wer=0.0, cer=0.0, handy_wer=None, samples_scored=len(samples)),
    )


def test_references_are_verified_only_when_every_scored_sample_is() -> None:
    assert _report(True, True).references_verified
    assert not _report(True, False).references_verified
    # A run that scored nothing has verified nothing.
    assert not _report().references_verified
