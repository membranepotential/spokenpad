use crate::core::font::Dpi;
use std::num::NonZeroU16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Rect {
    pub fn intersection(self, other: Self) -> i64 {
        let w = (i64::from(self.x) + i64::from(self.width))
            .min(i64::from(other.x) + i64::from(other.width))
            - i64::from(self.x.max(other.x));
        let h = (i64::from(self.y) + i64::from(self.height))
            .min(i64::from(other.y) + i64::from(other.height))
            - i64::from(self.y.max(other.y));
        w.max(0) * h.max(0)
    }
}
#[derive(Debug, Clone)]
pub struct Output {
    pub rect: Rect,
    pub primary: bool,
}
/// The output the window opens on: the one under the pointer, or — with no
/// pointer to go by, or one outside every output — the primary, then the
/// first listed.
pub fn pick_output(outputs: &[Output], pointer: Option<(i32, i32)>) -> Option<Rect> {
    let under_pointer = pointer.and_then(|(x, y)| {
        let anchor = Rect {
            x,
            y,
            width: 1,
            height: 1,
        };
        outputs
            .iter()
            .enumerate()
            .max_by_key(|(i, o)| (o.rect.intersection(anchor), std::cmp::Reverse(*i)))
            .filter(|(_, o)| o.rect.intersection(anchor) > 0)
    });
    under_pointer
        .map(|(_, output)| output)
        .or_else(|| outputs.iter().find(|o| o.primary))
        .or_else(|| outputs.first())
        .map(|output| output.rect)
}
/// A window's size in character cells, as Alacritty's `window.dimensions`:
/// `{ columns = 72, lines = 20 }`. Neither can be zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dimensions {
    pub columns: NonZeroU16,
    pub lines: NonZeroU16,
}

impl Dimensions {
    /// 72 by 20: at the default 11.25 pt, 648x360 pixels at 96 dpi, which is
    /// about a third of a 1920x1080 screen each way, as the pane used to be;
    /// and the same third of a 3840x2160 screen at 192 dpi.
    pub const DEFAULT: Self = Self {
        columns: NonZeroU16::new(72).expect("not zero"),
        lines: NonZeroU16::new(20).expect("not zero"),
    };

    /// `columns` by `lines`, or `None` if either is zero.
    pub fn new(columns: u16, lines: u16) -> Option<Self> {
        Some(Self {
            columns: NonZeroU16::new(columns)?,
            lines: NonZeroU16::new(lines)?,
        })
    }

    /// These dimensions, cut down to as many cells of `cell` pixels (width,
    /// height) as fit on `output` with `padding` pixels left blank on every
    /// side, and never below one cell each way.
    pub fn fit(self, output: Rect, (cell_width, cell_height): (u32, u32), padding: u32) -> Self {
        let fitting = |wanted: NonZeroU16, extent: u32, cell: u32| {
            let room = extent.saturating_sub(padding.saturating_mul(2));
            let most = u16::try_from(room / cell.max(1)).unwrap_or(u16::MAX);
            NonZeroU16::new(wanted.get().min(most)).unwrap_or(NonZeroU16::MIN)
        };
        Self {
            columns: fitting(self.columns, output.width, cell_width),
            lines: fitting(self.lines, output.height, cell_height),
        }
    }
}

/// `logical` pixels at 96 dpi, in pixels at `dpi`: scaled by `dpi` / 96 and
/// rounded up, and never fewer than `logical`.
fn scaled(logical: u32, dpi: Dpi) -> u32 {
    let pixels = (f64::from(logical) * dpi.get() / Dpi::DEFAULT.get()).ceil();
    // `Dpi` is at most 2000, so this is far inside `u32`.
    (pixels as u32).max(logical)
}

/// How far a window's outer frame keeps from the pointer when it opens: the
/// number of pixels from the pointer to the nearest pixel of the frame,
/// along an axis on which the frame is beside the pointer.
///
/// Under focus-follows-mouse the pointer entering a window's frame focuses
/// it. A window that opens with the pointer in it, or a pixel from its edge,
/// is focused by a nudge the user did not mean as a move into it; one this
/// far away needs a deliberate move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap(u32);

