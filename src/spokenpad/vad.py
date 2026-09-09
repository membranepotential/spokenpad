"""Voice activity detection: split a capture into the speech it actually holds.

This is a **correctness** fix before it is anything else, which is worth
saying plainly because the obvious reading -- "skip the silence, decode less,
go faster" -- is not what happens and not why this module exists.

## The bug it fixes

Parakeet TDT returns an empty string when speech is a small fraction of the
window it is given. Not a short *utterance* -- a short utterance inside a long
quiet capture. Measured on ``eval-samples/handy-1787827474.wav``, varying only
the padding around 0.53 s of speech::

    speech core, no padding   -> 'D home.'
    + 2 s silence each side   -> 'Did the home?'
    + 5 s silence each side   -> ''          <- collapses entirely
    13.8 s speech + 10 s lead -> full text   <- length rescues it

So it is a ratio, not a duration. In use that is the failure where you press
the key, say two words, release, and nothing arrives -- the daemon logs
``decode produced no text`` and everything downstream is working perfectly.
It also explains the preview instability that ``docs/constraints.md`` had
already described from the other end: decoding 4.4 s of a sample returning
``"Okay."`` where 3.3 s of it returned a full sentence is the same collapse,
seen one preview at a time.

Segmenting first removes the condition. Each segment is tight around speech,
so the ratio is never small, and the same clips that decoded to ``''`` come
back with their text.

## What it does not fix

Decode *throughput*. Measured over three real captures (13.8 s / 27.2 s /
37.0 s), whole-buffer decoding runs at 12.3-12.6x real-time and segmented
decoding at 11.0-11.7x -- slightly **worse**, because per-segment overhead
costs about what the skipped silence saves. Anyone reaching for this module
to make decoding faster should stop; it will not.

What it buys is *incrementality*. The first chunk lands after ~1 s however
long the recording is, and the rest streams in behind it. That is the answer
to a long latched passage ending in a wall of silence: the transcript starts
landing immediately and grows, instead of arriving all at once at the end.

## Why chunks are merged, and padded

Both are accuracy, and neither is optional. Decoding every detected run of
speech separately costs about four WER points, because the model gets no
context across a boundary -- measured on the eval samples, 33.7% whole-buffer
against 37.7% per-run. Merging runs until a chunk holds
:attr:`VadConfig.chunk_seconds` of speech recovers all of it (33.4%), and
padding each chunk with :attr:`VadConfig.pad_seconds` of the *real*
surrounding audio was worth another 2-4 points at every chunk size tried,
because Silero's boundaries clip word onsets and endings.

Through ``scripts/eval.py --vad``, which scores the way that harness always
has: **13.4% WER either way**, with one more exercise check passing. Lowering
``chunk_seconds`` for faster first text spends accuracy; re-run that harness
if you do.

## Why this is not the streaming decode the project forbids

``docs/constraints.md`` forbids re-decoding a growing buffer, because that is
what made Handy slow and made it silently drop long utterances. This is a
different shape and keeps both properties that rule exists to protect: every
committed sample is decoded **exactly once** -- when its chunk settles, or at
key release for the open tail. No committed region is ever re-decoded as
more audio arrives, so cost stays linear in the audio and nothing is dropped
at any length.

## Settled chunks

Since 2026-09-08 the daemon commits chunks *while the user is still
speaking* (``docs/progressive-commit.md``), which needs one more fact per
chunk: whether later audio can still change it. :attr:`Segment.settled`
answers that, and the answer rests on Silero being causal. :meth:`split` is
run from the same offset on every tick, and a causal detector fed the same
prefix emits the same spans, so a span that ended by silence on one tick ends
at the same frame on every later one. The only span that can move is the one
``flush()`` cut at the buffer's end because the user was mid-word -- and that
one is always the last. Hence: every chunk before the last is settled; the
last is settled only if it reached the merge target *and* the buffer runs
:data:`SETTLE_SILENCE_SECONDS` past its end, which a flushed span never does.

Failure is a value, never an exception: if the VAD model is missing or will
not load, :func:`load_segmenter` returns ``None`` and the caller decodes the
whole buffer exactly as before. A missing 2 MB optional model must not stop a
daemon whose 630 MB required model is loaded and working.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Final, NamedTuple

from spokenpad.audio import MonoAudio
from spokenpad.config import VadConfig

log = logging.getLogger("spokenpad.vad")

#: Silero consumes fixed-size frames. sherpa-onnx exposes this as
#: ``window_size`` and rejects a mismatch, so feed it exactly this many
#: samples at a time and let the tail be handled by ``flush()``.
_WINDOW: Final = 512

#: Ring capacity for segments the detector has closed but we have not popped.
#: We pop after every frame, so this only has to outlast one frame -- but a
#: segment can be ``max_speech_seconds`` long, and the buffer holds samples,
#: not segments. Two maximal segments is headroom nothing realistic reaches.
_BUFFER_HEADROOM: Final = 2.0

#: Speech a chunk must already hold before its outer edge is widened by
#: ``VadConfig.edge_pad_seconds``. Below this the chunk is small enough that
#: extra silence could dominate it, which is the condition that makes the
#: recogniser return nothing -- so a brief utterance in a quiet capture keeps
#: the tight trim that fixes that bug, and only substantial speech gets the
#: wider margin that protects a first or last word.
_EDGE_MARGIN_MIN_SPEECH_S: Final = 3.0

SETTLE_SILENCE_SECONDS: Final = 1.0
"""Audio that must follow the *last* closed chunk before it counts as settled.

