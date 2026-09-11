"""The decode pipeline: split at silence, decode each chunk exactly once.

This is the offline Python reference for the Rust daemon and recovery command.
Both implementations use the same segmentation rules, model, silence rule and
whole-buffer recovery exception; ``scripts/verify_rust.py`` checks that they do
not drift.

Everything policy-shaped stays with the caller: this function decodes and
reports, it does not decide what to do with the text. The Rust implementation
has the same boundary; this Python reference lets offline callers collect each
chunk and the final joined string for differential checks.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Protocol

from spokenpad.asr import TranscriptionResult
from spokenpad.audio import MonoAudio
from spokenpad.vad import Segment

log = logging.getLogger("spokenpad.decode")


class Transcriber(Protocol):
    """The one operation the pure decode pipeline needs from an ASR model."""

    def transcribe(self, samples: MonoAudio, sample_rate: int) -> TranscriptionResult: ...


class Segmenter(Protocol):
    """The segmentation operation used by the pure decode pipeline."""

    def split(self, samples: MonoAudio) -> list[Segment]: ...


def _keep_going() -> bool:
    return False


def _ignore(text: str) -> None:
    del text


def decode_capture(
    samples: MonoAudio,
    *,
    transcriber: Transcriber,
    segmenter: Segmenter | None,
    sample_rate: int,
    on_segment: Callable[[str], None] = _ignore,
    abandoned: Callable[[], bool] = _keep_going,
) -> str:
    """Decode ``samples`` chunk by chunk, calling ``on_segment`` as each lands.

    Every sample is decoded exactly once, in one pass, over audio that is
    already complete -- no region is re-decoded as more arrives, which is the
    property ``docs/constraints.md`` protects. Live, ``samples`` is the
    remainder past what was committed while the user was still speaking
    (``docs/progressive-commit.md``); from a wav it is the whole recording.
    Splitting first is a correctness fix (see :mod:`spokenpad.vad`) that
    happens to also let text start appearing in a few hundred milliseconds
    instead of at the end.

    ``abandoned`` is checked between chunks, so a cancelled decode stops
    adding text rather than running to the end of a passage the user has
    already walked away from.

    A segmenter that reports no speech means there is nothing to decode, and
    the recogniser is not called at all -- not even for the whole-buffer
    recovery exception below, which exists for chunks that decoded to nothing,
    not for audio nobody claimed was speech.

    Raises whatever the recogniser raises; the caller decides whether that is
    a log line or an exit code.
    """
    segments = (
        segmenter.split(samples)
        if segmenter is not None
        else [Segment(samples=samples, start_seconds=0.0, end_frame=samples.size, settled=False)]
    )
    # Silence is not decoded. Only a loaded VAD can return nothing here -- the
    # segmenter-less branch above always yields one whole-buffer chunk -- so
    # nothing that knows less than the detector suppresses a capture. Asked to
    # transcribe silence the model invents speech: a 0.5s near-silent press
    # decoded to "Thank you." (see :mod:`spokenpad.vad`).
    if not segments:
        log.debug(
            "VAD found no speech in %.1fs; nothing to decode",
            samples.size / sample_rate,
        )
        return ""
    texts: list[str] = []
    cancelled = False
    for index, segment in enumerate(segments):
        if abandoned():
            log.info("decode cancelled after %d of %d chunks", index, len(segments))
            cancelled = True
            break
        text = transcriber.transcribe(segment.samples, sample_rate).text
        # Every chunk, with where it came from, at DEBUG. Words going missing
        # from the front of a dictation is the one report that cannot be
        # diagnosed after the fact without this: it says whether the first
        # chunk covered the start of the capture and what it decoded to, which
        # separates "the audio was not there" from "the recogniser returned
        # nothing for it".
        log.debug(
            "chunk %d/%d at %.1fs, %.1fs long -> %d chars: %r",
            index + 1,
            len(segments),
            segment.start_seconds,
            segment.samples.size / sample_rate,
            len(text),
            text[:60],
        )
        if not text.strip():
            continue
        texts.append(text)
        on_segment(text)

    # Chunking must never lose a whole utterance. It has: a 2.8s capture at
    # peak 0.28 -- unmistakably speech -- came back empty from every chunk, and
    # the words were simply gone. Whatever the detector did there, the audio is
    # still in hand, so decode it the old way rather than report nothing.
    #
    # Costs one extra decode, and only in a case that was already a total
    # failure. The floor this buys is worth stating plainly: segmentation can
    # now never do worse than not segmenting.
    if not texts and not cancelled and len(segments) > 1:
        log.warning(
            "all %d chunks decoded to nothing; retrying the whole %.1fs buffer",
            len(segments),
            samples.size / sample_rate,
        )
        text = transcriber.transcribe(samples, sample_rate).text
        if text.strip():
            log.info("the whole-buffer retry recovered the utterance")
            texts.append(text)
            on_segment(text)

    return " ".join(texts)