impl Gap {
    /// 20 pixels at 96 dpi. No point within 6 pixels of the pointer focused
    /// a window whose frame kept 20 away, on i3 or on Openbox:
    /// `docs/experiments/2026-09-22-pane-hover-focus.md`.
    pub const LOGICAL: u32 = 20;

    /// [`Self::LOGICAL`] at `dpi`, rounded up and never below 20 pixels: a
    /// desktop at 192 dpi draws everything, frames and pointer included,
    /// twice as large, and 20 pixels there are 10 at 96 dpi.
    pub fn at(dpi: Dpi) -> Self {
        Self(scaled(Self::LOGICAL, dpi))
    }

    pub fn pixels(self) -> u32 {
        self.0
    }
}

/// How far a window manager's frame reaches past the window it holds, on
/// each side, in pixels: `_NET_FRAME_EXTENTS`, or what the frame window's
/// own rectangle says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extents {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

impl Extents {
    /// No frame: a window manager that draws none, or no window manager.
    pub const NONE: Self = Self {
        left: 0,
        right: 0,
        top: 0,
        bottom: 0,
    };

    /// The pixels on each side that a window's position has to leave for a
    /// frame before it is mapped, when nobody has said how large the frame
    /// will be: 48 at 96 dpi, scaled as [`Gap::at`] scales.
    ///
    /// 48 covers, with room to spare, a frame on either side of the position
    /// asked for, whichever way a window manager reads it (i3 puts the frame
    /// there, Openbox and KWin the window), and the largest frame measured,
    /// KWin's 36-pixel title bar: `docs/experiments/2026-09-22-pane-frame-extents.md`.
    pub fn assumed(dpi: Dpi) -> Self {
        let side = scaled(48, dpi);
        Self {
            left: side,
            right: side,
            top: side,
            bottom: side,
        }
    }

    /// How far `frame` reaches past `window`, both in root coordinates; zero
    /// on a side where it does not.
    pub fn between(window: Rect, frame: Rect) -> Self {
        let reach = |outer: i64, inner: i64| u32::try_from((outer - inner).max(0)).unwrap_or(0);
        let right = |rect: Rect| i64::from(rect.x) + i64::from(rect.width);
        let bottom = |rect: Rect| i64::from(rect.y) + i64::from(rect.height);
        Self {
            left: reach(window.x.into(), frame.x.into()),
            right: reach(right(frame), right(window)),
            top: reach(window.y.into(), frame.y.into()),
            bottom: reach(bottom(frame), bottom(window)),
        }
    }
}

/// Where a window opens on its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// Beside the pointer at `at`, its outer frame `gap` away from it.
    Pointer { at: (i32, i32), gap: Gap },
    /// Flush in the output's bottom-right corner, frame and all: where the
    /// pointer cannot be read.
    Corner,
}

/// Where a window's frame goes, on one axis, relative to the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Right of the pointer, or below it, by the gap.
    After,
    /// Left of it, or above it, by the gap: there is no room after it.
    Before,
    /// Centred on the pointer, as far as the output allows: there is room on
    /// neither side. The frame is still beside the pointer when the other
    /// axis found a side. When neither did, the window opens around the
    /// pointer: the window itself, not its frame, is centred on it and kept
    /// on the output, so the pointer is as deep inside it as the output
    /// allows, and a frame edge the output would bring within its reach lies
    /// beyond the output instead. i3 focuses a window whose frame the pointer
    /// moves onto from inside it, as much as from outside.
    Centred,
}

/// Where a frame of `size` pixels goes on one axis of an output that starts
/// at `start` and is `extent` long, with the pointer at `pointer`; see
/// [`Side`].
fn beside(start: i32, extent: u32, pointer: i32, size: u32, gap: Gap) -> (i32, Side) {
    let (start, end) = (i64::from(start), i64::from(start) + i64::from(extent));
    let (pointer, size, gap) = (i64::from(pointer), i64::from(size), i64::from(gap.pixels()));
    let fits = |origin: i64| origin >= start && origin + size <= end;
    // The first pixel `gap` past the pointer, or the last `gap` before it.
    let after = pointer + gap;
    let before = pointer - gap + 1 - size;
    let (origin, side) = if fits(after) {
        (after, Side::After)
    } else if fits(before) {
        (before, Side::Before)
    } else {
        (centred(pointer, start, end, size), Side::Centred)
    };
    (narrow(origin), side)
}

