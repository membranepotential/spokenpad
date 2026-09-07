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

What it buys is *incrementality*. Time to the first text on those same clips
drops from 2.2-3.4 s to 0.27-0.49 s, and the rest streams in behind it. That
is the answer to a long latched recording ending in a wall of silence: the
transcript starts landing almost immediately and grows, instead of arriving
all at once several seconds after the key comes up.

## Why this is not the streaming decode the project forbids

``docs/constraints.md`` forbids re-decoding a growing buffer, because that is
what made Handy slow and made it silently drop long utterances. This is a
different shape and keeps both properties that rule exists to protect: every
sample is decoded **exactly once**, in one pass, after the key comes up. No
region is ever re-decoded as more audio arrives, so cost stays linear in the
audio and nothing is dropped at any length.

Failure is a value, never an exception: if the VAD model is missing or will
not load, :func:`load_segmenter` returns ``None`` and the caller decodes the
whole buffer exactly as before. A missing 2 MB optional model must not stop a
daemon whose 630 MB required model is loaded and working.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Final

import numpy as np

from voice_kb.audio import MonoAudio
from voice_kb.config import VadConfig

log = logging.getLogger(__name__)

#: Silero consumes fixed-size frames. sherpa-onnx exposes this as
#: ``window_size`` and rejects a mismatch, so feed it exactly this many
#: samples at a time and let the tail be handled by ``flush()``.
_WINDOW: Final = 512

#: Ring capacity for segments the detector has closed but we have not popped.
#: We pop after every frame, so this only has to outlast one frame -- but a
#: segment can be ``max_speech_seconds`` long, and the buffer holds samples,
#: not segments. Two maximal segments is headroom nothing realistic reaches.
_BUFFER_HEADROOM: Final = 2.0


@dataclass(frozen=True, slots=True)
class Segment:
    """One run of speech, tight around its edges.

    ``start_seconds`` is the offset into the capture, kept for logging and
    tests: it is the only way to tell "the VAD found two segments" from "the
    VAD found the same segment twice".
    """

    samples: MonoAudio
    start_seconds: float


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
        self._detector = sherpa_onnx.VoiceActivityDetector(
            model, buffer_size_in_seconds=config.max_speech_seconds * _BUFFER_HEADROOM
        )

    def split(self, samples: MonoAudio) -> list[Segment]:
        """Speech segments in ``samples``, in order.

        Returns a single segment covering the whole buffer if the detector
        finds no speech at all. That is deliberate: "the VAD heard nothing" is
        not the same claim as "there is nothing to hear", and the cost of
        being wrong is asymmetric -- decoding a silent buffer wastes a few
        hundred milliseconds and logs an empty result, while discarding a real
        utterance loses something the user cannot get back by repeating
        themselves, because they have already stopped speaking.
        """
        self._detector.reset()
        segments: list[Segment] = []

        for start in range(0, samples.size - _WINDOW + 1, _WINDOW):
            self._detector.accept_waveform(samples[start : start + _WINDOW])
            self._drain(segments)
        # The final partial frame is never fed -- sherpa-onnx rejects a short
        # one -- so flush() closes whatever segment is still open, including
        # the common case of releasing the key mid-word.
        self._detector.flush()
        self._drain(segments)

        if not segments:
            log.debug(
                "VAD found no speech in %.1fs; decoding the whole buffer",
                samples.size / self._sample_rate,
            )
            return [Segment(samples=samples, start_seconds=0.0)]
        return segments

    def _drain(self, into: list[Segment]) -> None:
        while not self._detector.empty():
            front = self._detector.front
            into.append(
                Segment(
                    samples=np.asarray(front.samples, dtype=np.float32),
                    start_seconds=front.start / self._sample_rate,
                )
            )
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
