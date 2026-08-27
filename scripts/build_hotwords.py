"""Regenerate ``bpe.vocab`` and render a hotwords file, for standalone tuning.

``Transcriber`` does this itself at construction (writing the hotwords file
to a temp path that is deleted once the recognizer is built), so this script
is not on the hot path. It exists to inspect the generated files directly --
e.g. to check what a candidate ``asr.vocabulary`` renders to before adjusting
``hotwords_score``.

Usage:
    uv run scripts/build_hotwords.py [--config PATH] [--out PATH]
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from voice_kb.asr import ensure_model_files, render_hotwords, write_bpe_vocab
from voice_kb.config import Config


def build(config_path: Path | None, out_path: Path | None) -> int:
    asr = Config.load(config_path).asr
    ensure_model_files(asr)

    write_bpe_vocab(asr.tokens, asr.bpe_vocab)
    print(f"wrote {asr.bpe_vocab}")

    out = out_path or asr.model_dir / "hotwords.txt"
    out.write_text(render_hotwords(asr.vocabulary), encoding="utf-8")
    print(f"wrote {out} ({len(asr.vocabulary)} entries)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", type=Path, default=None, help="Path to config.toml (default: XDG config path)."
    )
    parser.add_argument(
        "--out", type=Path, default=None, help="Where to write the rendered hotwords file."
    )
    args = parser.parse_args()
    return build(args.config, args.out)


if __name__ == "__main__":
    sys.exit(main())
