"""Evaluation metric and WAV-boundary regressions."""

import wave
from pathlib import Path

import numpy as np

from scripts.eval import character_error_rate, load_wav, normalize, word_error_rate
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
