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
    /// The output holding keyboard focus, where the window manager reports one
    /// (sway does; i3 does not).
    pub focused: bool,
}
/// The output the window opens on: the one under the pointer, or — with no
/// pointer to go by, or one outside every output — the focused output, then
/// the primary, then the first listed.
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
        .or_else(|| outputs.iter().find(|o| o.focused))
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

/// `fraction` of `output` on each axis, in pixels, at least one.
pub fn fraction_of(output: Rect, fraction: f64) -> (u32, u32) {
    let scale = |extent: u32| ((f64::from(extent) * fraction) as u32).max(1);
    (scale(output.width), scale(output.height))
}

/// A window of `width` x `height` pixels on `output`, with its top-left
/// corner at `anchor` (the pointer) or, with none, in the bottom-right
/// corner; clamped so the whole window is on the output.
pub fn placement(output: Rect, anchor: Option<(i32, i32)>, (width, height): (u32, u32)) -> Rect {
    let width = width.max(1).min(output.width);
    let height = height.max(1).min(output.height);
    let right = output.x + (output.width - width) as i32;
    let bottom = output.y + (output.height - height) as i32;
    let (x, y) = anchor.unwrap_or((right, bottom));
    Rect {
        x: x.clamp(output.x, right),
        y: y.clamp(output.y, bottom),
        width,
        height,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negative_monitor_and_clamping() {
        let r = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            placement(r, Some((-1, 1079)), fraction_of(r, 0.5)),
            Rect {
                x: -960,
                y: 540,
                width: 960,
                height: 540
            }
        );
        assert_eq!(
            pick_output(
                &[Output {
                    rect: r,
                    primary: true,
                    focused: false,
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
        // The window is then clamped whole onto the output at the pointer.
        let rect = placement(output, Some((1900, 1000)), (72 * 9, 20 * 18));
        assert_eq!(
            (rect.x, rect.y, rect.width, rect.height),
            (1920 - 648, 1080 - 360, 648, 360)
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
            focused: false,
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
        // A focused output (sway reports one) wins over the primary.
        let mut focused = outputs.clone();
        focused[2].focused = true;
        assert_eq!(pick_output(&focused, None).map(|rect| rect.x), Some(1920));
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
        // The tie-break is the order the window manager listed them, so the choice is
        // stable across queries rather than whichever compared last.
        let mirrored = |width: u32, primary: bool| Output {
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height: 1080,
            },
            primary,
            focused: false,
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
}
