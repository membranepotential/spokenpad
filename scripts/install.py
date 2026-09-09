"""Install spokenpad's window manager rules and its systemd user unit.

Everything spokenpad needs outside this repository is installed from inside it,
by this script, so the checkout is the single source of truth for how the tool
behaves. Two files leave the repo:

  packaging/i3/spokenpad.conf   -> ~/.config/i3/i3.d/spokenpad.conf
  packaging/spokenpad.service   -> ~/.config/systemd/user/spokenpad.service

Both are **symlinked** by default, so editing them here takes effect on the
next `i3-msg reload` / `systemctl --user daemon-reload` with no second step
and no copy to drift out of date. ``--copy`` installs independent copies
instead, for a checkout you intend to move or delete.

Idempotent: re-running repairs a link that points somewhere else and leaves a
correct one alone. Nothing is overwritten without saying so, and a file that
is *not* ours -- one you wrote by hand -- is never touched.

Usage:
    uv run scripts/install.py [--copy] [--dry-run] [--uninstall]
"""

from __future__ import annotations

import argparse
import filecmp
import os
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

#: The marker that says a file at a destination came from this project. Both
#: installed files carry it in their first lines, so ``--uninstall`` and the
#: overwrite check can tell spokenpad's file from one the user wrote.
MARKER = "spokenpad"


@dataclass(frozen=True, slots=True)
class Item:
    source: Path
    dest: Path
    what: str
    after: str
    """What to run to make the change take effect, for the closing summary."""


def _config_home() -> Path:
    return Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config")


def items() -> list[Item]:
    config = _config_home()
    return [
        Item(
            source=REPO / "packaging" / "i3" / "spokenpad.conf",
            dest=config / "i3" / "i3.d" / "spokenpad.conf",
            what="i3 window rules",
            after="i3-msg reload",
        ),
        Item(
            source=REPO / "packaging" / "spokenpad.service",
            dest=config / "systemd" / "user" / "spokenpad.service",
            what="systemd user unit",
            after="systemctl --user daemon-reload && systemctl --user enable --now spokenpad",
        ),
    ]


def _is_ours(path: Path) -> bool:
    """Whether ``path`` is a file this script installed.

    A symlink into this repository obviously is. A plain file counts only if
    it carries the marker, which is what stops ``--uninstall`` deleting an i3
    snippet somebody wrote themselves at the same path.
    """
    if path.is_symlink():
        try:
            return REPO in path.resolve().parents
        except OSError:
            return False
    try:
        return MARKER in path.read_text(encoding="utf-8")[:400]
    except (OSError, UnicodeDecodeError):
        return False


def _status(item: Item, *, link: bool) -> str:
    """``"ok"`` if the destination is already what we would install."""
    if not item.dest.exists() and not item.dest.is_symlink():
        return "missing"
    if link:
        if item.dest.is_symlink() and item.dest.resolve() == item.source.resolve():
            return "ok"
        return "differs"
    if item.dest.is_symlink():
        return "differs"
    return "ok" if filecmp.cmp(item.source, item.dest, shallow=False) else "differs"


def install(*, link: bool, dry_run: bool) -> int:
    changed: list[Item] = []
    for item in items():
        if not item.source.exists():
            print(f"  {item.what}: MISSING SOURCE {item.source}", file=sys.stderr)
            return 1

        state = _status(item, link=link)
        if state == "ok":
            print(f"  {item.what}: already installed at {item.dest}")
            continue
        if state == "differs" and not _is_ours(item.dest):
            # Somebody else's file at our path. Refusing is the only safe
            # answer: this script cannot tell a deliberate override from a
            # collision, and silently replacing an i3 config is not recoverable
            # from a terminal that has just lost its window rules.
            print(
                f"  {item.what}: {item.dest} exists and was not written by spokenpad."
                "\n      Move it aside and re-run, or install by hand.",
                file=sys.stderr,
            )
            return 1

        verb = "would link" if dry_run else ("linking" if link else "copying")
        print(f"  {item.what}: {verb} {item.dest} -> {item.source}")
        if dry_run:
            continue
        item.dest.parent.mkdir(parents=True, exist_ok=True)
        item.dest.unlink(missing_ok=True)
        if link:
            item.dest.symlink_to(item.source)
        else:
            shutil.copyfile(item.source, item.dest)
        changed.append(item)

    _check_i3_include()
    if changed and not dry_run:
        print("\nTo apply:")
        for item in changed:
            print(f"  {item.after}")
    return 0


def uninstall(*, dry_run: bool) -> int:
    for item in items():
        if not item.dest.exists() and not item.dest.is_symlink():
            print(f"  {item.what}: not installed")
            continue
        if not _is_ours(item.dest):
            print(f"  {item.what}: {item.dest} is not ours; leaving it alone")
            continue
        print(f"  {item.what}: {'would remove' if dry_run else 'removing'} {item.dest}")
        if not dry_run:
            item.dest.unlink()
    if not dry_run:
        print("\nThen: systemctl --user disable --now spokenpad && i3-msg reload")
    return 0


def _check_i3_include() -> None:
    """Warn if i3 does not actually read the directory we install into.

    Installing a rules file i3 never loads produces a dictation window that
    tiles and steals focus, with nothing anywhere saying why -- so this is
    worth one subprocess.
    """
    config = _config_home() / "i3" / "config"
    if not config.exists():
        return
    try:
        text = config.read_text(encoding="utf-8")
    except OSError:
        return
    if "i3.d/" in text:
        return
    print(
        f"\n  NOTE: {config} has no `include i3.d/*.conf` line, so the window"
        "\n  rules will not be loaded. Add it, or source the file directly.",
        file=sys.stderr,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--copy",
        action="store_true",
        help="Install independent copies instead of symlinks into this checkout.",
    )
    parser.add_argument("--dry-run", action="store_true", help="Say what would happen.")
    parser.add_argument("--uninstall", action="store_true", help="Remove what was installed.")
    args = parser.parse_args()

    if args.uninstall:
        print("Removing spokenpad's installed files:")
        return uninstall(dry_run=args.dry_run)

    print(f"Installing from {REPO}:")
    return install(link=not args.copy, dry_run=args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
