//! The pane's monospace font: which file, what a cell measures, and a cache
//! of rasterised glyphs.
//!
//! The face comes from `fc-match`, so the pane uses the font the rest of the
//! desktop calls "monospace" and honours the user's fontconfig rules. Reading
//! a whole font database instead would mean parsing several thousand faces at
//! every start to answer one question fontconfig has already answered.
//!
//! Glyphs are rasterised with swash, hinted, into 8-bit coverage, and kept per
//! grapheme and face. A cell holds a grapheme rather than a character, so a
//! base letter and its combining marks are rendered onto one bitmap: there is
//! no shaper here, and stacking the marks at the same pen position is what a
//! monospace grid wants anyway.
use anyhow::{Context, Result, bail, ensure};
use std::{collections::HashMap, path::PathBuf, process::Command};
use swash::{
    FontRef, GlyphId,
    scale::{Render, ScaleContext, Source, StrikeWith, image::Content},
};

/// Which of the four faces a cell is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Face {
    pub bold: bool,
    pub italic: bool,
}

impl Face {
    pub const PLAIN: Self = Self {
        bold: false,
        italic: false,
    };

    /// The fontconfig pattern for this face of the given family.
    fn pattern(self, family: &str) -> String {
        let mut pattern = family.to_owned();
        if self.bold {
            pattern.push_str(":bold");
        }
        if self.italic {
            pattern.push_str(":italic");
        }
        pattern
    }

    fn index(self) -> usize {
        usize::from(self.bold) | (usize::from(self.italic) << 1)
    }

    /// The face a highlight asks for.
    pub fn of(bold: bool, italic: bool) -> Self {
        Self { bold, italic }
    }
}

/// What one cell of the grid measures, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMetrics {
    pub width: u32,
    pub height: u32,
    /// Distance from the top of the cell down to the baseline.
    pub baseline: u32,
    /// Where an underline sits, as a distance below the baseline.
    pub underline: i32,
    /// How thick an underline or strikethrough is drawn.
    pub thickness: u32,
    /// Distance from the top of the cell down to a strikethrough.
    pub strikethrough: u32,
}

/// A rasterised grapheme, positioned relative to the pen: `left` from the pen
/// and `top` **above** the baseline, which is swash's convention.
#[derive(Debug, Clone)]
pub struct Glyph {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub coverage: Coverage,
}

/// What a rasterised glyph holds per pixel.
#[derive(Debug, Clone)]
pub enum Coverage {
    /// One byte of coverage per pixel, to be blended with the text colour.
    Mask(Vec<u8>),
    /// Four bytes per pixel, already coloured: an emoji or another colour
    /// glyph, which is drawn as it is.
    Colour(Vec<u8>),
}

/// One font file, loaded.
struct Loaded {
    data: Vec<u8>,
    index: u32,
    /// Whether this is the same file fontconfig gave for the plain face, so a
    /// bold or italic run has to be synthesised rather than looked up.
    substituted: bool,
}

impl Loaded {
    fn font(&self) -> Result<FontRef<'_>> {
        FontRef::from_index(&self.data, self.index as usize)
            .context("the font file holds no face at that index")
    }
}

/// The font the pane draws with, and everything already rasterised from it.
pub struct Font {
    faces: [Loaded; 4],
    caches: [HashMap<String, Option<Glyph>>; 4],
    context: ScaleContext,
    metrics: CellMetrics,
    size: f32,
}

