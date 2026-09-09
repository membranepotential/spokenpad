"""Post-processing applied to raw ASR output.

Everything here is a pure function of its inputs: no I/O, no model, no
clipboard. :func:`postprocess` composes the individual passes in the order
they should run -- filler stripping (which can leave behind doubled spaces
and stranded commas, so a whitespace/punctuation cleanup follows it),
exact-match replacements, then the optional trailing space.
"""

from __future__ import annotations

import re
from collections.abc import Mapping

from spokenpad.config import TextConfig

_WHITESPACE_RUN = re.compile(r"[ \t]+")
_SPACE_BEFORE_PUNCT = re.compile(r"[ \t]+([,.!?;:])")
_COMMA_BEFORE_TERMINATOR = re.compile(r",(?=[.!?;:])")
_LEADING_JUNK = re.compile(r"^[ ,]+")


def _filler_pattern(fillers: tuple[str, ...]) -> re.Pattern[str]:
    """Match a filler as a whole word, plus a trailing comma and the
    whitespace that separated it from its neighbours.

    Case-insensitive, so ``Um``/``UM`` at a sentence start are caught too.
    ``\\b`` on both sides is what keeps this from touching substrings:
    ``um`` never matches inside ``umbrella``, and ``hmm`` never matches the
    first three letters of ``hmmm`` -- there is no word boundary between the
    two ``m``s, so a longer run of the same interjection is left alone
    rather than treated as the same word with extra emphasis.
    """
    alternatives = "|".join(re.escape(f) for f in fillers)
    return re.compile(rf"\s*\b(?:{alternatives})\b,?\s*", re.IGNORECASE)


def strip_fillers(text: str, fillers: tuple[str, ...]) -> str:
    """Remove standalone filler words and tidy up what removal leaves behind."""
    if not fillers or not text:
        return text
    without_fillers = _filler_pattern(fillers).sub(" ", text)
    return _clean_punctuation(without_fillers)


def _clean_punctuation(text: str) -> str:
    """Collapse doubled spaces and stranded commas left by word removal."""
    text = _WHITESPACE_RUN.sub(" ", text)
    text = _SPACE_BEFORE_PUNCT.sub(r"\1", text)
    text = _COMMA_BEFORE_TERMINATOR.sub("", text)
    text = _LEADING_JUNK.sub("", text)
    return text.strip()


def apply_replacements(text: str, replacements: Mapping[str, str]) -> str:
    """Exact, whole-word, case-sensitive substitution.

    Deliberately not fuzzy and not case-folded: a replacement map containing
    ``sed`` must never touch the word ``set``. Matching whole words with
    exact case is what keeps that guarantee -- there is no edit-distance or
    normalisation step here that could blur the two apart.
    """
    if not replacements or not text:
        return text
    alternatives = "|".join(re.escape(k) for k in replacements)
    pattern = re.compile(rf"\b(?:{alternatives})\b")
    return pattern.sub(lambda m: replacements[m.group(0)], text)


def postprocess(text: str, cfg: TextConfig) -> str:
    """Apply the full post-processing pipeline described by ``cfg``."""
    result = text
    if cfg.strip_fillers:
        result = strip_fillers(result, cfg.fillers)
    result = apply_replacements(result, cfg.replacements)
    if cfg.trailing_space and result:
        result += " "
    return result
