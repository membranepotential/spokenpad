"""Gate test: does Parakeet TDT v3 support modified_beam_search (and thus hotwords)?

Run:  uv run scripts/spike_decode.py
"""
from __future__ import annotations

import sys
import time
import wave
from pathlib import Path

import numpy as np
import sherpa_onnx

ROOT = Path(__file__).resolve().parent.parent
MODEL = ROOT / "models" / "parakeet-tdt-0.6b-v3-int8"


def read_wav(path: Path) -> tuple[np.ndarray, int]:
    with wave.open(str(path), "rb") as w:
        rate = w.getframerate()
        n_ch = w.getnchannels()
        width = w.getsampwidth()
        raw = w.readframes(w.getnframes())
    if width != 2:
        raise ValueError(f"{path}: expected 16-bit PCM, got {width * 8}-bit")
    pcm = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    if n_ch > 1:
        pcm = pcm.reshape(-1, n_ch).mean(axis=1)
    return pcm, rate


def build(decoding_method: str, **extra: object) -> sherpa_onnx.OfflineRecognizer:
    return sherpa_onnx.OfflineRecognizer.from_transducer(
        encoder=str(MODEL / "encoder.int8.onnx"),
        decoder=str(MODEL / "decoder.int8.onnx"),
        joiner=str(MODEL / "joiner.int8.onnx"),
        tokens=str(MODEL / "tokens.txt"),
        num_threads=6,
        provider="cpu",
        model_type="nemo_transducer",
        decoding_method=decoding_method,
        **extra,  # type: ignore[arg-type]
    )


def decode(rec: sherpa_onnx.OfflineRecognizer, pcm: np.ndarray, rate: int) -> tuple[str, float]:
    t0 = time.perf_counter()
    stream = rec.create_stream()
    stream.accept_waveform(rate, pcm)
    rec.decode_stream(stream)
    return stream.result.text, time.perf_counter() - t0


def main() -> int:
    wavs = sorted((ROOT / "eval-samples").glob("*.wav")) or [MODEL / "test_en.wav"]
    pcm, rate = read_wav(wavs[0])
    dur = len(pcm) / rate
    print(f"sample: {wavs[0].name}  ({dur:.1f}s @ {rate} Hz)\n")

    for method in ("greedy_search", "modified_beam_search"):
        print(f"--- {method} ---")
        try:
            rec = build(method)
            text, elapsed = decode(rec, pcm, rate)
            rtf = elapsed / dur
            print(f"  OK  {elapsed:.2f}s  ({1/rtf:.1f}x real-time, RTF {rtf:.3f})")
            print(f"  text: {text[:160]}")
        except Exception as e:  # noqa: BLE001 - this is a probe
            print(f"  FAILED: {type(e).__name__}: {e}")
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
