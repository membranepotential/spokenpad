"""Voice activity detection, against the real Silero model where it exists.

The interesting assertions here are not "the VAD finds speech" -- they are the
two ways it is allowed to be wrong. It must hand back nothing when it heard
nothing, because silence handed to Parakeet comes back as invented words, and
it must never carry state from one capture into the next.

The reproduction of the bug this module exists for lives in
``test_short_utterance_in_silence`` and needs the *recogniser* too, so it is
marked and skipped unless both models are present.
"""

from __future__ import annotations

import wave
from pathlib import Path

import numpy as np
import pytest

from spokenpad.audio import MonoAudio
from spokenpad.config import Config, VadConfig
from spokenpad.vad import SETTLE_SILENCE_SECONDS, SpeechSegmenter, load_segmenter

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


def test_silence_yields_no_segments(segmenter: SpeechSegmenter) -> None:
    """Silence is not decoded (2026-09-11), so it comes back as no chunks.

    This test asserted the opposite until a 0.5s near-silent press (peak
    0.013) was handed to Parakeet whole and came back as "Thank you.", which
    landed in the file. A model asked to transcribe silence invents speech, so
    "decode it anyway, it will come back empty" was never what happened. The
    price is that speech the detector misses entirely is now lost, and
    ``vad.threshold`` is the knob for that.
    """
    silence = np.zeros(3 * RATE, dtype=np.float32)

    assert segmenter.split(silence) == []


def test_a_buffer_shorter_than_one_vad_frame_is_handled_rather_than_crashing(
    segmenter: SpeechSegmenter,
) -> None:
    """A capture too short to feed the detector even once is still valid input.

    The frame loop cannot run at all here -- 100 samples is under one 512
    sample Silero window -- so this is the path where an off-by-one would
    raise or hand back a bogus span instead of returning cleanly. Nothing in
    6ms can be confirmed as speech, so under the silence rule there is nothing
    to decode.
    """
    tiny = np.zeros(100, dtype=np.float32)

    assert segmenter.split(tiny) == []


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
    model changed and ``spokenpad.vad`` deserves a re-measurement.
    """
    from spokenpad.asr import Transcriber, ensure_model_files
    from spokenpad.config import AsrConfig

    asr: AsrConfig = Config().asr
    try:
        ensure_model_files(asr)
    except Exception:
        pytest.skip("ASR model not present")
    transcriber = Transcriber(asr)

    buried = _padded(_speech_core(_load(SAMPLE)), seconds=5.0)

    whole_buffer = transcriber.transcribe(buried, RATE).text
    chunks = segmenter.split(buried)
    segmented = " ".join(transcriber.transcribe(s.samples, RATE).text for s in chunks)

    assert chunks, "real speech buried in silence is still found and still decoded"
    assert whole_buffer.strip() == "", "the bug is gone; re-measure spokenpad.vad"
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

    ``spokenpad.vad`` is not under ``spokenpad`` -- underscore against hyphen --
    so a ``getLogger(__name__)`` here produced a module whose warnings, "no
    VAD model" among them, went nowhere at all. Caught in use: the daemon
    started with segmentation silently unavailable and said nothing.
    """
    from spokenpad import vad

    for module in (vad,):
        name = getattr(module, "log", None) or module.logger
        assert name.name.startswith("spokenpad."), f"{module.__name__} logs outside the tree"


# --------------------------------------------------- merging and padding


def _synthetic_segmenter(config: VadConfig, rate: int = 100) -> SpeechSegmenter:
    segmenter = object.__new__(SpeechSegmenter)
    segmenter._sample_rate = rate
    segmenter._chunk_seconds = config.chunk_seconds
    segmenter._pad_seconds = config.pad_seconds
    segmenter._edge_pad_seconds = config.edge_pad_seconds
    segmenter._split_silence_seconds = max(
        SETTLE_SILENCE_SECONDS,
        2 * max(config.edge_pad_seconds, config.pad_seconds),
    )
    return segmenter


def test_a_long_pause_closes_the_pending_chunk(monkeypatch: pytest.MonkeyPatch) -> None:
    segmenter = _synthetic_segmenter(VadConfig())
    monkeypatch.setattr(segmenter, "_speech_spans", lambda _samples: [(100, 300), (700, 800)])
    audio = np.arange(900, dtype=np.float32)

    chunks = segmenter.split(audio)

    assert [(chunk.end_frame, chunk.settled) for chunk in chunks] == [(300, True), (800, False)]
    assert np.array_equal(chunks[0].samples, audio[50:350])
    assert np.array_equal(chunks[1].samples, audio[650:850])


