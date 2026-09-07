"""Config parsing/validation: defaults, TOML round-trip, and every error path."""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from voice_kb.config import (
    AsrConfig,
    Config,
    ConfigError,
    HotkeyConfig,
    NvimConfig,
    OverlayConfig,
    PreviewConfig,
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
        preview_height = 40

        [preview]
        enabled = false
        interval_ms = 500
        max_seconds = 6.5
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
    assert cfg.overlay == OverlayConfig(width=300, height=120, margin_px=10, preview_height=40)
    assert cfg.preview == PreviewConfig(enabled=False, interval_ms=500, max_seconds=6.5)


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


def test_too_frequent_preview_interval_is_rejected() -> None:
    with pytest.raises(ConfigError, match="interval_ms"):
        Config.from_mapping({"preview": {"interval_ms": 199}})


def test_non_positive_preview_window_is_rejected() -> None:
    with pytest.raises(ConfigError, match="max_seconds"):
        Config.from_mapping({"preview": {"max_seconds": 0}})


def test_negative_preview_height_is_rejected() -> None:
    with pytest.raises(ConfigError, match="preview_height"):
        Config.from_mapping({"overlay": {"preview_height": -1}})


def test_total_height_includes_the_preview_band_only_when_it_is_enabled() -> None:
    """The overlay widget and its placement both size off ``total_height``, so
    this is the single place the preview band's existence is expressed."""
    cfg = OverlayConfig(height=96, preview_height=64)

    assert cfg.total_height(preview_band=True) == 160
    assert cfg.total_height(preview_band=False) == 96


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


# -- NvimConfig validation -----------------------------------------------------


def test_nvim_window_instance_rejects_a_name_with_a_quote() -> None:
    with pytest.raises(ConfigError, match="window_instance"):
        NvimConfig(window_instance='voice"kb')


def test_nvim_window_instance_rejects_a_name_with_a_bracket() -> None:
    with pytest.raises(ConfigError, match="window_instance"):
        NvimConfig(window_instance="voice[kb]")


def test_nvim_empty_editor_is_rejected() -> None:
    with pytest.raises(ConfigError, match="editor"):
        NvimConfig(editor=())


def test_nvim_file_template_rendering_with_a_slash_is_rejected() -> None:
    """A template that renders with a path separator would make the dictation
    file land in a directory named for it rather than under ``dictation_dir``
    -- or escape it entirely with a leading ``/``."""
    with pytest.raises(ConfigError, match="file_template"):
        NvimConfig(file_template="%Y/%m/%d.md")


def test_nvim_non_positive_startup_timeout_is_rejected() -> None:
    with pytest.raises(ConfigError, match="startup_timeout_s"):
        NvimConfig(startup_timeout_s=0)


def test_nvim_window_fraction_out_of_range_is_rejected() -> None:
    with pytest.raises(ConfigError, match="window_fraction"):
        NvimConfig(window_fraction=0.0)
    with pytest.raises(ConfigError, match="window_fraction"):
        NvimConfig(window_fraction=1.5)


def test_nvim_window_fraction_of_one_is_accepted() -> None:
    assert NvimConfig(window_fraction=1.0).window_fraction == 1.0


def test_nvim_spawn_argv_substitutes_instance_and_appends_listen_and_target() -> None:
    cfg = NvimConfig(
        terminal=("alacritty", "--class", "Floating,{instance}", "-e"),
        editor=("nvim",),
        window_instance="voice-kb-test",
        init=Path("/tmp/x-init.lua"),
    )

    argv = cfg.spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=(10, 20))

    assert argv == [
        "alacritty",
        "--class",
        "Floating,voice-kb-test",
        "-e",
        "nvim",
        "-u",
        "/tmp/x-init.lua",
        "--listen",
        "/tmp/x.sock",
        "/tmp/x.md",
    ]


