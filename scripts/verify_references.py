"""Interactive checker for ``eval-samples/references.json``.

Every reference in that file was *reconstructed* -- from the session in which
the samples were recorded, and in one case partly from the output of the tool
this project replaced. Until a human has listened to the audio and confirmed
the words, the WER the eval harness reports is a number without a meaning, and
the ``handy WER`` baseline column is biased in Handy's favour. This script is
the way to remove that caveat: it plays each clip, shows what the file claims
was said, and writes ``"verified": true`` only for the ones you confirm.

    uv run scripts/verify_references.py
    uv run scripts/verify_references.py --blind      # don't show the claim first
    uv run scripts/verify_references.py --only 7757  # one sample

``--blind`` is the honest mode for a reference that may be circular: it hides
the stored text until you have written down what you actually heard, then shows
you both. Use it for ``handy-1787828395`` (derived from Handy's own transcript)
and for anything you are re-checking rather than checking for the first time.

Progress is written back after every sample, so quitting half way keeps what
you have already confirmed.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import wave
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

import numpy as np
import numpy.typing as npt

SAMPLES_DIR = Path(__file__).resolve().parent.parent / "eval-samples"
REFERENCES = SAMPLES_DIR / "references.json"

# Tried in order; the first one present wins. All are blocking players that
# exit when the clip ends, which is what lets the prompt come back on its own.
_PLAYERS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("paplay", ()),
    ("play", ("-q",)),
    ("aplay", ("-q",)),
    ("ffplay", ("-nodisp", "-autoexit", "-loglevel", "quiet")),
)

_DIM = "\033[2m"
_BOLD = "\033[1m"
_YELLOW = "\033[33m"
_GREEN = "\033[32m"
_RED = "\033[31m"
_OFF = "\033[0m"


def _colour(text: str, code: str) -> str:
    return text if not sys.stdout.isatty() else f"{code}{text}{_OFF}"


@dataclass(frozen=True, slots=True)
class Player:
    executable: str
    args: tuple[str, ...]

    def play(self, path: Path) -> None:
        """Play ``path``, returning early and quietly if the user hits Ctrl-C."""
        proc = subprocess.Popen(
            [self.executable, *self.args, str(path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            proc.wait()
        except KeyboardInterrupt:
            proc.terminate()
            proc.wait(timeout=2)
            print(_colour("  (playback stopped)", _DIM))


def find_player() -> Player | None:
    for executable, args in _PLAYERS:
        if shutil.which(executable):
            return Player(executable=executable, args=args)
    return None


def read_wav(path: Path) -> tuple[npt.NDArray[np.float32], int]:
    """Load a 16-bit PCM wav as mono float32 in [-1, 1], with its sample rate."""
    with wave.open(str(path), "rb") as w:
        rate = w.getframerate()
        channels = w.getnchannels()
        width = w.getsampwidth()
        frames = w.readframes(w.getnframes())
    if width != 2:
        raise ValueError(f"{path.name}: expected 16-bit PCM, got {width * 8}-bit")
    audio = np.frombuffer(frames, dtype=np.int16).astype(np.float32) / 32768.0
    if channels > 1:
        audio = audio.reshape(-1, channels).mean(axis=1)
    return audio, rate


def edit_in_editor(initial: str) -> str | None:
    """Open ``$EDITOR`` on ``initial``; return the edited text, or ``None`` if
    the editor failed or the user emptied the buffer."""
    editor = os.environ.get("EDITOR") or os.environ.get("VISUAL") or "nvim"
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".txt", prefix="spokenpad-reference-", delete=False, encoding="utf-8"
    ) as f:
        f.write(initial)
        tmp = Path(f.name)
    try:
        result = subprocess.run([editor, str(tmp)], check=False)
        if result.returncode != 0:
            print(_colour(f"  {editor} exited with {result.returncode}; nothing changed", _RED))
            return None
        text = " ".join(tmp.read_text(encoding="utf-8").split())
        return text or None
    finally:
        tmp.unlink(missing_ok=True)


def draft_from_local_decode(path: Path) -> str | None:
    """Transcribe ``path`` with this project's own model, as a typing aid.

    Deliberately gated behind an explicit keypress and a warning: accepting
    this output unedited makes the reference circular -- the eval harness would
    then be scoring the model against itself, which is exactly the flaw this
    script exists to remove from the Handy baseline column.
    """
    print(_colour("  loading the model (a few seconds)...", _DIM))
    try:
        from spokenpad.asr import Transcriber
        from spokenpad.config import Config
    except ImportError as e:  # pragma: no cover - developer environment only
        print(_colour(f"  cannot import spokenpad: {e}", _RED))
        return None
    audio, rate = read_wav(path)
    try:
        result = Transcriber(Config.load().asr).transcribe(audio, rate)
    except Exception as e:
        print(_colour(f"  decode failed: {e}", _RED))
        return None
    print(_colour(f"  decoded in {result.elapsed_seconds:.1f}s", _DIM))
    return result.text


def show_diff(old: str | None, new: str) -> None:
    """Report how far the confirmed text moved from what the file claimed."""
    if old is None:
        print(_colour("  (there was no stored reference for this sample)", _DIM))
        return
    if old == new:
        print(_colour("  identical to the stored reference", _GREEN))
        return
    ratio = difflib.SequenceMatcher(None, old.split(), new.split()).ratio()
    print(_colour(f"  differs from the stored reference ({ratio:.0%} word overlap):", _YELLOW))
    for line in difflib.unified_diff(
        old.split(), new.split(), fromfile="stored", tofile="heard", lineterm="", n=2
    ):
        if line.startswith("+++") or line.startswith("---") or line.startswith("@@"):
            continue
        colour = _GREEN if line.startswith("+") else _RED if line.startswith("-") else _DIM
        print("   ", _colour(line, colour))


def wrap(text: str, width: int = 76, indent: str = "    ") -> str:
    words, lines, current = text.split(), [], ""
    for word in words:
        if current and len(current) + 1 + len(word) > width:
            lines.append(current)
            current = word
        else:
            current = f"{current} {word}".strip()
    if current:
        lines.append(current)
    return "\n".join(indent + line for line in lines)


_MENU = (
    "  [enter] confirm   e edit   r replay   d draft from a local decode\n"
    "  s skip            u unverify           q save and quit"
)


def review(
    sample: dict[str, Any],
    index: int,
    total: int,
    *,
    player: Player | None,
    blind: bool,
    save: Callable[[], None],
) -> Literal["next", "quit"]:
    """Review one sample in place. Returns ``"next"`` or ``"quit"``."""
    path = SAMPLES_DIR / str(sample["file"])
    stored: str | None = sample.get("reference")
    already = bool(sample.get("verified"))

    header = f"[{index}/{total}] {sample['file']}  {sample.get('duration_s', '?')}s"
    print()
    print(_colour(header, _BOLD), _colour("(already verified)", _GREEN) if already else "")
    for probe in sample.get("exercises", []):
        if probe:
            print(_colour(f"  probes: {probe}", _DIM))

    revealed = not blind
    if revealed:
        print("\n  stored reference:")
        print(wrap(stored) if stored else _colour("    (none)", _DIM))
    else:
        print(
            _colour("\n  blind mode: write what you hear before seeing the stored text.", _YELLOW)
        )

    if not path.exists():
        print(_colour(f"  missing audio file: {path}", _RED))
        return "next"
    if player is None:
        print(_colour("  no audio player found (tried paplay, play, aplay, ffplay)", _RED))
    else:
        player.play(path)

    heard = stored
    while True:
        print(f"\n{_MENU}")
        try:
            choice = input("> ").strip().lower()
        except EOFError:
            return "quit"
        except KeyboardInterrupt:
            print()
            return "quit"

        if choice == "q":
            return "quit"
        if choice == "s":
            print(_colour("  skipped; left unverified", _DIM))
            return "next"
        if choice == "r":
            if player is not None:
                player.play(path)
            continue
        if choice == "u":
            sample["verified"] = False
            save()
            print(_colour("  marked unverified", _YELLOW))
            return "next"
        if choice == "d":
            print(
                _colour(
                    "  WARNING: this is our own model's output. Accepting it unedited\n"
                    "  makes the reference circular and its WER meaningless. Edit it\n"
                    "  against what you actually heard.",
                    _YELLOW,
                )
            )
            drafted = draft_from_local_decode(path)
            if drafted is None:
                continue
            edited = edit_in_editor(drafted)
            if edited is None:
                continue
            heard = edited
            choice = ""
        elif choice == "e":
            edited = edit_in_editor("" if blind and not revealed else (heard or ""))
            if edited is None:
                continue
            heard = edited
            choice = ""

        if choice != "":
            print(_colour(f"  unrecognised: {choice!r}", _RED))
            continue

        if not revealed:
            revealed = True
            print("\n  stored reference:")
            print(wrap(stored) if stored else _colour("    (none)", _DIM))

        if heard is None or not heard.strip():
            print(_colour("  nothing to confirm -- press e to write what you heard", _RED))
            continue

        print("\n  confirming:")
        print(wrap(heard))
        show_diff(stored, heard)
        sample["reference"] = heard
        sample["verified"] = True
        save()
        print(_colour("  verified", _GREEN))
        return "next"


def main() -> int:
    parser = argparse.ArgumentParser(
        prog="verify_references.py",
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--blind",
        action="store_true",
        help="hide the stored reference until you have written what you heard",
    )
    parser.add_argument(
        "--only", default=None, metavar="SUBSTRING", help="review only matching filenames"
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="also review samples already marked verified (default: skip them)",
    )
    args = parser.parse_args()

    if not REFERENCES.exists():
        print(f"no such file: {REFERENCES}", file=sys.stderr)
        return 1
    data: dict[str, Any] = json.loads(REFERENCES.read_text(encoding="utf-8"))
    samples: list[dict[str, Any]] = data["samples"]

    def save() -> None:
        REFERENCES.write_text(
            json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )

    matching = [s for s in samples if args.only is None or args.only in str(s["file"])]
    if not matching:
        print(f"no sample matches --only {args.only!r}.")
        print("available: " + ", ".join(str(s["file"]) for s in samples))
        return 1
    todo = [s for s in matching if args.all or not s.get("verified")]
    if not todo:
        print("nothing to review -- every matching sample is already verified.")
        print("re-check one anyway with --all, or a single clip with --only.")
        return 0

    player = find_player()
    print(f"reviewing {len(todo)} of {len(samples)} samples from {REFERENCES}")
    if player is not None:
        print(_colour(f"playing with {player.executable}", _DIM))

    for i, sample in enumerate(todo, start=1):
        if review(sample, i, len(todo), player=player, blind=args.blind, save=save) == "quit":
            break

    save()
    verified = sum(1 for s in samples if s.get("verified"))
    print(f"\n{verified}/{len(samples)} references verified.")
    if verified < len(samples):
        print("run again to finish; aggregate WER stays untrustworthy until all are done.")
    else:
        print(
            _colour("all verified -- eval WER and the Handy baseline are now meaningful.", _GREEN)
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
