//! Neovim's screen, as a value.
//!
//! A Neovim attached with `ext_linegrid` describes its whole display as one
//! grid of cells and a stream of changes to it: "this run of cells now reads
//! this", "copy this rectangle up by two rows", "highlight 7 means bold red".
//! [`RedrawEvent`] is that stream, typed, and [`Screen`] is the grid the
//! events fold into. [`Screen::apply`] returns the [`Damage`] one event did,
//! so the renderer can redraw the rows that changed instead of all of them.
//!
//! Everything here is pure. The window, the font and the process are in
//! `shell::pane`; the events arrive there as msgpack and are parsed by
//! [`RedrawEvent::parse_batch`] before they reach this side.
//!
//! Only the events an `ext_linegrid` UI without any other `ext_` option can
//! receive are handled. Anything else Neovim sends is logged once and dropped
//! — it cannot be acted on, and dropping it is what the protocol asks a UI to
//! do with an event it does not know.
use rmpv::Value;
use std::collections::HashMap;

/// A 24-bit colour, as `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb(pub u32);

/// How a run of underline is drawn. Neovim has one flag per style rather than
/// one field, so this keeps them apart: a cell has at most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Underline {
    Single,
    Double,
    Curl,
    Dotted,
    Dashed,
}

/// One entry of the highlight table. A colour left out means "the default",
/// which is why these are options rather than resolved values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attributes {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub special: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub reverse: bool,
    pub strikethrough: bool,
    pub underline: Option<Underline>,
}

/// A highlight with every colour filled in and `reverse` already applied:
/// what a renderer needs and nothing it has to resolve itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub foreground: Rgb,
    pub background: Rgb,
    pub special: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: Option<Underline>,
}

/// The colours every highlight falls back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Defaults {
    pub foreground: Rgb,
    pub background: Rgb,
    pub special: Rgb,
}

impl Default for Defaults {
    /// What Neovim uses before it has said otherwise: light on dark.
    fn default() -> Self {
        Self {
            foreground: Rgb(0xe0e0e0),
            background: Rgb(0x000000),
            special: Rgb(0xe0e0e0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Horizontal,
    Vertical,
}

/// How the cursor is drawn in one editor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    /// How much of the cell the bar or underline covers, 1 to 100. A block
    /// covers all of it.
    pub percentage: u8,
    /// The highlight to draw the cursor with, or `None` for "swap the cell's
    /// own colours", which is what Neovim means by attribute id 0.
    pub attribute: Option<u64>,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self {
            shape: CursorShape::Block,
            percentage: 100,
            attribute: None,
        }
    }
}

/// What typing does in the mode Neovim is in, from the name `mode_change`
/// gives it: the one thing about the mode a key press depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Normal, Visual, Operator-pending, a `-- More --` prompt, or a mode
    /// this UI does not know: keys are commands.
    #[default]
    Normal,
    /// Insert or Replace mode, or a terminal buffer: keys type text.
    Insert,
    /// The command line (`:`, `/`, `?`): keys type the command.
    CommandLine,
}

impl Mode {
    /// The mode `name` is, as Neovim names it in `mode_info_set` and
    /// `mode_change` (`:help guicursor` lists them).
    fn from_name(name: &str) -> Self {
        match name {
            // `showmatch` is Insert mode while the cursor visits a match.
            "insert" | "replace" | "showmatch" | "terminal" => Self::Insert,
            "cmdline_normal" | "cmdline_insert" | "cmdline_replace" => Self::CommandLine,
            _ => Self::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    pub row: u16,
    pub column: u16,
}

/// One cell of the grid.
///
/// `text` is a whole grapheme — a character and any combining marks — and is
/// **empty** for the right half of a double-width character, which is how
/// Neovim says "this cell belongs to the one before it".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub text: String,
    pub highlight: u64,
}

impl Cell {
    /// A cell with nothing in it: a space in the default highlight.
    fn blank() -> Self {
        Self {
            text: " ".to_owned(),
            highlight: 0,
        }
    }

    /// Whether this cell is the second half of a double-width character.
    pub fn is_continuation(&self) -> bool {
        self.text.is_empty()
    }
}

/// Which rows changed. Row ranges are inclusive, and unions widen, so this can
/// name more than changed but never less.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Damage(Option<(u16, u16)>);

impl Damage {
    pub const NONE: Self = Self(None);

    pub fn row(row: u16) -> Self {
        Self(Some((row, row)))
    }

    pub fn rows(first: u16, last: u16) -> Self {
        Self(Some((first.min(last), first.max(last))))
    }

    pub fn union(self, other: Self) -> Self {
        match (self.0, other.0) {
            (Some((a, b)), Some((c, d))) => Self(Some((a.min(c), b.max(d)))),
            (some, None) | (None, some) => Self(some),
        }
    }

    /// The first and last row that need redrawing, both inclusive.
    pub fn range(self) -> Option<(u16, u16)> {
        self.0
    }

