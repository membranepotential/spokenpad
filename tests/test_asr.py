"""Pure ASR setup helpers; no model is loaded by these tests."""

from pathlib import Path
from typing import cast

import numpy as np
import pytest
import sherpa_onnx

from spokenpad.asr import (
    ModelMissingError,
    TrailingSilence,
    Transcriber,
    ensure_model_files,
    generate_bpe_vocab,
    render_hotwords,
)
from spokenpad.config import AsrConfig


class _Result:
    text = "decoded"


class _Stream:
    result = _Result()

    def __init__(self) -> None:
        self.waveforms: list[tuple[int, np.ndarray]] = []

    def accept_waveform(self, sample_rate: int, samples: np.ndarray) -> None:
        self.waveforms.append((sample_rate, samples))


class _Recognizer:
    def __init__(self) -> None:
        self.stream = _Stream()
        self.decode_calls = 0

    def create_stream(self) -> _Stream:
        return self.stream

    def decode_stream(self, stream: _Stream) -> None:
        assert stream is self.stream
        self.decode_calls += 1


def test_bpe_vocab_uses_negative_token_ids(tmp_path: Path) -> None:
    tokens = tmp_path / "tokens.txt"
    tokens.write_text("<blk> 0\n▁hello 7\npiece 12\n", encoding="utf-8")
    assert generate_bpe_vocab(tokens) == "<blk>\t0\n▁hello\t-7\npiece\t-12\n"


def test_hotwords_are_one_phrase_per_line() -> None:
    assert render_hotwords(("cargo test", "Neovim")) == "cargo test\nNeovim\n"
    assert render_hotwords(()) == ""


def test_missing_models_name_the_setup_command(tmp_path: Path) -> None:
    with pytest.raises(ModelMissingError, match=r"scripts/fetch_model\.py"):
        ensure_model_files(AsrConfig(model_dir=tmp_path))


def test_transcribe_submits_audio_plus_one_second_silence_once() -> None:
    recognizer = _Recognizer()
    transcriber = object.__new__(Transcriber)
    transcriber._recognizer = cast(sherpa_onnx.OfflineRecognizer, recognizer)
    transcriber._trailing_silence = {}
    samples = np.ones(80, dtype=np.float32)

    result = transcriber.transcribe(samples, 16000)

    assert result.text == "decoded"
    assert recognizer.decode_calls == 1
    assert len(recognizer.stream.waveforms) == 1
    rate, padded_samples = recognizer.stream.waveforms[0]
    assert rate == 16000
    assert padded_samples.dtype == np.float32
    assert padded_samples.shape == (16080,)
    np.testing.assert_array_equal(padded_samples[:80], samples)
    assert not padded_samples[80:].any()


def test_bare_transcribe_submits_the_audio_unpadded() -> None:
    recognizer = _Recognizer()
    transcriber = object.__new__(Transcriber)
    transcriber._recognizer = cast(sherpa_onnx.OfflineRecognizer, recognizer)
    transcriber._trailing_silence = {}
    samples = np.ones(80, dtype=np.float32)

    transcriber.transcribe(samples, 16000, TrailingSilence.BARE)

    [(_, submitted)] = recognizer.stream.waveforms
    np.testing.assert_array_equal(submitted, samples)
