"""Download the ASR and VAD models, then generate ``bpe.vocab``.

Idempotent: a file already present with the correct remote size is skipped,
so re-running repairs a partial download without re-fetching everything.

Usage:
    uv run scripts/fetch_model.py [--config PATH]
"""

from __future__ import annotations

import argparse
import sys
import urllib.request
from pathlib import Path

from voice_kb.asr import write_bpe_vocab
from voice_kb.config import Config

HF_REPO = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8"
BASE_URL = f"https://huggingface.co/{HF_REPO}/resolve/main"
MODEL_FILES = ("encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt")

#: Silero VAD, ~2 MB, taken from sherpa-onnx's own release assets rather than
#: upstream: it is the build sherpa-onnx is tested against, and this project
#: already depends on that vendor for the recogniser.
VAD_URL = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"

_CHUNK_SIZE = 1 << 20  # 1 MiB
_TIMEOUT_S = 30


def _remote_size(url: str) -> int | None:
    """The file's size on the server, or ``None`` if it cannot be determined."""
    req = urllib.request.Request(url, method="HEAD")
    try:
        with urllib.request.urlopen(req, timeout=_TIMEOUT_S) as resp:
            length = resp.headers.get("Content-Length")
    except OSError:
        return None
    return int(length) if length is not None else None


def _print_progress(name: str, downloaded: int, total: int | None) -> None:
    mib = downloaded / _CHUNK_SIZE
    if total:
        pct = 100 * downloaded / total
        print(f"\r  {name}: {mib:8.1f} MiB ({pct:5.1f}%)", end="", flush=True)
    else:
        print(f"\r  {name}: {mib:8.1f} MiB", end="", flush=True)


def _download(url: str, dest: Path) -> None:
    tmp = dest.with_name(dest.name + ".part")
    req = urllib.request.Request(url)
    with urllib.request.urlopen(req, timeout=_TIMEOUT_S) as resp, tmp.open("wb") as out:
        total_header = resp.headers.get("Content-Length")
        total = int(total_header) if total_header is not None else None
        downloaded = 0
        while chunk := resp.read(_CHUNK_SIZE):
            out.write(chunk)
            downloaded += len(chunk)
            _print_progress(dest.name, downloaded, total)
    print()
    tmp.replace(dest)


def fetch(config_path: Path | None) -> int:
    asr = Config.load(config_path).asr
    model_dir = asr.model_dir
    model_dir.mkdir(parents=True, exist_ok=True)
    print(f"model directory: {model_dir}")

    for filename in MODEL_FILES:
        dest = model_dir / filename
        url = f"{BASE_URL}/{filename}"
        remote_size = _remote_size(url)
        if dest.exists() and remote_size is not None and dest.stat().st_size == remote_size:
            print(f"  {filename}: already present ({remote_size / _CHUNK_SIZE:.1f} MiB), skipping")
            continue
        print(f"  {filename}: downloading from {url}")
        _download(url, dest)

    write_bpe_vocab(asr.tokens, asr.bpe_vocab)
    print(f"wrote {asr.bpe_vocab}")

    _fetch_vad(Config.load(config_path).vad.model)
    return 0


def _fetch_vad(dest: Path) -> None:
    """Fetch the VAD model, reporting rather than raising if it cannot.

    The daemon degrades to whole-buffer decoding without this file, so a
    failure here must not fail a run that has just downloaded 630 MB of
    recogniser successfully.
    """
    dest.parent.mkdir(parents=True, exist_ok=True)
    remote_size = _remote_size(VAD_URL)
    if dest.exists() and remote_size is not None and dest.stat().st_size == remote_size:
        print(f"  {dest.name}: already present, skipping")
        return
    print(f"  {dest.name}: downloading from {VAD_URL}")
    try:
        _download(VAD_URL, dest)
    except OSError as e:
        print(f"  {dest.name}: FAILED ({e}) -- voice-kb will decode whole captures instead")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", type=Path, default=None, help="Path to config.toml (default: XDG config path)."
    )
    args = parser.parse_args()
    return fetch(args.config)


if __name__ == "__main__":
    sys.exit(main())
