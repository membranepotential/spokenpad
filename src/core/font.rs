//! The pane's font size and cell metrics, computed the way Alacritty computes
//! them, so that `nvim.font_size = 12` in the pane and `font.size = 12` in
//! Alacritty give the same cells on the same display.
//!
//! Alacritty 0.17 does it in three layers, and each is reproduced here with its
//! own arithmetic, rounding included:
//!
//! 1. **Points to pixels** ([`PixelSize::new`]). winit's X11 scale factor is
//!    `Xft.dpi / 96`; Alacritty scales the configured point size by it, and
//!    crossfont 0.8 turns points into pixels as `pt * 96 / 72`, keeping the
//!    size in millionths of a point on the way. FreeType is then asked for
//!    that size in 26.6 fixed point.
//! 2. **FreeType's size metrics** ([`cell_metrics`]). `FT_Set_Char_Size`
//!    scales the face's ascender, descender and line height and grid-fits
//!    them: ascender rounded up, descender down, height to the nearest pixel.
//!    The advance of `0` is what crossfont calls the average advance, and how
//!    it is rounded depends on the hinting fontconfig asks for ([`Hinting`]).
//! 3. **The cell** (`compute_cell_size` in Alacritty's display). The width is
//!    the advance, the height the larger of FreeType's line height and
//!    ascender minus descender, both floored. The baseline sits the descent
//!    above the cell's bottom, and underline and strikeout are placed by
//!    Alacritty's `create_rect`.
//!
//! Sources, read for this module, are listed in
//! `docs/experiments/2026-09-22-pane-hidpi.md`.
use anyhow::{Context, Result, ensure};
use std::fmt;

/// A font size in points, which is what `nvim.font_size` holds and what
/// Alacritty's `font.size` means.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, serde::Deserialize)]
#[serde(try_from = "f32")]
pub struct Points(f32);

impl Points {
    /// Alacritty's own default, so an unconfigured pane matches an
    /// unconfigured Alacritty.
    pub const DEFAULT: Self = Self(11.25);
    /// Anything smaller cannot be read and anything larger does not fit a
    /// usable number of cells on any screen.
    const MIN: f32 = 1.0;
    const MAX: f32 = 200.0;

    pub fn get(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for Points {
    type Error = anyhow::Error;

    fn try_from(value: f32) -> Result<Self> {
        ensure!(
            value.is_finite() && (Self::MIN..=Self::MAX).contains(&value),
            "nvim.font_size must be a size in points in [{}, {}], as in Alacritty's font.size",
            Self::MIN,
            Self::MAX
        );
        Ok(Self(value))
    }
}

impl fmt::Display for Points {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} pt", self.0)
    }
}

/// The display's resolution in dots per inch, as the `Xft.dpi` resource
/// states it.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Dpi(f64);

impl Dpi {
    /// What X11 toolkits assume when nobody set `Xft.dpi`: a scale factor
    /// of one.
    pub const DEFAULT: Self = Self(96.0);

    pub fn new(value: f64) -> Option<Self> {
        // winit accepts any positive, finite factor; the bound above keeps a
        // typo such as `Xft.dpi: 19200` from asking for a 3000-pixel font.
        (value.is_finite() && value > 0.0 && value <= 2000.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// The blank margin the pane keeps around its grid on every side, in
    /// pixels: four at 96 dpi, scaled by `Xft.dpi` / 96 and rounded down, as
    /// Alacritty scales its `window.padding`. Fixed, not a setting: the grid
    /// touching the window's edge looked cramped, and four logical pixels is
    /// the small margin asked for.
    pub fn padding(self) -> u32 {
        const LOGICAL: f64 = 4.0;
        (LOGICAL * self.0 / Self::DEFAULT.0).floor() as u32
    }
}

impl fmt::Display for Dpi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} dpi", self.0)
    }
}

/// What the display's `Xft.dpi` resource says, as the X resource database
/// answers the query `Xft.dpi` (wildcards such as `*dpi` and the last of
/// equal entries included; the shell asks x11rb's database, as winit does).
#[derive(Debug, Clone, PartialEq)]
pub enum XftDpi {
    Set(Dpi),
    /// No entry matches.
    Unset,
    /// An entry matches but is not a resolution the pane can use; the text
    /// is kept so the user can be told what was found.
    Unusable(String),
}

