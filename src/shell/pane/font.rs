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
//!
//! `fc-match` is a process, and spawning one costs tens of milliseconds, so
//! it is kept off the drawing path in three ways: a face already loaded for
//! another character is used when it covers this one, so a page of Japanese
//! asks fontconfig once rather than five hundred times; a paint that has
//! spent [`FALLBACK_BUDGET`] asking stops and leaves the rest to the next
//! one, which is what [`Font::deferred`] tells the caller; and a single
//! `fc-match` that does not answer is killed after [`MATCH_TIMEOUT`]. The
//! family's own faces are looked up once, when the pane opens, under the
//! looser [`LOAD_TIMEOUT`].
pub use crate::core::font::CellMetrics;
use crate::{
    config::FontFamily,
    core::font::{self as metrics, Dpi, FaceMetrics, Hinting, PixelSize, Points, Tables},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
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

    /// The fontconfig pattern for this face of the given family at a size.
    /// The size is in it because Alacritty's pattern has it: a fontconfig
    /// rule may choose a file or a hinting style by pixel size.
    fn pattern(self, family: &FontFamily, size: PixelSize) -> String {
        let mut pattern = format!("{family}:pixelsize={}", size.pixels());
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

/// How long one paint may spend asking fontconfig for fonts it has not
/// needed before.
///
/// The pane draws in the thread that owns the window, so time spent here is
/// time the window answers nothing. One `fc-match` costs about 60 ms, so in
/// practice this is "one new font per frame": the characters that did not fit
/// are blank for that frame and asked for again in the next, which the paint
/// arranges by waking the loop. Every frame settles at least one of them, so
/// it converges.
const FALLBACK_BUDGET: Duration = Duration::from_millis(50);

/// How long a single `fc-match` for a fallback character may take before it
/// is killed. Four times what it costs on a cold cache, and the bound on the
/// worst one frame can do.
const MATCH_TIMEOUT: Duration = Duration::from_millis(250);

/// How long `fc-match` may take for the configured family's own faces, when
/// the pane opens. Not the drawing path's bound: nothing is drawn yet, the
/// open has its own deadline, and on a busy machine a single `fc-match` was
/// measured at 280 ms on average (96 busy processes on 12 cores). A pane that
/// failed to open because fontconfig was slow would be worse than one that
/// opened late.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// One font file, loaded.
struct Loaded {
    data: Vec<u8>,
    index: u32,
    /// Whether this is the same file fontconfig gave for the plain face, so a
    /// bold or italic run has to be synthesised rather than looked up.
    substituted: bool,
    /// Where it came from, so the same file is not loaded twice.
    source: Matched,
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
    /// Faces fontconfig named for characters the chosen family does not
    /// cover. A dictation transcript is whatever was said, and a cell drawn
    /// as nothing looks like lost text rather than like a missing glyph.
    fallbacks: Vec<Loaded>,
    /// Which of those covers a character, or that nothing does. Misses are
    /// remembered too: asking fontconfig again on every frame would put a
    /// process spawn on the drawing path.
    fallback_of: HashMap<char, Option<usize>>,
    /// What this paint has already spent asking fontconfig, against
    /// [`FALLBACK_BUDGET`].
    spent: Duration,
    /// Whether this paint left a character unasked because that budget ran
    /// out. Nothing about it is cached, so the next paint tries again.
    deferred: bool,
    /// What fontconfig answered for some character on each 256-character
    /// page of Unicode. A page is one script's worth, so this is what makes
    /// the number of `fc-match` calls grow with the scripts on screen rather
    /// than with the characters: when the best font for a page does not have
    /// this character either, nothing does, and asking again would be a
    /// process spawn to hear the same answer.
    page_of: HashMap<u32, Option<usize>>,
    context: ScaleContext,
    metrics: CellMetrics,
    size: PixelSize,
    family: FontFamily,
}

/// Which face draws a character the chosen family does not cover.
enum Fallback {
    /// This loaded fallback face has it.
    Face(usize),
    /// Fontconfig was asked and nothing has it.
    Nothing,
    /// Nobody has asked yet, because this paint had no budget left to.
    Deferred,
}

/// What a paint got back for one grapheme.
enum Rendered {
    /// Rasterised, or missing from every font that was asked. Either way it
    /// is an answer, and it is cached.
    Settled(Option<Glyph>),
    /// The font it needs has not been looked up yet. Nothing is cached.
    Deferred,
}

impl Font {
    /// Load the family at a point size on a display of the given resolution,
    /// measuring its cells the way Alacritty does. `family` is a fontconfig
    /// family name; `"monospace"` is the one every desktop defines.
    pub fn load(family: &FontFamily, points: Points, dpi: Dpi) -> Result<Self> {
        let size = PixelSize::new(points, dpi);
        let Answer {
            found: plain,
            hinting,
        } = read_match(&Face::PLAIN.pattern(family, size), LOAD_TIMEOUT)?;
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
                face => read_match(&face.pattern(family, size), LOAD_TIMEOUT)?.found,
            };
            let substituted = face != Face::PLAIN && found == plain;
            loaded.push(Loaded {
                data: std::fs::read(&found.0)
                    .with_context(|| format!("read the font file {}", found.0.display()))?,
                index: found.1,
                substituted,
                source: found,
            });
        }
        let faces: [Loaded; 4] = loaded
            .try_into()
            .map_err(|_| anyhow::anyhow!("expected four faces"))?;
        let metrics = measure(&faces[0], size, hinting)?;
        log::debug!(
            "pane font \"{family}\" at {points} on {dpi} ({size}, {hinting:?} hinting): \
             {}, cell {}x{}, baseline {}",
            plain.0.display(),
            metrics.width,
            metrics.height,
            metrics.baseline
        );
        Ok(Self {
            faces,
            caches: Default::default(),
            context: ScaleContext::new(),
            fallbacks: Vec::new(),
            fallback_of: HashMap::new(),
            spent: Duration::ZERO,
            deferred: false,
            page_of: HashMap::new(),
            metrics,
            size,
            family: family.clone(),
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Start a paint. The budget for asking fontconfig is per frame.
    pub fn new_frame(&mut self) {
        self.spent = Duration::ZERO;
        self.deferred = false;
    }

    /// Whether this paint left a character undrawn because looking up its
    /// font would have cost the frame too much. The caller repaints, so the
    /// next frame spends its own budget and the character appears.
    pub fn deferred(&self) -> bool {
        self.deferred
    }

    /// The rasterised grapheme, or `None` when the font has nothing for it.
    ///
    /// A missing glyph is cached as a miss too: a text full of characters this
    /// font does not cover must not rasterise on every frame. A grapheme whose
    /// font has not been looked up yet is `None` as well, and is not cached.
    pub fn glyph(&mut self, text: &str, face: Face) -> Option<&Glyph> {
        let index = face.index();
        if !self.caches[index].contains_key(text) {
            match self.render(text, index) {
                Rendered::Settled(glyph) => {
                    self.caches[index].insert(text.to_owned(), glyph);
                }
                Rendered::Deferred => {
                    self.deferred = true;
                    return None;
                }
            }
        }
        self.caches[index][text].as_ref()
    }

    /// Rasterise one grapheme from the face that has it.
    fn render(&mut self, text: &str, index: usize) -> Rendered {
        let Some(character) = text.chars().next() else {
            return Rendered::Settled(None);
        };
        let fallback = match covers(&self.faces[index], character) {
            true => Fallback::Nothing,
            false => self.fallback_for(character),
        };
        let Self {
            faces,
            fallbacks,
            context,
            size,
            ..
        } = self;
        let loaded = match fallback {
            Fallback::Face(fallback) => &fallbacks[fallback],
            Fallback::Nothing => &faces[index],
            Fallback::Deferred => return Rendered::Deferred,
        };
        Rendered::Settled(rasterise(loaded, context, size.pixels(), text))
    }

    /// The face that draws a character the family does not cover, loading it
    /// the first time it is asked for.
    fn fallback_for(&mut self, character: char) -> Fallback {
        if let Some(known) = self.fallback_of.get(&character) {
            return known.map_or(Fallback::Nothing, Fallback::Face);
        }
        // A face loaded for some earlier character comes before asking
        // fontconfig again. The fallback fonts a desktop installs cover whole
        // scripts, so this turns a page of Japanese from five hundred process
        // spawns into one. It can pick a face fontconfig would not have --
        // the first one that has the character wins -- which costs a little
        // typographic nicety and is what a terminal does.
        if let Some(already) = self
            .fallbacks
            .iter()
            .position(|face| covers(face, character))
        {
            self.fallback_of.insert(character, Some(already));
            return Fallback::Face(already);
        }
        // Fontconfig always names a font, and for a character nothing on the
        // machine covers that font is simply its best guess. Once it has
        // guessed for this page, the guess stands for the whole page.
        let page = character as u32 >> 8;
        if let Some(known) = self.page_of.get(&page).copied() {
            let slot = known.filter(|slot| covers(&self.fallbacks[*slot], character));
            self.fallback_of.insert(character, slot);
            return slot.map_or(Fallback::Nothing, Fallback::Face);
        }
        if self.spent >= FALLBACK_BUDGET {
            return Fallback::Deferred;
        }
        let asked = Instant::now();
        let found = match_character(&self.family, character)
            .inspect_err(|error| {
                log::debug!("no fallback font for {character:?}: {error:#}");
            })
            .ok();
        self.spent += asked.elapsed();
        let slot = found.and_then(|found| {
            if let Some(already) = self.fallbacks.iter().position(|face| face.source == found) {
                return Some(already);
            }
            let data = std::fs::read(&found.0)
                .inspect_err(|error| {
                    log::debug!(
                        "cannot read the fallback font {}: {error}",
                        found.0.display()
                    );
                })
                .ok()?;
            log::debug!(
                "pane font: {} for {character:?}, which \"{}\" does not cover",
                found.0.display(),
                self.family
            );
            self.fallbacks.push(Loaded {
                data,
                index: found.1,
                substituted: false,
                source: found,
            });
            Some(self.fallbacks.len() - 1)
        });
        self.fallback_of.insert(character, slot);
        self.page_of.insert(page, slot);
        slot.map_or(Fallback::Nothing, Fallback::Face)
    }
}

/// Whether this face has an outline for the character at all.
fn covers(face: &Loaded, character: char) -> bool {
    face.font()
        .map(|font| font.charmap().map(character) != 0)
        .unwrap_or(false)
}

/// The font fontconfig picks for one character, preferring the configured
/// family's own idea of a substitute.
fn match_character(family: &FontFamily, character: char) -> Result<Matched> {
    read_match(
        &format!("{family}:charset={:x}", character as u32),
        MATCH_TIMEOUT,
    )
    .map(|answer| answer.found)
}

/// A font file and the face inside it.
type Matched = (PathBuf, u32);

/// What fontconfig answered for a pattern: the file, and the hinting it
/// asks for, which is what decides how FreeType rounds the advance.
struct Answer {
    found: Matched,
    hinting: Hinting,
}

/// `fc-match`, which is fontconfig's own answer to "which file is this?".
///
/// The family needs no checking here: [`FontFamily`] is parsed where the
/// configuration is read, and cannot hold anything fontconfig would take as
/// pattern syntax.
fn read_match(pattern: &str, timeout: Duration) -> Result<Answer> {
    // Not `output()`: that waits for as long as the process takes, and this
    // runs in the thread that draws the window. A fontconfig rebuilding a
    // stale cache over a network mount would otherwise hold the pane for as
    // long as it liked.
    let mut child = Command::new("fc-match")
        .arg("--format=%{file}\n%{index}\n%{hinting}\n%{hintstyle}\n%{antialias}\n")
        .arg(pattern)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run fc-match; the pane needs fontconfig to find a font")?;
    let deadline = Instant::now() + timeout;
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("fc-match did not answer for {pattern:?} within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "fc-match failed for {pattern:?}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8(output.stdout).context("fc-match printed invalid UTF-8")?;
    let mut lines = text.lines();
    let (Some(file), Some(index)) = (lines.next(), lines.next()) else {
        bail!("fc-match printed no file for {pattern:?}");
    };
    ensure!(!file.is_empty(), "fc-match found no font for {pattern:?}");
    // An element the pattern does not have prints as an empty line, which is
    // fontconfig saying nothing and crossfont taking its default.
    let mut field = || {
        lines
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let boolean = |value: Option<&str>| value.map(|value| value == "True");
    let hinting = boolean(field());
    let hintstyle = field().and_then(|value| value.parse().ok());
    let antialias = boolean(field());
    Ok(Answer {
        found: (
            PathBuf::from(file),
            index.trim().parse().unwrap_or_default(),
        ),
        hinting: Hinting::from_fontconfig(hinting, hintstyle, antialias),
    })
}

/// The cell this face makes at this size, measured as Alacritty measures it:
/// the tables go to [`metrics::cell_metrics`], which does FreeType's and
/// Alacritty's arithmetic.
///
/// The width is the advance of `0`, the glyph crossfont loads for it; a face
/// that maps no `0` gives `.notdef`'s advance, as FreeType would.
fn measure(face: &Loaded, size: PixelSize, hinting: Hinting) -> Result<CellMetrics> {
    let font = face.font()?;
    let table = |name: &[u8; 4]| font.table(swash::tag_from_bytes(name));
    let tables = Tables {
        head: table(b"head").context("the font has no head table")?,
        hhea: table(b"hhea").context("the font has no hhea table")?,
        os2: table(b"OS/2"),
        post: table(b"post"),
        truetype: table(b"glyf").is_some(),
        fpgm: table(b"fpgm").map_or(0, <[u8]>::len),
        prep: table(b"prep").map_or(0, <[u8]>::len),
    };
    let zero = font.charmap().map('0');
    let advance = font.glyph_metrics(&[]).advance_width(zero);
    ensure!(
        advance > 0.0 && advance <= f32::from(u16::MAX),
        "the font advances no width; it is not usable as a grid font"
    );
    metrics::cell_metrics(&FaceMetrics::parse(tables)?, size, advance as u16, hinting)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn font_or_skip() -> Option<Font> {
        if read_match(FontFamily::default().as_str(), LOAD_TIMEOUT).is_err() {
            assert!(
                std::env::var_os("SPOKENPAD_ALLOW_MISSING_X11").is_some(),
                "fc-match found no monospace font; the pane cannot draw without one. \
                 Set SPOKENPAD_ALLOW_MISSING_X11=1 to skip the font tests deliberately."
            );
            return None;
        }
        Some(
            Font::load(&FontFamily::default(), Points::DEFAULT, Dpi::DEFAULT)
                .expect("load the system monospace font"),
        )
    }

    #[test]
    fn a_cell_has_a_positive_size_and_a_baseline_inside_it() {
        let Some(font) = font_or_skip() else { return };
        let metrics = font.metrics();
        assert!(metrics.width > 0 && metrics.height > 0, "{metrics:?}");
        assert!(metrics.baseline > 0, "{metrics:?}");
        assert!(metrics.baseline <= metrics.height, "{metrics:?}");
        for rule in [metrics.underline, metrics.strikeout] {
            assert!(rule.thickness >= 1, "{metrics:?}");
            assert!(rule.top + rule.thickness <= metrics.height, "{metrics:?}");
        }
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
    fn a_character_the_family_does_not_cover_comes_from_another_font() {
        let Some(mut font) = font_or_skip() else {
            return;
        };
        // A dictation transcript is whatever was said. A cell drawn as
        // nothing reads as lost text, so a character the monospace family
        // has no outline for is fetched from whichever font fontconfig
        // names for it — if this machine has one at all.
        for text in ["\u{6f22}", "\u{2713}"] {
            // One character per frame: a frame's budget for asking
            // fontconfig is spent by the first of them.
            font.new_frame();
            let Some(glyph) = font.glyph(text, Face::PLAIN) else {
                // No font on this machine covers it; nothing to assert.
                continue;
            };
            assert!(glyph.width > 0 && glyph.height > 0, "{text:?}");
            match &glyph.coverage {
                Coverage::Mask(mask) => {
                    assert!(mask.iter().any(|value| *value > 0), "{text:?} is blank")
                }
                Coverage::Colour(_) => {}
            }
        }
    }

    #[test]
    fn a_page_of_a_script_the_family_lacks_costs_a_bounded_number_of_frames() {
        let Some(mut font) = font_or_skip() else {
            return;
        };
        // Two hundred distinct ideographs: the case that used to put two
        // hundred `fc-match` spawns on the drawing path, about sixty
        // milliseconds each, with the window answering nothing in between.
        // Fontconfig is asked once for the page now, and every later
        // character of it is answered from the face that came back.
        let page: Vec<String> = (0x4e00..0x4e00 + 200_u32)
            .filter_map(char::from_u32)
            .map(String::from)
            .collect();
        let mut frames = 0;
        let mut worst = Duration::ZERO;
        loop {
            frames += 1;
            assert!(
                frames <= 4,
                "{frames} frames to draw one page of one script: fontconfig is still \
                 being asked per character"
            );
            let started = Instant::now();
            font.new_frame();
            for text in &page {
                let _ = font.glyph(text, Face::PLAIN);
            }
            worst = worst.max(started.elapsed());
            if !font.deferred() {
                break;
            }
        }
        // The frame count is the question under test: a frame asks
        // fontconfig at most until its budget is gone, so few frames means
        // few questions. The wall clock is the gross bound -- rasterising two
        // hundred ideographs for the first time is most of it, and this is an
        // unoptimised build -- against the twelve seconds it used to be.
        assert!(
            worst < Duration::from_secs(1),
            "one frame spent {worst:?} on a page of new characters"
        );

        // And a frame that draws them again asks nothing at all, so the
        // window redraws at the speed of rasterising.
        let started = Instant::now();
        font.new_frame();
        for text in &page {
            let _ = font.glyph(text, Face::PLAIN);
        }
        let again = started.elapsed();
        assert!(!font.deferred());
        assert!(
            again < Duration::from_millis(10),
            "redrawing the same page cost {again:?}: something is still spawning a process"
        );
        println!(
            "200 ideographs: {frames} frame(s), worst {} ms, again {} us",
            worst.as_millis(),
            again.as_micros()
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
    fn a_display_twice_as_dense_is_the_same_as_twice_the_points() {
        // What the user reported: 16 pixels on a 192 dpi display is half the
        // size of the same text in Alacritty. A point size scaled by the
        // display is what makes the two agree, and it means resolution and
        // size are interchangeable: 12 pt at 192 dpi is 24 pt at 96.
        if font_or_skip().is_none() {
            return;
        }
        let family = FontFamily::default();
        let points = |value| Points::try_from(value).unwrap();
        let dense = Font::load(&family, points(12.0), Dpi::new(192.0).unwrap()).unwrap();
        let large = Font::load(&family, points(24.0), Dpi::DEFAULT).unwrap();
        let plain = Font::load(&family, points(12.0), Dpi::DEFAULT).unwrap();
        assert_eq!(dense.metrics(), large.metrics());
        let (dense, plain) = (dense.metrics(), plain.metrics());
        // Twice the pixels, give or take the rounding of each.
        assert!(
            dense.width.abs_diff(2 * plain.width) <= 2,
            "{dense:?} {plain:?}"
        );
        assert!(
            dense.height.abs_diff(2 * plain.height) <= 2,
            "{dense:?} {plain:?}"
        );
    }
}
