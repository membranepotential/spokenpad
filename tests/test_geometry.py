"""Pure placement math, exercised against the real dual-4K layout:
HDMI-1-0 at 0,0 3840x2160 and eDP-1 (primary) at 3840,0 3840x2160."""

from __future__ import annotations

import pytest

from voice_kb.config import OverlayConfig
from voice_kb.geometry import Output, Rect, overlay_rect, pick_output

HDMI = Output(name="HDMI-1-0", rect=Rect(x=0, y=0, width=3840, height=2160))
EDP = Output(name="eDP-1", rect=Rect(x=3840, y=0, width=3840, height=2160), primary=True)
OUTPUTS = (HDMI, EDP)


# -- overlay placement --------------------------------------------------------


def test_overlay_centred_and_bottom_aligned_on_hdmi() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32, live_preview=False)
    rect = overlay_rect(HDMI.rect, cfg)
    assert rect == Rect(x=(3840 - 420) // 2, y=2160 - 32 - 96, width=420, height=96)


def test_overlay_centred_and_bottom_aligned_on_edp() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32, live_preview=False)
    rect = overlay_rect(EDP.rect, cfg)
    # eDP-1 starts at x=3840, so its centring is offset by that origin.
    assert rect == Rect(x=3840 + (3840 - 420) // 2, y=2160 - 32 - 96, width=420, height=96)


def test_overlay_uses_total_height_when_the_preview_band_is_enabled() -> None:
    """With ``live_preview`` on the widget is ``height + preview_height`` tall,
    and placement must be computed against *that* -- otherwise the pill is
    positioned as if it were only its status row and the preview band hangs
    below where the margin says the overlay ends."""
    cfg = OverlayConfig(width=720, height=96, preview_height=64, margin_px=32, live_preview=True)
    assert cfg.total_height == 160

    rect = overlay_rect(HDMI.rect, cfg)
    assert rect == Rect(x=(3840 - 720) // 2, y=2160 - 32 - 160, width=720, height=160)


def test_overlay_is_always_fully_within_its_output() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32)
    for output in OUTPUTS:
        rect = overlay_rect(output.rect, cfg)
        assert rect.x >= output.rect.x
        assert rect.y >= output.rect.y
        assert rect.right <= output.rect.right
        assert rect.bottom <= output.rect.bottom


def test_regression_overlay_never_hangs_off_the_bottom_edge() -> None:
    """The previous tool placed a 400x200 overlay at y=1995 on a 2160-tall
    screen -- 35px hung off the bottom edge. A margin_px that would
    reproduce that naive (unclamped) y must instead be clamped fully
    on-screen."""
    output = Rect(x=0, y=0, width=3840, height=2160)
    cfg = OverlayConfig(width=400, height=200, margin_px=-35, live_preview=False)

    naive_y = output.bottom - cfg.margin_px - cfg.total_height
    assert naive_y == 1995  # reproduces the exact regression numbers
    assert naive_y + cfg.total_height > output.bottom  # ... which was off-screen

    rect = overlay_rect(output, cfg)
    assert rect.bottom <= output.bottom
    assert rect.y == output.bottom - cfg.total_height


def test_preview_overlay_taller_than_the_margin_still_lands_fully_inside() -> None:
    """The same clamp, but for the failure the preview band introduces: a
    widget whose *total* height exceeds what the margin leaves room for must
    still be entirely on-screen. Clamping ``height`` instead of
    ``total_height`` would put the whole preview band off the bottom edge."""
    output = Rect(x=0, y=0, width=1280, height=200)
    cfg = OverlayConfig(width=720, height=96, preview_height=64, margin_px=80, live_preview=True)
    assert cfg.total_height == 160
    # Naive placement would start above the output entirely.
    assert output.bottom - cfg.margin_px - cfg.total_height < output.y

    rect = overlay_rect(output, cfg)
    assert rect.height == cfg.total_height
    assert rect.y >= output.y
    assert rect.bottom <= output.bottom


# -- focus following -----------------------------------------------------------


def test_pick_output_window_fully_inside_hdmi() -> None:
    window = Rect(x=100, y=100, width=800, height=600)
    assert pick_output(OUTPUTS, window) is HDMI


def test_pick_output_window_fully_inside_edp() -> None:
    window = Rect(x=4000, y=100, width=800, height=600)
    assert pick_output(OUTPUTS, window) is EDP


def test_pick_output_picks_largest_intersection_when_window_straddles_both() -> None:
    # x range 3700..4100: 140px overlap with HDMI (ends at 3840),
    # 260px overlap with eDP (starts at 3840) -- eDP wins.
    window = Rect(x=3700, y=100, width=400, height=600)
    assert pick_output(OUTPUTS, window) is EDP


def test_pick_output_falls_back_to_primary_when_no_overlap() -> None:
    window = Rect(x=-1000, y=-1000, width=10, height=10)
    assert pick_output(OUTPUTS, window) is EDP


def test_pick_output_falls_back_to_first_output_when_none_marked_primary() -> None:
    a = Output(name="a", rect=Rect(x=0, y=0, width=100, height=100))
    b = Output(name="b", rect=Rect(x=1000, y=0, width=100, height=100))
    window = Rect(x=-1000, y=-1000, width=10, height=10)
    assert pick_output((a, b), window) is a


def test_rect_rejects_non_positive_area() -> None:
    with pytest.raises(ValueError, match="positive area"):
        Rect(x=0, y=0, width=0, height=100)
    with pytest.raises(ValueError, match="positive area"):
        Rect(x=0, y=0, width=100, height=-1)
