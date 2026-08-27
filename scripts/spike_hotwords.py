"""Does the reconstructed bpe.vocab actually make hotwords bias the beam?"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from spike_decode import MODEL, ROOT, decode, read_wav  # noqa: E402

import sherpa_onnx  # noqa: E402

HOTWORDS = Path("/tmp/claude-1000/-home-felix-Documents-voice-kb/6d065317-6fd0-4ab1-b8ee-d2181b29d736/scratchpad/hotwords.txt")


def build(score: float | None) -> sherpa_onnx.OfflineRecognizer:
    extra: dict[str, object] = {}
    if score is not None:
        extra = {
            "hotwords_file": str(HOTWORDS),
            "hotwords_score": score,
            "modeling_unit": "bpe",
            "bpe_vocab": str(MODEL / "bpe.vocab"),
        }
    return sherpa_onnx.OfflineRecognizer.from_transducer(
        encoder=str(MODEL / "encoder.int8.onnx"),
        decoder=str(MODEL / "decoder.int8.onnx"),
        joiner=str(MODEL / "joiner.int8.onnx"),
        tokens=str(MODEL / "tokens.txt"),
        num_threads=6, provider="cpu", model_type="nemo_transducer",
        decoding_method="modified_beam_search",
        **extra,  # type: ignore[arg-type]
    )


def main() -> int:
    pcm, rate = read_wav(sorted((ROOT / "eval-samples").glob("*.wav"))[0])
    for score in (None, 1.5, 3.0, 6.0):
        label = "no hotwords" if score is None else f"hotwords_score={score}"
        try:
            text, elapsed = decode(build(score), pcm, rate)
            print(f"--- {label} ({elapsed:.2f}s)")
            print(f"    {text[:190]}")
        except Exception as e:  # noqa: BLE001
            print(f"--- {label}: FAILED {type(e).__name__}: {e}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
