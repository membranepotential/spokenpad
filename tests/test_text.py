"""Pure post-processing: filler stripping, exact-match replacements, and
their composition in ``postprocess``."""

from __future__ import annotations

from voice_kb.config import TextConfig
from voice_kb.text import apply_replacements, postprocess, strip_fillers

DEFAULT_FILLERS = TextConfig().fillers


# -- filler stripping ---------------------------------------------------------


def test_strips_standalone_filler_mid_sentence() -> None:
    assert strip_fillers("I um think this works", DEFAULT_FILLERS) == "I think this works"


def test_does_not_touch_filler_as_substring() -> None:
    assert strip_fillers("umbrella is not um a filler", DEFAULT_FILLERS) == (
        "umbrella is not a filler"
    )


def test_hmm_does_not_touch_longer_run_of_ms() -> None:
    # "hmmm" is treated as a distinct (more emphatic) word, not the same
    # token as "hmm" with extra characters -- there's no word boundary
    # between the fourth and third "m", so it's left alone.
    assert strip_fillers("hmm well hmmm not touched", DEFAULT_FILLERS) == (
        "well hmmm not touched"
    )


def test_filler_stripping_is_case_insensitive() -> None:
    assert strip_fillers("UM shout case", DEFAULT_FILLERS) == "shout case"
    assert strip_fillers("Uh well then", DEFAULT_FILLERS) == "well then"


def test_strips_filler_with_trailing_comma_without_stray_punctuation() -> None:
    assert strip_fillers("Hello, um, how are you?", DEFAULT_FILLERS) == "Hello, how are you?"


def test_strips_filler_before_terminal_punctuation() -> None:
    assert strip_fillers("I think, uh.", DEFAULT_FILLERS) == "I think."


def test_strips_consecutive_fillers() -> None:
    assert strip_fillers("erm erm double filler", DEFAULT_FILLERS) == "double filler"


def test_no_fillers_configured_is_a_noop() -> None:
    assert strip_fillers("um this stays", fillers=()) == "um this stays"


def test_empty_string_is_a_noop() -> None:
    assert strip_fillers("", DEFAULT_FILLERS) == ""


# -- exact-match replacements: the anti-fuzzy regression ----------------------


def test_replacements_are_exact_whole_word_not_fuzzy() -> None:
    """The tool this project replaced used edit-distance matching and turned
    "set" into "sed" and "reset" into "rust". A replacement map containing
    "sed" must never touch the word "set"."""
    replacements = {"sed": "stream editor"}
    assert apply_replacements("please set the value", replacements) == "please set the value"


def test_replacements_apply_to_exact_word() -> None:
    replacements = {"sed": "stream editor"}
    assert apply_replacements("run sed on this", replacements) == "run stream editor on this"


def test_replacements_are_case_sensitive() -> None:
    replacements = {"sed": "stream editor"}
    assert apply_replacements("Sed is different", replacements) == "Sed is different"


def test_replacements_do_not_touch_substrings() -> None:
    replacements = {"cat": "feline"}
    assert apply_replacements("concatenate cat catalog", replacements) == (
        "concatenate feline catalog"
    )


def test_no_replacements_configured_is_a_noop() -> None:
    assert apply_replacements("unchanged text", {}) == "unchanged text"


# -- composition ----------------------------------------------------------------


def test_postprocess_composes_filler_stripping_and_replacements() -> None:
    cfg = TextConfig(replacements={"sed": "stream editor"})
    assert postprocess("um please run sed here", cfg) == "please run stream editor here"


def test_postprocess_respects_strip_fillers_false() -> None:
    cfg = TextConfig(strip_fillers=False)
    assert postprocess("um this stays", cfg) == "um this stays"


def test_postprocess_trailing_space_appends_single_space() -> None:
    cfg = TextConfig(trailing_space=True)
    assert postprocess("hello", cfg) == "hello "


def test_postprocess_trailing_space_skipped_for_empty_result() -> None:
    cfg = TextConfig(trailing_space=True, fillers=("hello",))
    assert postprocess("hello", cfg) == ""


def test_postprocess_default_config_is_close_to_identity() -> None:
    cfg = TextConfig()
    assert postprocess("please set the value", cfg) == "please set the value"