def test_a_long_pause_gives_both_wide_chunks_an_internal_context_edge(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    segmenter = _synthetic_segmenter(VadConfig())
    monkeypatch.setattr(segmenter, "_speech_spans", lambda _samples: [(100, 500), (900, 1300)])
    audio = np.arange(1400, dtype=np.float32)

    chunks = segmenter.split(audio)

    assert np.array_equal(chunks[0].samples, audio[0:700])
    assert np.array_equal(chunks[1].samples, audio[700:1400])


@pytest.mark.parametrize(
    ("second_start", "expected_windows"),
    [
        (1500, [(0, 1300), (1450, 1650)]),
        (1499, [(0, 1150), (1449, 1650)]),
    ],
)
def test_a_long_pause_after_a_target_closed_chunk_still_marks_its_context_edge(
    monkeypatch: pytest.MonkeyPatch,
    second_start: int,
    expected_windows: list[tuple[int, int]],
) -> None:
    segmenter = _synthetic_segmenter(VadConfig())
    monkeypatch.setattr(
        segmenter,
        "_speech_spans",
        lambda _samples: [(100, 1100), (second_start, 1600)],
    )
    audio = np.arange(1650, dtype=np.float32)

    chunks = segmenter.split(audio)

    actual_windows = [
        (round(chunk.start_seconds * 100), round(chunk.start_seconds * 100) + len(chunk.samples))
        for chunk in chunks
    ]
    assert actual_windows == expected_windows


def test_a_pause_below_the_padding_aware_boundary_still_merges(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    segmenter = _synthetic_segmenter(VadConfig())
    monkeypatch.setattr(segmenter, "_speech_spans", lambda _samples: [(100, 300), (699, 800)])
    chunks = segmenter.split(np.arange(900, dtype=np.float32))
    assert len(chunks) == 1
    assert chunks[0].end_frame == 800


def test_larger_padding_raises_the_long_pause_boundary(monkeypatch: pytest.MonkeyPatch) -> None:
    segmenter = _synthetic_segmenter(VadConfig(pad_seconds=3))
    monkeypatch.setattr(segmenter, "_speech_spans", lambda _samples: [(100, 300), (700, 800)])
    assert len(segmenter.split(np.arange(900, dtype=np.float32))) == 1


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


# ------------------------------------------------------------- settled chunks
#
# docs/progressive-commit.md: a chunk is settled when no later audio can
# change it, and that is decided on a growing buffer.


@pytest.mark.skipif(not LONG_SAMPLE.exists(), reason="eval sample not present")
def test_every_chunk_but_the_last_is_settled(segmenter: SpeechSegmenter) -> None:
    core = _speech_core(_load(LONG_SAMPLE))
    gap = np.zeros(int(0.6 * RATE), dtype=np.float32)
    many = np.concatenate([x for _ in range(4) for x in (core, gap)])

    chunks = segmenter.split(many)

    assert len(chunks) > 1, "the sample must be long enough to close a chunk"
    assert all(c.settled for c in chunks[:-1])


@pytest.mark.skipif(not LONG_SAMPLE.exists(), reason="eval sample not present")
def test_the_last_chunk_settles_only_after_a_second_of_silence(
    segmenter: SpeechSegmenter,
) -> None:
    """The span that closes the last chunk may be the one ``flush()`` cut at
    the buffer's end, which moves on the next tick. A second of trailing
    silence is the proof it did not."""
    core = _speech_core(_load(LONG_SAMPLE))
    config = Config().vad
    assert core.size / RATE > config.chunk_seconds, "needs a full chunk of speech"

    ending_mid_word = segmenter.split(core)[-1]
    assert not ending_mid_word.settled

    # sherpa reports a span's end ~0.9s after the speech stops (measured), so
    # a second of silence is not yet a *reported* second past the end.
    too_soon = np.zeros(int((SETTLE_SILENCE_SECONDS + 0.2) * RATE), dtype=np.float32)
    assert not segmenter.split(np.concatenate([core, too_soon]))[-1].settled

    quiet = np.zeros(int((SETTLE_SILENCE_SECONDS + 1.5) * RATE), dtype=np.float32)
    after_a_pause = segmenter.split(np.concatenate([core, quiet]))
    assert after_a_pause[-1].settled, "a closed chunk followed by real silence must settle"
    assert after_a_pause[-1].end_frame < core.size + RATE, "and its end is where the speech ended"


@pytest.mark.skipif(not LONG_SAMPLE.exists(), reason="eval sample not present")
def test_end_frame_lies_inside_the_chunk_and_after_its_speech(
    segmenter: SpeechSegmenter,
) -> None:
    core = _speech_core(_load(LONG_SAMPLE))
    gap = np.zeros(int(0.6 * RATE), dtype=np.float32)
    audio = np.concatenate([core, gap, core, gap])

    for chunk in segmenter.split(audio):
        start = int(chunk.start_seconds * RATE)
        assert start < chunk.end_frame <= start + chunk.samples.size
        assert chunk.end_frame <= audio.size


@pytest.mark.skipif(not LONG_SAMPLE.exists(), reason="eval sample not present")
def test_splitting_from_a_settled_end_frame_finds_the_same_next_chunk(
    segmenter: SpeechSegmenter,
) -> None:
    """What the tick does: re-split the audio past the last settled chunk. The
    next chunk must be the one the whole-buffer split would have produced, to
    within the pad -- or the live transcript and ``spokenpad transcribe`` would
    disagree about where sentences begin."""
    core = _speech_core(_load(LONG_SAMPLE))
    gap = np.zeros(int(0.6 * RATE), dtype=np.float32)
    audio = np.concatenate([core, gap, core, gap])
    pad = int(Config().vad.pad_seconds * RATE)

    whole = segmenter.split(audio)
    settled = [c for c in whole if c.settled]
    assert settled, "the sample must close at least one chunk"
    first = settled[0]
    remainder = segmenter.split(audio[first.end_frame :])

    # Same speech, located in the same place, allowing the lead pad to be
    # clipped at the remainder's start.
    expected = whole[whole.index(first) + 1]
    expected_start = int(expected.start_seconds * RATE)
    remainder_start = first.end_frame + int(remainder[0].start_seconds * RATE)
    assert abs(remainder_start - expected_start) <= pad
