"""Pure placement math, exercised against the real dual-4K layout:
HDMI-1-0 at 0,0 3840x2160 and eDP-1 (primary) at 3840,0 3840x2160."""

from __future__ import annotations

import pytest

from voice_kb.config import OverlayConfig
from voice_kb.geometry import Output, Rect, dictation_rect, overlay_rect, pick_output

HDMI = Output(name="HDMI-1-0", rect=Rect(x=0, y=0, width=3840, height=2160))
EDP = Output(name="eDP-1", rect=Rect(x=3840, y=0, width=3840, height=2160), primary=True)
OUTPUTS = (HDMI, EDP)


# -- overlay placement --------------------------------------------------------


def test_overlay_centred_and_bottom_aligned_on_hdmi() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32)
    rect = overlay_rect(HDMI.rect, cfg, preview_band=False)
    assert rect == Rect(x=(3840 - 420) // 2, y=2160 - 32 - 96, width=420, height=96)


def test_overlay_centred_and_bottom_aligned_on_edp() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32)
    rect = overlay_rect(EDP.rect, cfg, preview_band=False)
    # eDP-1 starts at x=3840, so its centring is offset by that origin.
    assert rect == Rect(x=3840 + (3840 - 420) // 2, y=2160 - 32 - 96, width=420, height=96)


def test_overlay_uses_total_height_when_the_preview_band_is_enabled() -> None:
    """With ``preview_band`` on the widget is ``height + preview_height`` tall,
    and placement must be computed against *that* -- otherwise the pill is
    positioned as if it were only its status row and the preview band hangs
    below where the margin says the overlay ends."""
    cfg = OverlayConfig(width=720, height=96, preview_height=64, margin_px=32)
    assert cfg.total_height(preview_band=True) == 160

    rect = overlay_rect(HDMI.rect, cfg, preview_band=True)
    assert rect == Rect(x=(3840 - 720) // 2, y=2160 - 32 - 160, width=720, height=160)


def test_overlay_is_always_fully_within_its_output() -> None:
    cfg = OverlayConfig(width=420, height=96, margin_px=32)
    for output in OUTPUTS:
        rect = overlay_rect(output.rect, cfg, preview_band=False)
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
    cfg = OverlayConfig(width=400, height=200, margin_px=-35)

    naive_y = output.bottom - cfg.margin_px - cfg.total_height(preview_band=False)
    assert naive_y == 1995  # reproduces the exact regression numbers
    assert naive_y + cfg.total_height(preview_band=False) > output.bottom  # ... off-screen

    rect = overlay_rect(output, cfg, preview_band=False)
    assert rect.bottom <= output.bottom
    assert rect.y == output.bottom - cfg.total_height(preview_band=False)


def test_preview_overlay_taller_than_the_margin_still_lands_fully_inside() -> None:
    """The same clamp, but for the failure the preview band introduces: a
    widget whose *total* height exceeds what the margin leaves room for must
    still be entirely on-screen. Clamping ``height`` instead of
    ``total_height`` would put the whole preview band off the bottom edge."""
    output = Rect(x=0, y=0, width=1280, height=200)
    cfg = OverlayConfig(width=720, height=96, preview_height=64, margin_px=80)
    assert cfg.total_height(preview_band=True) == 160
    # Naive placement would start above the output entirely.
    assert output.bottom - cfg.margin_px - cfg.total_height(preview_band=True) < output.y

    rect = overlay_rect(output, cfg, preview_band=True)
    assert rect.height == cfg.total_height(preview_band=True)
    assert rect.y >= output.y
    assert rect.bottom <= output.bottom


# -- dictation window placement ------------------------------------------------


def test_dictation_rect_is_anchored_at_the_pointers_top_left() -> None:
    output = Rect(x=0, y=0, width=3840, height=2160)
    rect = dictation_rect(output, (1000, 500), 0.5)
    assert rect == Rect(x=1000, y=500, width=1920, height=1080)


def test_dictation_rect_fraction_scales_each_axis() -> None:
    output = Rect(x=0, y=0, width=2000, height=1000)
    rect = dictation_rect(output, (0, 0), 0.25)
    assert rect.width == 500
    assert rect.height == 250


def test_dictation_rect_falls_back_to_the_bottom_right_quarter_without_a_pointer() -> None:
    """``anchor=None`` is what a pointer that could not be read produces; the
    window must still land somewhere sane rather than failing to open."""
    output = Rect(x=0, y=0, width=3840, height=2160)
    rect = dictation_rect(output, None, 0.5)
    assert rect == Rect(x=1920, y=1080, width=1920, height=1080)


def test_dictation_rect_clamps_a_pointer_near_the_right_or_bottom_edge() -> None:
    """A pointer close to an edge would otherwise anchor a window that hangs
    off the screen -- the same clamp ``overlay_rect`` applies, and for the
    same reason."""
    output = Rect(x=0, y=0, width=3840, height=2160)
    rect = dictation_rect(output, (3800, 2140), 0.5)
    assert rect.right <= output.right
    assert rect.bottom <= output.bottom
    assert rect.x == output.right - rect.width
    assert rect.y == output.bottom - rect.height


def test_dictation_rect_clamps_within_a_non_origin_output() -> None:
    output = Rect(x=3840, y=0, width=3840, height=2160)
    rect = dictation_rect(output, (10, 10), 0.5)  # anchor outside this output entirely
    assert rect.x >= output.x
    assert rect.y >= output.y
    assert rect.right <= output.right
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
