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
pub fn pick_output(outputs: &[Output], anchor: Rect) -> Option<Rect> {
    let best = outputs
        .iter()
        .enumerate()
        .max_by_key(|(i, o)| (o.rect.intersection(anchor), std::cmp::Reverse(*i)))?
        .1;
    Some(
        if best.rect.intersection(anchor) > 0 {
            best
        } else {
            outputs.iter().find(|o| o.primary).unwrap_or(&outputs[0])
        }
        .rect,
    )
}
pub fn placement(output: Rect, anchor: Option<(i32, i32)>, fraction: f64) -> Rect {
    let width = (f64::from(output.width) * fraction) as u32;
    let height = (f64::from(output.height) * fraction) as u32;
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
            placement(r, Some((-1, 1079)), 0.5),
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
                    primary: true
                }],
                Rect {
                    x: 9999,
                    y: 9999,
                    width: 1,
                    height: 1
                }
            ),
            Some(r)
        );
        assert_eq!(pick_output(&[], r), None);
    }
}
