"""The recording overlay.

The only UI in v1, so it carries the whole interface budget: it must say what
the daemon is doing at a glance, from across the room, without ever getting in
the way.

Two properties are non-negotiable, both learned the hard way:

* **It must never take focus.** In the tool this replaces, clicking the overlay
  moved X focus away from the target window and aborted the in-flight
  transcription. ``WA_ShowWithoutActivating`` plus ``WindowDoesNotAcceptFocus``
  plus the ``Tool`` window type keeps it inert.
* **It must sit fully on screen.** That same tool placed a 400x200 overlay at
  y=1995 on a 2160px-tall output, hanging 35px off the bottom. Placement is
  computed in :mod:`voice_kb.geometry` and passed in here as absolute
  coordinates -- this module never guesses.

Qt is used rather than GTK4 because GTK4 removed both ``Window.move()`` and
``set_type_hint()``, which makes a positioned, non-focusable X11 window
impossible to express.
"""

from __future__ import annotations

import math
from collections import deque
from typing import Final

from PySide6.QtCore import Qt, QTimer, Signal
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetrics,
    QGuiApplication,
    QPainter,
    QPainterPath,
    QPaintEvent,
)
from PySide6.QtWidgets import QWidget

from voice_kb.config import OverlayConfig
from voice_kb.geometry import Rect
from voice_kb.state import Phase

# A dark translucent pill reads correctly over both light and dark windows,
# which a theme-following surface would not.
_BACKDROP: Final = QColor(18, 18, 22, 235)
_BORDER: Final = QColor(255, 255, 255, 28)
_TEXT: Final = QColor(238, 238, 242)
_MUTED: Final = QColor(238, 238, 242, 140)
_ACCENT_RECORDING: Final = QColor(239, 83, 80)
_ACCENT_WORKING: Final = QColor(120, 170, 255)

_BAR_COUNT: Final = 32
_FRAME_MS: Final = 33  # ~30 fps; enough for a level meter, cheap enough to ignore

_MAX_RADIUS: Final = 28.0
"""Cap on the pill's corner radius, applied only when the preview band is on.

An unconditional ``height / 2`` is right for a short status pill and wrong for
a tall one -- with the preview band the widget is ~160px tall, and an 80px
radius turns the pill into a lozenge. Capping it unconditionally has the
opposite problem: it turns the short pill into a rounded rectangle. So the cap
is applied only where it is needed, and the preview-off overlay keeps the exact
capsule it has always had.
"""

_PREVIEW_LINES: Final = 2
_PREVIEW_ALPHA: Final = 0.85


def screen_rect(name: str) -> Rect | None:
    """The named screen's geometry **in Qt's own coordinate space**.

    Everything else in this project measures the desktop in device pixels,
    because that is what ``xrandr`` and ``xdotool`` report. Qt does not: with a
    device pixel ratio above 1 (this machine sets ``Xft.dpi: 192``, so Qt uses
    2.0), :meth:`QWidget.move` and :meth:`QWidget.resize` take *logical* units.

    Passing device pixels to ``move()`` is silently wrong rather than loudly
    wrong, and it put the overlay 1776px below the bottom of a 2160px screen --
    mapped, viewable, correctly painted, and completely invisible. Offscreen
    render tests could not catch it, because they never involve a screen.

    There is no single factor to divide by, either: Qt reports screen *origins*
    in device pixels but screen *sizes* in logical ones (here, eDP-1 is
    ``(3840, 0, 1920, 1080)`` for a 3840x2160 panel at x=3840). So this returns
    Qt's rect verbatim, and the rule for callers is simply: anything handed to
    Qt must be computed from a Qt rect.

    ``xrandr`` remains the right source for deciding *which* output to use --
    that is a question about the physical desktop, and the focused window's
    geometry is in device pixels too. Only the placement maths moves into Qt's
    coordinate space.
    """
    if QGuiApplication.instance() is None:
        return None
    for candidate in QGuiApplication.screens():
        if candidate.name() == name:
            g = candidate.geometry()
            return Rect(x=g.x(), y=g.y(), width=g.width(), height=g.height())
    return None


