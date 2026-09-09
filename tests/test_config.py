"""Offline helper configuration parsing and validation."""

from pathlib import Path

import pytest

from spokenpad.config import AsrConfig, Config, ConfigError, TextConfig


def test_defaults_cover_the_reference_pipeline() -> None:
    config = Config()
    assert config.audio.sample_rate == 16000
    assert config.asr.decoding == "modified_beam_search"
    assert config.vad.enabled is True
    assert config.text.fillers == ("uh", "um", "erm", "hmm")


def test_missing_file_uses_defaults(tmp_path: Path) -> None:
    assert Config.load(tmp_path / "missing.toml") == Config()


def test_helper_sections_round_trip_and_rust_sections_are_tolerated(tmp_path: Path) -> None:
    path = tmp_path / "config.toml"
    path.write_text(
        """
[audio]
sample_rate = 48000
[asr]
model_dir = "models/custom"
num_threads = 2
vocabulary = ["kubectl"]
[vad]
model = "models/vad.onnx"
[text]
fillers = ["uh"]
replacements = { teh = "the" }
[hotkey]
key_code = 30
[recording]
enabled = false
[nvim]
window_instance = "test"
[preview]
enabled = false
""",
        encoding="utf-8",
    )

    config = Config.load(path)

    assert config.audio.sample_rate == 48000
    assert config.asr.model_dir == tmp_path / "models/custom"
    assert config.asr.vocabulary == ("kubectl",)
    assert config.vad.model == tmp_path / "models/vad.onnx"
    assert config.text == TextConfig(fillers=("uh",), replacements={"teh": "the"})


def test_unknown_section_is_rejected() -> None:
    with pytest.raises(ConfigError, match="unknown config section"):
        Config.from_mapping({"unknown": {}})


def test_unknown_helper_key_names_its_section() -> None:
    with pytest.raises(ConfigError, match=r"\[asr\]"):
        Config.from_mapping({"asr": {"threads": 2}})


def test_zero_threads_are_rejected() -> None:
    with pytest.raises(ConfigError, match="num_threads"):
        AsrConfig(num_threads=0)


def test_vocabulary_requires_beam_search() -> None:
    with pytest.raises(ConfigError, match="vocabulary"):
        AsrConfig(decoding="greedy_search", vocabulary=("kubectl",))


@pytest.mark.parametrize(
    ("section", "message"),
    [
        ({"threshold": 1.0}, "threshold"),
        ({"min_speech_seconds": 0}, "min_speech_seconds"),
        ({"max_speech_seconds": 0.1}, "max_speech_seconds"),
        ({"pad_seconds": -1}, "pad_seconds"),
    ],
)
def test_invalid_vad_values_are_rejected(section: dict[str, float], message: str) -> None:
    with pytest.raises(ConfigError, match=message):
        Config.from_mapping({"vad": section})


def test_with_model_dir_is_immutable() -> None:
    original = Config()
    changed = original.with_model_dir(Path("other"))
    assert original.asr.model_dir != changed.asr.model_dir
    assert changed.asr.model_dir == Path("other")