impl XftDpi {
    /// Parse the resource's value, as winit does: a floating-point number,
    /// surrounding whitespace ignored.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            None => Self::Unset,
            Some(text) => text
                .trim()
                .parse()
                .ok()
                .and_then(Dpi::new)
                .map_or_else(|| Self::Unusable(text.to_owned()), Self::Set),
        }
    }

    /// The resolution the pane uses: the resource's, or 96.
    pub fn dpi(&self) -> Dpi {
        match self {
            Self::Set(dpi) => *dpi,
            Self::Unset | Self::Unusable(_) => Dpi::DEFAULT,
        }
    }
}

impl fmt::Display for XftDpi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set(dpi) => write!(formatter, "{dpi} (Xft.dpi)"),
            Self::Unset => write!(formatter, "{} (Xft.dpi is not set)", Dpi::DEFAULT),
            Self::Unusable(text) => write!(
                formatter,
                "{} (Xft.dpi is {text:?}, not a resolution in (0, 2000])",
                Dpi::DEFAULT
            ),
        }
    }
}

/// The size a face is rasterised at, in FreeType's 26.6 fixed point: what
/// Alacritty hands `FT_Set_Char_Size` for a point size on a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelSize(i64);

impl PixelSize {
    /// crossfont's arithmetic, in its own `f32` steps: the configured size and
    /// the scaled one are each stored as whole millionths of a point, and the
    /// pixel size is rounded to 1/64 only when FreeType is asked for it.
    pub fn new(points: Points, dpi: Dpi) -> Self {
        fn micro_points(points: f32) -> u32 {
            (points.clamp(1.0, 3999.0) * 1_000_000.0) as u32
        }
        fn as_points(micro: u32) -> f32 {
            (f64::from(micro) / 1_000_000.0) as f32
        }
        let scale = (dpi.0 / 96.0) as f32;
        let scaled = micro_points(as_points(micro_points(points.0)) * scale);
        let pixels = as_points(scaled) * 96.0 / 72.0;
        Self((64.0 * pixels).round() as i64)
    }

    /// The size in pixels, as a rasteriser takes it.
    pub fn pixels(self) -> f32 {
        self.0 as f32 / 64.0
    }
}

impl fmt::Display for PixelSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} px", self.pixels())
    }
}

/// How FreeType hints the face, which decides how the advance is rounded.
///
/// Derived from fontconfig's answer the way crossfont derives its load flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hinting {
    /// `FT_LOAD_NO_HINTING`: the advance keeps its fraction, and the cell
    /// width is its floor.
    None,
    /// `FT_LOAD_TARGET_LIGHT`, fontconfig's `hintslight` and the usual
    /// desktop default. A TrueType face goes to the autohinter, which scales
    /// by the requested size and rounds the advance to a whole pixel.
    Light,
    /// Every other mode. A TrueType face with bytecode is hinted natively,
    /// at a whole-pixel em when its `head` table asks for that.
    Full,
}

impl Hinting {
    /// crossfont's `ft_load_flags`: `hinting` and `antialias` default to
    /// true and `hintstyle` to full when fontconfig does not say.
    pub fn from_fontconfig(
        hinting: Option<bool>,
        hintstyle: Option<u8>,
        antialias: Option<bool>,
    ) -> Self {
        const NONE: u8 = 0;
        const SLIGHT: u8 = 1;
        const FULL: u8 = 3;
        let style = match hinting.unwrap_or(true) {
            true => hintstyle.unwrap_or(FULL),
            false => NONE,
        };
        match (antialias.unwrap_or(true), style) {
            (_, NONE) => Self::None,
            (true, SLIGHT) => Self::Light,
            _ => Self::Full,
        }
    }
}

/// What a face's tables say about its vertical metrics, in font units, as
/// FreeType reads them into its `FT_Face`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceMetrics {
    units_per_em: i64,
    ascender: i64,
    descender: i64,
    height: i64,
    /// `FT_Face.underline_position`: the `post` table's position moved from
    /// the top of the stroke to its middle.
    underline_position: i64,
    underline_thickness: i64,
    /// OS/2's strikeout position and size, when the face has that table.
    strikeout: Option<(i64, i64)>,
    /// Whether native hinting scales this face at a whole-pixel em: TrueType
    /// outlines with hinting bytecode, and bit 3 of `head.flags`.
    integer_ppem: bool,
}

