"""Pure placement math, exercised against the real dual-4K layout:
HDMI-1-0 at 0,0 3840x2160 and eDP-1 (primary) at 3840,0 3840x2160."""

from __future__ import annotations

import json

import pytest

from voice_kb import x11
from voice_kb.geometry import Output, Rect, dictation_rect, pick_output

HDMI = Output(name="HDMI-1-0", rect=Rect(x=0, y=0, width=3840, height=2160))
EDP = Output(name="eDP-1", rect=Rect(x=3840, y=0, width=3840, height=2160), primary=True)
OUTPUTS = (HDMI, EDP)


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
    off the screen."""
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


# ---------------------------------------------------------- i3 window lookup


def test_i3_window_exists_finds_a_window_nested_in_floating_nodes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The dictation window is floating, so it hangs off ``floating_nodes``
    rather than ``nodes`` -- walking only the latter would never find it."""
    tree = {
        "nodes": [
            {
                "nodes": [],
                "floating_nodes": [
                    {"window_properties": {"instance": "voice-kb"}, "nodes": []},
                ],
            }
        ]
    }
    monkeypatch.setattr(x11, "_run", lambda args: json.dumps(tree))

    assert x11.i3_window_exists("voice-kb") is True
    assert x11.i3_window_exists("something-else") is False


def test_i3_window_exists_is_false_when_i3_cannot_be_reached(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The caller's fallback is to leave placement to the window manager,
    which is the right answer when i3 is not the window manager at all."""
    monkeypatch.setattr(x11, "_run", lambda args: None)

    assert x11.i3_window_exists("voice-kb") is False


def test_i3_window_exists_survives_unparseable_output(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(x11, "_run", lambda args: "not json")

    assert x11.i3_window_exists("voice-kb") is False
