"""Regression harness: decode ``eval-samples/*.wav`` and score against ground truth.

Answers, repeatably and numerically, whether a change to ``asr.vocabulary``,
``asr.hotwords_score``, or ``asr.decoding`` makes transcription better or
worse -- see [docs/evaluation.md](../docs/evaluation.md) for what the numbers
mean and why one sample is a pass/fail check instead of a WER row.

Loads ``eval-samples/references.json`` (ground truth, reconstructed from the
recording session and since confirmed against the audio by the speaker) and
``eval-samples/transcripts.json`` (the Handy 0.9.6 baseline this project
replaces), decodes each sample through the
real :class:`~spokenpad.asr.Transcriber`, applies
:func:`~spokenpad.text.postprocess`, and reports WER/CER plus decode timing.

Usage:
    uv run scripts/eval.py [--config PATH] [--samples-dir PATH]
        [--vocabulary WORD[,WORD...] ...] [--hotwords-score SCORE]
        [--decoding {greedy_search,modified_beam_search}] [--vad] [--json]
    uv run scripts/eval.py --sweep 0 1.5 3.0 6.0 --vocabulary mkdir [--json]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import time
import wave
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path

import numpy as np

from spokenpad.asr import ModelMissingError, Transcriber, TranscriptionResult
from spokenpad.audio import MonoAudio
from spokenpad.config import Config, ConfigError, DecodingMethod
from spokenpad.text import postprocess
from spokenpad.vad import SpeechSegmenter, load_segmenter

SCRIPT_DIR = Path(__file__).resolve().parent
DEFAULT_SAMPLES_DIR = SCRIPT_DIR.parent / "eval-samples"

LONG_CLIP_MARGIN = 0.5
"""The 37 s regression clip must decode in under this fraction of its own
duration. Not a tight bound -- ``docs/asr.md`` measures ~14.5x real-time
warm and idle, degrading to roughly 3x under heavy load, i.e. ~13 s for a
37 s clip. A 0.5x margin (18.5 s) stays well clear of that even under load,
so this assertion does not go flaky on a busy machine."""


# -- normalisation & WER/CER --------------------------------------------------

_PUNCTUATION_RE = re.compile(r"[^\w\s']", re.UNICODE)
_WHITESPACE_RE = re.compile(r"\s+")


def normalize(text: str) -> str:
    """Casefold, replace punctuation with a space, collapse whitespace.

    Apostrophes are kept (``let's`` stays one token). Underscores are ``\\w``
    and so are *not* stripped: ``test_file`` and ``test file`` are genuinely
    different transcriptions here -- one symbolised spoken punctuation, one
    didn't -- and collapsing them would hide exactly that distinction.
    Everything else (hyphens, commas, periods, ...) becomes a space, so
    ``rm-rf`` and ``rm -rf`` both normalise to the two tokens ``rm rf``,
    distinguishable from the collapsed baseline error ``rmrf``.
    """
    text = text.casefold()
    text = _PUNCTUATION_RE.sub(" ", text)
    return _WHITESPACE_RE.sub(" ", text).strip()


def tokenize(text: str) -> list[str]:
    normalized = normalize(text)
    return normalized.split() if normalized else []


def _edit_distance(a: Sequence[str], b: Sequence[str]) -> int:
    """Levenshtein distance (substitution/insertion/deletion cost 1)."""
    if not a:
        return len(b)
    if not b:
        return len(a)
    previous = list(range(len(b) + 1))
    for i, item_a in enumerate(a, start=1):
        current = [i] + [0] * len(b)
        for j, item_b in enumerate(b, start=1):
            cost = 0 if item_a == item_b else 1
            current[j] = min(
                previous[j] + 1,
                current[j - 1] + 1,
                previous[j - 1] + cost,
            )
        previous = current
    return previous[-1]


def word_error_rate(reference: str, hypothesis: str) -> float:
    ref_tokens = tokenize(reference)
    hyp_tokens = tokenize(hypothesis)
    if not ref_tokens:
        return 0.0 if not hyp_tokens else 1.0
    return _edit_distance(ref_tokens, hyp_tokens) / len(ref_tokens)


def character_error_rate(reference: str, hypothesis: str) -> float:
    ref_chars = list(normalize(reference))
    hyp_chars = list(normalize(hypothesis))
    if not ref_chars:
        return 0.0 if not hyp_chars else 1.0
    return _edit_distance(ref_chars, hyp_chars) / len(ref_chars)


# -- ground truth & baseline --------------------------------------------------


@dataclass(frozen=True, slots=True)
class Reference:
    file: str
    duration_s: float
    verified: bool
    reference: str | None
    """``None`` for a sample with no ground truth yet; such a sample is skipped
    for WER rather than scored against nothing."""
    exercises: tuple[str, ...]
    long_clip_check: bool = False
    """Additionally assert this clip decodes to non-empty text well inside its
    own duration -- see :data:`LONG_CLIP_MARGIN`.

    Independent of whether the sample has a reference. It used to be inferred
    from ``reference is None``, which silently disabled the assertion the
    moment the 37s regression clip was finally transcribed: exactly the sample
    the check exists for would have stopped being checked."""


def load_references(path: Path) -> list[Reference]:
    raw = json.loads(path.read_text(encoding="utf-8"))
    return [
        Reference(
            file=s["file"],
            duration_s=s["duration_s"],
            verified=s["verified"],
            reference=s["reference"],
            exercises=tuple(s["exercises"]),
            long_clip_check=bool(s.get("long_clip_check", False)),
        )
        for s in raw["samples"]
    ]


def load_handy_baseline(path: Path) -> Mapping[str, str]:
    raw = json.loads(path.read_text(encoding="utf-8"))
    return {entry["file"]: entry["text"] for entry in raw}


def load_wav(path: Path) -> tuple[MonoAudio, int, float]:
    """Read a mono 16-bit PCM wav as float32 samples in ``[-1, 1]``.

    Uses the stdlib ``wave`` module rather than a new dependency -- every
    eval sample is exactly this format (checked against the real files).
    """
    with wave.open(str(path), "rb") as wf:
        n_channels = wf.getnchannels()
        sample_width = wf.getsampwidth()
        frame_rate = wf.getframerate()
        n_frames = wf.getnframes()
        raw = wf.readframes(n_frames)
    if n_channels != 1:
        raise ValueError(f"{path}: expected mono audio, got {n_channels} channels")
    if sample_width != 2:
        raise ValueError(f"{path}: expected 16-bit PCM, got {sample_width * 8}-bit")
    pcm16 = np.frombuffer(raw, dtype=np.int16)
    samples: MonoAudio = pcm16.astype(np.float32) / 32768.0
    return samples, frame_rate, n_frames / frame_rate


# -- exercise checks -----------------------------------------------------------


@dataclass(frozen=True, slots=True)
class ExerciseCheck:
    label: str
    expect: tuple[re.Pattern[str], ...]
    """All of these must match the normalised hypothesis for a pass."""
    forbid: tuple[re.Pattern[str], ...]
    """None of these may match the normalised hypothesis for a pass."""


def _p(pattern: str) -> re.Pattern[str]:
    return re.compile(pattern)


EXERCISE_CHECKS: Mapping[str, tuple[ExerciseCheck, ...]] = {
    "handy-1787827422.wav": (
        ExerciseCheck(
            "commands (not comments): technical word lost to a common neighbour",
            expect=(_p(r"\bcommands\b"),),
            forbid=(_p(r"\bcomments\b"),),
        ),
        ExerciseCheck(
            "a dir (not there): context error -- hotword biasing cannot fix this",
            expect=(_p(r"\ba dir\b"),),
            forbid=(_p(r"\bthere\b"),),
        ),
        ExerciseCheck(
            "mkdir (not mkir / MK D I R)",
            expect=(_p(r"\bmkdir\b"),),
            forbid=(_p(r"\bmkir\b"), _p(r"\bmk d i r\b")),
        ),
        ExerciseCheck(
            "rm -rf (not RMRF run together)",
            expect=(_p(r"\brm\b"), _p(r"\brf\b")),
            forbid=(_p(r"\brmrf\b"),),
        ),
    ),
    "handy-1787827701.wav": (
        ExerciseCheck(
            "set (not sed): the fuzzy-replacement regression this project bans",
            expect=(_p(r"\bset\b"),),
            forbid=(_p(r"\bsed\b"),),
        ),
        ExerciseCheck(
            "reset (not rust): same regression, second word",
            expect=(_p(r"\breset\b"),),
            forbid=(_p(r"\brust\b"),),
        ),
    ),
    "handy-1787827757.wav": (
        ExerciseCheck(
            "set AND sed both survive -- spoken in one sentence, about each other",
            expect=(_p(r"\bset\b"), _p(r"\bsed\b")),
            forbid=(),
        ),
        ExerciseCheck(
            "reset AND rust both survive -- same sentence, same trap",
            expect=(_p(r"\breset\b"), _p(r"\brust\b")),
            forbid=(),
        ),
        ExerciseCheck(
            "i3 (not 'I three' / 'eye three') -- hotword candidate, expected to fail today",
            expect=(_p(r"\bi3\b"),),
            forbid=(),
        ),
    ),
    "handy-1787828395.wav": (
        ExerciseCheck(
            "system (not systemd)",
            expect=(_p(r"\bsystem\b"),),
            forbid=(_p(r"\bsystemd\b"),),
        ),
    ),
}
"""Only the exercises with a concrete, checkable before/after word pair are
covered. ``test_file -> test underscore file`` (spoken-punctuation
symbolisation) is a known, undesigned-for limitation with no target
behaviour to assert against, so it is left out -- see
``eval-samples/references.json`` and ``docs/evaluation.md``."""


@dataclass(frozen=True, slots=True)
class ExerciseResult:
    label: str
    passed: bool


def _run_checks(file: str, hypothesis: str) -> tuple[ExerciseResult, ...]:
    normalized = normalize(hypothesis)
    results = []
    for check in EXERCISE_CHECKS.get(file, ()):
        passed = all(p.search(normalized) for p in check.expect) and not any(
            p.search(normalized) for p in check.forbid
        )
        results.append(ExerciseResult(label=check.label, passed=passed))
    return tuple(results)


# -- evaluation ----------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class SampleResult:
    file: str
    verified: bool
    duration_s: float
    reference: str
    hypothesis: str
    decode_seconds: float
    rtf: float
    wer: float
    cer: float
    handy_hypothesis: str | None
    handy_wer: float | None
    exercises: tuple[ExerciseResult, ...]


@dataclass(frozen=True, slots=True)
class LongClipCheck:
    file: str
    duration_s: float
    decode_seconds: float
    threshold_s: float
    text_nonempty: bool
    within_time_margin: bool
    hypothesis: str

    @property
    def passed(self) -> bool:
        return self.text_nonempty and self.within_time_margin


@dataclass(frozen=True, slots=True)
class AggregateStats:
    wer: float
    cer: float
    handy_wer: float | None
    samples_scored: int


@dataclass(frozen=True, slots=True)
class EvalReport:
    vocabulary: tuple[str, ...]
    hotwords_score: float
    decoding: DecodingMethod
    samples: tuple[SampleResult, ...]
    long_clip: LongClipCheck | None
    aggregate: AggregateStats

    @property
    def unverified(self) -> int:
        """How many scored samples carry a reference nobody has checked."""
        return sum(1 for s in self.samples if not s.verified)

    @property
    def references_verified(self) -> bool:
        """Every scored sample has a hand-checked reference, and there is at
        least one. A run that scored nothing has verified nothing."""
        return bool(self.samples) and not self.unverified


def _aggregate(samples: Sequence[SampleResult]) -> AggregateStats:
    if not samples:
        return AggregateStats(wer=0.0, cer=0.0, handy_wer=None, samples_scored=0)
    total_ref_tokens = total_edit = 0
    total_ref_chars = total_cedit = 0
    handy_total_tokens = handy_total_edit = 0
    handy_scored = 0
    for s in samples:
        ref_tokens = tokenize(s.reference)
        total_ref_tokens += len(ref_tokens)
        total_edit += _edit_distance(ref_tokens, tokenize(s.hypothesis))
        ref_chars = list(normalize(s.reference))
        total_ref_chars += len(ref_chars)
        total_cedit += _edit_distance(ref_chars, list(normalize(s.hypothesis)))
        if s.handy_hypothesis is not None:
            handy_total_tokens += len(ref_tokens)
            handy_total_edit += _edit_distance(ref_tokens, tokenize(s.handy_hypothesis))
            handy_scored += 1
    handy_wer = (
        handy_total_edit / handy_total_tokens if handy_scored and handy_total_tokens else None
    )
    return AggregateStats(
        wer=total_edit / total_ref_tokens if total_ref_tokens else 0.0,
        cer=total_cedit / total_ref_chars if total_ref_chars else 0.0,
        handy_wer=handy_wer,
        samples_scored=len(samples),
    )


def _decode(
    transcriber: Transcriber,
    segmenter: SpeechSegmenter | None,
    samples: MonoAudio,
    sample_rate: int,
) -> TranscriptionResult:
    """One decode of the whole buffer, or one per chunk joined in order.

    The chunked branch reports the *total* elapsed time, so the reported RTF
    stays the cost of transcribing the file rather than of its first chunk.
    """
    if segmenter is None:
        return transcriber.transcribe(samples, sample_rate)
    started = time.perf_counter()
    parts = [transcriber.transcribe(c.samples, sample_rate).text for c in segmenter.split(samples)]
    text = " ".join(p for p in parts if p.strip())
    return TranscriptionResult(text=text, elapsed_seconds=time.perf_counter() - started)


def evaluate(
    config: Config,
    samples_dir: Path,
    references: Sequence[Reference],
    handy_baseline: Mapping[str, str],
    *,
    use_vad: bool = False,
) -> EvalReport:
    """Load the model once, warm it up, then decode every sample in order.

    ``use_vad`` decodes each sample the way the daemon does -- split at silence
    into merged, padded chunks -- instead of in one pass over the whole file.
    Off by default so the historical numbers in STATUS.md and docs/asr.md stay
    comparable; ``--vad`` is how you check that segmentation has not cost
    accuracy, which is the question it exists to answer.
    """
    transcriber = Transcriber(config.asr)
    warmup = np.zeros(config.audio.sample_rate, dtype=np.float32)
    transcriber.transcribe(warmup, config.audio.sample_rate)  # discard; first decode is slow
    segmenter = load_segmenter(config.vad, config.audio.sample_rate) if use_vad else None
    if use_vad and segmenter is None:
        print("warning: --vad requested but no VAD model loaded", file=sys.stderr)

    sample_results: list[SampleResult] = []
    long_clip: LongClipCheck | None = None

    for ref in references:
        samples, sample_rate, wav_duration_s = load_wav(samples_dir / ref.file)
        if abs(wav_duration_s - ref.duration_s) > 1.0:
            print(
                f"warning: {ref.file}: references.json says duration_s={ref.duration_s}, "
                f"actual wav is {wav_duration_s:.1f}s -- file may have been replaced",
                file=sys.stderr,
            )
        result = _decode(transcriber, segmenter, samples, sample_rate)
        hypothesis = postprocess(result.text, config.text)
        rtf = (
            wav_duration_s / result.elapsed_seconds if result.elapsed_seconds > 0 else float("inf")
        )

        if ref.long_clip_check:
            threshold = wav_duration_s * LONG_CLIP_MARGIN
            long_clip = LongClipCheck(
                file=ref.file,
                duration_s=wav_duration_s,
                decode_seconds=result.elapsed_seconds,
                threshold_s=threshold,
                text_nonempty=bool(hypothesis.strip()),
                within_time_margin=result.elapsed_seconds < threshold,
                hypothesis=hypothesis,
            )

        reference_text = ref.reference
        if reference_text is None:
            continue

        handy_text = handy_baseline.get(ref.file)
        sample_results.append(
            SampleResult(
                file=ref.file,
                verified=ref.verified,
                duration_s=wav_duration_s,
                reference=reference_text,
                hypothesis=hypothesis,
                decode_seconds=result.elapsed_seconds,
                rtf=rtf,
                wer=word_error_rate(reference_text, hypothesis),
                cer=character_error_rate(reference_text, hypothesis),
                handy_hypothesis=handy_text,
                handy_wer=(
                    word_error_rate(reference_text, handy_text) if handy_text is not None else None
                ),
                exercises=_run_checks(ref.file, hypothesis),
            )
        )

    return EvalReport(
        vocabulary=config.asr.vocabulary,
        hotwords_score=config.asr.hotwords_score,
        decoding=config.asr.decoding,
        samples=tuple(sample_results),
        long_clip=long_clip,
        aggregate=_aggregate(sample_results),
    )


# -- reporting -------------------------------------------------------------


def _verified_tag(verified: bool) -> str:
    return "" if verified else " [UNVERIFIED ref]"


def print_report(report: EvalReport) -> None:
    print(
        f"asr.vocabulary={list(report.vocabulary)} "
        f"asr.hotwords_score={report.hotwords_score} "
        f"asr.decoding={report.decoding}"
    )
    print()
    header = f"{'file':<24} {'WER':>7} {'CER':>7} {'handy WER':>10} {'decode':>8} {'RTF':>7}"
    print(header)
    print("-" * len(header))
    for s in report.samples:
        handy = f"{s.handy_wer * 100:9.1f}%" if s.handy_wer is not None else f"{'N/A':>10}"
        print(
            f"{s.file:<24} {s.wer * 100:6.1f}% {s.cer * 100:6.1f}% {handy} "
            f"{s.decode_seconds:7.2f}s {s.rtf:6.1f}x{_verified_tag(s.verified)}"
        )
    print("-" * len(header))
    agg = report.aggregate
    handy_agg = f"{agg.handy_wer * 100:9.1f}%" if agg.handy_wer is not None else f"{'N/A':>10}"
    unverified = report.unverified
    caveat = (
        f" -- {unverified}/{len(report.samples)} references are UNVERIFIED; this "
        "aggregate is not quotable, see docs/evaluation.md"
        if unverified
        else ""
    )
    print(
        f"{'AGGREGATE (' + str(agg.samples_scored) + ' samples)':<24} "
        f"{agg.wer * 100:6.1f}% {agg.cer * 100:6.1f}% {handy_agg}{caveat}"
    )
    print()

    print("Exercises (specific regressions probed, not covered by aggregate WER):")
    total = passed = 0
    for s in report.samples:
        for ex in s.exercises:
            total += 1
            passed += ex.passed
            status = "PASS" if ex.passed else "FAIL"
            print(f"  [{status}] {s.file}: {ex.label}")
    print(f"  {passed}/{total} exercise checks passing")
    print()

    if report.long_clip is not None:
        lc = report.long_clip
        status = "PASS" if lc.passed else "FAIL"
        print("Long-clip check (the 37 s clip Handy discarded; also scored above):")
        print(
            f"  [{status}] {lc.file}: decoded in {lc.decode_seconds:.2f}s "
            f"(threshold {lc.threshold_s:.1f}s = {LONG_CLIP_MARGIN}x{lc.duration_s:.1f}s), "
            f"non-empty text: {lc.text_nonempty}"
        )
        if not lc.passed:
            print(f"  hypothesis: {lc.hypothesis!r}")


def report_to_dict(report: EvalReport) -> dict[str, object]:
    return {
        "config": {
            "vocabulary": list(report.vocabulary),
            "hotwords_score": report.hotwords_score,
            "decoding": report.decoding,
        },
        "samples": [
            {
                "file": s.file,
                "verified": s.verified,
                "duration_s": s.duration_s,
                "reference": s.reference,
                "hypothesis": s.hypothesis,
                "decode_seconds": s.decode_seconds,
                "rtf": s.rtf,
                "wer": s.wer,
                "cer": s.cer,
                "handy_hypothesis": s.handy_hypothesis,
                "handy_wer": s.handy_wer,
                "exercises": [{"label": e.label, "passed": e.passed} for e in s.exercises],
            }
            for s in report.samples
        ],
        "long_clip": (
            {
                "file": report.long_clip.file,
                "duration_s": report.long_clip.duration_s,
                "decode_seconds": report.long_clip.decode_seconds,
                "threshold_s": report.long_clip.threshold_s,
                "text_nonempty": report.long_clip.text_nonempty,
                "within_time_margin": report.long_clip.within_time_margin,
                "passed": report.long_clip.passed,
                "hypothesis": report.long_clip.hypothesis,
            }
            if report.long_clip is not None
            else None
        ),
        "aggregate": {
            "wer": report.aggregate.wer,
            "cer": report.aggregate.cer,
            "handy_wer": report.aggregate.handy_wer,
            "samples_scored": report.aggregate.samples_scored,
            "references_verified": report.references_verified,
        },
    }


def print_sweep_table(rows: Sequence[tuple[float, EvalReport]]) -> None:
    header = f"{'hotwords_score':>14} {'agg WER':>9} {'agg CER':>9} {'exercises':>11}"
    print(header)
    print("-" * len(header))
    for score, report in rows:
        total = sum(len(s.exercises) for s in report.samples)
        passed = sum(ex.passed for s in report.samples for ex in s.exercises)
        print(
            f"{score:14.2f} {report.aggregate.wer * 100:8.1f}% "
            f"{report.aggregate.cer * 100:8.1f}% {passed:>4}/{total:<6}"
        )
    print()
    print("(aggregate figures are corpus-level WER/CER against UNVERIFIED references)")


def sweep_row_to_dict(score: float, report: EvalReport) -> dict[str, object]:
    total = sum(len(s.exercises) for s in report.samples)
    passed = sum(ex.passed for s in report.samples for ex in s.exercises)
    return {
        "hotwords_score": score,
        "aggregate_wer": report.aggregate.wer,
        "aggregate_cer": report.aggregate.cer,
        "exercises_passed": passed,
        "exercises_total": total,
    }


# -- CLI -------------------------------------------------------------------


def _resolve_vocabulary(raw: list[str] | None) -> tuple[str, ...] | None:
    """``None`` means "leave the configured vocabulary alone"."""
    if raw is None:
        return None
    words: list[str] = []
    for item in raw:
        words.extend(w.strip() for w in item.split(",") if w.strip())
    return tuple(words)


def _configure(
    base: Config, args: argparse.Namespace, *, hotwords_score: float | None = None
) -> Config:
    asr = base.asr
    vocabulary = _resolve_vocabulary(args.vocabulary)
    if vocabulary is not None:
        asr = replace(asr, vocabulary=vocabulary)
    score = hotwords_score if hotwords_score is not None else args.hotwords_score
    if score is not None:
        asr = replace(asr, hotwords_score=score)
    if args.decoding is not None:
        asr = replace(asr, decoding=args.decoding)
    return replace(base, asr=asr)


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--config", type=Path, default=None, help="Path to config.toml (default: XDG config path)."
    )
    parser.add_argument(
        "--samples-dir",
        type=Path,
        default=DEFAULT_SAMPLES_DIR,
        help="Directory with *.wav, references.json, transcripts.json (default: eval-samples/).",
    )
    parser.add_argument(
        "--vad",
        action="store_true",
        help=(
            "Decode the way the daemon does: split at silence into merged, padded "
            "chunks, instead of one pass over the whole file. Off by default so the "
            "historical numbers stay comparable."
        ),
    )
    parser.add_argument(
        "--vocabulary",
        action="append",
        default=None,
        metavar="WORD[,WORD...]",
        help="Override asr.vocabulary. Repeatable; each value may be comma-separated. "
        "Requires --decoding modified_beam_search (the config default).",
    )
    parser.add_argument(
        "--hotwords-score", type=float, default=None, help="Override asr.hotwords_score."
    )
    parser.add_argument(
        "--decoding",
        choices=["greedy_search", "modified_beam_search"],
        default=None,
        help="Override asr.decoding.",
    )
    parser.add_argument(
        "--sweep",
        type=float,
        nargs="+",
        default=None,
        metavar="SCORE",
        help="Run the eval once per hotwords_score value and tabulate the result "
        "(combine with --vocabulary; a sweep with no vocabulary set has nothing to bias).",
    )
    parser.add_argument(
        "--json", action="store_true", help="Emit machine-readable JSON instead of a table."
    )
    return parser


def main() -> int:
    args = build_arg_parser().parse_args()
    base_config = Config.load(args.config)
    references = load_references(args.samples_dir / "references.json")
    handy_baseline = load_handy_baseline(args.samples_dir / "transcripts.json")

    try:
        if args.sweep:
            rows = []
            for score in args.sweep:
                print(f"decoding sweep point hotwords_score={score} ...", file=sys.stderr)
                cfg = _configure(base_config, args, hotwords_score=score)
                rows.append(
                    (
                        score,
                        evaluate(
                            cfg, args.samples_dir, references, handy_baseline, use_vad=args.vad
                        ),
                    )
                )
            if args.json:
                print(json.dumps({"sweep": [sweep_row_to_dict(s, r) for s, r in rows]}, indent=2))
            else:
                print_sweep_table(rows)
            return 0

        print("loading model and warming up ...", file=sys.stderr)
        cfg = _configure(base_config, args)
        report = evaluate(cfg, args.samples_dir, references, handy_baseline, use_vad=args.vad)
        if args.json:
            print(json.dumps(report_to_dict(report), indent=2))
        else:
            print_report(report)
        if report.long_clip is not None and not report.long_clip.passed:
            return 1
        return 0
    except (ConfigError, ModelMissingError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