/// The raw tables [`FaceMetrics::parse`] reads, as a font file holds them.
#[derive(Debug, Clone, Copy)]
pub struct Tables<'a> {
    pub head: &'a [u8],
    pub hhea: &'a [u8],
    pub os2: Option<&'a [u8]>,
    pub post: Option<&'a [u8]>,
    /// Whether the outlines are TrueType (`glyf`) rather than CFF.
    pub truetype: bool,
    /// The `fpgm` and `prep` tables' sizes, which tell FreeType whether the
    /// face carries hinting bytecode at all.
    pub fpgm: usize,
    pub prep: usize,
}

fn u16_at(table: &[u8], offset: usize) -> Option<u16> {
    table
        .get(offset..offset + 2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn i16_at(table: &[u8], offset: usize) -> Option<i64> {
    u16_at(table, offset).map(|value| i64::from(value as i16))
}

impl FaceMetrics {
    /// FreeType's choice of vertical metrics (`sfobjs.c`): OS/2's typographic
    /// values when its `USE_TYPO_METRICS` bit is set, `hhea` otherwise, and
    /// OS/2 again when `hhea` holds zeros.
    pub fn parse(tables: Tables<'_>) -> Result<Self> {
        let units_per_em = u16_at(tables.head, 18).context("the font's head table is short")?;
        let flags = u16_at(tables.head, 16).context("the font's head table is short")?;
        ensure!(units_per_em > 0, "the font has no units per em");
        let short = || "the font's hhea table is short";
        let hhea_ascender = i16_at(tables.hhea, 4).with_context(short)?;
        let hhea_descender = i16_at(tables.hhea, 6).with_context(short)?;
        let hhea_gap = i16_at(tables.hhea, 8).with_context(short)?;
        // FreeType treats an OS/2 table too short for version 0 as absent.
        let os2 = tables.os2.filter(|table| table.len() >= 78);
        let typo = os2.map(|table| {
            let field = |offset| i16_at(table, offset).unwrap_or(0);
            (field(68), field(70), field(72))
        });
        let use_typo = os2
            .and_then(|table| u16_at(table, 62))
            .is_some_and(|selection| selection & 0x80 != 0);
        let (ascender, descender, height) = match (use_typo, typo) {
            (true, Some((ascender, descender, gap))) => {
                (ascender, descender, ascender - descender + gap)
            }
            _ if hhea_ascender != 0 || hhea_descender != 0 => (
                hhea_ascender,
                hhea_descender,
                hhea_ascender - hhea_descender + hhea_gap,
            ),
            _ => match (typo, os2) {
                (Some((ascender, descender, gap)), _) if ascender != 0 || descender != 0 => {
                    (ascender, descender, ascender - descender + gap)
                }
                (_, Some(table)) => {
                    let ascent = i64::from(u16_at(table, 74).unwrap_or(0));
                    let descent = i64::from(u16_at(table, 76).unwrap_or(0));
                    (ascent, -descent, ascent + descent)
                }
                _ => (hhea_ascender, hhea_descender, hhea_gap),
            },
        };
        let (underline_position, underline_thickness) = tables
            .post
            .and_then(|table| Some((i16_at(table, 8)?, i16_at(table, 10)?)))
            // C's integer division truncates towards zero, and so does Rust's.
            .map(|(position, thickness)| (position - thickness / 2, thickness))
            .unwrap_or((0, 0));
        let strikeout = os2.and_then(|table| Some((i16_at(table, 28)?, i16_at(table, 26)?)));
        let bytecode = tables.fpgm > 0 || tables.prep > 7;
        Ok(Self {
            units_per_em: i64::from(units_per_em),
            ascender,
            descender,
            height,
            underline_position,
            underline_thickness,
            strikeout,
            integer_ppem: tables.truetype && bytecode && flags & 8 != 0,
        })
    }
}

/// One horizontal rule inside a cell: an underline or a strikeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// Distance from the top of the cell down to the rule's first row.
    pub top: u32,
    /// How many rows of pixels it covers, at least one.
    pub thickness: u32,
}

/// What one cell of the grid measures, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Distance from the top of the cell down to the baseline.
    pub baseline: u32,
    pub underline: Rule,
    pub strikeout: Rule,
}

impl CellMetrics {
    /// How thick the outline of an unfocused cursor is: Alacritty's default
    /// `cursor.thickness` of 0.15 of a cell's width.
    pub fn hollow_cursor(self) -> u32 {
        ((0.15 * self.width as f32).round() as u32).max(1)
    }
}

