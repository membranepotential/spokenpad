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
from PySide6.QtGui import QColor, QFont, QPainter, QPainterPath, QPaintEvent
from PySide6.QtWidgets import QWidget

from voice_kb.config import OverlayConfig
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


class Overlay(QWidget):
    """Frameless status pill. Driven by :meth:`set_phase` and :meth:`push_level`."""

    #: Emitted when the overlay decides it should be hidden (post-transcribe fade).
    finished = Signal()

    def __init__(self, cfg: OverlayConfig) -> None:
        super().__init__(None)
        self._cfg = cfg
        self._phase = Phase.IDLE
        self._levels: deque[float] = deque([0.0] * _BAR_COUNT, maxlen=_BAR_COUNT)
        self._spin = 0.0

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
        self.resize(cfg.width, cfg.height)

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
        radius = h / 2.0

        path = QPainterPath()
        path.addRoundedRect(0.5, 0.5, w - 1.0, h - 1.0, radius, radius)
        p.fillPath(path, _BACKDROP)
        p.setPen(_BORDER)
        p.drawPath(path)

        pad = int(h * 0.30)
        dot_r = h * 0.10
        cx = pad + dot_r

        # Status dot: pulses while recording, orbits while transcribing.
        p.setPen(Qt.PenStyle.NoPen)
        accent = QColor(self._accent())
        if self._phase is Phase.TRANSCRIBING:
            accent.setAlphaF(0.45 + 0.55 * abs(math.sin(self._spin * math.pi)))
        p.setBrush(accent)
        p.drawEllipse(int(cx - dot_r), int(h / 2 - dot_r), int(dot_r * 2), int(dot_r * 2))

        # Label
        font = QFont(self.font())
        font.setPointSizeF(max(9.0, h * 0.20))
        font.setWeight(QFont.Weight.Medium)
        p.setFont(font)
        p.setPen(_TEXT)
        text_x = int(cx + dot_r + pad * 0.6)
        metrics = p.fontMetrics()
        label = self._label()
        p.drawText(text_x, int(h / 2 + metrics.capHeight() / 2), label)

        # Level meter fills the remaining width, only while recording.
        if self._phase is not Phase.RECORDING:
            return
        meter_x = text_x + metrics.horizontalAdvance(label) + pad
        # The right edge is a half-circle, so pad by the radius rather than by
        # `pad` -- otherwise the last bars crowd the curve.
        meter_w = int(w - meter_x - radius * 0.55)
        if meter_w < _BAR_COUNT * 2:
            return
        self._paint_meter(p, meter_x, meter_w, h)

    def _paint_meter(self, p: QPainter, x: int, width: int, h: int) -> None:
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
