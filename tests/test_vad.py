"""Voice activity detection, against the real Silero model where it exists.

The interesting assertions here are not "the VAD finds speech" -- they are the
two ways it is allowed to be wrong. It must never hand back nothing, and it
must never carry state from one capture into the next.

The reproduction of the bug this module exists for lives in
``test_short_utterance_in_silence`` and needs the *recogniser* too, so it is
marked and skipped unless both models are present.
"""

from __future__ import annotations

import wave
from pathlib import Path

import numpy as np
import pytest

from voice_kb.audio import MonoAudio
from voice_kb.config import Config, VadConfig
from voice_kb.vad import SpeechSegmenter, load_segmenter

RATE = 16000
SAMPLE = Path("eval-samples/handy-1787827474.wav")
#: A sample with enough continuous speech to earn the wide edge margin; the
#: short one above is deliberately brief and would not.
LONG_SAMPLE = Path("eval-samples/handy-1787827701.wav")


@pytest.fixture(scope="module")
def segmenter() -> SpeechSegmenter:
    config = Config().vad
    if not config.model.exists():
        pytest.skip(f"no VAD model at {config.model}; run scripts/fetch_model.py")
    built = load_segmenter(config, RATE)
    assert built is not None
    return built


def _load(path: Path) -> MonoAudio:
    with wave.open(str(path)) as w:
        channels = w.getnchannels()
        raw = np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)
    return (raw.reshape(-1, channels).mean(axis=1) / 32768.0).astype(np.float32)


def _speech_core(samples: MonoAudio) -> MonoAudio:
    """The loud middle of a clip, with its silence shaved off."""
    window = np.ones(320) / 320
    envelope = np.convolve(np.abs(samples), window, mode="same")
    loud = np.flatnonzero(envelope > 0.02)
    return samples[loud[0] : loud[-1] + 1]


def _padded(core: MonoAudio, seconds: float) -> MonoAudio:
    silence = np.zeros(int(seconds * RATE), dtype=np.float32)
    return np.concatenate([silence, core, silence])


# --------------------------------------------------------------- the two rules


def test_silence_yields_the_whole_buffer_rather_than_nothing(
    segmenter: SpeechSegmenter,
) -> None:
    """"The VAD heard nothing" is not "there is nothing to hear".

    Returning no segments would silently discard a capture. Decoding a buffer
    that really is silent costs a few hundred milliseconds and logs an empty
    result; dropping a real utterance loses words the user cannot recover,
    because they have already stopped speaking. So the fallback is always the
    whole buffer.
    """
    silence = np.zeros(3 * RATE, dtype=np.float32)
    segments = segmenter.split(silence)

    assert len(segments) == 1
    assert segments[0].start_seconds == 0.0
    assert segments[0].samples.size == silence.size


def test_a_buffer_shorter_than_one_vad_frame_still_comes_back(
    segmenter: SpeechSegmenter,
) -> None:
    """A capture too short to feed the detector even once must not vanish.

    The frame loop cannot run at all here, so this is the path where an
    off-by-one would return an empty list rather than the audio.
    """
    tiny = np.zeros(100, dtype=np.float32)
    segments = segmenter.split(tiny)

    assert [s.samples.size for s in segments] == [100]


@pytest.mark.skipif(not SAMPLE.exists(), reason="eval sample not present")
def test_split_is_repeatable_so_one_capture_cannot_leak_into_the_next(
    segmenter: SpeechSegmenter,
) -> None:
    """The detector carries state across frames, so ``split`` resets it.

    Without the reset the second dictation of a session would be segmented
    against the tail of the first -- a bug that only ever shows up after the
    daemon has been running a while, which is the worst kind.
    """
    samples = _load(SAMPLE)
    first = segmenter.split(samples)
    second = segmenter.split(samples)

    assert [(s.start_seconds, s.samples.size) for s in first] == [
        (s.start_seconds, s.samples.size) for s in second
    ]


@pytest.mark.skipif(not SAMPLE.exists(), reason="eval sample not present")
def test_segments_are_tight_around_speech_and_ordered(segmenter: SpeechSegmenter) -> None:
    samples = _load(SAMPLE)
    padded = _padded(_speech_core(samples), seconds=5.0)

    segments = segmenter.split(padded)

    starts = [s.start_seconds for s in segments]
    assert starts == sorted(starts)
    total_speech = sum(s.samples.size for s in segments)
    assert total_speech < padded.size / 2, "segments should exclude the 10s of padding"
    assert segments[0].start_seconds > 1.0, "the 5s lead-in should not be inside a segment"


# ------------------------------------------------- the bug this module is for


@pytest.mark.skipif(not SAMPLE.exists(), reason="eval sample not present")
def test_short_utterance_in_silence_decodes_to_nothing_without_vad_and_to_text_with_it(
    segmenter: SpeechSegmenter,
) -> None:
    """The regression this module was written for, end to end.

    Parakeet TDT returns an empty string when speech is a small fraction of
    the window. Same audio, same recogniser -- the only difference is whether
    it was segmented first. If this ever starts passing *without* the VAD, the
    model changed and ``voice_kb.vad`` deserves a re-measurement.
    """
    from voice_kb.asr import Transcriber, ensure_model_files
    from voice_kb.config import AsrConfig

    asr: AsrConfig = Config().asr
    try:
        ensure_model_files(asr)
    except Exception:
        pytest.skip("ASR model not present")
    transcriber = Transcriber(asr)

    buried = _padded(_speech_core(_load(SAMPLE)), seconds=5.0)

    whole_buffer = transcriber.transcribe(buried, RATE).text
    segmented = " ".join(
        transcriber.transcribe(s.samples, RATE).text for s in segmenter.split(buried)
    )

    assert whole_buffer.strip() == "", "the bug is gone; re-measure voice_kb.vad"
    assert segmented.strip() != "", "segmenting must recover the text"


