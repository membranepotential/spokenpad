"""Config parsing/validation: defaults, TOML round-trip, and every error path."""

from __future__ import annotations

from pathlib import Path

import pytest

from voice_kb.config import (
    AsrConfig,
    Config,
    ConfigError,
    HotkeyConfig,
    OverlayConfig,
    TextConfig,
)

# -- defaults ---------------------------------------------------------------


def test_defaults_construct_without_a_file() -> None:
    cfg = Config()
    assert cfg.hotkey.key_code == 186
    assert cfg.audio.sample_rate == 16000
    assert cfg.asr.decoding == "modified_beam_search"
    assert cfg.text.fillers == ("uh", "um", "erm", "hmm")


def test_load_missing_path_falls_back_to_defaults(tmp_path: Path) -> None:
    cfg = Config.load(tmp_path / "does-not-exist.toml")
    assert cfg == Config()


# -- TOML round-trip ----------------------------------------------------------


def test_toml_round_trip(tmp_path: Path) -> None:
    config_path = tmp_path / "config.toml"
    config_path.write_text(
        """
        [hotkey]
        key_code = 30

        [audio]
        sample_rate = 48000
        preroll_ms = 0

        [asr]
        num_threads = 2
        decoding = "greedy_search"
        hotwords_score = 2.0

        [text]
        strip_fillers = false
        fillers = ["uh", "erm"]
        replacements = { teh = "the" }
        trailing_space = true

        [overlay]
        width = 300
        height = 120
        margin_px = 10
        """,
        encoding="utf-8",
    )
    cfg = Config.load(config_path)
    assert cfg.hotkey == HotkeyConfig(key_code=30)
    assert cfg.audio.sample_rate == 48000
    assert cfg.audio.preroll_ms == 0
    assert cfg.asr.num_threads == 2
    assert cfg.asr.decoding == "greedy_search"
    assert cfg.asr.hotwords_score == 2.0
    assert cfg.text.strip_fillers is False
    assert cfg.text.fillers == ("uh", "erm")
    assert cfg.text.replacements == {"teh": "the"}
    assert cfg.text.trailing_space is True
    assert cfg.overlay == OverlayConfig(width=300, height=120, margin_px=10)


# -- unknown section / key ---------------------------------------------------


def test_unknown_section_raises_config_error() -> None:
    with pytest.raises(ConfigError, match="unknown config section"):
        Config.from_mapping({"nonsense": {}})


def test_unknown_key_raises_config_error_naming_the_section() -> None:
    with pytest.raises(ConfigError, match=r"\[hotkey\]"):
        Config.from_mapping({"hotkey": {"not_a_real_key": 1}})


# -- validation ---------------------------------------------------------------


def test_negative_preroll_ms_is_rejected() -> None:
    with pytest.raises(ConfigError, match="preroll_ms"):
        Config.from_mapping({"audio": {"preroll_ms": -1}})


def test_zero_num_threads_is_rejected() -> None:
    with pytest.raises(ConfigError, match="num_threads"):
        Config.from_mapping({"asr": {"num_threads": 0}})


def test_vocabulary_with_greedy_search_is_rejected() -> None:
    with pytest.raises(ConfigError, match="vocabulary"):
        Config.from_mapping(
            {"asr": {"decoding": "greedy_search", "vocabulary": ["kubectl"]}}
        )


def test_vocabulary_with_modified_beam_search_is_accepted() -> None:
    cfg = Config.from_mapping(
        {"asr": {"decoding": "modified_beam_search", "vocabulary": ["kubectl"]}}
    )
    assert cfg.asr == AsrConfig(decoding="modified_beam_search", vocabulary=("kubectl",))


def test_out_of_range_hotkey_code_is_rejected() -> None:
    with pytest.raises(ConfigError, match="key_code"):
        Config.from_mapping({"hotkey": {"key_code": -1}})


# -- relative model_dir resolution -------------------------------------------


def test_relative_model_dir_resolves_against_config_file_directory(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    project_dir = tmp_path / "project"
    project_dir.mkdir()
    config_path = project_dir / "config.toml"
    config_path.write_text('[asr]\nmodel_dir = "models/foo"\n', encoding="utf-8")

    # Run from a completely different cwd to prove resolution isn't cwd-based.
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    monkeypatch.chdir(elsewhere)

    cfg = Config.load(config_path)
    assert cfg.asr.model_dir == project_dir / "models" / "foo"


def test_absolute_model_dir_is_left_untouched(tmp_path: Path) -> None:
    config_path = tmp_path / "config.toml"
    absolute = tmp_path / "somewhere-else" / "model"
    config_path.write_text(f'[asr]\nmodel_dir = "{absolute.as_posix()}"\n', encoding="utf-8")
    cfg = Config.load(config_path)
    assert cfg.asr.model_dir == absolute


def test_text_config_replacements_default_is_empty() -> None:
    assert TextConfig().replacements == {}