/// Where a span of `size` centred on `pointer` starts, moved as little as
/// keeps it between `start` and `end`.
fn centred(pointer: i64, start: i64, end: i64, size: i64) -> i64 {
    clamped(pointer - size / 2, start, end, size)
}

/// `origin`, moved as little as keeps a span of `size` between `start` and
/// `end`; `start` when the span is longer than that.
fn clamped(origin: i64, start: i64, end: i64, size: i64) -> i64 {
    origin.min(end - size).max(start)
}

/// Every origin here lies within an output or a window's length of one, so
/// it fits; saturate rather than wrap if it ever does not.
fn narrow(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// Where a window of `width` x `height` pixels goes on `output`, inside a
/// frame of `frame`: the window's own rectangle, with its frame placed by
/// `anchor`.
///
/// Beside the pointer, each axis is placed on its own ([`Side`]): after the
/// pointer if the frame fits there, else before it, else centred on it. The
/// pointer is outside the frame, at least the gap away, whenever one axis
/// found a side; when neither did, the window opens around it. A frame
/// larger than the output starts at its top-left corner.
pub fn placement(
    output: Rect,
    anchor: Anchor,
    (width, height): (u32, u32),
    frame: Extents,
) -> Rect {
    let outer_width = width.saturating_add(frame.left).saturating_add(frame.right);
    let outer_height = height
        .saturating_add(frame.top)
        .saturating_add(frame.bottom);
    let (x, y) = match anchor {
        Anchor::Pointer { at, gap } => match (
            beside(output.x, output.width, at.0, outer_width, gap),
            beside(output.y, output.height, at.1, outer_height, gap),
        ) {
            ((_, Side::Centred), (_, Side::Centred)) => {
                let around = |start: i32, extent: u32, pointer: i32, size: u32| {
                    let start = i64::from(start);
                    let end = start + i64::from(extent);
                    narrow(centred(pointer.into(), start, end, size.into()))
                };
                return Rect {
                    x: around(output.x, output.width, at.0, width),
                    y: around(output.y, output.height, at.1, height),
                    width,
                    height,
                };
            }
            ((x, _), (y, _)) => (x, y),
        },
        Anchor::Corner => {
            let corner = |start: i32, extent: u32, size: u32| {
                let start = i64::from(start);
                let end = start + i64::from(extent);
                narrow(clamped(end, start, end, i64::from(size)))
            };
            (
                corner(output.x, output.width, outer_width),
                corner(output.y, output.height, outer_height),
            )
        }
    };
    Rect {
        x: narrow(i64::from(x) + i64::from(frame.left)),
        y: narrow(i64::from(y) + i64::from(frame.top)),
        width,
        height,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const GAP: Gap = Gap(20);

    fn pointer_at(x: i32, y: i32) -> Anchor {
        Anchor::Pointer {
            at: (x, y),
            gap: GAP,
        }
    }
    #[test]
    fn negative_monitor_and_clamping() {
        let r = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        // No room right of or below a pointer in the bottom-right corner of
        // a monitor left of the origin: the window goes left of and above it.
        assert_eq!(
            placement(r, pointer_at(-1, 1079), (960, 540), Extents::NONE),
            Rect {
                x: -1 - 20 + 1 - 960,
                y: 1079 - 20 + 1 - 540,
                width: 960,
                height: 540
            }
        );
        assert_eq!(
            pick_output(
                &[Output {
                    rect: r,
                    primary: true,
                }],
                Some((9999, 9999))
            ),
            Some(r)
        );
        assert_eq!(pick_output(&[], Some((0, 0))), None);
        assert_eq!(pick_output(&[], None), None);
    }

    #[test]
    fn dimensions_are_cut_to_the_output_and_never_to_zero() {
        let cells = |columns, lines| Dimensions {
            columns: NonZeroU16::new(columns).unwrap(),
            lines: NonZeroU16::new(lines).unwrap(),
        };
        let output = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        // 9x18 cells: 213 columns and 60 lines fit.
        assert_eq!(cells(72, 20).fit(output, (9, 18), 0), cells(72, 20));
        assert_eq!(cells(500, 500).fit(output, (9, 18), 0), cells(213, 60));
        // The padding on both sides comes off first: 1904x1064 is left.
        assert_eq!(cells(500, 500).fit(output, (9, 18), 8), cells(211, 59));
        // A cell wider than the output still leaves one.
        assert_eq!(cells(72, 20).fit(output, (4000, 4000), 0), cells(1, 1));
        assert_eq!(cells(72, 20).fit(output, (9, 18), 5000), cells(1, 1));
        // The window then goes beside the pointer, here left of and above it.
        let rect = placement(
            output,
            pointer_at(1900, 1000),
            (72 * 9, 20 * 18),
            Extents::NONE,
        );
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (1900 - 20 + 1 - 648, 1000 - 20 + 1 - 360, 648, 360)
        );
    }

    fn screen(x: i32, primary: bool) -> Output {
        Output {
            rect: Rect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
            primary,
        }
    }

    fn pointer(x: i32) -> Option<(i32, i32)> {
        Some((x, 10))
    }

    #[test]
    fn multi_monitor_pick_follows_the_pointer_then_the_primary() {
        let outputs = [screen(-1920, false), screen(0, true), screen(1920, false)];
        for (x, expected) in [(-1000, -1920), (10, 0), (2000, 1920)] {
            assert_eq!(
                pick_output(&outputs, pointer(x)).map(|rect| rect.x),
                Some(expected),
                "pointer at {x} landed on the wrong monitor"
            );
        }
        // A pointer off every monitor, or one nobody can read, falls back to
        // the primary, not to the first-listed monitor.
        assert_eq!(
            pick_output(&outputs, pointer(i32::MAX)).map(|rect| rect.x),
            Some(0)
        );
        assert_eq!(pick_output(&outputs, None).map(|rect| rect.x), Some(0));
        // With no primary declared, the first monitor is the fallback.
        let unmarked = [screen(-1920, false), screen(0, false)];
        assert_eq!(
            pick_output(&unmarked, pointer(i32::MAX)).map(|rect| rect.x),
            Some(-1920)
        );
    }

    #[test]
    fn mirrored_monitors_tie_break_on_the_earlier_one() {
        // Two outputs covering the same corner intersect the pointer equally.
        // The tie-break is the order RandR listed them, so the choice is
        // stable across queries rather than whichever compared last.
        let mirrored = |width: u32, primary: bool| Output {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height: 1080,
            },
            primary,
        };
        assert_eq!(
            pick_output(&[mirrored(1920, false), mirrored(1280, true)], pointer(10))
                .map(|rect| rect.width),
            Some(1920)
        );
        assert_eq!(
            pick_output(&[mirrored(1280, true), mirrored(1920, false)], pointer(10))
                .map(|rect| rect.width),
            Some(1280)
        );
    }

    #[test]
    fn each_axis_goes_after_the_pointer_then_before_it_then_centred_on_it() {
        // An output from 0 to 1000, a frame 300 long.
        assert_eq!(beside(0, 1000, 100, 300, GAP), (120, Side::After));
        // The last position after the pointer that still fits.
        assert_eq!(beside(0, 1000, 680, 300, GAP), (700, Side::After));
        // One pixel further: before it, its last pixel 20 short of it.
        assert_eq!(
            beside(0, 1000, 681, 300, GAP),
            (681 - 20 + 1 - 300, Side::Before)
        );
        assert_eq!(beside(0, 1000, 999, 300, GAP), (680, Side::Before));
        // 900 long: room on neither side of a pointer at 500.
        assert_eq!(beside(0, 1000, 500, 900, GAP), (50, Side::Centred));
        // Centred as far as the output allows: flush with the nearer edge.
        assert_eq!(beside(0, 1000, 900, 900, GAP), (100, Side::Centred));
        assert_eq!(beside(0, 1000, 100, 900, GAP), (0, Side::Centred));
        // Near the edge there is room before it again.
        assert_eq!(beside(0, 1000, 990, 900, GAP), (71, Side::Before));
        // Longer than the output: from its start.
        assert_eq!(beside(-1920, 1920, -5, 4000, GAP), (-1920, Side::Centred));
        // A pointer off the output (on no monitor) still gets a frame on it.
        assert_eq!(beside(0, 1000, -50, 300, GAP), (0, Side::Centred));
        assert_eq!(beside(0, 1000, 1400, 300, GAP), (700, Side::Centred));
        assert_eq!(beside(0, 1000, -500, 300, GAP), (0, Side::Centred));
    }

    /// Everywhere on a monitor, for frames of every shape: either the frame
    /// is on the monitor and the pointer outside it, `GAP` from its nearest
    /// pixel along an axis that found a side; or neither axis found one, the
    /// window is on the monitor, and the pointer is inside it, `GAP` from
    /// every edge of it the monitor does not hold back.
    #[test]
    fn the_pointer_is_outside_the_frame_unless_no_axis_has_room() {
        let output = Rect {
            x: 1920,
            y: -200,
            width: 1280,
            height: 800,
        };
        let frames = [
            Extents::NONE,
            Extents {
                left: 2,
                right: 2,
                top: 18,
                bottom: 2,
            },
            Extents::assumed(Dpi::DEFAULT),
        ];
        let sizes = [
            (648, 360),
            (1250, 200),
            (200, 780),
            (620, 380),
            (1280, 800),
            (1000, 700),
        ];
        let (mut beside_count, mut inside_count) = (0, 0);
        for frame in frames {
            for size in sizes {
                for px in (output.x..output.x + 1280).step_by(7) {
                    for py in (output.y..output.y + 800).step_by(11) {
                        let window = placement(output, pointer_at(px, py), size, frame);
                        let outer = Rect {
                            x: window.x - frame.left as i32,
                            y: window.y - frame.top as i32,
                            width: size.0 + frame.left + frame.right,
                            height: size.1 + frame.top + frame.bottom,
                        };
                        let (_, horizontal) = beside(output.x, output.width, px, outer.width, GAP);
                        let (_, vertical) = beside(output.y, output.height, py, outer.height, GAP);
                        if horizontal == Side::Centred && vertical == Side::Centred {
                            inside_count += 1;
                            let edges = [
                                (window.x, output.x, px - window.x),
                                (
                                    window.x + size.0 as i32,
                                    output.x + output.width as i32,
                                    window.x + size.0 as i32 - 1 - px,
                                ),
                                (window.y, output.y, py - window.y),
                                (
                                    window.y + size.1 as i32,
                                    output.y + output.height as i32,
                                    window.y + size.1 as i32 - 1 - py,
                                ),
                            ];
                            for (edge, limit, depth) in edges {
                                assert!(
                                    edge == limit || depth >= 20,
                                    "{size:?} framed {frame:?} at {px},{py}: {window:?} has an \
                                     edge {depth} pixels from the pointer"
                                );
                            }
                            assert!(
                                window.x >= output.x
                                    && window.y >= output.y
                                    && window.x + size.0 as i32 <= output.x + output.width as i32
                                    && window.y + size.1 as i32 <= output.y + output.height as i32,
                                "{window:?} off the output"
                            );
                            continue;
                        }
                        beside_count += 1;
                        let fits_x = outer.width <= output.width;
                        let fits_y = outer.height <= output.height;
                        assert!(
                            !fits_x
                                || (outer.x >= output.x
                                    && outer.x + outer.width as i32
                                        <= output.x + output.width as i32),
                            "{outer:?} off the output"
                        );
                        assert!(
                            !fits_y
                                || (outer.y >= output.y
                                    && outer.y + outer.height as i32
                                        <= output.y + output.height as i32),
                            "{outer:?} off the output"
                        );
                        let distance = |start: i32, length: u32, pointer: i32| {
                            let end = start + length as i32 - 1;
                            if pointer < start {
                                start - pointer
                            } else if pointer > end {
                                pointer - end
                            } else {
                                0
                            }
                        };
                        let dx = distance(outer.x, outer.width, px);
                        let dy = distance(outer.y, outer.height, py);
                        assert!(
                            dx >= 20 || dy >= 20,
                            "{size:?} framed {frame:?} at {px},{py}: {outer:?} is {dx},{dy} \
                             from the pointer"
                        );
                        for (side, d) in [(horizontal, dx), (vertical, dy)] {
                            if side != Side::Centred {
                                assert_eq!(d, 20, "{size:?} at {px},{py}: {side:?}");
                            }
                        }
                    }
                }
            }
        }
        assert!(beside_count > 0 && inside_count > 0);
    }

    #[test]
    fn the_frame_and_not_the_window_keeps_the_gap() {
        let output = Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        // i3's floating frame: 2 pixels of border, an 18-pixel title bar.
        let i3 = Extents {
            left: 2,
            right: 2,
            top: 18,
            bottom: 2,
        };
        let window = placement(output, pointer_at(300, 200), (648, 360), i3);
        assert_eq!((window.x, window.y), (300 + 20 + 2, 200 + 20 + 18));
        // Before the pointer, the frame's far side keeps the gap.
        let window = placement(output, pointer_at(1200, 700), (648, 360), i3);
        assert_eq!(window.x + 648 - 1 + 2, 1200 - 20);
        assert_eq!(window.y + 360 - 1 + 2, 700 - 20);
    }

    #[test]
    fn with_no_room_beside_the_pointer_the_window_opens_around_it() {
        let output = Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        let i3 = Extents {
            left: 2,
            right: 2,
            top: 18,
            bottom: 2,
        };
        // In the middle of the screen: the window centred on the pointer.
        let window = placement(output, pointer_at(640, 400), (1000, 700), i3);
        assert_eq!((window.x, window.y), (140, 50));
        // Near the bottom-right corner: the window, not its frame, is flush
        // with the screen's edges, so the pointer cannot reach the border
        // there; the frame hangs past them.
        let window = placement(output, pointer_at(1275, 795), (1277, 800), i3);
        assert_eq!((window.x + 1277, window.y + 800), (1280, 800));
    }

    #[test]
    fn without_a_pointer_the_frame_is_flush_in_the_bottom_right_corner() {
        let output = Rect {
            x: -1280,
            y: 100,
            width: 1280,
            height: 800,
        };
        let frame = Extents {
            left: 1,
            right: 1,
            top: 20,
            bottom: 5,
        };
        let window = placement(output, Anchor::Corner, (648, 360), frame);
        assert_eq!(window.x + 648 + 1, 0);
        assert_eq!(window.y + 360 + 5, 900);
        // Too large for the output: from its top-left corner.
        let window = placement(output, Anchor::Corner, (2000, 360), frame);
        assert_eq!((window.x, window.y + 360 + 5), (-1280 + 1, 900));
    }

    #[test]
    fn gaps_and_assumed_frames_scale_with_the_resolution_and_round_up() {
        let dpi = |value| Dpi::new(value).expect("a resolution");
        assert_eq!(Gap::at(Dpi::DEFAULT).pixels(), 20);
        assert_eq!(Gap::at(dpi(144.0)).pixels(), 30);
        assert_eq!(Gap::at(dpi(192.0)).pixels(), 40);
        // 20.8 pixels are 21: never below 20 logical pixels.
        assert_eq!(Gap::at(dpi(100.0)).pixels(), 21);
        // Never below 20 device pixels either.
        assert_eq!(Gap::at(dpi(48.0)).pixels(), 20);
        assert_eq!(Extents::assumed(Dpi::DEFAULT).top, 48);
        assert_eq!(Extents::assumed(dpi(192.0)).left, 96);
    }

    #[test]
    fn extents_are_read_off_the_frame_around_the_window() {
        let window = Rect {
            x: 302,
            y: 218,
            width: 400,
            height: 200,
        };
        let frame = Rect {
            x: 300,
            y: 200,
            width: 404,
            height: 220,
        };
        assert_eq!(
            Extents::between(window, frame),
            Extents {
                left: 2,
                right: 2,
                top: 18,
                bottom: 2
            }
        );
        assert_eq!(Extents::between(window, window), Extents::NONE);
    }
}
