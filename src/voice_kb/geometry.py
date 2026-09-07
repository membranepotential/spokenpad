"""Pure placement math for the overlay window.

No ``xrandr``, no subprocess, no Qt: the caller (the imperative shell) is
responsible for discovering real output geometry and the focused window's
rect, and hands them in as plain :class:`Rect` values. That's what makes
this testable without an X server.
"""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass

from voice_kb.config import OverlayConfig


@dataclass(frozen=True, slots=True)
class Rect:
    """An axis-aligned pixel rectangle: X11 output geometry, a window, or
    the overlay itself.

    ``width``/``height`` rather than a second corner point, matching how
    ``xrandr`` and Qt both report geometry -- a rect with zero or negative
    area cannot represent a real monitor or window, so it's rejected here
    rather than producing nonsense areas downstream.
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
    """Choose the output that should show the overlay for a focused ``window``.

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

    Clamped into ``output`` exactly like :func:`overlay_rect`, and for the
    same reason -- a pointer near the right or bottom edge would otherwise
    anchor a window that hangs off the screen. Clamping means a pointer in the
    bottom-right corner lands the window flush in the bottom-right quarter,
    which is also the placement used when ``anchor`` is ``None`` because the
    pointer could not be read.
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


def overlay_rect(output: Rect, cfg: OverlayConfig, *, preview_band: bool) -> Rect:
    """Where the overlay goes on ``output``: horizontally centred, bottom-aligned
    with ``cfg.margin_px`` above the edge, and always fully inside ``output``.

    The naive centred/bottom-aligned position is clamped into ``output``'s
    bounds rather than trusted outright -- an overlay taller or wider than
    the margin leaves room for must still land on-screen, not hang off the
    bottom edge the way the previous tool's overlay did.

    Height comes from :meth:`OverlayConfig.total_height`, not ``height``: with
    the preview band shown the widget is taller than its status row, and
    clamping the shorter number would put the band off-screen -- the exact
    failure this clamp exists to prevent. ``preview_band`` is passed in rather
    than read off ``cfg`` because previews are policy from ``[preview]`` now,
    not an overlay setting.
    """
    height = cfg.total_height(preview_band=preview_band)
    x = output.x + (output.width - cfg.width) // 2
    y = output.bottom - cfg.margin_px - height
    x = max(output.x, min(x, output.right - cfg.width))
    y = max(output.y, min(y, output.bottom - height))
    return Rect(x=x, y=y, width=cfg.width, height=height)
