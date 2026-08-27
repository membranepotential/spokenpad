"""Text injection: clipboard round-trip + paste, never character synthesis.

The only way text reaches the focused window is: save its clipboard, put
the transcript on the clipboard, send a fixed paste key combo, wait for the
target to read it, then restore what was there before. There is
deliberately no code path here that types characters (`xdotool type`,
`xdotool key <unicode keysym>`, or anything enigo-style) -- synthesising
arbitrary characters means remapping keycodes through the *core* X keymap,
which is exactly what destroyed the user's per-device
``setxkbmap -device N -layout us -variant de_se_fi`` in the tool this
project replaces. Measured on this machine: 183 ms for the clipboard round
trip vs. 3.3 s to type the same 159 characters.

Every subprocess call is bounded by a timeout and never raises into the
caller; failure is reported through :type:`InjectResult` instead.
"""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass

from voice_kb.config import PasteConfig

_XDOTOOL_TIMEOUT_S = 2.0
_XCLIP_TIMEOUT_S = 2.0


@dataclass(frozen=True, slots=True)
class Injected:
    """The paste completed. ``elapsed_ms`` covers the whole sequence below,
    including the configured ``restore_delay_ms`` sleep."""

    elapsed_ms: float


@dataclass(frozen=True, slots=True)
class InjectionFailed:
    """Some step of the sequence did not complete. ``reason`` is for logs,
    not for parsing -- it names the step and, where available, the
    subprocess's own error output."""

    reason: str


type InjectResult = Injected | InjectionFailed


def focused_window_class() -> str | None:
    """The focused window's ``WM_CLASS``, or ``None`` if it cannot be determined
    (no window manager, no focused window, ``xdotool`` missing, etc.)."""
    try:
        proc = subprocess.run(
            ["xdotool", "getactivewindow", "getwindowclassname"],
            capture_output=True,
            timeout=_XDOTOOL_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    name = proc.stdout.decode("utf-8", errors="replace").strip()
    return name or None


def inject_text(text: str, config: PasteConfig) -> InjectResult:
    """Paste ``text`` into the focused window via the clipboard.

    Sequence: read the current clipboard, overwrite it with ``text``, send
    the paste combo for the focused window's class, wait
    ``config.restore_delay_ms``, then restore the original clipboard
    contents. The restore is best-effort: if the original clipboard could
    not be read (empty, holding binary data ``xclip`` refused, or ``xclip``
    itself failing), it is left holding ``text`` rather than guessing.
    """
    start = time.monotonic()

    combo = config.combo_for(focused_window_class())
    previous = _read_clipboard()

    if not _write_clipboard(text.encode("utf-8")):
        return InjectionFailed(reason="failed to set clipboard via xclip")

    try:
        proc = subprocess.run(
            ["xdotool", "key", "--clearmodifiers", combo],
            timeout=_XDOTOOL_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return InjectionFailed(reason=f"xdotool key {combo!r} failed: {e}")
    if proc.returncode != 0:
        return InjectionFailed(reason=f"xdotool key {combo!r} exited {proc.returncode}")

    time.sleep(config.restore_delay_ms / 1000)

    if previous is not None:
        _write_clipboard(previous)  # best-effort; the paste already succeeded

    return Injected(elapsed_ms=(time.monotonic() - start) * 1000)


def _read_clipboard() -> bytes | None:
    """Best-effort read of the current clipboard. ``None`` on any failure,
    including an empty/ownerless clipboard, which ``xclip`` also reports as
    a nonzero exit -- there is no way to distinguish "empty" from "unreadable"
    through its exit code alone, and for a restore step that distinction
    does not matter."""
    try:
        proc = subprocess.run(
            ["xclip", "-o", "-selection", "clipboard"],
            capture_output=True,
            timeout=_XCLIP_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    return proc.stdout


def _write_clipboard(data: bytes) -> bool:
    """Set the clipboard to ``data``, passed via stdin (never as an argv
    string) so arbitrary transcript text can never be interpreted as a
    shell token."""
    try:
        proc = subprocess.run(
            ["xclip", "-i", "-selection", "clipboard"],
            input=data,
            timeout=_XCLIP_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return proc.returncode == 0