class Overlay(QWidget):
    """Frameless status pill. Driven by :meth:`set_phase` and :meth:`push_level`."""

    #: Emitted when the overlay decides it should be hidden (post-transcribe fade).
    finished = Signal()

    def __init__(self, cfg: OverlayConfig, *, preview_band: bool) -> None:
        super().__init__(None)
        self._cfg = cfg
        #: Whether the preview band is drawn at all. Passed in rather than
        #: read off ``cfg``: previews are ``[preview]`` policy shared with the
        #: nvim indicator, and this widget only decides how to draw them.
        self._preview_band = preview_band
        self._phase = Phase.IDLE
        self._levels: deque[float] = deque([0.0] * _BAR_COUNT, maxlen=_BAR_COUNT)
        self._spin = 0.0
        self._preview = ""

        self.setWindowFlags(
            Qt.WindowType.FramelessWindowHint
            | Qt.WindowType.WindowStaysOnTopHint
            | Qt.WindowType.Tool
            | Qt.WindowType.WindowDoesNotAcceptFocus
            | Qt.WindowType.BypassWindowManagerHint
        )
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.setAttribute(Qt.WidgetAttribute.WA_ShowWithoutActivating)
        # Belt and braces: even if a WM ignores the flags, refuse the focus.
        self.setFocusPolicy(Qt.FocusPolicy.NoFocus)
        self.resize(cfg.width, cfg.total_height(preview_band=preview_band))

        self._timer = QTimer(self)
        self._timer.setInterval(_FRAME_MS)
        self._timer.timeout.connect(self._tick)

    # ------------------------------------------------------------------ control

    def show_at(self, x: int, y: int) -> None:
        """Place at absolute screen coordinates and show without stealing focus."""
        self.move(x, y)
        if not self.isVisible():
            self.show()
        self.raise_()

    def set_phase(self, phase: Phase) -> None:
        if phase == self._phase:
            return
        self._phase = phase
        if phase is Phase.IDLE:
            self._levels.extend([0.0] * _BAR_COUNT)
            self._preview = ""
            self._timer.stop()
            self.hide()
            self.finished.emit()
        else:
            if not self._timer.isActive():
                self._timer.start()
            self.update()

    def push_level(self, level: float) -> None:
        """Feed the meter. ``level`` is 0.0-1.0; called from the Qt thread only."""
        self._levels.append(max(0.0, min(1.0, level)))

    def set_preview_text(self, text: str) -> None:
        """Show ``text`` in the preview band; ``""`` clears it.

        Purely cosmetic. Nothing shown here is ever injected -- the committed
        transcript comes from a single decode of the whole buffer at key
        release (``docs/constraints.md``). Called from the Qt thread only.
        """
        if text == self._preview:
            return
        self._preview = text
        if self._preview_band:
            self.update()

    # ------------------------------------------------------------------ internals

    def _tick(self) -> None:
        if self._phase is Phase.TRANSCRIBING:
            self._spin = (self._spin + _FRAME_MS / 900.0) % 1.0
        self.update()

    def _label(self) -> str:
        match self._phase:
            case Phase.RECORDING:
                return "Listening"
            case Phase.TRANSCRIBING:
                return "Transcribing"
            case Phase.IDLE:
                return ""

    def _accent(self) -> QColor:
        return _ACCENT_RECORDING if self._phase is Phase.RECORDING else _ACCENT_WORKING

    # --------------------------------------------------------------------- paint

    def paintEvent(self, event: QPaintEvent) -> None:
        del event
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)

        w, h = self.width(), self.height()
        # `status_h` is the widget height when the preview is off, and the top
        # band of it when the preview is on. Every measurement in the status
        # row derives from `status_h`, never from `h`, so turning the preview
        # on does not move the dot, the label or the meter.
        status_h = min(self._cfg.height, h)
        radius = min(h / 2.0, _MAX_RADIUS) if self._preview_band else h / 2.0

        path = QPainterPath()
        path.addRoundedRect(0.5, 0.5, w - 1.0, h - 1.0, radius, radius)
        p.fillPath(path, _BACKDROP)
        p.setPen(_BORDER)
        p.drawPath(path)

        self._paint_status_row(p, w, status_h)
        self._paint_preview(p, w, h, status_h)

    def _paint_status_row(self, p: QPainter, w: int, status_h: int) -> None:
        """Dot, label and level meter, laid out within the top ``status_h`` pixels."""
        pad = int(status_h * 0.30)
        dot_r = status_h * 0.10
        cx = pad + dot_r

        # Status dot: pulses while recording, orbits while transcribing.
        p.setPen(Qt.PenStyle.NoPen)
        accent = QColor(self._accent())
        if self._phase is Phase.TRANSCRIBING:
            accent.setAlphaF(0.45 + 0.55 * abs(math.sin(self._spin * math.pi)))
        p.setBrush(accent)
        p.drawEllipse(int(cx - dot_r), int(status_h / 2 - dot_r), int(dot_r * 2), int(dot_r * 2))

        # Label
        font = QFont(self.font())
        font.setPointSizeF(max(9.0, status_h * 0.20))
        font.setWeight(QFont.Weight.Medium)
        p.setFont(font)
        p.setPen(_TEXT)
        text_x = int(cx + dot_r + pad * 0.6)
        metrics = p.fontMetrics()
        label = self._label()
        p.drawText(text_x, int(status_h / 2 + metrics.capHeight() / 2), label)

        # Level meter fills the remaining width, only while recording.
        if self._phase is not Phase.RECORDING:
            return
        meter_x = text_x + metrics.horizontalAdvance(label) + pad
        # The status row's right edge is a half-circle, so pad by *that* row's
        # radius rather than by `pad` -- otherwise the last bars crowd the
        # curve. Deliberately `status_h / 2`, not the pill's (capped) corner
        # radius: the meter must not shift when the preview band makes the
        # widget taller.
        meter_w = int(w - meter_x - (status_h / 2.0) * 0.55)
        if meter_w < _BAR_COUNT * 2:
            return
        self._paint_meter(p, meter_x, meter_w, status_h)

    def _paint_preview(self, p: QPainter, w: int, h: int, status_h: int) -> None:
        """The rolling transcript preview, in the band below the status row.

        Cosmetic only: this text never reaches the clipboard. It exists so the
        user can see they are being heard, while the text that actually gets
        injected still comes from one decode of the whole buffer at key
        release (``docs/constraints.md``).
        """
        band_h = h - status_h
        if not self._preview_band or band_h <= 0 or not self._preview:
            return

        font = QFont(self.font())
        font.setPointSizeF(max(8.0, status_h * 0.17))
        p.setFont(font)
        metrics = p.fontMetrics()

        pad = int(status_h * 0.30)  # same horizontal inset as the status row
        avail = w - 2 * pad
        if avail <= 0:
            return
        lines = _tail_lines(self._preview, metrics, avail, _PREVIEW_LINES)
        if not lines:
            return

        colour = QColor(_TEXT)
        colour.setAlphaF(_PREVIEW_ALPHA)
        p.setPen(colour)

        line_h = metrics.height()
        top = status_h + (band_h - line_h * len(lines)) / 2.0
        for i, line in enumerate(lines):
            p.drawText(pad, int(top + line_h * i + metrics.ascent()), line)

    def _paint_meter(self, p: QPainter, x: int, width: int, h: int) -> None:
        """``h`` is the *status row* height, not the widget's."""
        gap = 2.0
        bar_w = max(1.0, (width - gap * (_BAR_COUNT - 1)) / _BAR_COUNT)
        max_h = h * 0.42
        mid = h / 2.0
        p.setPen(Qt.PenStyle.NoPen)
        for i, level in enumerate(self._levels):
            # Perceptual curve: quiet speech should still move the meter.
            amp = level**0.6
            bar_h = max(2.0, amp * max_h)
            colour = QColor(_ACCENT_RECORDING if amp > 0.02 else _MUTED)
            colour.setAlphaF(0.35 + 0.65 * min(1.0, amp * 1.6))
            p.setBrush(colour)
            bx = x + i * (bar_w + gap)
            p.drawRoundedRect(
                int(bx), int(mid - bar_h / 2), int(bar_w), int(bar_h), bar_w / 2, bar_w / 2
            )