/// `FT_MulFix`: `a * b / 65536`, rounded half away from zero.
fn mul_fix(a: i64, b: i64) -> i64 {
    let product = a * b;
    (product + 0x8000 + (product >> 63)) >> 16
}

/// `FT_DivFix`: `a * 65536 / b`, rounded half away from zero.
fn div_fix(a: i64, b: i64) -> i64 {
    let quotient = ((a.abs() << 16) + (b.abs() >> 1)) / b.abs();
    match (a < 0) != (b < 0) {
        true => -quotient,
        false => quotient,
    }
}

fn pix_floor(value: i64) -> i64 {
    value & !63
}

fn pix_round(value: i64) -> i64 {
    pix_floor(value + 32)
}

fn pix_ceil(value: i64) -> i64 {
    pix_floor(value + 63)
}

/// The cell a face makes at a size, as Alacritty measures it.
///
/// `advance` is the advance width, in font units, of the glyph the face maps
/// `0` to (`.notdef`'s when it maps it to none), which is the glyph crossfont
/// loads for the width.
pub fn cell_metrics(
    face: &FaceMetrics,
    size: PixelSize,
    advance: u16,
    hinting: Hinting,
) -> Result<CellMetrics> {
    // `FT_Request_Metrics` for a nominal size: one scale for both axes, and
    // the size metrics crossfont reads are grid-fitted at that scale.
    let scale = div_fix(size.0, face.units_per_em);
    let ascender = pix_ceil(mul_fix(face.ascender, scale));
    let descender = pix_floor(mul_fix(face.descender, scale));
    let line = pix_round(mul_fix(face.height, scale));
    let advance = i64::from(advance);
    let advance = match hinting {
        Hinting::None => mul_fix(advance, scale),
        Hinting::Light => pix_round(mul_fix(advance, scale)),
        Hinting::Full => {
            let scale = match face.integer_ppem {
                true => div_fix(((size.0 + 32) >> 6) << 6, face.units_per_em),
                false => scale,
            };
            pix_round(mul_fix(advance, scale))
        }
    };
    let width = (advance >> 6).max(1);
    // Both terms are whole pixels, so Alacritty's floor changes nothing.
    let height = (line.max(ascender - descender) >> 6).max(1);
    let descent = (descender >> 6) as f32;
    let cell_height = height as f32;

    // crossfont's decorations, in pixels, scaled by the horizontal scale.
    let x_scale = scale as f32 / 65536.0;
    let pixels = |units: i64| units as f32 * x_scale / 64.0;
    let (underline_position, underline_thickness) = match pixels(face.underline_position) {
        // Bitmap fonts carry no underline; crossfont derives one.
        0.0 => (descent / 2.0, (descent.abs() / 5.0).round()),
        position => (position, pixels(face.underline_thickness)),
    };
    let (strikeout_position, strikeout_thickness) = match face.strikeout {
        Some((position, size)) => (pixels(position), pixels(size)),
        None => (cell_height / 2.0 + descent, underline_thickness),
    };
    let rule = |position: f32, thickness: f32| {
        // Alacritty's `create_rect`, with the cell's top as zero.
        let thickness = thickness.max(1.0);
        let baseline = cell_height + descent;
        let top = (baseline - position - thickness / 2.0)
            .round()
            .min(cell_height - thickness)
            .max(0.0);
        // A rectangle drawn by the GPU covers the pixel rows whose centres
        // it contains.
        Rule {
            top: top as u32,
            thickness: ((thickness - 0.5).ceil() as u32).max(1),
        }
    };
    ensure!(
        width < 1 << 16 && height < 1 << 16,
        "a {width}x{height} pixel cell is larger than any window"
    );
    Ok(CellMetrics {
        width: width as u32,
        height: height as u32,
        baseline: (height + (descender >> 6)).clamp(0, height) as u32,
        underline: rule(underline_position, underline_thickness),
        strikeout: rule(strikeout_position, strikeout_thickness),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_padding_is_four_logical_pixels_rounded_down() {
        let padding = |dpi: f64| Dpi::new(dpi).expect("a resolution").padding();
        assert_eq!(padding(96.0), 4);
        assert_eq!(padding(144.0), 6);
        assert_eq!(padding(192.0), 8);
        assert_eq!(padding(108.0), 4);
        assert_eq!(padding(12.0), 0);
    }

    /// `fsSelection`, the typographic ascender, descender and gap, and the
    /// strikeout size and position.
    type Os2 = (u16, (i16, i16, i16), (i16, i16));

    /// The header fields [`FaceMetrics::parse`] reads, laid out as a font
    /// file holds them.
    struct Face {
        units_per_em: u16,
        flags: u16,
        hhea: (i16, i16, i16),
        os2: Option<Os2>,
        underline: (i16, i16),
        truetype: bool,
        bytecode: bool,
    }

    fn put(table: &mut [u8], offset: usize, value: i16) {
        table[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    impl Face {
        fn metrics(&self) -> FaceMetrics {
            let mut head = vec![0; 54];
            put(&mut head, 16, self.flags as i16);
            put(&mut head, 18, self.units_per_em as i16);
            let mut hhea = vec![0; 36];
            put(&mut hhea, 4, self.hhea.0);
            put(&mut hhea, 6, self.hhea.1);
            put(&mut hhea, 8, self.hhea.2);
            let os2 = self.os2.map(|(selection, typo, strikeout)| {
                let mut os2 = vec![0; 96];
                put(&mut os2, 26, strikeout.0);
                put(&mut os2, 28, strikeout.1);
                put(&mut os2, 62, selection as i16);
                put(&mut os2, 68, typo.0);
                put(&mut os2, 70, typo.1);
                put(&mut os2, 72, typo.2);
                os2
            });
            let mut post = vec![0; 32];
            put(&mut post, 8, self.underline.0);
            put(&mut post, 10, self.underline.1);
            FaceMetrics::parse(Tables {
                head: &head,
                hhea: &hhea,
                os2: os2.as_deref(),
                post: Some(&post),
                truetype: self.truetype,
                fpgm: if self.bytecode { 1000 } else { 0 },
                prep: 0,
            })
            .expect("a well-formed face")
        }
    }

    /// Source Code Pro as the Nerd Fonts patch ships it
    /// (`SauceCodeProNerdFontMono-Regular.ttf`): typographic metrics on, and
    /// no integer ppem (`head.flags` is 23, bit 3 clear).
    const SAUCE_CODE_PRO: Face = Face {
        units_per_em: 1000,
        flags: 23,
        hhea: (984, -273, 0),
        os2: Some((0x01c0, (984, -273, 0), (50, 291))),
        underline: (-50, 50),
        truetype: true,
        bytecode: true,
    };
    const SAUCE_CODE_PRO_ZERO: u16 = 600;

    /// Noto Mono, what `monospace` resolved to on the machine this was
    /// measured on: `hhea` metrics and integer ppem (`head.flags` is 11).
    const NOTO_MONO: Face = Face {
        units_per_em: 2048,
        flags: 11,
        hhea: (1900, -500, 0),
        os2: Some((0x0040, (1900, -500, 0), (102, 498))),
        underline: (-154, 102),
        truetype: true,
        bytecode: true,
    };
    const NOTO_MONO_ZERO: u16 = 1229;

    fn points(value: f32) -> Points {
        Points::try_from(value).unwrap()
    }

    fn dpi(value: f64) -> Dpi {
        Dpi::new(value).unwrap()
    }

    fn cell(face: &Face, zero: u16, size: f32, on: f64, hinting: Hinting) -> (u32, u32) {
        let metrics = cell_metrics(
            &face.metrics(),
            PixelSize::new(points(size), dpi(on)),
            zero,
            hinting,
        )
        .unwrap();
        (metrics.width, metrics.height)
    }

    #[test]
    fn cells_are_the_ones_alacritty_measures() {
        // Every expected value was measured with Alacritty 0.17.0 itself, on
        // an Xvfb with `Xft.dpi` set, from its window size at 100x40 cells:
        // see docs/experiments/2026-09-22-pane-hidpi.md.
        use Hinting::{Full, Light};
        let sauce =
            |size, on, hinting| cell(&SAUCE_CODE_PRO, SAUCE_CODE_PRO_ZERO, size, on, hinting);
        let noto = |size, on, hinting| cell(&NOTO_MONO, NOTO_MONO_ZERO, size, on, hinting);
        // The user's setting on the user's display, and on a plain one.
        assert_eq!(sauce(12.0, 192.0, Light), (19, 41));
        assert_eq!(sauce(12.0, 96.0, Light), (10, 21));
        // Alacritty's default size, at every common scale.
        assert_eq!(sauce(11.25, 96.0, Light), (9, 20));
        assert_eq!(sauce(11.25, 120.0, Light), (11, 25));
        assert_eq!(sauce(11.25, 144.0, Light), (14, 30));
        assert_eq!(sauce(11.25, 168.0, Light), (16, 34));
        assert_eq!(sauce(11.25, 192.0, Light), (18, 39));
        assert_eq!(sauce(16.5, 168.0, Light), (23, 49));
        assert_eq!(noto(12.0, 96.0, Light), (10, 19));
        assert_eq!(noto(12.0, 192.0, Light), (19, 38));
        assert_eq!(noto(16.5, 120.0, Light), (17, 33));
        // Without hinting the advance keeps its fraction and the width is
        // its floor: 9.6 pixels make a 9-pixel cell.
        assert_eq!(sauce(12.0, 96.0, Hinting::None), (9, 21));
        assert_eq!(noto(12.0, 96.0, Hinting::None), (9, 19));
        assert_eq!(noto(9.0, 168.0, Hinting::None), (12, 26));
        // Full hinting scales Noto Mono at a whole-pixel em: 15.5 pixels are
        // hinted as 16, and the advance rounds up where light hinting's
        // rounds down. Source Code Pro does not ask for that.
        assert_eq!(noto(11.625, 96.0, Light), (9, 19));
        assert_eq!(noto(11.625, 96.0, Full), (10, 19));
        assert_eq!(noto(9.3, 120.0, Full), (10, 19));
        assert_eq!(sauce(11.625, 96.0, Full), (9, 21));
    }

    #[test]
    fn the_baseline_and_rules_sit_where_alacritty_draws_them() {
        let metrics = cell_metrics(
            &SAUCE_CODE_PRO.metrics(),
            PixelSize::new(points(12.0), dpi(192.0)),
            SAUCE_CODE_PRO_ZERO,
            Hinting::Light,
        )
        .unwrap();
        // 32 pixels: the descender is 0.273 em, 8.7 pixels, floored to 9, and
        // the baseline is that far above the bottom of a 41-pixel cell.
        assert_eq!(metrics.baseline, 32);
        // FreeType puts the underline's middle 75 units below the baseline,
        // 2.4 pixels, and makes it 50 units thick, 1.6 pixels: Alacritty's
        // rectangle starts at round(32 + 2.4 - 0.8) = 34 and covers two rows.
        assert_eq!(
            metrics.underline,
            Rule {
                top: 34,
                thickness: 2
            }
        );
        // OS/2's strikeout is 291 units up, 9.3 pixels: round(32 - 9.3 - 0.8).
        assert_eq!(
            metrics.strikeout,
            Rule {
                top: 22,
                thickness: 2
            }
        );
        // 0.15 of a 19-pixel cell.
        assert_eq!(metrics.hollow_cursor(), 3);
    }

    #[test]
    fn a_point_is_four_thirds_of_a_pixel_at_96_dpi_and_scales_with_the_display() {
        assert_eq!(PixelSize::new(points(12.0), Dpi::DEFAULT).pixels(), 16.0);
        assert_eq!(PixelSize::new(points(12.0), dpi(192.0)).pixels(), 32.0);
        assert_eq!(PixelSize::new(points(11.25), Dpi::DEFAULT).pixels(), 15.0);
        // A fractional size is asked for in 64ths of a pixel.
        assert_eq!(PixelSize::new(points(11.25), dpi(120.0)).pixels(), 18.75);
        assert_eq!(PixelSize::new(points(13.0), Dpi::DEFAULT).0, 1109);
    }

    #[test]
    fn the_resolution_is_the_xft_dpi_value_or_96_with_a_reason() {
        assert_eq!(XftDpi::parse(Some("192")), XftDpi::Set(dpi(192.0)));
        assert_eq!(XftDpi::parse(Some(" 144.5 ")), XftDpi::Set(dpi(144.5)));
        assert_eq!(XftDpi::parse(None), XftDpi::Unset);
        assert_eq!(XftDpi::parse(None).dpi(), Dpi::DEFAULT);
        // Nonsense or impossible: 96, and the text is kept to be reported.
        for text in ["large", "0", "-96", "inf", "NaN", "19200", ""] {
            let parsed = XftDpi::parse(Some(text));
            assert_eq!(parsed, XftDpi::Unusable(text.to_owned()), "{text:?}");
            assert_eq!(parsed.dpi(), Dpi::DEFAULT);
        }
        assert_eq!(
            XftDpi::parse(Some("large")).to_string(),
            "96 dpi (Xft.dpi is \"large\", not a resolution in (0, 2000])"
        );
        assert_eq!(
            XftDpi::parse(None).to_string(),
            "96 dpi (Xft.dpi is not set)"
        );
        assert_eq!(XftDpi::parse(Some("192")).to_string(), "192 dpi (Xft.dpi)");
    }

    #[test]
    fn a_point_size_is_one_alacritty_would_accept_and_a_person_could_read() {
        assert_eq!(Points::DEFAULT.get(), 11.25);
        for good in [1.0, 11.25, 12.0, 200.0] {
            assert!(Points::try_from(good).is_ok(), "{good}");
        }
        for bad in [0.0, 0.5, -12.0, 200.5, f32::NAN, f32::INFINITY] {
            assert!(Points::try_from(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn hinting_follows_fontconfig_as_crossfont_reads_it() {
        use Hinting::{Full, Light};
        // Nothing said: crossfont assumes hinting, antialiasing, and full.
        assert_eq!(Hinting::from_fontconfig(None, None, None), Full);
        assert_eq!(
            Hinting::from_fontconfig(Some(true), Some(1), Some(true)),
            Light
        );
        assert_eq!(
            Hinting::from_fontconfig(Some(true), Some(2), Some(true)),
            Full
        );
        assert_eq!(
            Hinting::from_fontconfig(Some(true), Some(0), Some(true)),
            Hinting::None
        );
        assert_eq!(
            Hinting::from_fontconfig(Some(false), Some(1), Some(true)),
            Hinting::None
        );
        // Without antialiasing, slight hinting becomes monochrome hinting,
        // which is not the light mode.
        assert_eq!(
            Hinting::from_fontconfig(Some(true), Some(1), Some(false)),
            Full
        );
    }

    #[test]
    fn vertical_metrics_come_from_the_table_freetype_trusts() {
        // `USE_TYPO_METRICS` set: OS/2's typographic values, whatever hhea says.
        let typo = Face {
            hhea: (800, -200, 100),
            os2: Some((0x80, (700, -300, 50), (50, 250))),
            ..SAUCE_CODE_PRO
        }
        .metrics();
        assert_eq!(
            (typo.ascender, typo.descender, typo.height),
            (700, -300, 1050)
        );
        // Not set: hhea.
        let hhea = Face {
            hhea: (800, -200, 100),
            os2: Some((0, (700, -300, 50), (50, 250))),
            ..SAUCE_CODE_PRO
        }
        .metrics();
        assert_eq!(
            (hhea.ascender, hhea.descender, hhea.height),
            (800, -200, 1100)
        );
        // hhea empty: OS/2's typographic values after all.
        let empty = Face {
            hhea: (0, 0, 0),
            os2: Some((0, (700, -300, 50), (50, 250))),
            ..SAUCE_CODE_PRO
        }
        .metrics();
        assert_eq!(
            (empty.ascender, empty.descender, empty.height),
            (700, -300, 1050)
        );
        // The underline position moves from the stroke's top to its middle.
        assert_eq!(SAUCE_CODE_PRO.metrics().underline_position, -75);
        // Only hinted TrueType with bit 3 of head.flags scales at a whole em.
        assert!(NOTO_MONO.metrics().integer_ppem);
        assert!(!SAUCE_CODE_PRO.metrics().integer_ppem);
        let unhinted = Face {
            bytecode: false,
            ..NOTO_MONO
        };
        assert!(!unhinted.metrics().integer_ppem);
        let cff = Face {
            truetype: false,
            ..NOTO_MONO
        };
        assert!(!cff.metrics().integer_ppem);
    }

    #[test]
    fn fixed_point_rounds_half_away_from_zero_like_freetype() {
        assert_eq!(mul_fix(3, 0x8000), 2);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        assert_eq!(mul_fix(1, 0x7fff), 0);
        assert_eq!(div_fix(1, 2), 0x8000);
        assert_eq!(div_fix(-1, 2), -0x8000);
        assert_eq!(div_fix(1024, 1000), 67109);
        assert_eq!(pix_floor(-1), -64);
        assert_eq!(pix_ceil(-1), 0);
        assert_eq!(pix_round(31), 0);
        assert_eq!(pix_round(32), 64);
    }
}