    pub fn is_empty(self) -> bool {
        self.0.is_none()
    }
}

// ------------------------------------------------------------------ events

/// One `redraw` event from an `ext_linegrid` UI connection.
#[derive(Debug, Clone, PartialEq)]
pub enum RedrawEvent {
    GridResize {
        grid: u64,
        width: u16,
        height: u16,
    },
    GridLine {
        grid: u64,
        row: u16,
        column: u16,
        cells: Vec<LineCell>,
    },
    GridClear {
        grid: u64,
    },
    GridDestroy {
        grid: u64,
    },
    /// Copy a rectangle of the grid `rows` rows upwards. A positive count
    /// moves the content up; a negative one moves it down. The rows it
    /// uncovers are *not* cleared: Neovim sends `grid_line` for them.
    GridScroll {
        grid: u64,
        top: u16,
        bottom: u16,
        left: u16,
        right: u16,
        rows: i32,
    },
    GridCursorGoto {
        grid: u64,
        row: u16,
        column: u16,
    },
    DefaultColorsSet(Defaults),
    HlAttrDefine {
        id: u64,
        attributes: Attributes,
    },
    ModeInfoSet {
        styles: Vec<CursorStyle>,
    },
    ModeChange {
        mode: Mode,
        /// Which of the `mode_info_set` styles the cursor takes.
        index: usize,
    },
    BusyStart,
    BusyStop,
    /// Everything up to here belongs to one frame: draw now.
    Flush,
}

/// A run of identical cells inside a `grid_line` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineCell {
    pub text: String,
    pub highlight: u64,
    /// How many cells this run covers. Zero covers none: Neovim 0.12 ends a
    /// winbar line that stops short of the window edge with `[" ", 0, 0]`,
    /// and the cells after it are to stay as they are.
    pub repeat: u16,
}

impl RedrawEvent {
    /// The events in one `redraw` notification's argument list.
    ///
    /// Neovim batches: the payload is an array of `[name, args, args, …]`,
    /// one entry per event name with one argument tuple per occurrence.
    /// Anything unrecognised is logged and dropped, which is what the protocol
    /// asks of a UI that does not know an event.
    pub fn parse_batch(payload: &[Value]) -> Vec<Self> {
        let mut events = Vec::new();
        for entry in payload {
            let Some(parts) = entry.as_array() else {
                log::debug!("redraw entry is not an array: {entry}");
                continue;
            };
            let Some(name) = parts.first().and_then(Value::as_str) else {
                log::debug!("redraw entry has no name: {entry}");
                continue;
            };
            for arguments in &parts[1..] {
                let Some(arguments) = arguments.as_array() else {
                    log::debug!("redraw {name} arguments are not an array");
                    continue;
                };
                match Self::parse_one(name, arguments) {
                    Some(event) => events.push(event),
                    None => log::debug!("ignoring redraw event {name}"),
                }
            }
        }
        events
    }

    fn parse_one(name: &str, arguments: &[Value]) -> Option<Self> {
        let number = |index: usize| arguments.get(index).and_then(Value::as_i64);
        let small = |index: usize| number(index).and_then(|value| u16::try_from(value).ok());
        let handle = |index: usize| arguments.get(index).and_then(Value::as_u64);
        match name {
            "grid_resize" => Some(Self::GridResize {
                grid: handle(0)?,
                width: small(1)?,
                height: small(2)?,
            }),
            "grid_line" => Some(Self::GridLine {
                grid: handle(0)?,
                row: small(1)?,
                column: small(2)?,
                cells: parse_cells(arguments.get(3)?.as_array()?),
            }),
            "grid_clear" => Some(Self::GridClear { grid: handle(0)? }),
            "grid_destroy" => Some(Self::GridDestroy { grid: handle(0)? }),
            "grid_scroll" => Some(Self::GridScroll {
                grid: handle(0)?,
                top: small(1)?,
                bottom: small(2)?,
                left: small(3)?,
                right: small(4)?,
                rows: i32::try_from(number(5)?).ok()?,
            }),
            "grid_cursor_goto" => Some(Self::GridCursorGoto {
                grid: handle(0)?,
                row: small(1)?,
                column: small(2)?,
            }),
            "default_colors_set" => {
                let fallback = Defaults::default();
                Some(Self::DefaultColorsSet(Defaults {
                    foreground: colour(number(0)).unwrap_or(fallback.foreground),
                    background: colour(number(1)).unwrap_or(fallback.background),
                    special: colour(number(2)).unwrap_or(fallback.special),
                }))
            }
            "hl_attr_define" => Some(Self::HlAttrDefine {
                id: handle(0)?,
                attributes: parse_attributes(arguments.get(1)?.as_map()?),
            }),
            "mode_info_set" => Some(Self::ModeInfoSet {
                styles: arguments
                    .get(1)?
                    .as_array()?
                    .iter()
                    .map(parse_cursor_style)
                    .collect(),
            }),
            "mode_change" => Some(Self::ModeChange {
                mode: arguments
                    .first()
                    .and_then(Value::as_str)
                    .map_or(Mode::Normal, Mode::from_name),
                index: usize::try_from(number(1)?).ok()?,
            }),
            "busy_start" => Some(Self::BusyStart),
            "busy_stop" => Some(Self::BusyStop),
            "flush" => Some(Self::Flush),
            _ => None,
        }
    }
}

/// `-1` is how Neovim says "no colour, use the default".
fn colour(value: Option<i64>) -> Option<Rgb> {
    u32::try_from(value?).ok().map(Rgb)
}

/// The cells of one `grid_line`. Each entry is `[text]`, `[text, hl]` or
/// `[text, hl, repeat]`, and a missing highlight repeats the previous cell's.
fn parse_cells(cells: &[Value]) -> Vec<LineCell> {
    let mut parsed = Vec::with_capacity(cells.len());
    let mut highlight = 0;
    for cell in cells {
        let Some(parts) = cell.as_array() else {
            // The cell is the user's text: its shape is logged, never it.
            log::debug!("grid_line cell is not an array");
            continue;
        };
        let Some(text) = parts.first().and_then(Value::as_str) else {
            log::debug!("grid_line cell has no text");
            continue;
        };
        if let Some(id) = parts.get(1).and_then(Value::as_u64) {
            highlight = id;
        }
        // "Repeated `repeat` times (including the first time)", so zero means
        // no cell at all. Read as one, it painted a default-coloured cell
        // into the middle of the winbar after every stop.
        let repeat = parts
            .get(2)
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(1);
        parsed.push(LineCell {
            text: text.to_owned(),
            highlight,
            repeat,
        });
    }
    parsed
}

