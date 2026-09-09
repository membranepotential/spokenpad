"""Config parsing/validation: defaults, TOML round-trip, and every error path."""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from spokenpad.config import (
    AsrConfig,
    Config,
    ConfigError,
    HotkeyConfig,
    NvimConfig,
    PreviewConfig,
    RecordingConfig,
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

        [preview]
        enabled = false
        interval_ms = 500
        max_seconds = 6.5

        [recording]
        enabled = false
        dir = "~/captures"
        max_total_bytes = 1024
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
    assert cfg.preview == PreviewConfig(enabled=False, interval_ms=500, max_seconds=6.5)
    assert cfg.recording == RecordingConfig(
        enabled=False, dir=Path.home() / "captures", max_total_bytes=1024
    )


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
        window_instance="spokenpad-test",
        init=Path("/tmp/x-init.lua"),
    )

    argv = cfg.spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=(10, 20))

    assert argv == [
        "alacritty",
        "--class",
        "Floating,spokenpad-test",
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
    ``spokenpad.vad``. Anyone changing this default should read that first."""
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


def test_the_window_runs_the_users_own_nvim_config_by_default() -> None:
    """No ``-u`` at all, so nvim resolves its configuration the way it always
    does. The window *is* the user's editor -- their colourscheme, their
    keybindings, their yank flash. A bundled config was the default first and
    the verdict from use was that it never looked like their neovim."""
    argv = NvimConfig().spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None)

    assert "-u" not in argv


def test_the_bundled_config_is_available_for_a_machine_without_one() -> None:
    bundled = NvimConfig().bundled_init
    assert bundled.name == "dictation_init.lua"
    assert bundled.exists(), "the fallback config must ship with the package"

    argv = NvimConfig(init=bundled).spawn_argv(
        socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None
    )
    assert argv[argv.index("-u") + 1] == str(bundled)


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


def test_a_terminal_that_names_the_window_is_one_spokenpad_can_wait_for() -> None:
    assert NvimConfig().announces_instance is True


def test_spawning_the_editor_directly_announces_no_instance() -> None:
    """Nothing communicates the instance name, so there is no window for the
    manager rules to match and none for spokenpad to wait for."""
    assert NvimConfig(terminal=()).announces_instance is False


def test_bundled_is_a_named_value_not_a_path() -> None:
    """A user config should not have to name a checkout directory that may
    move -- the file lives inside the installed package. nvim's own ``-u NONE``
    is the same idea."""
    cfg = Config.from_mapping({"nvim": {"init": "bundled"}}).nvim

    assert cfg.resolved_init == cfg.bundled_init
    assert cfg.resolved_init.exists()


def test_resolving_init_when_it_is_unset_is_a_bug_not_a_default() -> None:
    """Unset means "pass no -u at all", which is a different thing from "use
    some default file". Silently returning one would hide the distinction."""
    with pytest.raises(ConfigError, match=re.escape("nvim.init is unset")):
        _ = NvimConfig().resolved_init


def test_a_colorscheme_is_applied_in_a_way_that_works_under_either_config() -> None:
    """The bundled config defines the loader that finds the scheme among
    installed plugins; a user's own config already has its theme loaded. One
    command has to cover both, and must never leave a message waiting for a
    keypress in a window that cannot take focus."""
    argv = NvimConfig(colorscheme="tokyonight-moon").spawn_argv(
        socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None
    )

    command = argv[argv.index("-c") + 1]
    assert "SpokenpadColorscheme" in command
    assert "pcall" in command
    assert "tokyonight-moon" in command


def test_no_colorscheme_adds_no_command() -> None:
    argv = NvimConfig().spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None)

    assert "-c" not in argv


def test_the_window_is_transparent_by_default() -> None:
    """A theme loaded from its plugin directory is the theme's defaults, not
    the theme as its owner configured it. Someone running it with
    ``transparent = true`` would otherwise get an opaque block of the theme's
    own background inside their terminal's border."""
    command = _colorscheme_argument(NvimConfig(colorscheme="tokyonight-moon"))

    assert command.endswith("false) else pcall(vim.cmd.colorscheme, 'tokyonight-moon') end")


def test_opting_out_of_transparency_is_passed_through() -> None:
    command = _colorscheme_argument(NvimConfig(colorscheme="tokyonight-moon", transparent=False))

    assert "SpokenpadColorscheme('tokyonight-moon', true)" in command


def _colorscheme_argument(cfg: NvimConfig) -> str:
    argv = cfg.spawn_argv(socket=Path("/tmp/x.sock"), target=Path("/tmp/x.md"), at=None)
    return argv[argv.index("-c") + 1]


# -- recording ---------------------------------------------------------------


def test_recording_is_on_by_default_and_lives_in_the_state_directory() -> None:
    """A safety net that has to be switched on is switched off exactly when it
    turns out to have been needed -- which is what happened on 2026-09-08."""
    cfg = RecordingConfig()
    assert cfg.enabled is True
    assert cfg.dir.parts[-2:] == ("spokenpad", "audio")
    assert cfg.max_total_bytes == 5 * 1024**3


def test_non_positive_recording_budget_is_rejected() -> None:
    """Zero would mean pruning every capture the moment it is written, which
    is indistinguishable from recording being off but looks like it is on."""
    with pytest.raises(ConfigError, match="max_total_bytes"):
        Config.from_mapping({"recording": {"max_total_bytes": 0}})


def test_recording_dir_expands_a_variable_and_a_tilde(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SPOKENPAD_TEST_STATE", "/var/tmp/state")
    cfg = Config.from_mapping({"recording": {"dir": "$SPOKENPAD_TEST_STATE/audio"}})
    assert cfg.recording.dir == Path("/var/tmp/state/audio")


def test_recording_dir_referencing_an_unset_variable_is_rejected(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Left as-is, ``expandvars`` would create a directory literally named
    ``$SPOKENPAD_UNSET`` in the user's home."""
    monkeypatch.delenv("SPOKENPAD_UNSET", raising=False)
    with pytest.raises(ConfigError, match=re.escape("recording.dir")):
        Config.from_mapping({"recording": {"dir": "$SPOKENPAD_UNSET/audio"}})


def test_unknown_recording_key_is_rejected() -> None:
    with pytest.raises(ConfigError, match=r"\[recording\]"):
        Config.from_mapping({"recording": {"max_bytes": 1}})
