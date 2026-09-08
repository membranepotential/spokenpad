"""The decode pipeline: split at silence, decode each chunk exactly once.

This is the functional core the daemon's worker thread runs and that
``voice-kb transcribe`` runs over a recovered wav. It lives on its own rather
than inside :mod:`voice_kb.app` so that recovering a lost transcript from disk
goes through *the same* pipeline as live dictation -- same segmentation, same
model, same whole-buffer fallback. A second, parallel implementation of this
would drift, and the day it is needed is the day nobody is in a position to
notice that it has.

Everything policy-shaped stays with the caller: this function decodes and
reports, it does not decide what to do with the text. The daemon appends each
chunk to nvim as it lands; the CLI collects them. Both get the same string
back at the end.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from voice_kb.asr import Transcriber
from voice_kb.audio import MonoAudio
from voice_kb.vad import Segment, SpeechSegmenter

log = logging.getLogger("voice-kb.decode")


def _keep_going() -> bool:
    return False


def _ignore(text: str) -> None:
    del text


def decode_capture(
    samples: MonoAudio,
    *,
    transcriber: Transcriber,
    segmenter: SpeechSegmenter | None,
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
    Splitting first is a correctness fix (see :mod:`voice_kb.vad`) that
    happens to also let text start appearing in a few hundred milliseconds
    instead of at the end.

    ``abandoned`` is checked between chunks, so a cancelled decode stops
    adding text rather than running to the end of a passage the user has
    already walked away from.

    Raises whatever the recogniser raises; the caller decides whether that is
    a log line or an exit code.
    """
    segments = (
        segmenter.split(samples)
        if segmenter is not None
        else [Segment(samples=samples, start_seconds=0.0, end_frame=samples.size, settled=False)]
    )
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
    if not texts and not cancelled and len(segments) != 1:
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