fn parse_attributes(map: &[(Value, Value)]) -> Attributes {
    let mut attributes = Attributes::default();
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        let flag = value.as_bool().unwrap_or(false);
        match key {
            "foreground" => attributes.foreground = colour(value.as_i64()),
            "background" => attributes.background = colour(value.as_i64()),
            "special" => attributes.special = colour(value.as_i64()),
            "bold" => attributes.bold = flag,
            "italic" => attributes.italic = flag,
            "reverse" => attributes.reverse = flag,
            "strikethrough" => attributes.strikethrough = flag,
            "underline" if flag => attributes.underline = Some(Underline::Single),
            "underdouble" if flag => attributes.underline = Some(Underline::Double),
            "undercurl" if flag => attributes.underline = Some(Underline::Curl),
            "underdotted" if flag => attributes.underline = Some(Underline::Dotted),
            "underdashed" if flag => attributes.underline = Some(Underline::Dashed),
            _ => {}
        }
    }
    attributes
}

fn parse_cursor_style(info: &Value) -> CursorStyle {
    let mut style = CursorStyle::default();
    let Some(map) = info.as_map() else {
        return style;
    };
    for (key, value) in map {
        match key.as_str() {
            Some("cursor_shape") => {
                style.shape = match value.as_str() {
                    Some("horizontal") => CursorShape::Horizontal,
                    Some("vertical") => CursorShape::Vertical,
                    _ => CursorShape::Block,
                }
            }
            Some("cell_percentage") => {
                style.percentage = value
                    .as_u64()
                    .and_then(|value| u8::try_from(value).ok())
                    .filter(|percentage| (1..=100).contains(percentage))
                    .unwrap_or(100);
            }
            Some("attr_id") => style.attribute = value.as_u64().filter(|id| *id != 0),
            _ => {}
        }
    }
    style
}

// ------------------------------------------------------------------ the grid

/// The grid Neovim draws into, folded from the event stream.
///
/// Only the global grid (handle 1, the one an `ext_linegrid` UI without
/// `ext_multigrid` ever sees) is kept; events for any other handle are
/// dropped.
#[derive(Debug, Clone)]
pub struct Screen {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
    highlights: HashMap<u64, Attributes>,
    defaults: Defaults,
    cursor: Position,
    styles: Vec<CursorStyle>,
    mode: Mode,
    /// The style of `styles` the cursor has in `mode`.
    style: usize,
    busy: bool,
}

/// The handle of the one grid an `ext_linegrid` UI is given.
const GLOBAL_GRID: u64 = 1;

