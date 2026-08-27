"""One-shot ASR: load the Parakeet TDT transducer once, decode audio on demand.

CPU only, one-shot decode -- never streaming (see README/STATUS for why: a
streaming decode re-decodes a growing buffer and silently drops long
utterances). :class:`Transcriber` loads the sherpa-onnx recognizer once at
construction and holds it resident, since loading is slow (seconds) and
decode is fast (~2s for 20s of audio at 6 threads).

Threading: a :class:`Transcriber` is **not** thread-safe. sherpa-onnx does
not document ``OfflineRecognizer.decode_stream`` as safe for concurrent
calls, so callers must serialize every call to :meth:`Transcriber.transcribe`
on a single worker thread (or hold an external lock) -- never call it
concurrently from multiple threads against the same instance. The overlay's
live-preview decodes share this same recognizer and rely on exactly that: they
are dispatched to the same single worker thread as the committed decode, so
the two are serialized and never overlap.
"""

from __future__ import annotations

import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import sherpa_onnx

from voice_kb.config import AsrConfig


class ModelMissingError(RuntimeError):
    """A required model file is absent. Run ``scripts/fetch_model.py``."""


@dataclass(frozen=True, slots=True)
class TranscriptionResult:
    text: str
    elapsed_seconds: float
    """Wall-clock decode time, for logging and the eval harness."""


def generate_bpe_vocab(tokens_path: Path) -> str:
    """Render ``bpe.vocab`` content (piece TAB score) from ``tokens.txt``.

    The Parakeet checkpoint ships no ``bpe.model``/``bpe.vocab``, but
    sherpa-onnx's ``bpe_vocab`` parameter is not the SentencePiece protobuf --
    it is the two-column ``.vocab`` text file (piece, log-probability). It is
    reconstructible from ``tokens.txt`` (``<piece> <id>`` per line) using the
    SentencePiece BPE convention ``score = -id``. Verified working: this
    fixes ``mkir`` -> ``mkdir`` at ``hotwords_score=1.5``.
    """
    lines: list[str] = []
    for line in tokens_path.read_text(encoding="utf-8").splitlines():
        if not line:
            continue
        piece, id_str = line.rsplit(maxsplit=1)
        lines.append(f"{piece}\t{-int(id_str)}")
    return "\n".join(lines) + "\n"


def write_bpe_vocab(tokens_path: Path, vocab_path: Path) -> None:
    """(Re)generate ``bpe.vocab`` at ``vocab_path`` from ``tokens.txt``."""
    vocab_path.write_text(generate_bpe_vocab(tokens_path), encoding="utf-8")


def render_hotwords(vocabulary: tuple[str, ...]) -> str:
    """Render a sherpa-onnx hotwords file: one phrase per line, no scores.

    Per-phrase scores are not used -- ``AsrConfig.hotwords_score`` applies as
    a single global bias via the recognizer's ``hotwords_score`` parameter.
    """
    if not vocabulary:
        return ""
    return "\n".join(vocabulary) + "\n"


def write_hotwords_file(vocabulary: tuple[str, ...], path: Path) -> None:
    path.write_text(render_hotwords(vocabulary), encoding="utf-8")


def ensure_model_files(config: AsrConfig) -> None:
    """Raise :class:`ModelMissingError` if any required model file is absent."""
    model_files = (config.encoder, config.decoder, config.joiner, config.tokens)
    missing = [p for p in model_files if not p.exists()]
    if missing:
        names = ", ".join(str(p) for p in missing)
        raise ModelMissingError(
            f"missing ASR model file(s): {names}. "
            "Run `uv run scripts/fetch_model.py` to download the model."
        )


class Transcriber:
    """A resident sherpa-onnx offline recognizer for one-shot decoding.

    Not thread-safe -- see module docstring.
    """

    def __init__(self, config: AsrConfig) -> None:
        ensure_model_files(config)

        hotwords_path: Path | None = None
        try:
            kwargs: dict[str, object] = {}
            if config.vocabulary:
                if not config.bpe_vocab.exists():
                    write_bpe_vocab(config.tokens, config.bpe_vocab)
                with tempfile.NamedTemporaryFile(
                    mode="w",
                    prefix="voice-kb-hotwords-",
                    suffix=".txt",
                    delete=False,
                    encoding="utf-8",
                ) as f:
                    f.write(render_hotwords(config.vocabulary))
                    hotwords_path = Path(f.name)
                kwargs = {
                    "hotwords_file": str(hotwords_path),
                    "hotwords_score": config.hotwords_score,
                    "modeling_unit": "bpe",
                    "bpe_vocab": str(config.bpe_vocab),
                }

            self._recognizer: sherpa_onnx.OfflineRecognizer = (
                sherpa_onnx.OfflineRecognizer.from_transducer(
                    encoder=str(config.encoder),
                    decoder=str(config.decoder),
                    joiner=str(config.joiner),
                    tokens=str(config.tokens),
                    num_threads=config.num_threads,
                    provider="cpu",
                    model_type="nemo_transducer",
                    decoding_method=config.decoding,
                    **kwargs,
                )
            )
        finally:
            if hotwords_path is not None:
                hotwords_path.unlink(missing_ok=True)

    def transcribe(self, samples: np.ndarray, sample_rate: int) -> TranscriptionResult:
        """Decode ``samples`` (mono float32 PCM in ``[-1, 1]``) in one shot."""
        t0 = time.perf_counter()
        stream = self._recognizer.create_stream()
        stream.accept_waveform(sample_rate, samples)
        self._recognizer.decode_stream(stream)
        elapsed = time.perf_counter() - t0
        text: str = stream.result.text
        return TranscriptionResult(text=text, elapsed_seconds=elapsed)
