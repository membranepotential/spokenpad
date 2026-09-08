"""Pure placement math for the dictation window.

No ``xrandr``, no subprocess: the caller (the imperative shell) is
responsible for discovering real output geometry and the pointer position,
and hands them in as plain :class:`Rect` values. That's what makes this
testable without an X server.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Rect:
    """An axis-aligned pixel rectangle: X11 output geometry, a window, or
    the pointer as a 1x1 anchor.

    ``width``/``height`` rather than a second corner point, matching how
    ``xrandr`` reports geometry -- a rect with zero or negative area cannot
    represent a real monitor or window, so it's rejected here rather than
    producing nonsense areas downstream.
    """

    x: int
    y: int
    width: int
    height: int

    def __post_init__(self) -> None:
        if self.width <= 0 or self.height <= 0:
            raise ValueError(f"Rect must have positive area: {self.width}x{self.height}")

    @property
    def right(self) -> int:
        return self.x + self.width

    @property
    def bottom(self) -> int:
        return self.y + self.height

    def intersection_area(self, other: Rect) -> int:
        overlap_w = max(0, min(self.right, other.right) - max(self.x, other.x))
        overlap_h = max(0, min(self.bottom, other.bottom) - max(self.y, other.y))
        return overlap_w * overlap_h


@dataclass(frozen=True, slots=True)
class Output:
    """One monitor, as reported by ``xrandr``."""

    name: str
    rect: Rect
    primary: bool = False


def pick_output(outputs: Sequence[Output], window: Rect) -> Output:
    """Choose the output that holds ``window`` -- for the dictation window,
    a 1x1 rect at the mouse pointer.

    The output with the largest intersection with ``window`` wins. If the
    window doesn't overlap any output at all (off-screen, or the caller
    passed stale geometry), fall back to the primary output, or the first
    one if none is marked primary.
    """
    if not outputs:
        raise ValueError("pick_output requires at least one output")
    best = max(outputs, key=lambda o: o.rect.intersection_area(window))
    if best.rect.intersection_area(window) > 0:
        return best
    return next((o for o in outputs if o.primary), outputs[0])


def dictation_rect(output: Rect, anchor: tuple[int, int] | None, fraction: float) -> Rect:
    """Where the dictation window goes on ``output``.

    ``fraction`` scales each axis, so the default of 0.5 gives a window
    covering a quarter of the screen's area. ``anchor`` is the mouse pointer,
    used as the window's *top-left* corner: the window appears next to what
    the user is reading rather than in a fixed corner they have to look away
    to find.

    Clamped into ``output``: a pointer near the right or bottom edge would
    otherwise anchor a window that hangs off the screen. Clamping means a
    pointer in the bottom-right corner lands the window flush in the
    bottom-right quarter, which is also the placement used when ``anchor`` is
    ``None`` because the pointer could not be read.
    """
    width = max(1, int(output.width * fraction))
    height = max(1, int(output.height * fraction))
    if anchor is None:
        x, y = output.right - width, output.bottom - height
    else:
        x, y = anchor
    x = max(output.x, min(x, output.right - width))
    y = max(output.y, min(y, output.bottom - height))
    return Rect(x=x, y=y, width=width, height=height)

