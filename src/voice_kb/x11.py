"""Thin X11 queries, kept out of :mod:`voice_kb.geometry` so the placement math
stays pure and testable.

Everything here shells out and can fail; failures degrade to ``None`` or an
empty list rather than raising, because a missing overlay is a papercut and a
crashed daemon is not.
"""

from __future__ import annotations

import re
import subprocess

from voice_kb.geometry import Output, Rect

_TIMEOUT = 2.0

# e.g. "HDMI-1-0 connected 3840x2160+0+0 (normal left inverted ...) 600mm x 340mm"
_OUTPUT_RE = re.compile(
    r"^(?P<name>\S+)\s+connected\s+(?P<primary>primary\s+)?"
    r"(?P<w>\d+)x(?P<h>\d+)\+(?P<x>\d+)\+(?P<y>\d+)",
    re.MULTILINE,
)


def _run(args: list[str]) -> str | None:
    try:
        proc = subprocess.run(
            args, capture_output=True, text=True, timeout=_TIMEOUT, check=False
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return proc.stdout if proc.returncode == 0 else None


def outputs() -> list[Output]:
    """Connected monitors, in xrandr order. Empty if xrandr is unavailable."""
    out = _run(["xrandr", "--query"])
    if out is None:
        return []
    found: list[Output] = []
    for m in _OUTPUT_RE.finditer(out):
        found.append(
            Output(
                name=m.group("name"),
                rect=Rect(
                    x=int(m.group("x")),
                    y=int(m.group("y")),
                    width=int(m.group("w")),
                    height=int(m.group("h")),
                ),
                primary=bool(m.group("primary")),
            )
        )
    return found


def pointer_position() -> tuple[int, int] | None:
    """The mouse pointer in root coordinates, or ``None`` if it can't be read.

    Device pixels, like everything else here -- it is compared against
    ``xrandr`` output geometry to decide which monitor the user is looking at.
    """
    out = _run(["xdotool", "getmouselocation", "--shell"])
    if out is None:
        return None
    values: dict[str, int] = {}
    for line in out.splitlines():
        key, _, raw = line.partition("=")
        if raw.lstrip("-").isdigit():
            values[key.strip()] = int(raw)
    if "X" not in values or "Y" not in values:
        return None
    return values["X"], values["Y"]


def focused_window_rect() -> Rect | None:
    """Geometry of the focused window in root coordinates.

    ``xdotool getwindowgeometry`` reports position relative to the window's
    parent under some reparenting WMs, so ``--shell`` output plus the root
    offsets it already resolves is used rather than parsing the human form.
    """
    out = _run(["xdotool", "getactivewindow", "getwindowgeometry", "--shell"])
    if out is None:
        return None
    values: dict[str, int] = {}
    for line in out.splitlines():
        key, _, raw = line.partition("=")
        if raw.lstrip("-").isdigit():
            values[key.strip()] = int(raw)
    try:
        return Rect(
            x=values["X"], y=values["Y"], width=values["WIDTH"], height=values["HEIGHT"]
        )
    except (KeyError, ValueError):
        return None