impl Font {
    /// Load the family at the given pixel size. `family` is a fontconfig
    /// pattern name; `"monospace"` is the one every desktop defines.
    pub fn load(family: &str, size: f32) -> Result<Self> {
        ensure!(
            (4.0..=400.0).contains(&size),
            "font size {size} is outside 4..400 pixels"
        );
        let plain = match_face(Face::PLAIN, family)?;
        let faces = [
            Face::PLAIN,
            Face {
                bold: true,
                italic: false,
            },
            Face {
                bold: false,
                italic: true,
            },
            Face {
                bold: true,
                italic: true,
            },
        ];
        let mut loaded = Vec::with_capacity(4);
        for face in faces {
            let found = match face {
                Face::PLAIN => plain.clone(),
                face => match_face(face, family)?,
            };
            let substituted = face != Face::PLAIN && found == plain;
            loaded.push(Loaded {
                data: std::fs::read(&found.0)
                    .with_context(|| format!("read the font file {}", found.0.display()))?,
                index: found.1,
                substituted,
            });
        }
        let faces: [Loaded; 4] = loaded
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected four faces"))?;
        let metrics = measure(&faces[0], size)?;
        log::debug!(
            "pane font {family:?} at {size}px: {} cell {}x{}, baseline {}",
            plain.0.display(),
            metrics.width,
            metrics.height,
            metrics.baseline
        );
        Ok(Self {
            faces,
            caches: Default::default(),
            context: ScaleContext::new(),
            metrics,
            size,
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// The rasterised grapheme, or `None` when the font has nothing for it.
    ///
    /// A missing glyph is cached as a miss too: a text full of characters this
    /// font does not cover must not rasterise on every frame.
    pub fn glyph(&mut self, text: &str, face: Face) -> Option<&Glyph> {
        let index = face.index();
        let Self {
            faces,
            caches,
            context,
            size,
            ..
        } = self;
        if !caches[index].contains_key(text) {
            let rendered = rasterise(&faces[index], context, *size, text);
            caches[index].insert(text.to_owned(), rendered);
        }
        caches[index][text].as_ref()
    }
}

/// `fc-match`, which is fontconfig's own answer to "which file is this?".
fn match_face(face: Face, family: &str) -> Result<(PathBuf, u32)> {
    ensure!(
        !family.is_empty() && !family.contains([':', ',', '-', '\\']),
        "font family {family:?} is not a plain fontconfig family name"
    );
    let output = Command::new("fc-match")
        .arg("--format=%{file}\n%{index}\n")
        .arg(face.pattern(family))
        .output()
        .context("run fc-match; the pane needs fontconfig to find a font")?;
    ensure!(
        output.status.success(),
        "fc-match failed for {family:?}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("fc-match printed invalid UTF-8")?;
    let mut lines = text.lines();
    let (Some(file), Some(index)) = (lines.next(), lines.next()) else {
        bail!("fc-match printed no file for {family:?}");
    };
    ensure!(!file.is_empty(), "fc-match found no font for {family:?}");
    Ok((
        PathBuf::from(file),
        index.trim().parse().unwrap_or_default(),
    ))
}

/// A cell is as wide as the font advances and as tall as a line of it.
///
/// The width comes from a glyph rather than from `max_width`, because a
/// monospace face advances every glyph the same and `max_width` counts
/// whatever the widest outline happens to be.
fn measure(face: &Loaded, size: f32) -> Result<CellMetrics> {
    let font = face.font()?;
    let metrics = font.metrics(&[]).scale(size);
    let charmap = font.charmap();
    let advance = ['M', 'x', '0']
        .into_iter()
        .map(|character| charmap.map(character))
        .find(|glyph| *glyph != 0)
        .map(|glyph| font.glyph_metrics(&[]).scale(size).advance_width(glyph))
        .filter(|advance| *advance > 0.0)
        .unwrap_or(metrics.average_width);
    ensure!(
        advance > 0.0,
        "the font advances no width at {size} pixels; it is not usable as a grid font"
    );
    let width = advance.ceil().max(1.0) as u32;
    let height = (metrics.ascent + metrics.descent + metrics.leading)
        .ceil()
        .max(1.0) as u32;
    let baseline = metrics.ascent.ceil().max(0.0) as u32;
    let thickness = metrics.stroke_size.round().max(1.0) as u32;
    // `underline_offset` is negative below the baseline; the renderer wants a
    // distance downwards, and a zero offset would draw on the baseline itself.
    let underline = (-metrics.underline_offset).round() as i32;
    let strikethrough = baseline.saturating_sub(metrics.strikeout_offset.round().max(0.0) as u32);
    Ok(CellMetrics {
        width,
        height,
        baseline,
        underline: underline.max(1),
        thickness,
        strikethrough,
    })
}

/// Rasterise one grapheme: the base character, with any combining marks drawn
/// over it at the same pen position, into one bitmap.
fn rasterise(face: &Loaded, context: &mut ScaleContext, size: f32, text: &str) -> Option<Glyph> {
    let font = face.font().ok()?;
    let charmap = font.charmap();
    let glyphs: Vec<GlyphId> = text
        .chars()
        .map(|character| charmap.map(character))
        .collect();
    if glyphs.is_empty() || glyphs[0] == 0 {
        return None;
    }
    let mut scaler = context.builder(font).size(size).hint(true).build();
    // Faux bold when fontconfig had no bold face to give: the strength is the
    // usual fraction of the em, and it is the difference between a bold run
    // looking bold and looking like everything else.
    let embolden = if face.substituted { size * 0.03 } else { 0.0 };
    let render = {
        let mut render = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ]);
        render.embolden(embolden);
        render
    };
    let images: Vec<_> = glyphs
        .iter()
        .filter(|glyph| **glyph != 0)
        .filter_map(|glyph| render.render(&mut scaler, *glyph))
        .filter(|image| image.placement.width > 0 && image.placement.height > 0)
        .collect();
    let first = images.first()?;
    // A colour glyph stands on its own: there is nothing sensible to blend a
    // combining mark into an emoji with, and no monospace grid needs it.
    if first.content == Content::Color {
        return Some(Glyph {
            left: first.placement.left,
            top: first.placement.top,
            width: first.placement.width,
            height: first.placement.height,
            coverage: Coverage::Colour(first.data.clone()),
        });
    }
    let masks: Vec<_> = images
        .iter()
        .filter(|image| image.content == Content::Mask)
        .collect();
    let first = masks.first()?;
    let (mut left, mut top) = (first.placement.left, first.placement.top);
    let (mut right, mut bottom) = (
        left + first.placement.width as i32,
        top - first.placement.height as i32,
    );
    for image in &masks[1..] {
        left = left.min(image.placement.left);
        top = top.max(image.placement.top);
        right = right.max(image.placement.left + image.placement.width as i32);
        bottom = bottom.min(image.placement.top - image.placement.height as i32);
    }
    let width = (right - left).max(0) as u32;
    let height = (top - bottom).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    let mut coverage = vec![0_u8; (width * height) as usize];
    for image in masks {
        let offset_x = (image.placement.left - left) as u32;
        let offset_y = (top - image.placement.top) as u32;
        for row in 0..image.placement.height {
            for column in 0..image.placement.width {
                let source = image.data[(row * image.placement.width + column) as usize];
                let target = ((row + offset_y) * width + column + offset_x) as usize;
                // Marks overlay rather than replace: whichever covers the
                // pixel more wins, so a mark never erases the letter under it.
                coverage[target] = coverage[target].max(source);
            }
        }
    }
    Some(Glyph {
        left,
        top,
        width,
        height,
        coverage: Coverage::Mask(coverage),
    })
}

/// Whether the font this pane needs can be found at all.
pub fn available() -> bool {
    match_face(Face::PLAIN, "monospace").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font_or_skip() -> Option<Font> {
        if !available() {
            assert!(
                std::env::var_os("SPOKENPAD_ALLOW_MISSING_X11").is_some(),
                "fc-match found no monospace font; the pane cannot draw without one. \
                 Set SPOKENPAD_ALLOW_MISSING_X11=1 to skip the font tests deliberately."
            );
            return None;
        }
        Some(Font::load("monospace", 16.0).expect("load the system monospace font"))
    }

    #[test]
    fn a_cell_has_a_positive_size_and_a_baseline_inside_it() {
        let Some(font) = font_or_skip() else { return };
        let metrics = font.metrics();
        assert!(metrics.width > 0 && metrics.height > 0, "{metrics:?}");
        assert!(metrics.baseline > 0, "{metrics:?}");
        assert!(metrics.baseline <= metrics.height, "{metrics:?}");
        assert!(metrics.thickness >= 1, "{metrics:?}");
    }

    #[test]
    fn ordinary_letters_rasterise_and_a_space_does_not() {
        let Some(mut font) = font_or_skip() else {
            return;
        };
        for text in ["a", "M", "ü", "ß", "0"] {
            let glyph = font
                .glyph(text, Face::PLAIN)
                .unwrap_or_else(|| panic!("no glyph for {text:?}"));
            assert!(glyph.width > 0 && glyph.height > 0, "{text:?}");
            match &glyph.coverage {
                Coverage::Mask(mask) => {
                    assert_eq!(mask.len(), (glyph.width * glyph.height) as usize);
                    assert!(mask.iter().any(|value| *value > 0), "{text:?} is blank");
                }
                Coverage::Colour(data) => {
                    assert_eq!(data.len(), (glyph.width * glyph.height * 4) as usize)
                }
            }
        }
        // A space has no outline, and asking for it twice must not rasterise
        // twice either.
        assert!(font.glyph(" ", Face::PLAIN).is_none());
        assert!(font.glyph(" ", Face::PLAIN).is_none());
    }

    #[test]
    fn a_grapheme_with_a_combining_mark_is_taller_than_its_base_letter() {
        let Some(mut font) = font_or_skip() else {
            return;
        };
        let Some(plain) = font.glyph("e", Face::PLAIN).cloned() else {
            return;
        };
        let Some(combined) = font.glyph("e\u{301}", Face::PLAIN).cloned() else {
            return;
        };
        assert!(
            combined.top >= plain.top && combined.height >= plain.height,
            "the acute accent should extend the bitmap upwards: {plain:?} vs {combined:?}"
        );
    }

    #[test]
    fn text_the_font_cannot_draw_is_a_miss_rather_than_a_panic() {
        let Some(mut font) = font_or_skip() else {
            return;
        };
        // A private-use code point no font maps, and the empty text a
        // double-width continuation cell holds.
        assert!(font.glyph("\u{f8ff}", Face::PLAIN).is_none());
        assert!(font.glyph("", Face::PLAIN).is_none());
    }

    #[test]
    fn a_family_name_that_is_a_fontconfig_pattern_is_refused() {
        for family in ["", "monospace:bold", "mono,serif", "a\\b", "x-y"] {
            assert!(
                Font::load(family, 16.0).is_err(),
                "{family:?} should not be accepted as a family name"
            );
        }
    }

    #[test]
    fn an_unusable_size_is_refused_before_anything_is_loaded() {
        assert!(Font::load("monospace", 0.0).is_err());
        assert!(Font::load("monospace", 1e6).is_err());
    }
}