def _tail_lines(text: str, metrics: QFontMetrics, avail: int, max_lines: int) -> list[str]:
    """Wrap the *end* of ``text`` into at most ``max_lines`` lines of ``avail`` px.

    Lines are built back-to-front: the newest words are the ones that matter
    in a live preview, so it is the head that gets cut, never the tail. Any
    words that do not fit are folded into the first line and elided with
    :attr:`Qt.TextElideMode.ElideLeft`, which renders as a leading ellipsis.
    Each line is passed through ``elidedText`` regardless, so a single word
    wider than ``avail`` is truncated rather than overflowing the pill.
    """
    words = text.split()
    if not words:
        return []

    end = len(words)
    spans: list[tuple[int, int]] = []
    while end > 0 and len(spans) < max_lines:
        # Always take at least the last remaining word, then grow leftwards
        # while the joined line still fits.
        start = end - 1
        while start > 0 and metrics.horizontalAdvance(" ".join(words[start - 1 : end])) <= avail:
            start -= 1
        spans.append((start, end))
        end = start

    spans.reverse()
    lines = [" ".join(words[start:stop]) for start, stop in spans]
    if end > 0:
        # Words older than the last `max_lines` lines: prepend them so the
        # elide below shows the user that the head was cut.
        lines[0] = " ".join(words[:end]) + " " + lines[0]
    return [metrics.elidedText(line, Qt.TextElideMode.ElideLeft, avail) for line in lines]