A chunk before the last is settled by construction (see the module
docstring). The last one may have been closed by the span ``flush()`` cut at
the buffer's end, which moves on the next tick; a chunk followed by this much
audio with no new speech was closed by real silence. A flushed span ends
within one frame of the buffer's end, so it can never pass this test; a
span sherpa ended on its own is reported about 0.9 s after the speech stops
(measured on the eval samples: ``min_silence_seconds`` plus the detector's
own latency), so with this margin a sentence the user paused after settles
about two seconds into the pause. That is the cost of never committing a
chunk that could still move.
"""


class _Chunk(NamedTuple):
    """A merged run of spans, in input frames, before padding."""

    start: int
    end: int
    speech: int
    """How many of the frames the detector called speech, as opposed to the
    pauses between them the chunk also spans. Decides whether the chunk is
    substantial enough to earn the wider edge margin."""
    closed: bool
    """Whether the merge target closed it, as opposed to the input running out."""
    edge_lead: bool
    """A long internal silence makes this boundary act like a capture edge."""
    edge_trail: bool


@dataclass(frozen=True, slots=True)
class Segment:
    """One run of speech, tight around its edges.

    ``start_seconds`` is where ``samples`` begins in the capture, kept for
    logging and tests: it is the only way to tell "the VAD found two chunks"
    from "the VAD found the same chunk twice".
    """

    samples: MonoAudio
    start_seconds: float
    end_frame: int
    """Where the speech ends in the input, exclusive, *before* the trailing pad.

    The next remainder starts here, not at the end of ``samples``: the pad
    after the speech belongs to the silence the following chunk pads into
    from its own side, so starting past it would rob that chunk of its lead
    pad, and starting by the padded *length* -- as the preview prefix once
    did -- lands inside this chunk whenever a pause was longer than the lead
    pad, and finds the tail of its speech a second time.
    """
    settled: bool
    """No later audio can change this chunk. See the module docstring."""


class SpeechSegmenter:
    """Splits a captured buffer into its speech segments.

    Holds one resident detector: constructing it loads an ONNX model, and a
    committed decode is not the place to pay for that. Not thread-safe -- the
    detector carries state across frames, so this must be used from the single
    worker thread that owns the recogniser, and :meth:`split` resets that
    state before each capture so one utterance can never leak into the next.
    """

    def __init__(self, config: VadConfig, sample_rate: int) -> None:
        import sherpa_onnx

        model = sherpa_onnx.VadModelConfig()
        model.silero_vad.model = str(config.model)
        model.silero_vad.threshold = config.threshold
        model.silero_vad.min_silence_duration = config.min_silence_seconds
        model.silero_vad.min_speech_duration = config.min_speech_seconds
        model.silero_vad.max_speech_duration = config.max_speech_seconds
        model.sample_rate = sample_rate

        self._sample_rate = sample_rate
        self._chunk_seconds = config.chunk_seconds
        self._pad_seconds = config.pad_seconds
        self._edge_pad_seconds = config.edge_pad_seconds
        self._split_silence_seconds = max(
            SETTLE_SILENCE_SECONDS,
            2 * max(config.edge_pad_seconds, config.pad_seconds),
        )
        self._detector = sherpa_onnx.VoiceActivityDetector(
            model, buffer_size_in_seconds=config.max_speech_seconds * _BUFFER_HEADROOM
        )

    def split(self, samples: MonoAudio) -> list[Segment]:
        """``samples`` as chunks of speech, in order, with the silence dropped.

        Detected runs of speech are **merged** up to
        :attr:`VadConfig.chunk_seconds` before being returned, and each chunk
        is padded by :attr:`VadConfig.pad_seconds` of the real surrounding
        audio. Both matter, and both were measured (see the class docstring
        and ``docs/asr.md``): every segment decoded separately cost about four
        WER points against the whole buffer, merging to 10 s recovered all of
        it, and 0.5 s of padding was worth another 2-4 on top because Silero's
        boundaries clip word onsets and endings.

        Returns a single chunk covering the whole buffer if the detector finds
        no speech at all. That is deliberate: "the VAD heard nothing" is not
        the same claim as "there is nothing to hear", and the cost of being
        wrong is asymmetric -- decoding a silent buffer wastes a few hundred
        milliseconds and logs an empty result, while discarding a real
        utterance loses something the user cannot get back by repeating
        themselves, because they have already stopped speaking.
        """
        spans = self._speech_spans(samples)
        if not spans:
            log.debug(
                "VAD found no speech in %.1fs; decoding the whole buffer",
                samples.size / self._sample_rate,
            )
            return [
                Segment(samples=samples, start_seconds=0.0, end_frame=samples.size, settled=False)
            ]

        pad = int(self._pad_seconds * self._sample_rate)
        edge = int(self._edge_pad_seconds * self._sample_rate)
        settle_after = int(SETTLE_SILENCE_SECONDS * self._sample_rate)
        merged = self._merged(spans)
        last = len(merged) - 1
        chunks = []
        for index, chunk in enumerate(merged):
            # A wider margin at the very start and end of the capture, where
            # a clipped word is the first or last word of what was said.
            wide = chunk.speech >= _EDGE_MARGIN_MIN_SPEECH_S * self._sample_rate
            lead = edge if wide and (index == 0 or chunk.edge_lead) else pad
            trail = edge if wide and (index == last or chunk.edge_trail) else pad
            # `start_seconds` describes the samples actually returned, padding
            # included, so it always locates them in the capture rather than
            # pointing a little after where they begin.
            padded_start = max(0, chunk.start - lead)
            chunks.append(
                Segment(
                    samples=samples[padded_start : min(samples.size, chunk.end + trail)],
                    start_seconds=padded_start / self._sample_rate,
                    end_frame=chunk.end,
                    settled=chunk.closed
                    and (index < last or samples.size - chunk.end >= settle_after),
                )
            )
        return chunks

    def _merged(self, spans: list[tuple[int, int]]) -> list[_Chunk]:
        """Group spans into chunks holding enough speech.

        Merging is by *speech* accumulated, not elapsed time, so a passage
        with long thinking pauses in it still produces chunks the recogniser
        has enough context to work with rather than one chunk per phrase.
        """
        target = self._chunk_seconds * self._sample_rate
        split_after = self._split_silence_seconds * self._sample_rate
        chunks: list[_Chunk] = []
        start: int | None = None
        end: int | None = None
        speech = 0
        edge_lead = False
        for span_start, span_end in spans:
            previous_end = end if end is not None else chunks[-1].end if chunks else None
            if previous_end is not None and span_start - previous_end >= split_after:
                if start is not None and end is not None:
                    chunks.append(_Chunk(start, end, speech, True, edge_lead, False))
                chunks[-1] = chunks[-1]._replace(edge_trail=True)
                start, end, speech = None, None, 0
                edge_lead = True
            if start is None:
                start = span_start
            speech += span_end - span_start
            end = span_end
            if speech >= target:
                chunks.append(_Chunk(start, span_end, speech, True, edge_lead, False))
                start, end, speech = None, None, 0
                edge_lead = False
        if start is not None and end is not None:
            chunks.append(_Chunk(start, end, speech, False, edge_lead, False))
        return chunks

    def _speech_spans(self, samples: MonoAudio) -> list[tuple[int, int]]:
        """Raw ``(start, end)`` sample offsets of every run of speech."""
        self._detector.reset()
        spans: list[tuple[int, int]] = []

        for start in range(0, samples.size - _WINDOW + 1, _WINDOW):
            self._detector.accept_waveform(samples[start : start + _WINDOW])
            self._drain(spans)
        # The final partial frame is never fed -- sherpa-onnx rejects a short
        # one -- so flush() closes whatever run is still open, including the
        # common case of releasing the key mid-word.
        self._detector.flush()
        self._drain(spans)
        return spans

    def _drain(self, into: list[tuple[int, int]]) -> None:
        while not self._detector.empty():
            front = self._detector.front
            start = int(front.start)
            into.append((start, start + len(front.samples)))
            self._detector.pop()


def load_segmenter(config: VadConfig, sample_rate: int) -> SpeechSegmenter | None:
    """Build a segmenter, or ``None`` with a log line saying why not.

    Never raises. The caller's fallback -- decode the whole buffer -- is the
    behaviour that shipped before this module existed, so an unavailable VAD
    is a lost improvement rather than a broken daemon.
    """
    if not config.enabled:
        log.info("voice activity detection is disabled; decoding whole captures")
        return None
    if not config.model.exists():
        log.warning(
            "no VAD model at %s, so short utterances buried in silence will "
            "decode to nothing -- run `uv run scripts/fetch_model.py` to fetch it",
            config.model,
        )
        return None
    try:
        segmenter = SpeechSegmenter(config, sample_rate)
    except Exception as e:
        log.warning("could not load the VAD model at %s: %s", config.model, e)
        return None
    log.info("voice activity detection ready (%s)", config.model)
    return segmenter