# ------------------------------------------------------------------ availability


def test_a_missing_model_disables_segmentation_instead_of_raising(tmp_path: Path) -> None:
    """A 2 MB optional model must not stop a daemon whose 630 MB required one
    is loaded and working. The caller falls back to whole-buffer decoding."""
    config = VadConfig(model=tmp_path / "nope.onnx")

    assert load_segmenter(config, RATE) is None


def test_disabling_it_in_config_is_honoured() -> None:
    assert load_segmenter(VadConfig(enabled=False), RATE) is None


def test_a_corrupt_model_is_reported_and_not_raised(tmp_path: Path) -> None:
    broken = tmp_path / "broken.onnx"
    broken.write_bytes(b"not an onnx file")

    assert load_segmenter(VadConfig(model=broken), RATE) is None


def test_its_log_lines_reach_the_daemons_log() -> None:
    """Loggers here are named explicitly rather than from ``__name__``.

    ``voice_kb.vad`` is not under ``voice-kb`` -- underscore against hyphen --
    so a ``getLogger(__name__)`` here produced a module whose warnings, "no
    VAD model" among them, went nowhere at all. Caught in use: the daemon
    started with segmentation silently unavailable and said nothing.
    """
    from voice_kb import hotkey, vad

    for module in (vad, hotkey):
        name = getattr(module, "log", None) or module.logger
        assert name.name.startswith("voice-kb."), f"{module.__name__} logs outside the tree"


# --------------------------------------------------- merging and padding


@pytest.mark.skipif(not SAMPLE.exists(), reason="eval sample not present")
def test_speech_runs_are_merged_into_chunks_rather_than_decoded_one_by_one(
    segmenter: SpeechSegmenter,
) -> None:
    """A boundary costs accuracy, because the model sees no context across one.

    Measured on the eval samples: every run decoded separately scored 37.7%
    WER against 33.7% for the whole buffer, and merging to 10s recovered all
    of it. So a capture with many short runs in it must not produce many
    chunks.
    """
    core = _speech_core(_load(SAMPLE))
    gap = np.zeros(int(0.6 * RATE), dtype=np.float32)
    # Eight short utterances separated by pauses long enough to end a segment.
    many = np.concatenate([x for _ in range(8) for x in (core, gap)])

    chunks = segmenter.split(many)

    assert len(chunks) < 8, "runs of speech should be merged, not decoded one by one"


def test_a_chunk_carries_padding_from_the_real_audio_around_it() -> None:
    """Silero's boundaries clip word onsets and endings; 0.5s of padding was
    worth 2-4 WER points at every chunk size tried. It has to be the *real*
    surrounding audio, not silence, which is why it is sliced from the capture
    rather than concatenated on."""
    config = Config().vad
    if not config.model.exists():
        pytest.skip("no VAD model")
    built = load_segmenter(config, RATE)
    assert built is not None

    core = _speech_core(_load(SAMPLE))
    padded = _padded(core, seconds=2.0)
    chunk = built.split(padded)[0]

    pad = int(config.pad_seconds * RATE)
    assert chunk.samples.size >= core.size + pad, "padding should widen the chunk"
    # start_seconds must describe the samples returned, padding included, or it
    # does not locate them in the capture.
    start = int(chunk.start_seconds * RATE)
    assert np.array_equal(chunk.samples, padded[start : start + chunk.samples.size])


def test_padding_never_runs_off_either_end_of_the_capture() -> None:
    """Speech starting in the first frame, or ending in the last, must not
    produce a slice with a negative start or one past the end."""
    config = Config().vad
    if not config.model.exists():
        pytest.skip("no VAD model")
    built = load_segmenter(config, RATE)
    assert built is not None

    core = _speech_core(_load(SAMPLE))
    for audio in (core, np.concatenate([core, core])):
        for chunk in built.split(audio):
            assert chunk.start_seconds >= 0.0
            assert chunk.samples.size <= audio.size


@pytest.mark.skipif(not LONG_SAMPLE.exists(), reason="eval sample not present")
def test_the_first_and_last_chunk_get_a_wider_margin_than_interior_ones() -> None:
    """A word clipped off the front of a dictation is the first word of the
    sentence; one trimmed from the middle of a pause is nobody's loss. So the
    outer edges keep more audio than the interior boundaries do."""
    config = Config().vad
    if not config.model.exists():
        pytest.skip("no VAD model")
    built = load_segmenter(config, RATE)
    assert built is not None

    speech = _load(LONG_SAMPLE)
    lead = 3.0
    audio = np.concatenate([np.zeros(int(lead * RATE), dtype=np.float32), speech])

    first = built.split(audio)[0]

    # The chunk holds real speech, so it earns the wide margin: it should reach
    # back further than the ordinary pad_seconds would allow.
    assert first.start_seconds < lead - config.pad_seconds


def test_a_brief_utterance_in_a_quiet_capture_keeps_its_tight_trim() -> None:
    """The wide margin must not undo the fix it sits next to. A chunk swamped
    by silence is exactly what makes the recogniser return nothing, so a short
    utterance in a long quiet capture is still trimmed close."""
    config = Config().vad
    if not config.model.exists():
        pytest.skip("no VAD model")
    built = load_segmenter(config, RATE)
    assert built is not None

    brief = _speech_core(_load(SAMPLE))[: int(0.6 * RATE)]
    audio = _padded(brief, seconds=6.0)

    chunk = built.split(audio)[0]

    assert chunk.samples.size < audio.size / 2, "a brief utterance must stay tightly trimmed"
