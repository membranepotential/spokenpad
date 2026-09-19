"""Compare the native Rust pipeline to the offline Python reference.

Build: SHERPA_ONNX_LIB_DIR=<native-lib-dir> cargo build --release --example verify_native
Run: .venv/bin/python scripts/verify_rust.py
Compares exact segments, transcripts, progressive offsets, and final remainder.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import wave
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from spokenpad.asr import Transcriber
from spokenpad.audio import MonoAudio
from spokenpad.config import Config
from spokenpad.decode import decode_capture, transcribe_speech
from spokenpad.vad import SpeechSegmenter


@dataclass(frozen=True, slots=True)
class ReferenceCommit:
    tick_end: int
    through: int
    text: str


@dataclass(frozen=True, slots=True)
class ProgressiveReference:
    commits: list[ReferenceCommit]
    tail_frames: int
    text: str


def read_wav(path: Path) -> tuple[MonoAudio, int]:
    """Read the mono 16-bit WAV format used by the local evaluation set."""
    with wave.open(str(path), "rb") as source:
        channels = source.getnchannels()
        width = source.getsampwidth()
        rate = source.getframerate()
        raw = source.readframes(source.getnframes())
    if channels != 1 or width != 2:
        raise ValueError(f"{path}: expected mono 16-bit PCM")
    pcm16 = np.frombuffer(raw, dtype="<i2")
    samples: MonoAudio = (pcm16.astype(np.float32) / 32767.0).clip(-1.0, 1.0)
    return samples, rate


def progressive_reference(
    samples: MonoAudio,
    *,
    transcriber: Transcriber,
    segmenter: SpeechSegmenter,
    sample_rate: int,
    tick_frames: int,
) -> ProgressiveReference:
    """Pure reference for settled preview commits followed by release decode."""
    through = 0
    commits: list[ReferenceCommit] = []
    texts: list[str] = []
    for end in range(tick_frames, len(samples), tick_frames):
        remainder = samples[through:end]
        base = through
        for chunk in segmenter.split(remainder):
            if not chunk.settled:
                break  # the open tail is only previewed; nothing here compares it
            text = transcribe_speech(transcriber, chunk.samples, sample_rate)
            through = base + chunk.end_frame
            commits.append(ReferenceCommit(end, through, text))
            if text.strip():
                texts.append(text)

    tail = samples[through:]
    release_texts: list[str] = []
    decode_capture(
        tail,
        transcriber=transcriber,
        segmenter=segmenter,
        sample_rate=sample_rate,
        on_segment=release_texts.append,
    )
    texts.extend(release_texts)
    return ProgressiveReference(
        commits=commits,
        tail_frames=len(tail),
        text=" ".join(texts),
    )


def main() -> None:
    argparse.ArgumentParser(description=__doc__).parse_args()
    root = Path(__file__).resolve().parent.parent
    paths = sorted((root / "eval-samples").glob("*.wav"))
    assert paths, "evaluation WAVs missing"
    native = subprocess.run(
        [str(root / "target/release/examples/verify_native"), *map(str, paths)],
        check=True,
        capture_output=True,
        text=True,
        timeout=600,
    )
    actual = json.loads(native.stdout)
    config = Config.load()
    rate = config.audio.sample_rate
    transcriber = Transcriber(config.asr)
    segmenter = SpeechSegmenter(config.vad, rate)
    passage: list[MonoAudio] = []
    for path, case in zip(paths, actual["cases"], strict=True):
        samples, sample_rate = read_wav(path)
        if sample_rate != rate:
            raise ValueError(f"{path}: expected {rate} Hz, got {sample_rate} Hz")
        chunks = segmenter.split(samples)
        expected_segments = [
            [
                round(segment.start_seconds * rate),
                round(segment.start_seconds * rate) + len(segment.samples),
                segment.end_frame,
                segment.settled,
            ]
            for segment in chunks
        ]
        assert case["segments"] == expected_segments, f"segment mismatch: {path.name}"
        text = decode_capture(
            samples,
            transcriber=transcriber,
            segmenter=segmenter,
            sample_rate=rate,
        )
        assert case["text"] == text, (
            f"text mismatch: {path.name}\nRust: {case['text']}\nPython: {text}"
        )
        print(f"PASS {path.name}: exact segment and transcript parity", flush=True)
        if len(samples) > 32000:
            passage.extend([samples, np.zeros(rate, dtype=np.float32)])

    joined = np.concatenate(passage)
    expected = progressive_reference(
        joined,
        transcriber=transcriber,
        segmenter=segmenter,
        sample_rate=rate,
        tick_frames=17600,
    )
    progressive = actual["progressive"]
    expected_commits = [
        [commit.tick_end, commit.through, commit.text] for commit in expected.commits
    ]
    assert progressive["commits"] == expected_commits, "progressive commit mismatch"
    assert progressive["tail_frames"] == expected.tail_frames, "release offset mismatch"
    assert progressive["text"] == expected.text, "progressive transcript mismatch"
    print(
        f"PASS progressive {len(joined) / rate:.1f}s: {len(expected.commits)} exact commits, "
        f"tail {expected.tail_frames / rate:.1f}s, "
        f"Rust release {progressive['release_seconds']:.2f}s"
    )


if __name__ == "__main__":
    main()