/// The largest grid this screen will hold, in cells.
///
/// Neovim asks for the size the UI told it, so a real one is a few thousand
/// cells. The limit is here because `grid_resize` arrives as two numbers off a
/// socket: a `65535 x 65535` resize would ask for four thousand million cells
/// and take the process down with it. Sixteen million is far past any
/// display — a 4K screen of 6x13 pixel cells is about a quarter of a million.
const MAX_CELLS: usize = 1 << 24;

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            cells: Vec::new(),
            highlights: HashMap::new(),
            defaults: Defaults::default(),
            cursor: Position::default(),
            styles: Vec::new(),
            mode: Mode::Normal,
            style: 0,
            busy: false,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn defaults(&self) -> Defaults {
        self.defaults
    }

    /// The mode Neovim last said it is in.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// How the cursor is drawn in the mode Neovim is in.
    pub fn cursor_style(&self) -> CursorStyle {
        self.styles.get(self.style).copied().unwrap_or_default()
    }

    pub fn cell(&self, row: u16, column: u16) -> Option<&Cell> {
        self.index(row, column).map(|index| &self.cells[index])
    }

    /// One whole row, or an empty slice for a row outside the grid.
    pub fn row(&self, row: u16) -> &[Cell] {
        match self.index(row, 0) {
            Some(start) => &self.cells[start..start + usize::from(self.width)],
            None => &[],
        }
    }

    /// One row as text, which is what Neovim's own `screenstring` returns.
    /// Continuation cells contribute nothing, as they hold nothing.
    pub fn line(&self, row: u16) -> String {
        self.row(row)
            .iter()
            .map(|cell| cell.text.as_str())
            .collect()
    }

    /// A highlight with the defaults filled in and `reverse` applied.
    pub fn style(&self, highlight: u64) -> Style {
        let attributes = self.highlights.get(&highlight).copied().unwrap_or_default();
        let foreground = attributes.foreground.unwrap_or(self.defaults.foreground);
        let background = attributes.background.unwrap_or(self.defaults.background);
        let (foreground, background) = match attributes.reverse {
            true => (background, foreground),
            false => (foreground, background),
        };
        Style {
            foreground,
            background,
            special: attributes.special.unwrap_or(self.defaults.special),
            bold: attributes.bold,
            italic: attributes.italic,
            strikethrough: attributes.strikethrough,
            underline: attributes.underline,
        }
    }

    /// Fold one event in and say which rows it changed.
    pub fn apply(&mut self, event: &RedrawEvent) -> Damage {
        match event {
            RedrawEvent::GridResize {
                grid,
                width,
                height,
            } if *grid == GLOBAL_GRID => self.resize(*width, *height),
            RedrawEvent::GridClear { grid } if *grid == GLOBAL_GRID => {
                self.cells.fill(Cell::blank());
                self.whole()
            }
            RedrawEvent::GridLine {
                grid,
                row,
                column,
                cells,
            } if *grid == GLOBAL_GRID => self.write_line(*row, *column, cells),
            RedrawEvent::GridScroll {
                grid,
                top,
                bottom,
                left,
                right,
                rows,
            } if *grid == GLOBAL_GRID => self.scroll(*top, *bottom, *left, *right, *rows),
            RedrawEvent::GridCursorGoto { grid, row, column } if *grid == GLOBAL_GRID => {
                let was = self.cursor;
                self.cursor = Position {
                    row: *row,
                    column: *column,
                };
                Damage::row(was.row).union(Damage::row(*row))
            }
            RedrawEvent::DefaultColorsSet(defaults) => {
                if *defaults == self.defaults {
                    return Damage::NONE;
                }
                self.defaults = *defaults;
                self.whole()
            }
            RedrawEvent::HlAttrDefine { id, attributes } => {
                self.highlights.insert(*id, *attributes);
                // Redefining a highlight changes every cell that uses it, and
                // Neovim redefines the whole table at once when a colourscheme
                // loads. Repainting everything is cheaper than tracking which
                // cells use which id.
                self.whole()
            }
            RedrawEvent::ModeInfoSet { styles } => {
                self.styles = styles.clone();
                Damage::row(self.cursor.row)
            }
            RedrawEvent::ModeChange { mode, index } => {
                self.mode = *mode;
                self.style = *index;
                Damage::row(self.cursor.row)
            }
            RedrawEvent::BusyStart => {
                self.busy = true;
                Damage::row(self.cursor.row)
            }
            RedrawEvent::BusyStop => {
                self.busy = false;
                Damage::row(self.cursor.row)
            }
            // A grid that is not the global one, a destroyed grid, and the
            // frame marker itself change nothing here.
            RedrawEvent::GridResize { .. }
            | RedrawEvent::GridClear { .. }
            | RedrawEvent::GridLine { .. }
            | RedrawEvent::GridScroll { .. }
            | RedrawEvent::GridCursorGoto { .. }
            | RedrawEvent::GridDestroy { .. }
            | RedrawEvent::Flush => Damage::NONE,
        }
    }

    fn index(&self, row: u16, column: u16) -> Option<usize> {
        (row < self.height && column < self.width)
            .then(|| usize::from(row) * usize::from(self.width) + usize::from(column))
    }

    fn whole(&self) -> Damage {
        match self.height {
            0 => Damage::NONE,
            height => Damage::rows(0, height - 1),
        }
    }

    /// Resize, keeping what still fits. Neovim repaints after a resize, but a
    /// grid that threw its content away would flash empty in between.
    fn resize(&mut self, width: u16, height: u16) -> Damage {
        if (width, height) == (self.width, self.height) {
            return Damage::NONE;
        }
        let wanted = usize::from(width) * usize::from(height);
        if wanted > MAX_CELLS {
            log::warn!("ignoring a {width}x{height} grid: more than {MAX_CELLS} cells");
            return Damage::NONE;
        }
        let mut cells = vec![Cell::blank(); wanted];
        for row in 0..height.min(self.height) {
            for column in 0..width.min(self.width) {
                let source = usize::from(row) * usize::from(self.width) + usize::from(column);
                let target = usize::from(row) * usize::from(width) + usize::from(column);
                cells[target] = self.cells[source].clone();
            }
        }
        self.cells = cells;
        self.width = width;
        self.height = height;
        self.cursor = Position {
            row: self.cursor.row.min(height.saturating_sub(1)),
            column: self.cursor.column.min(width.saturating_sub(1)),
        };
        self.whole()
    }

    fn write_line(&mut self, row: u16, column: u16, cells: &[LineCell]) -> Damage {
        let Some(mut index) = self.index(row, column) else {
            return Damage::NONE;
        };
        let Some(start) = self.index(row, 0) else {
            return Damage::NONE;
        };
        let end = start + usize::from(self.width);
        for cell in cells {
            for _ in 0..cell.repeat {
                if index >= end {
                    return Damage::row(row);
                }
                self.cells[index] = Cell {
                    text: cell.text.clone(),
                    highlight: cell.highlight,
                };
                index += 1;
            }
        }
        Damage::row(row)
    }

    fn scroll(&mut self, top: u16, bottom: u16, left: u16, right: u16, rows: i32) -> Damage {
        let bottom = bottom.min(self.height);
        let right = right.min(self.width);
        if top >= bottom || left >= right || rows == 0 {
            return Damage::NONE;
        }
        let copy = |cells: &mut Vec<Cell>, from: u16, to: u16, width: u16| {
            for column in left..right {
                let source = usize::from(from) * usize::from(width) + usize::from(column);
                let target = usize::from(to) * usize::from(width) + usize::from(column);
                cells[target] = cells[source].clone();
            }
        };
        let width = self.width;
        // `unsigned_abs`, not `-rows`: negating `i32::MIN` overflows, and this
        // number came off a socket.
        let distance = u16::try_from(rows.unsigned_abs()).unwrap_or(u16::MAX);
        if rows > 0 {
            // Content moves up: row `top + n` takes what row `top + n + rows`
            // had, front to back so a row is read before it is written.
            for target in top..bottom.saturating_sub(distance) {
                copy(&mut self.cells, target + distance, target, width);
            }
        } else {
            // Content moves down, so back to front for the same reason.
            for target in (top.saturating_add(distance)..bottom).rev() {
                copy(&mut self.cells, target - distance, target, width);
            }
        }
        Damage::rows(top, bottom.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, highlight: u64) -> Vec<LineCell> {
        text.chars()
            .map(|character| LineCell {
                text: character.to_string(),
                highlight,
                repeat: 1,
            })
            .collect()
    }

    fn screen(width: u16, height: u16) -> Screen {
        let mut screen = Screen::new();
        screen.apply(&RedrawEvent::GridResize {
            grid: 1,
            width,
            height,
        });
        screen
    }

    fn write(screen: &mut Screen, row: u16, text: &str) -> Damage {
        screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row,
            column: 0,
            cells: line(text, 0),
        })
    }

    #[test]
    fn a_new_grid_is_blank_and_a_resize_keeps_what_still_fits() {
        let mut screen = screen(4, 2);
        assert_eq!(screen.size(), (4, 2));
        assert_eq!(screen.line(0), "    ");
        write(&mut screen, 0, "abcd");
        write(&mut screen, 1, "efgh");
        screen.apply(&RedrawEvent::GridResize {
            grid: 1,
            width: 2,
            height: 3,
        });
        assert_eq!(screen.size(), (2, 3));
        assert_eq!(screen.line(0), "ab");
        assert_eq!(screen.line(1), "ef");
        assert_eq!(screen.line(2), "  ");
        assert_eq!(screen.line(9), "");
    }

    #[test]
    fn a_cell_run_repeats_and_keeps_the_last_highlight() {
        let mut screen = screen(6, 1);
        let damage = screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            column: 1,
            cells: vec![
                LineCell {
                    text: "x".to_owned(),
                    highlight: 7,
                    repeat: 3,
                },
                LineCell {
                    text: "y".to_owned(),
                    highlight: 7,
                    repeat: 1,
                },
            ],
        });
        assert_eq!(screen.line(0), " xxxy ");
        assert_eq!(damage.range(), Some((0, 0)));
        assert_eq!(screen.cell(0, 1).map(|cell| cell.highlight), Some(7));
        assert_eq!(screen.cell(0, 4).map(|cell| cell.highlight), Some(7));
        assert_eq!(screen.cell(0, 5).map(|cell| cell.highlight), Some(0));
    }

    #[test]
    fn a_run_that_would_pass_the_end_of_the_row_stops_there() {
        let mut screen = screen(3, 1);
        screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            column: 1,
            cells: vec![LineCell {
                text: "z".to_owned(),
                highlight: 0,
                repeat: 9,
            }],
        });
        assert_eq!(screen.line(0), " zz");
    }

    #[test]
    fn a_double_width_character_owns_the_cell_after_it() {
        let mut screen = screen(4, 1);
        screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            column: 0,
            cells: vec![
                LineCell {
                    text: "漢".to_owned(),
                    highlight: 0,
                    repeat: 1,
                },
                LineCell {
                    text: String::new(),
                    highlight: 0,
                    repeat: 1,
                },
                LineCell {
                    text: "a".to_owned(),
                    highlight: 0,
                    repeat: 1,
                },
            ],
        });
        // `screenstring` reports the same: the continuation cell adds nothing.
        assert_eq!(screen.line(0), "漢a ");
        assert!(screen.cell(0, 1).expect("the cell").is_continuation());
        assert!(!screen.cell(0, 0).expect("the cell").is_continuation());
    }

    #[test]
    fn scrolling_up_moves_content_towards_the_top_and_leaves_the_rest_alone() {
        let mut screen = screen(3, 5);
        for (row, text) in ["aaa", "bbb", "ccc", "ddd", "eee"].into_iter().enumerate() {
            write(&mut screen, row as u16, text);
        }
        let damage = screen.apply(&RedrawEvent::GridScroll {
            grid: 1,
            top: 1,
            bottom: 4,
            left: 0,
            right: 3,
            rows: 1,
        });
        // Rows 1..4 move up by one; row 3 keeps what it had, because Neovim
        // sends a grid_line for it rather than expecting a clear.
        assert_eq!(screen.line(0), "aaa");
        assert_eq!(screen.line(1), "ccc");
        assert_eq!(screen.line(2), "ddd");
        assert_eq!(screen.line(3), "ddd");
        assert_eq!(screen.line(4), "eee");
        assert_eq!(damage.range(), Some((1, 3)));
    }

    #[test]
    fn scrolling_down_moves_content_towards_the_bottom() {
        let mut screen = screen(3, 5);
        for (row, text) in ["aaa", "bbb", "ccc", "ddd", "eee"].into_iter().enumerate() {
            write(&mut screen, row as u16, text);
        }
        screen.apply(&RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bottom: 5,
            left: 0,
            right: 3,
            rows: -2,
        });
        assert_eq!(screen.line(0), "aaa");
        assert_eq!(screen.line(1), "bbb");
        assert_eq!(screen.line(2), "aaa");
        assert_eq!(screen.line(3), "bbb");
        assert_eq!(screen.line(4), "ccc");
    }

    #[test]
    fn a_scroll_region_narrower_than_the_grid_leaves_the_columns_outside_it() {
        let mut screen = screen(5, 3);
        write(&mut screen, 0, "ABCDE");
        write(&mut screen, 1, "fghij");
        write(&mut screen, 2, "KLMNO");
        screen.apply(&RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bottom: 3,
            left: 1,
            right: 4,
            rows: 1,
        });
        assert_eq!(screen.line(0), "AghiE");
        assert_eq!(screen.line(1), "fLMNj");
        assert_eq!(screen.line(2), "KLMNO");
    }

    #[test]
    fn a_scroll_of_nothing_or_past_the_region_changes_nothing() {
        let mut screen = screen(3, 3);
        write(&mut screen, 0, "aaa");
        let before = screen.line(0);
        for rows in [0, 5, -5] {
            let damage = screen.apply(&RedrawEvent::GridScroll {
                grid: 1,
                top: 0,
                bottom: 3,
                left: 0,
                right: 3,
                rows,
            });
            if rows == 0 {
                assert!(damage.is_empty());
            }
        }
        assert_eq!(screen.line(0), before);
    }

    #[test]
    fn events_for_another_grid_are_ignored() {
        let mut screen = screen(3, 1);
        write(&mut screen, 0, "abc");
        let damage = screen.apply(&RedrawEvent::GridClear { grid: 4 });
        assert!(damage.is_empty());
        assert_eq!(screen.line(0), "abc");
        assert!(
            screen
                .apply(&RedrawEvent::GridDestroy { grid: 1 })
                .is_empty()
        );
    }

    #[test]
    fn a_highlight_resolves_against_the_defaults_and_reverse_swaps_it() {
        let mut screen = screen(1, 1);
        screen.apply(&RedrawEvent::DefaultColorsSet(Defaults {
            foreground: Rgb(0x112233),
            background: Rgb(0x445566),
            special: Rgb(0x778899),
        }));
        screen.apply(&RedrawEvent::HlAttrDefine {
            id: 1,
            attributes: Attributes {
                foreground: Some(Rgb(0xff0000)),
                bold: true,
                ..Attributes::default()
            },
        });
        screen.apply(&RedrawEvent::HlAttrDefine {
            id: 2,
            attributes: Attributes {
                reverse: true,
                ..Attributes::default()
            },
        });
        let plain = screen.style(0);
        assert_eq!(plain.foreground, Rgb(0x112233));
        assert_eq!(plain.background, Rgb(0x445566));
        let defined = screen.style(1);
        assert_eq!(defined.foreground, Rgb(0xff0000));
        assert_eq!(defined.background, Rgb(0x445566));
        assert!(defined.bold);
        let reversed = screen.style(2);
        assert_eq!(reversed.foreground, Rgb(0x445566));
        assert_eq!(reversed.background, Rgb(0x112233));
        // An id Neovim never defined is the default highlight, not a panic.
        assert_eq!(screen.style(99), plain);
    }

    #[test]
    fn the_cursor_damages_the_row_it_left_and_the_one_it_reached() {
        let mut screen = screen(4, 4);
        screen.apply(&RedrawEvent::GridCursorGoto {
            grid: 1,
            row: 1,
            column: 2,
        });
        let damage = screen.apply(&RedrawEvent::GridCursorGoto {
            grid: 1,
            row: 3,
            column: 0,
        });
        assert_eq!(screen.cursor(), Position { row: 3, column: 0 });
        assert_eq!(damage.range(), Some((1, 3)));
    }

    #[test]
    fn the_cursor_style_follows_the_mode() {
        let mut screen = screen(4, 4);
        assert_eq!(screen.cursor_style().shape, CursorShape::Block);
        screen.apply(&RedrawEvent::ModeInfoSet {
            styles: vec![
                CursorStyle {
                    shape: CursorShape::Block,
                    percentage: 100,
                    attribute: None,
                },
                CursorStyle {
                    shape: CursorShape::Vertical,
                    percentage: 25,
                    attribute: Some(3),
                },
            ],
        });
        assert_eq!(screen.mode(), Mode::Normal);
        screen.apply(&RedrawEvent::ModeChange {
            mode: Mode::Insert,
            index: 1,
        });
        assert_eq!(screen.mode(), Mode::Insert);
        assert_eq!(screen.cursor_style().shape, CursorShape::Vertical);
        assert_eq!(screen.cursor_style().percentage, 25);
        // A mode Neovim never described falls back rather than panicking.
        screen.apply(&RedrawEvent::ModeChange {
            mode: Mode::Normal,
            index: 99,
        });
        assert_eq!(screen.mode(), Mode::Normal);
        assert_eq!(screen.cursor_style(), CursorStyle::default());
    }

    #[test]
    fn a_mode_change_says_what_typing_does_by_the_modes_name() {
        let change = |name: &str| {
            let payload = vec![Value::Array(vec![
                value("mode_change"),
                Value::Array(vec![value(name), Value::from(2)]),
            ])];
            match RedrawEvent::parse_batch(&payload).as_slice() {
                [RedrawEvent::ModeChange { mode, index: 2 }] => *mode,
                other => panic!("{name}: {other:?}"),
            }
        };
        for name in ["insert", "replace", "showmatch", "terminal"] {
            assert_eq!(change(name), Mode::Insert, "{name}");
        }
        for name in ["cmdline_normal", "cmdline_insert", "cmdline_replace"] {
            assert_eq!(change(name), Mode::CommandLine, "{name}");
        }
        for name in ["normal", "visual", "visual_select", "operator", "more", "?"] {
            assert_eq!(change(name), Mode::Normal, "{name}");
        }
        // No name at all: the style still applies, and keys stay commands.
        let payload = vec![Value::Array(vec![
            value("mode_change"),
            Value::Array(vec![Value::Nil, Value::from(1)]),
        ])];
        assert_eq!(
            RedrawEvent::parse_batch(&payload),
            vec![RedrawEvent::ModeChange {
                mode: Mode::Normal,
                index: 1
            }]
        );
    }

    #[test]
    fn damage_unions_widen_and_never_shrink() {
        assert!(Damage::NONE.is_empty());
        assert_eq!(Damage::NONE.union(Damage::row(3)).range(), Some((3, 3)));
        assert_eq!(
            Damage::rows(2, 4).union(Damage::rows(7, 1)).range(),
            Some((1, 7))
        );
        assert_eq!(Damage::rows(5, 2).range(), Some((2, 5)));
    }

    // ------------------------------------------------------------ parsing

    fn value(text: &str) -> Value {
        Value::from(text)
    }

    #[test]
    fn a_batch_carries_several_events_per_name() {
        let payload = vec![
            Value::Array(vec![
                value("grid_resize"),
                Value::Array(vec![Value::from(1), Value::from(80), Value::from(24)]),
            ]),
            Value::Array(vec![
                value("grid_cursor_goto"),
                Value::Array(vec![Value::from(1), Value::from(2), Value::from(3)]),
                Value::Array(vec![Value::from(1), Value::from(4), Value::from(5)]),
            ]),
            Value::Array(vec![value("flush"), Value::Array(Vec::new())]),
        ];
        assert_eq!(
            RedrawEvent::parse_batch(&payload),
            vec![
                RedrawEvent::GridResize {
                    grid: 1,
                    width: 80,
                    height: 24
                },
                RedrawEvent::GridCursorGoto {
                    grid: 1,
                    row: 2,
                    column: 3
                },
                RedrawEvent::GridCursorGoto {
                    grid: 1,
                    row: 4,
                    column: 5
                },
                RedrawEvent::Flush,
            ]
        );
    }

    #[test]
    fn a_grid_line_inherits_the_previous_cells_highlight() {
        let cells = Value::Array(vec![
            Value::Array(vec![value("a"), Value::from(5)]),
            Value::Array(vec![value("b")]),
            Value::Array(vec![value("c"), Value::from(6), Value::from(3)]),
        ]);
        let payload = vec![Value::Array(vec![
            value("grid_line"),
            Value::Array(vec![
                Value::from(1),
                Value::from(0),
                Value::from(0),
                cells,
                Value::from(false),
            ]),
        ])];
        let events = RedrawEvent::parse_batch(&payload);
        let RedrawEvent::GridLine { cells, .. } = &events[0] else {
            panic!("expected a grid_line, got {events:?}");
        };
        assert_eq!(
            cells,
            &vec![
                LineCell {
                    text: "a".to_owned(),
                    highlight: 5,
                    repeat: 1
                },
                LineCell {
                    text: "b".to_owned(),
                    highlight: 5,
                    repeat: 1
                },
                LineCell {
                    text: "c".to_owned(),
                    highlight: 6,
                    repeat: 3
                },
            ]
        );
    }

    /// The winbar line Neovim 0.12.5 sent when a dictation stopped: the idle
    /// bar is shorter than the one before it, so the line ends in a run of
    /// zero cells, and the old bar's fill to its right stays.
    #[test]
    fn a_run_of_zero_cells_writes_nothing() {
        let cell = |text: &str, rest: &[u64]| {
            let mut parts = vec![value(text)];
            parts.extend(rest.iter().map(|number| Value::from(*number)));
            Value::Array(parts)
        };
        let payload = vec![Value::Array(vec![
            value("grid_line"),
            Value::Array(vec![
                Value::from(1),
                Value::from(0),
                Value::from(0),
                Value::Array(vec![
                    cell("o", &[300]),
                    cell(" ", &[68, 3]),
                    cell(" ", &[0, 0]),
                ]),
                Value::from(false),
            ]),
        ])];
        let mut screen = screen(8, 1);
        screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            column: 0,
            cells: vec![LineCell {
                text: "-".to_owned(),
                highlight: 68,
                repeat: 8,
            }],
        });
        for event in RedrawEvent::parse_batch(&payload) {
            screen.apply(&event);
        }
        assert_eq!(screen.line(0), "o   ----");
        let highlights: Vec<u64> = screen.row(0).iter().map(|cell| cell.highlight).collect();
        assert_eq!(highlights, [300, 68, 68, 68, 68, 68, 68, 68]);
    }

    #[test]
    fn highlight_attributes_and_default_colours_are_read_as_neovim_sends_them() {
        let attributes = Value::Map(vec![
            (value("foreground"), Value::from(0xff_00_00)),
            (value("bold"), Value::from(true)),
            (value("undercurl"), Value::from(true)),
            (value("italic"), Value::from(false)),
            (value("blend"), Value::from(30)),
        ]);
        let payload = vec![
            Value::Array(vec![
                value("hl_attr_define"),
                Value::Array(vec![
                    Value::from(9),
                    attributes,
                    Value::Map(Vec::new()),
                    Value::Array(Vec::new()),
                ]),
            ]),
            Value::Array(vec![
                value("default_colors_set"),
                Value::Array(vec![
                    Value::from(0xaa_bb_cc),
                    Value::from(-1),
                    Value::from(0x11_22_33),
                ]),
            ]),
        ];
        let events = RedrawEvent::parse_batch(&payload);
        assert_eq!(
            events[0],
            RedrawEvent::HlAttrDefine {
                id: 9,
                attributes: Attributes {
                    foreground: Some(Rgb(0xff0000)),
                    bold: true,
                    underline: Some(Underline::Curl),
                    ..Attributes::default()
                }
            }
        );
        let RedrawEvent::DefaultColorsSet(defaults) = events[1] else {
            panic!("expected default_colors_set, got {:?}", events[1]);
        };
        assert_eq!(defaults.foreground, Rgb(0xaabbcc));
        // -1 is "no colour given": the fallback stands.
        assert_eq!(defaults.background, Defaults::default().background);
        assert_eq!(defaults.special, Rgb(0x112233));
    }

    #[test]
    fn mode_info_is_read_down_to_what_the_cursor_needs() {
        let mode = |shape: &str, percentage: i64, attribute: i64| {
            Value::Map(vec![
                (value("cursor_shape"), value(shape)),
                (value("cell_percentage"), Value::from(percentage)),
                (value("attr_id"), Value::from(attribute)),
                (value("blinkon"), Value::from(500)),
            ])
        };
        let payload = vec![Value::Array(vec![
            value("mode_info_set"),
            Value::Array(vec![
                Value::from(true),
                Value::Array(vec![
                    mode("block", 0, 0),
                    mode("vertical", 25, 4),
                    mode("horizontal", 20, 0),
                ]),
            ]),
        ])];
        let events = RedrawEvent::parse_batch(&payload);
        assert_eq!(
            events,
            vec![RedrawEvent::ModeInfoSet {
                styles: vec![
                    CursorStyle {
                        shape: CursorShape::Block,
                        percentage: 100,
                        attribute: None
                    },
                    CursorStyle {
                        shape: CursorShape::Vertical,
                        percentage: 25,
                        attribute: Some(4)
                    },
                    CursorStyle {
                        shape: CursorShape::Horizontal,
                        percentage: 20,
                        attribute: None
                    },
                ]
            }]
        );
    }

    #[test]
    fn an_event_this_ui_does_not_handle_is_dropped_rather_than_failing() {
        let payload = vec![
            Value::Array(vec![
                value("win_viewport"),
                Value::Array(vec![Value::from(1); 6]),
            ]),
            Value::Array(vec![value("set_title"), Value::Array(vec![value("nvim")])]),
            Value::Array(vec![value("mouse_on"), Value::Array(Vec::new())]),
            Value::from(7),
            Value::Array(vec![value("flush"), Value::Array(Vec::new())]),
        ];
        assert_eq!(RedrawEvent::parse_batch(&payload), vec![RedrawEvent::Flush]);
    }

    // ------------------------------------------------- nothing may panic

    #[test]
    fn the_numbers_at_the_edges_of_the_protocol_are_survivable() {
        let mut screen = screen(8, 4);
        // `i32::MIN` cannot be negated, and this number comes off a socket.
        screen.apply(&RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bottom: 4,
            left: 0,
            right: 8,
            rows: i32::MIN,
        });
        screen.apply(&RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bottom: u16::MAX,
            left: 0,
            right: u16::MAX,
            rows: i32::MAX,
        });
        // A grid of four thousand million cells is refused, not allocated.
        let damage = screen.apply(&RedrawEvent::GridResize {
            grid: 1,
            width: u16::MAX,
            height: u16::MAX,
        });
        assert!(damage.is_empty());
        assert_eq!(screen.size(), (8, 4));
        // A run that claims the whole row from the last column writes one cell.
        screen.apply(&RedrawEvent::GridLine {
            grid: 1,
            row: 3,
            column: 7,
            cells: vec![LineCell {
                text: "x".to_owned(),
                highlight: u64::MAX,
                repeat: u16::MAX,
            }],
        });
        assert_eq!(screen.line(3), "       x");
        // A cursor outside the grid is a position, not an index.
        screen.apply(&RedrawEvent::GridCursorGoto {
            grid: 1,
            row: u16::MAX,
            column: u16::MAX,
        });
        assert!(screen.cell(u16::MAX, u16::MAX).is_none());
        assert_eq!(screen.line(u16::MAX), "");
    }

    /// A seeded xorshift, so a failing case replays exactly.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> u64 {
            let mut state = self.0;
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            self.0 = state;
            state
        }

        fn below(&mut self, limit: u64) -> u64 {
            self.next() % limit.max(1)
        }

        /// A value that is plausible msgpack but need not be sensible.
        fn value(&mut self, depth: u32) -> Value {
            match self.below(8) {
                0 => Value::Nil,
                1 => Value::from(self.next().is_multiple_of(2)),
                2 => Value::from(self.next() as i64),
                3 => Value::from(self.next()),
                4 => Value::from(NAMES[(self.below(NAMES.len() as u64)) as usize]),
                5 if depth > 0 => {
                    Value::Array((0..self.below(5)).map(|_| self.value(depth - 1)).collect())
                }
                6 if depth > 0 => Value::Map(
                    (0..self.below(4))
                        .map(|_| (self.value(depth - 1), self.value(depth - 1)))
                        .collect(),
                ),
                _ => Value::from(f64::from_bits(self.next())),
            }
        }
    }

    /// Event names, real and invented, so the walk hits the handled arms too.
    const NAMES: [&str; 16] = [
        "grid_resize",
        "grid_line",
        "grid_clear",
        "grid_destroy",
        "grid_scroll",
        "grid_cursor_goto",
        "default_colors_set",
        "hl_attr_define",
        "mode_info_set",
        "mode_change",
        "busy_start",
        "busy_stop",
        "flush",
        "win_viewport",
        "",
        "grid_line",
    ];

    #[test]
    fn random_and_malformed_redraw_batches_never_panic() {
        // Neovim does not send nonsense, but the decoder is a boundary and a
        // boundary that can panic takes the daemon with it.
        let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
        let mut screen = Screen::new();
        for _ in 0..4000 {
            let payload: Vec<Value> = (0..noise.below(4) + 1)
                .map(|_| {
                    let name = NAMES[noise.below(NAMES.len() as u64) as usize];
                    let mut entry = vec![Value::from(name)];
                    for _ in 0..noise.below(3) {
                        entry.push(Value::Array(
                            (0..noise.below(7)).map(|_| noise.value(2)).collect(),
                        ));
                    }
                    Value::Array(entry)
                })
                .collect();
            for event in RedrawEvent::parse_batch(&payload) {
                screen.apply(&event);
            }
            // Whatever it did, the grid stays self-consistent.
            let (width, height) = screen.size();
            assert_eq!(
                screen.cells.len(),
                usize::from(width) * usize::from(height),
                "the cell store no longer matches the grid size"
            );
            for row in 0..height {
                assert_eq!(screen.row(row).len(), usize::from(width));
            }
        }
    }
}
