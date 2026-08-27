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

Clipboard restore vs. the paste: X11 selection transfer is asynchronous, so
a fixed sleep-then-restore is a race -- a slow target can still be reading
the old selection after the restore has already handed CLIPBOARD to a new
owner holding the previous contents, and the transcription is lost with no
error anywhere. Rather than guess a delay long enough for every target,
``inject_text`` asks ``xclip`` itself for evidence: the transcript is written
by an ``xclip -loops 1 -quiet`` instance, which runs in the foreground (no
self-daemonizing) and exits the moment it has served exactly one selection
request. Polling that process is a real signal that *some* client read the
clipboard after the paste keystroke, not a hopeful sleep -- see
:func:`_wait_for_paste_confirmation`. The restore still cannot be delayed
forever, so unconfirmed pastes are restored anyway after
``restore_delay_ms`` and reported as such via :attr:`Injected.confirmed`
rather than silently claimed as a clean success.
"""

from __future__ import annotations

import logging
import subprocess
import threading
import time
from dataclasses import dataclass

from voice_kb.config import PasteConfig

log = logging.getLogger("voice-kb.inject")

_XDOTOOL_TIMEOUT_S = 2.0
_XCLIP_TIMEOUT_S = 2.0

_CONFIRM_LOOPS = 1
"""How many served selection requests ``xclip`` waits for before exiting.

One is a lower bound, not a precise signal: a target that first queries
``TARGETS`` before pulling the actual text consumes this same loop, so
``confirmed=True`` proves *a* client touched the clipboard, not that the
text itself was already transferred. See :attr:`Injected.confirmed`.
"""

_POST_CONFIRM_SETTLE_S = 0.05
"""Extra wait after a confirmed request, before restoring.

Covers the common case where the confirmed request was a ``TARGETS`` query
immediately followed by the actual content pull -- observed to be the usual
pattern for a plain ``xclip -o``. Narrows the residual race; does not close
it (see the module docstring)."""

_POLL_INTERVAL_S = 0.01


@dataclass(frozen=True, slots=True)
class Injected:
    """The paste sequence ran to completion. ``elapsed_ms`` covers all of it,
    including whatever confirmation wait and/or restore happened.

    ``confirmed`` is ``True`` only when this function observed direct
    evidence -- a selection request served by the ``xclip`` instance holding
    the transcript -- that some client read the clipboard after the paste
    keystroke was sent. It is ``False`` when no such evidence arrived within
    ``restore_delay_ms``: the clipboard is still restored (best-effort, so a
    slow target is not left with a stuck clipboard forever), but the paste
    may have been lost, and the caller should log this rather than treat it
    as an ordinary success. This is how the residual race documented in the
    module docstring is surfaced instead of hidden.
    """

    elapsed_ms: float
    confirmed: bool


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

    Sequence: read the current clipboard, overwrite it with ``text`` via an
    ``xclip`` instance we can watch for confirmation, send the paste combo
    for the focused window's class, wait for evidence that the clipboard was
    read (up to ``config.restore_delay_ms``), then restore the original
    clipboard contents. The restore is best-effort: if the original
    clipboard could not be read (empty, holding binary data ``xclip``
    refused, or ``xclip`` itself failing), it is left holding ``text``
    rather than guessing. Whether the paste was actually confirmed is
    reported via :attr:`Injected.confirmed` -- see the module docstring for
    why confirmation cannot be made airtight with this toolset.
    """
    start = time.monotonic()

    window_class = focused_window_class()
    combo = config.combo_for(window_class)
    log.debug("target window class %r -> paste combo %r", window_class, combo)
    previous = _read_clipboard()

    confirm_proc = _write_clipboard_confirmable(text.encode("utf-8"))
    if confirm_proc is None:
        return InjectionFailed(reason="failed to set clipboard via xclip")
    # However this function returns, the holder above must eventually be
    # reaped: it runs in the foreground (not self-daemonizing, so we can
    # poll it), and it isn't necessarily reaped by anyone else -- it only
    # exits itself once it serves its one loop or loses selection ownership
    # (e.g. to the restore write below). A daemon thread doing the blocking
    # wait costs nothing and guarantees no zombie regardless of which path
    # below is taken.
    threading.Thread(target=confirm_proc.wait, daemon=True).start()

    try:
        proc = subprocess.run(
            ["xdotool", "key", "--clearmodifiers", combo],
            timeout=_XDOTOOL_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return InjectionFailed(reason=f"xdotool key {combo!r} failed: {e}")
    if proc.returncode != 0:
        return InjectionFailed(reason=f"xdotool key {combo!r} exited {proc.returncode}")

    confirmed = _wait_for_paste_confirmation(confirm_proc, config.restore_delay_ms / 1000)
    if confirmed:
        time.sleep(_POST_CONFIRM_SETTLE_S)

    if previous is not None:
        _write_clipboard(previous)  # best-effort; the paste already succeeded

    return Injected(elapsed_ms=(time.monotonic() - start) * 1000, confirmed=confirmed)


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


def _write_clipboard_confirmable(data: bytes) -> subprocess.Popen[bytes] | None:
    """Like :func:`_write_clipboard`, but returns the running process instead
    of blocking for it, so the caller can watch for it exiting.

    ``-quiet`` keeps this ``xclip`` in the foreground -- by default (also
    under ``-silent``, its default) it forks into the background and the
    launched process exits immediately, which would make the returned
    handle useless for polling. ``-loops N`` (see :data:`_CONFIRM_LOOPS`)
    makes it exit itself once it has served that many selection requests,
    on top of its unconditional exit when it loses selection ownership (to
    the restore write, or to some other client taking the clipboard).
    """
    try:
        proc = subprocess.Popen(
            [
                "xclip",
                "-i",
                "-selection",
                "clipboard",
                "-loops",
                str(_CONFIRM_LOOPS),
                "-quiet",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except OSError:
        return None
    try:
        assert proc.stdin is not None  # guaranteed by stdin=PIPE above
        proc.stdin.write(data)
        proc.stdin.close()
    except OSError:
        proc.kill()
        return None
    return proc


def _wait_for_paste_confirmation(proc: subprocess.Popen[bytes], timeout_s: float) -> bool:
    """Poll ``proc`` (an ``xclip -loops N -quiet`` instance holding the
    transcript) until it exits -- direct evidence that a selection request
    was served -- or ``timeout_s`` elapses. Returns whether it exited in
    time."""
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return True
        time.sleep(_POLL_INTERVAL_S)
    return proc.poll() is not None