def test_latch_modifier_defaults_to_shift_and_covers_both_sides() -> None:
    """Both left and right, so it does not matter which hand is on the key."""
    assert HotkeyConfig().latch_key_codes == frozenset({42, 54})  # KEY_*SHIFT


def test_latching_can_be_turned_off() -> None:
    assert HotkeyConfig(latch_modifier=None).latch_key_codes == frozenset()


def test_unknown_latch_modifier_is_rejected() -> None:
    with pytest.raises(ConfigError, match="latch_modifier"):
        Config.from_mapping({"hotkey": {"latch_modifier": "super"}})


def test_latch_modifier_reads_from_toml() -> None:
    config = Config.from_mapping({"hotkey": {"latch_modifier": "ctrl"}})
    assert config.hotkey.latch_key_codes == frozenset({29, 97})  # KEY_*CTRL


# ------------------------------------------------------------------------- vad


def test_vad_defaults_to_on_because_the_default_is_load_bearing() -> None:
    """Off means short utterances buried in silence decode to nothing -- see
    ``voice_kb.vad``. Anyone changing this default should read that first."""
    assert Config().vad.enabled is True


@pytest.mark.parametrize(
    ("section", "expected"),
    [
        ({"threshold": 1.5}, "vad.threshold"),
        ({"threshold": 0.0}, "vad.threshold"),
        ({"min_speech_seconds": 0}, "vad.min_speech_seconds"),
        ({"min_silence_seconds": -1}, "vad.min_silence_seconds"),
        ({"max_speech_seconds": 0.1}, "vad.max_speech_seconds"),
    ],
)
def test_nonsensical_vad_settings_are_rejected_at_the_boundary(
    section: dict[str, float], expected: str
) -> None:
    with pytest.raises(ConfigError, match=re.escape(expected)):
        Config.from_mapping({"vad": section})


def test_a_relative_vad_model_resolves_against_the_config_file(tmp_path: Path) -> None:
    """Same rule as ``asr.model_dir``: a config file is read from wherever the
    daemon happened to be started, and a path in it should mean what it looks
    like it means."""
    config_file = tmp_path / "config.toml"
    config_file.write_text('[vad]\nmodel = "models/silero_vad.onnx"\n', encoding="utf-8")

    assert Config.load(config_file).vad.model == tmp_path / "models" / "silero_vad.onnx"


def test_the_dictation_window_uses_the_bundled_nvim_config_by_default() -> None:
    """The repo owns the window's behaviour: no config option set means the
    file shipped inside the package, not whatever is in the user's dotfiles."""
    init = NvimConfig().init_path

    assert init.name == "dictation_init.lua"
    assert init.exists(), "the bundled config must ship with the package"


def test_pointing_init_somewhere_else_wins() -> None:
    assert NvimConfig(init=Path("/tmp/mine.lua")).init_path == Path("/tmp/mine.lua")


def test_spawn_argv_tells_the_terminal_where_to_open() -> None:
    """The window's first frame should already be in the right place. Placing
    it afterwards made it appear wherever the window manager chose -- often
    the middle of the other monitor -- and then fly across the screen."""
    argv = NvimConfig().spawn_argv(
        socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=(1234, 567)
    )

    assert "window.position.x=1234" in argv
    assert "window.position.y=567" in argv


def test_spawn_argv_without_a_known_position_still_produces_one_command_line() -> None:
    """No usable screen geometry substitutes 0 rather than dropping the flags:
    that is no worse than the window manager's own choice, and it keeps the
    command line one shape instead of two."""
    argv = NvimConfig().spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None)

    assert "window.position.x=0" in argv
    assert "{x}" not in " ".join(argv)


def test_a_terminal_that_names_the_window_is_one_voice_kb_can_wait_for() -> None:
    assert NvimConfig().announces_instance is True


def test_spawning_the_editor_directly_announces_no_instance() -> None:
    """Nothing communicates the instance name, so there is no window for the
    manager rules to match and none for voice-kb to wait for."""
    assert NvimConfig(terminal=()).announces_instance is False
