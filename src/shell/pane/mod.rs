//! The dictation pane: an X11 window spokenpad draws itself, with an embedded
//! Neovim behind it.
//!
//! The window never takes keyboard focus when it appears ([`x11`] explains the
//! properties that make that true), floats on tiling window managers, and
//! needs no window-manager rule. The user can click into it and type.
//!
//! How the pieces fit:
//!
//! - [`ui`] starts `nvim --embed` and attaches as an `ext_linegrid` UI, so
//!   Neovim describes its display as redraw events instead of drawing to a
//!   terminal.
//! - [`core::grid`](crate::core::grid) folds those events into a screen of
//!   cells and says which rows changed.
//! - [`font`] rasterises a grapheme, `paint_row` turns cells into pixels, and
//!   [`x11::Window::present`] copies them into the window.
//! - [`keyboard`] reads the user's real layout, and
//!   [`core::keys`](crate::core::keys) spells a press the way Neovim reads it.
//!
//! **Committed text does not come through here.** The embedded Neovim also
//! listens on its own socket, and the daemon appends over that socket exactly
//! as it does for a terminal editor. The pane draws; it never writes to the
//! buffer, and it holds no clipboard code — the editor's own provider does
//! that, as in every other mode.
//!
//! The daemon reaches this through `nvim.mode = "pane"`, and drives it from a
//! thread of its own ([`host`]).
pub mod font;
pub mod host;
pub mod keyboard;
pub mod place;
pub mod ui;
pub mod x11;
pub mod xkb;

use crate::config::{FontFamily, PaneLayout};
use crate::core::{
    font::{Dpi, Points},
    geometry::{self, Dimensions, Rect},
    grid::{Cell, CursorShape, Damage, RedrawEvent, Rgb, Screen, Style, Underline},
};
use anyhow::{Context, Result, bail, ensure};
use font::{CellMetrics, Coverage, Face, Font};
use keyboard::Keyboard;
use rmpv::Value;
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use ui::{Editor, FromEditor};
use x11::Window;
use x11rb::protocol::xproto::{ButtonPressEvent, KeyButMask, MotionNotifyEvent};

/// Something the pane has to react to. One channel carries both sources, so
/// the loop blocks in one place and uses no processor time while idle.
pub enum Event {
    Window(x11rb::protocol::Event),
    Editor(FromEditor),
}

/// What the pane is told to be when it opens.
#[derive(Debug, Clone)]
pub struct Options {
    /// The X display to open on. Named, never looked up: the environment is
    /// read once where the configuration is, and passed from there.
    pub display: String,
    /// A fontconfig family name; `monospace` is the one every desktop has.
    pub family: FontFamily,
    /// Font size in points, turned into pixels by the display's `Xft.dpi`
    /// exactly as Alacritty does it.
    pub size: Points,
    /// The grid, in cells. With a `target`, it is cut down to what fits on
    /// the target's monitor.
    pub dimensions: Dimensions,
    /// Floating or tiled; see [`x11`] for what each sets on the window.
    pub layout: PaneLayout,
    /// How long the editor inside it has to answer `nvim_ui_attach`. The
    /// daemon passes what is left of the one deadline it gave the whole
    /// open, so this cannot outlive it.
    pub attach_timeout: Duration,
    /// The monitor to open on and the pointer to put the top-left corner at,
    /// or `None` to open at the origin and let the window manager decide.
    pub target: Option<place::Target>,
    pub title: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            display: ":0".to_owned(),
            family: FontFamily::default(),
            size: Points::DEFAULT,
            dimensions: Dimensions::DEFAULT,
            layout: PaneLayout::Floating,
            attach_timeout: Duration::from_secs(20),
            target: None,
            title: "spokenpad dictation".to_owned(),
        }
    }
}

/// Whether the pane is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Neovim exited, or the window manager closed the window.
    Finished,
}

pub struct Pane {
    window: Window,
    keyboard: Keyboard,
    font: Font,
    metrics: CellMetrics,
    screen: Screen,
    editor: Editor,
    events: Receiver<Event>,
    watcher: Option<JoinHandle<()>>,
    /// Set when the pane is going away, so the thread waiting for X events
    /// stops instead of blocking on the next one.
    stopping: Arc<AtomicBool>,
    canvas: Canvas,
    columns: u16,
    rows: u16,
    /// The top-left corner the pane was placed at, which [`Self::show`]
    /// moves it to again once it is mapped; `None` without a target.
    origin: Option<(i32, i32)>,
    /// Rows changed since the last frame was drawn.
    pending: Damage,
    /// Whether what is pending is a whole frame. Neovim ends one with
    /// `flush`, and a `redraw` notification can carry half of it: painting
    /// then would show a scroll before the lines that refill what it
    /// uncovered. Damage the window itself reports — an expose, a resize —
    /// is whole by definition and sets this too.
    frame_ready: bool,
    focused: bool,
    /// Which mouse button is down, so motion can be reported as a drag.
    dragging: Option<&'static str>,
    /// Whether the editor has already exited, so nothing tries to talk to it.
    editor_gone: bool,
}

impl Pane {
    /// Open the window and start the editor. The window is **not** mapped yet:
    /// [`Self::show`] does that, once the caller has placed it.
    ///
    /// `command` must already carry `--embed`, and should carry `--listen` too
    /// so the daemon can append to the same editor.
    pub fn open(options: &Options, command: Command) -> Result<Self> {
        // The display first: its resolution decides how many pixels a point
        // is, and so how big the window has to be.
        let display = x11::Display::connect(&options.display)?;
        let font = Font::load(&options.family, options.size, display.dpi())?;
        let metrics = font.metrics();
        // A window is a whole number of cells: the pane draws cells, and a
        // partial column at the right edge is a strip nothing ever paints.
        // On a monitor, no more cells than fit on it.
        let dimensions = match &options.target {
            Some(target) => options
                .dimensions
                .fit(target.monitor, (metrics.width, metrics.height)),
            None => options.dimensions,
        };
        let (columns, rows) = (dimensions.columns.get(), dimensions.lines.get());
        let width = metrics.width * u32::from(columns);
        let height = metrics.height * u32::from(rows);
        // X11 counts a window's size in 16 bits, so a grid that would not fit
        // on any screen has to be refused rather than silently wrapped.
        let (width, height) = match (u16::try_from(width), u16::try_from(height)) {
            (Ok(width), Ok(height)) => (width, height),
            _ => bail!(
                "a {columns}x{rows} grid of {}x{} pixel cells is larger than any X11 window",
                metrics.width,
                metrics.height
            ),
        };
        let size = (u32::from(width), u32::from(height));
        let placed = options
            .target
            .as_ref()
            .map(|target| geometry::placement(target.monitor, target.pointer, size));
        let window = Window::open(
            display,
            placed.unwrap_or(Rect {
                x: 0,
                y: 0,
                width: size.0,
                height: size.1,
            }),
            &options.title,
            options.layout,
        )?;
        let keyboard = Keyboard::new(window.connection())?;
        let (sender, events) = channel();
        let stopping = Arc::new(AtomicBool::new(false));
        // The editor first, and the pane around it, before the thread that
        // waits for X events exists: everything that can fail from here on
        // drops a whole `Pane`, which stops the editor. A watcher started
        // before that would outlive a failed open, blocked in `wait_for_event`
        // and holding the X connection with nobody left to close it.
        let editor = Editor::spawn(command, sender.clone())?;
        let mut pane = Self {
            window,
            keyboard,
            font,
            metrics,
            screen: Screen::new(),
            editor,
            events,
            watcher: None,
            stopping,
            canvas: Canvas::new(width, height, metrics),
            columns,
            rows,
            origin: placed.map(|rect| (rect.x, rect.y)),
            pending: Damage::NONE,
            frame_ready: false,
            focused: false,
            dragging: None,
            editor_gone: false,
        };
        let attach = pane.editor.attach(columns, rows)?;
        pane.await_response(attach, options.attach_timeout)
            .context("attach to the embedded nvim as a UI")?;
        pane.watcher = Some(watch(&pane.window, sender, Arc::clone(&pane.stopping))?);
        Ok(pane)
    }

    /// A handle another thread can use to make [`Self::step`] return.
    pub fn waker(&self) -> x11::Waker {
        self.window.waker()
    }

    /// Map the window. It will not take the focus; see [`x11`].
    ///
    /// With a target, the window is then moved to the corner it was placed
    /// at: the position asked for before the map gets it close, and this
    /// corrects for whatever frame the window manager drew around it. It is
    /// a few pixels, and it happens before Neovim has drawn anything.
    pub fn show(&mut self) -> Result<()> {
        self.window.map()?;
        if let Some((x, y)) = self.origin
            && let Err(error) = self.place_at(x, y)
        {
            log::debug!("could not correct the pane's position: {error:#}");
        }
        Ok(())
    }

    /// Move the window's top-left corner, keeping the size it was opened at.
    ///
    /// The position asked for before the map gets it close; a window manager
    /// that draws a frame places that frame rather than the window inside it,
    /// and correcting it afterwards is exact. Measured on i3 in
    /// `docs/experiments/2026-09-21-own-window-p0-properties.md`.
    pub fn place_at(&mut self, x: i32, y: i32) -> Result<()> {
        self.window.place(Rect {
            x,
            y,
            width: u32::from(self.canvas.width),
            height: u32::from(self.canvas.height),
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn size(&self) -> (u16, u16) {
        (self.columns, self.rows)
    }

    /// The window's pixels as they were last drawn, with their width and
    /// height. Only a caller that wants to save or inspect the image needs
    /// this; the pane itself keeps them up to date.
    pub fn framebuffer(&self) -> (&[u32], u16, u16) {
        (&self.canvas.pixels, self.canvas.width, self.canvas.height)
    }

    /// Wait for one thing to happen, handle it and everything queued behind
    /// it, then draw whatever changed.
    pub fn step(&mut self, timeout: Duration) -> Result<Status> {
        let first = match self.events.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(Status::Finished),
        };
        let queued: Vec<Event> = self.events.try_iter().collect();
        let mut status = Status::Running;
        for event in first.into_iter().chain(queued) {
            if self.handle(event)? == Status::Finished {
                status = Status::Finished;
            }
        }
        // Nothing is drawn into a window that is gone: the X requests would
        // fail, and the caller would hear an error where the truth is simply
        // that the window closed and the passage ended.
        if status == Status::Running {
            self.draw()?;
        }
        Ok(status)
    }

    /// Call a Neovim API function over the pane's own UI channel and wait for
    /// the answer, drawing whatever arrives meanwhile.
    pub fn call(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.editor.request(method, arguments)?;
        let answer = self.await_response(id, timeout)?;
        self.draw()?;
        Ok(answer)
    }

    /// Everything the editor says until it answers `id`.
    fn await_response(&mut self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "nvim did not answer request {id}");
            let event = self
                .events
                .recv_timeout(remaining)
                .context("the pane's editor channel closed while waiting for an answer")?;
            match &event {
                Event::Editor(FromEditor::Response {
                    id: answered,
                    error,
                    result,
                }) if *answered == id => {
                    ensure!(error.is_nil(), "nvim RPC {id} failed: {error}");
                    return Ok(result.clone());
                }
                // The answer is never coming. Without this the wait runs to
                // its whole timeout — twenty seconds per open attempt — for
                // the commonest startup failures there are: an `nvim.editor`
                // that is not a program, an `nvim.init` file that is not
                // there, a plugin that raises on load.
                Event::Editor(FromEditor::Gone) => {
                    bail!("nvim exited before answering request {id}")
                }
                _ => {}
            }
            self.handle(event)?;
        }
    }

    fn handle(&mut self, event: Event) -> Result<Status> {
        match event {
            Event::Editor(FromEditor::Redraw(events)) => {
                for event in &events {
                    self.pending = self.pending.union(self.screen.apply(event));
                    self.frame_ready |= matches!(event, RedrawEvent::Flush);
                }
                Ok(Status::Running)
            }
            Event::Editor(FromEditor::Response { id, error, .. }) => {
                if !error.is_nil() {
                    log::warn!("pane: nvim reported an error for request {id}: {error}");
                }
                Ok(Status::Running)
            }
            Event::Editor(FromEditor::Gone) => {
                self.editor_gone = true;
                Ok(Status::Finished)
            }
            Event::Window(event) => self.window_event(event),
        }
    }

    fn window_event(&mut self, event: x11rb::protocol::Event) -> Result<Status> {
        use x11rb::protocol::Event as X;
        match event {
            X::Expose(_) => self.damage_everything(),
            X::ConfigureNotify(event) => self.resize(event.width, event.height)?,
            X::KeyPress(event) => {
                if let Some(keys) = self.keyboard.press(event.detail, event.state) {
                    self.editor.input(&keys)?;
                }
            }
            X::ButtonPress(event) => self.button(&event, true)?,
            X::ButtonRelease(event) => self.button(&event, false)?,
            X::MotionNotify(event) => self.motion(&event)?,
            X::FocusIn(_) => {
                self.focused = true;
                self.editor.set_focus(true)?;
                self.damage(Damage::row(self.screen.cursor().row));
            }
            X::FocusOut(_) => {
                self.focused = false;
                self.editor.set_focus(false)?;
                self.damage(Damage::row(self.screen.cursor().row));
            }
            X::MappingNotify(_) => {
                if let Err(error) = self.keyboard.refresh(self.window.connection()) {
                    log::warn!("pane: the keyboard layout changed but would not reload: {error:#}");
                }
            }
            X::ClientMessage(event) if self.window.is_close_request(&event) => {
                return Ok(Status::Finished);
            }
            _ => {}
        }
        Ok(Status::Running)
    }

    /// Damage the pane itself decided on, which is always a whole frame:
    /// there is no half of an expose.
    fn damage(&mut self, damage: Damage) {
        self.pending = self.pending.union(damage);
        self.frame_ready = true;
    }

    fn damage_everything(&mut self) {
        self.damage(everything(self.rows));
    }

    /// A new window size means a new number of cells, which only Neovim can
    /// act on: it answers with a `grid_resize` and repaints.
    fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        if (width, height) == (self.canvas.width, self.canvas.height) {
            return Ok(());
        }
        self.canvas.resize(width, height);
        let columns = (u32::from(width) / self.metrics.width).max(1) as u16;
        let rows = (u32::from(height) / self.metrics.height).max(1) as u16;
        if (columns, rows) != (self.columns, self.rows) {
            self.columns = columns;
            self.rows = rows;
            self.editor.try_resize(columns, rows)?;
        }
        self.damage_everything();
        Ok(())
    }

    fn button(&mut self, event: &ButtonPressEvent, pressed: bool) -> Result<()> {
        let (row, column) = self.cell_at(event.event_x, event.event_y);
        let modifiers = modifier_names(event.state);
        let action = if pressed { "press" } else { "release" };
        let (button, action) = match event.detail {
            1 => ("left", action),
            2 => ("middle", action),
            3 => ("right", action),
            // The wheel reports a press and a release for one step; only the
            // press is a scroll.
            4 if pressed => ("wheel", "up"),
            5 if pressed => ("wheel", "down"),
            6 if pressed => ("wheel", "left"),
            7 if pressed => ("wheel", "right"),
            _ => return Ok(()),
        };
        self.dragging = match (button, pressed) {
            ("wheel", _) => self.dragging,
            (button, true) => Some(button),
            (_, false) => None,
        };
        self.editor
            .input_mouse(button, action, &modifiers, row, column)
    }

    fn motion(&mut self, event: &MotionNotifyEvent) -> Result<()> {
        let Some(button) = self.dragging else {
            return Ok(());
        };
        let (row, column) = self.cell_at(event.event_x, event.event_y);
        let modifiers = modifier_names(event.state);
        self.editor
            .input_mouse(button, "drag", &modifiers, row, column)
    }

    /// Which cell a window coordinate is in, clamped to the grid.
    fn cell_at(&self, x: i16, y: i16) -> (u16, u16) {
        let column = (x.max(0) as u32 / self.metrics.width) as u16;
        let row = (y.max(0) as u32 / self.metrics.height) as u16;
        (
            row.min(self.rows.saturating_sub(1)),
            column.min(self.columns.saturating_sub(1)),
        )
    }

    // ------------------------------------------------------------- drawing

    /// Paint the rows that changed and copy them into the window.
    fn draw(&mut self) -> Result<()> {
        if !self.frame_ready {
            // Half a frame. The damage keeps accumulating until Neovim says
            // the frame is whole.
            return Ok(());
        }
        let Some((first, last)) = self.pending.range() else {
            self.frame_ready = false;
            return Ok(());
        };
        self.pending = Damage::NONE;
        self.frame_ready = false;
        let last = last.min(self.rows.saturating_sub(1));
        if first > last || self.canvas.width == 0 || self.canvas.height == 0 {
            return Ok(());
        }
        self.font.new_frame();
        for row in first..=last {
            self.paint_row(row);
        }
        if self.font.deferred() {
            // Some cell holds a character whose font nobody has looked up,
            // because this frame had already spent its budget on fontconfig.
            // It is blank for now. Ask for these rows again and wake the
            // loop, so the next frame spends its own budget on them: text the
            // user dictated must not stay invisible, and a window that stops
            // answering for a second while it asks is worse than a character
            // that appears a frame late.
            self.damage(Damage::rows(first, last));
            self.window.wake()?;
        }
        let height = usize::from(self.canvas.height);
        let top = usize::from(first) * self.metrics.height as usize;
        let mut bottom = ((usize::from(last) + 1) * self.metrics.height as usize).min(height);
        if last == self.rows.saturating_sub(1) {
            // A window is rarely a whole number of cells tall. The strip under
            // the last row belongs to no cell, so nothing would ever paint it
            // and it would stay black under a light colourscheme.
            let background = self.screen.defaults().background;
            let leftover = u32::from(self.canvas.height) - bottom as u32;
            self.canvas.fill(
                0,
                bottom as u32,
                u32::from(self.canvas.width),
                leftover,
                background,
            );
            bottom = height;
        }
        if top >= bottom {
            return Ok(());
        }
        let stride = usize::from(self.canvas.width);
        self.window.present(
            0,
            i16::try_from(top).unwrap_or(i16::MAX),
            self.canvas.width,
            &self.canvas.pixels[top * stride..bottom * stride],
        )
    }

    fn paint_row(&mut self, row: u16) {
        // Disjoint borrows: the screen is read while the canvas and the glyph
        // cache are written. Copying the row out to satisfy one borrow would
        // allocate a String per cell on every painted row of every frame.
        let Self {
            screen,
            canvas,
            font,
            focused,
            columns,
            ..
        } = self;
        let metrics = canvas.metrics;
        let top = u32::from(row) * metrics.height;
        canvas.fill(
            0,
            top,
            u32::from(canvas.width),
            metrics.height,
            screen.defaults().background,
        );
        let cells = screen.row(row);
        let mut column = 0;
        while column < cells.len() {
            let cell = &cells[column];
            if cell.is_continuation() {
                column += 1;
                continue;
            }
            // A double-width character owns the cell after it, which Neovim
            // marks by leaving that cell's text empty.
            let span = match cells.get(column + 1) {
                Some(next) if next.is_continuation() => 2,
                _ => 1,
            };
            let style = screen.style(cell.highlight);
            let left = column as u32 * metrics.width;
            canvas.fill(
                left,
                top,
                metrics.width * span,
                metrics.height,
                style.background,
            );
            canvas.cell(font, &cell.text, style, left, top);
            column += span as usize;
        }
        if screen.cursor().row == row {
            paint_cursor(canvas, font, screen, row, *columns, *focused);
        }
    }
}

/// The cursor: filled while the pane has the focus, an outline when it does
/// not, so the window says plainly that typing would go elsewhere.
fn paint_cursor(
    canvas: &mut Canvas,
    font: &mut Font,
    screen: &Screen,
    row: u16,
    columns: u16,
    focused: bool,
) {
    let position = screen.cursor();
    if position.column >= columns {
        return;
    }
    if screen.busy() {
        // Neovim is working, and where its cursor sits means nothing until it
        // is done. A terminal hides the cursor then, and so does this.
        return;
    }
    let style = screen.cursor_style();
    let empty = Cell::default();
    let cell = screen.cell(row, position.column).unwrap_or(&empty);
    let under = screen.style(cell.highlight);
    let (foreground, background) = match style.attribute {
        Some(highlight) => {
            let cursor = screen.style(highlight);
            (cursor.foreground, cursor.background)
        }
        // Attribute 0 means "swap the cell's own colours".
        None => (under.background, under.foreground),
    };
    let metrics = canvas.metrics;
    let left = u32::from(position.column) * metrics.width;
    let top = u32::from(row) * metrics.height;
    let fraction = |whole: u32| (whole * u32::from(style.percentage) / 100).max(1);
    if !focused {
        canvas.outline(left, top, metrics.width, metrics.height, background);
        return;
    }
    match style.shape {
        CursorShape::Block => {
            canvas.fill(left, top, metrics.width, metrics.height, background);
            canvas.cell(
                font,
                &cell.text,
                Style {
                    foreground,
                    ..under
                },
                left,
                top,
            );
        }
        CursorShape::Vertical => canvas.fill(
            left,
            top,
            fraction(metrics.width),
            metrics.height,
            background,
        ),
        CursorShape::Horizontal => {
            let thickness = fraction(metrics.height);
            canvas.fill(
                left,
                top + metrics.height - thickness,
                metrics.width,
                thickness,
                background,
            );
        }
    }
}

/// The window's pixels, and everything that puts colour into them.
///
/// Separate from [`Pane`] so a row can be read from the screen while the
/// pixels are written, which is the whole reason this is its own type.
struct Canvas {
    /// `0x00RRGGBB` per pixel, row by row.
    pixels: Vec<u32>,
    width: u16,
    height: u16,
    metrics: CellMetrics,
}

impl Canvas {
    fn new(width: u16, height: u16, metrics: CellMetrics) -> Self {
        Self {
            pixels: vec![0; usize::from(width) * usize::from(height)],
            width,
            height,
            metrics,
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.pixels = vec![0; usize::from(width) * usize::from(height)];
        self.width = width;
        self.height = height;
    }

    /// One cell's text, underline and strikethrough, over the background the
    /// caller has already laid down.
    fn cell(&mut self, font: &mut Font, text: &str, style: Style, left: u32, top: u32) {
        let metrics = self.metrics;
        let baseline = top + metrics.baseline;
        if let Some(glyph) = font
            .glyph(text, Face::of(style.bold, style.italic))
            .cloned()
        {
            self.blit(&glyph, left, baseline, style.foreground);
        }
        if let Some(underline) = style.underline {
            self.rule(
                underline,
                left,
                top + metrics.underline.top,
                metrics.width,
                style.special,
            );
        }
        if style.strikethrough {
            self.fill(
                left,
                top + metrics.strikeout.top,
                metrics.width,
                metrics.strikeout.thickness,
                style.special,
            );
        }
    }

    fn fill(&mut self, left: u32, top: u32, width: u32, height: u32, colour: Rgb) {
        let stride = u32::from(self.width);
        let right = (left + width).min(stride);
        let bottom = (top + height).min(u32::from(self.height));
        if left >= right {
            return;
        }
        for y in top..bottom {
            let start = (y * stride + left) as usize;
            let end = (y * stride + right) as usize;
            self.pixels[start..end].fill(colour.0);
        }
    }

    /// A frame, for the cursor of an unfocused pane, as thick as Alacritty
    /// draws its hollow cursor.
    fn outline(&mut self, left: u32, top: u32, width: u32, height: u32, colour: Rgb) {
        let line = self.metrics.hollow_cursor();
        self.fill(left, top, width, line, colour);
        self.fill(left, top + height.saturating_sub(line), width, line, colour);
        self.fill(left, top, line, height, colour);
        self.fill(left + width.saturating_sub(line), top, line, height, colour);
    }

    /// An underline in one of Neovim's styles.
    ///
    /// Every pattern is measured in the underline's own thickness, so it
    /// scales with the font: one pixel at 96 dpi, two at 192.
    fn rule(&mut self, style: Underline, left: u32, y: u32, width: u32, colour: Rgb) {
        let unit = self.metrics.underline.thickness;
        match style {
            Underline::Single => self.fill(left, y, width, unit, colour),
            Underline::Double => {
                self.fill(left, y, width, unit, colour);
                self.fill(left, y + unit * 2, width, unit, colour);
            }
            Underline::Dotted => {
                for x in (left..left + width).step_by(2 * unit as usize) {
                    self.fill(x, y, unit.min(left + width - x), unit, colour);
                }
            }
            Underline::Dashed => {
                for x in (left..left + width).step_by(4 * unit as usize) {
                    self.fill(x, y, (2 * unit).min(left + width - x), unit, colour);
                }
            }
            Underline::Curl => {
                // A zigzag one stroke high reads as a wave at text sizes and
                // needs no curve rasteriser.
                for (step, x) in (left..left + width).enumerate() {
                    let offset = unit * u32::from(step as u32 / unit % 4 < 2);
                    self.fill(x, y + offset, 1, unit, colour);
                }
            }
        }
    }

    /// Draw one rasterised grapheme with its pen on the baseline.
    fn blit(&mut self, glyph: &font::Glyph, pen_x: u32, baseline: u32, colour: Rgb) {
        let stride = i64::from(self.width);
        let start_x = i64::from(pen_x) + i64::from(glyph.left);
        let start_y = i64::from(baseline) - i64::from(glyph.top);
        for row in 0..glyph.height {
            let y = start_y + i64::from(row);
            if y < 0 || y >= i64::from(self.height) {
                continue;
            }
            for column in 0..glyph.width {
                let x = start_x + i64::from(column);
                if x < 0 || x >= stride {
                    continue;
                }
                let target = (y * stride + x) as usize;
                let source = (row * glyph.width + column) as usize;
                match &glyph.coverage {
                    Coverage::Mask(mask) => {
                        let alpha = mask[source];
                        if alpha > 0 {
                            self.pixels[target] = blend(self.pixels[target], colour, alpha);
                        }
                    }
                    Coverage::Colour(data) => {
                        let pixel = &data[source * 4..source * 4 + 4];
                        let alpha = pixel[3];
                        if alpha > 0 {
                            let over = Rgb(u32::from_be_bytes([0, pixel[0], pixel[1], pixel[2]]));
                            self.pixels[target] = blend(self.pixels[target], over, alpha);
                        }
                    }
                }
            }
        }
    }
}

/// Write every modified buffer, and say which ones would not go.
///
/// Dictated text is already on disk: the Lua side writes the file after every
/// append. What is not on disk is anything the user typed into the pane since
/// then, and closing the window must not be the thing that loses it. Managed
/// mode never has this problem, because its editor outlives the daemon; a
/// pane's editor does not, so it writes first.
const WRITE_EVERYTHING: &str = r#"
local unwritten = {}
for _, buffer in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buffer)
     and vim.bo[buffer].modified
     and vim.bo[buffer].buftype == "" then
    local name = vim.api.nvim_buf_get_name(buffer)
    local ok, why = pcall(vim.api.nvim_buf_call, buffer, function()
      vim.cmd("silent noautocmd write")
    end)
    if not ok then
      -- The text comes back with the failure: whatever stopped the write --
      -- a read-only file, a directory that is gone, a full disk -- the
      -- daemon still has the only copy and can put it somewhere else.
      unwritten[#unwritten + 1] = {
        name,
        tostring(why),
        vim.api.nvim_buf_get_lines(buffer, 0, -1, false),
      }
    end
  end
end
return unwritten
"#;

/// The whole teardown — writing every buffer and stopping the editor — has to
/// fit inside the grace the daemon gives its editor thread, because when that
/// runs out the process exits and this thread stops wherever it had got to.
/// Writing comes first and takes most of it.
const WRITE_TIMEOUT: Duration = Duration::from_millis(1_200);
/// Then the editor is asked to quit. It has nothing left to save by then, so
/// this is short and it is killed rather than waited for.
const QUIT_GRACE: Duration = Duration::from_millis(500);
const _: () = assert!(
    WRITE_TIMEOUT.as_millis() + QUIT_GRACE.as_millis()
        < crate::shell::daemon::SHUTDOWN_GRACE.as_millis(),
    "the pane's teardown must fit inside the daemon's shutdown grace"
);

impl Pane {
    fn write_buffers(&mut self) {
        if self.editor_gone {
            // Neovim decided for itself — `:q` with nothing unsaved, or a
            // deliberate `:q!`. Either way there is nothing left to ask.
            return;
        }
        let call = self.call(
            "nvim_exec_lua",
            vec![Value::from(WRITE_EVERYTHING), Value::Array(Vec::new())],
            WRITE_TIMEOUT,
        );
        match call {
            Ok(value) => {
                for entry in value.as_array().into_iter().flatten() {
                    rescue(entry);
                }
            }
            Err(error) => {
                log::error!(
                    "could not write the pane's buffers before closing: {error:#}; \
                     anything typed into the window since the last utterance is lost"
                )
            }
        }
    }
}

/// Keep the text of a buffer Neovim could not write.
///
/// The editor is about to be closed, so this is the last moment the text
/// exists anywhere. It goes beside the file it belongs to, with the same
/// private permissions the dictation files have, and the log says where.
fn rescue(entry: &Value) {
    let field = |index: usize| entry.as_array().and_then(|parts| parts.get(index));
    let name = field(0).and_then(Value::as_str).unwrap_or_default();
    let why = field(1).and_then(Value::as_str).unwrap_or("unknown");
    let text: String = field(2)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let named = if name.is_empty() {
        "an unnamed buffer"
    } else {
        name
    };
    if text.trim().is_empty() {
        log::warn!("the pane could not write {named} ({why}), and it held nothing");
        return;
    }
    match keep_beside(name, &text) {
        Ok(path) => log::error!(
            "the pane could not write {named} ({why}); its text is in {}",
            path.display()
        ),
        Err(error) => {
            log::error!(
                "the pane could not write {named} ({why}) and could not keep its text \
                     either ({error:#}); it is in this log"
            );
            log::debug!("unwritten text: {text:?}");
        }
    }
}

/// Write rescued text next to the file it came from, or into the state
/// directory when the buffer had no file.
fn keep_beside(name: &str, text: &str) -> Result<PathBuf> {
    let path = match name.is_empty() {
        false => PathBuf::from(format!("{name}.unsaved")),
        true => {
            let directory = crate::config::state_dir();
            std::fs::create_dir_all(&directory)?;
            directory.join(format!(
                "unsaved-{}.md",
                chrono::Local::now().format("%Y-%m-%d-%H%M%S")
            ))
        }
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(path)
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.write_buffers();
        self.editor.shutdown(QUIT_GRACE);
        // The watcher is blocked waiting for an X event. Tell it to stop, then
        // send it one of our own so it wakes up and sees that. Relying on the
        // channel closing instead would deadlock: the receiver is still alive
        // while this runs.
        self.stopping.store(true, Ordering::Release);
        let _ = self.window.wake();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Blend `colour` over `under` at `alpha`, in the sRGB values the X server
/// wants. Gamma-correct blending would look a shade different and cost a lot
/// more arithmetic; this is what terminals do.
fn blend(under: u32, colour: Rgb, alpha: u8) -> u32 {
    let mix = |shift: u32| {
        let under = (under >> shift) & 0xff;
        let over = (colour.0 >> shift) & 0xff;
        let value = (over * u32::from(alpha) + under * (255 - u32::from(alpha)) + 127) / 255;
        (value & 0xff) << shift
    };
    mix(16) | mix(8) | mix(0)
}

fn everything(rows: u16) -> Damage {
    match rows {
        0 => Damage::NONE,
        rows => Damage::rows(0, rows - 1),
    }
}

/// The modifier prefix `nvim_input_mouse` expects, e.g. `C-S`.
fn modifier_names(state: KeyButMask) -> String {
    let raw = u16::from(state);
    let mut names = Vec::new();
    for (bit, name) in [(0x04, "C"), (0x01, "S"), (0x08, "A")] {
        if raw & bit != 0 {
            names.push(name);
        }
    }
    names.join("-")
}

/// One thing `nvim.mode = "pane"` needs, and what was found when it was
/// looked for. `spokenpad check` prints these, so a machine that cannot open
/// a pane says which piece is missing before anyone dictates into it.
pub struct Requirement {
    pub what: &'static str,
    pub found: Result<String>,
}

/// Look for everything a pane needs, without opening one.
pub fn requirements(config: &crate::config::Nvim) -> Vec<Requirement> {
    let libraries = xkb::load().map(|()| {
        xkb::libraries()
            .iter()
            .map(|(file, _)| *file)
            .collect::<Vec<_>>()
            .join(", ")
    });
    let display = display_summary(config.display.as_deref());
    // Cells are measured at the display's resolution; with no display to ask,
    // at the 96 dpi an X server without `Xft.dpi` means.
    let dpi = display.as_ref().map_or(Dpi::DEFAULT, |(_, dpi)| *dpi);
    let font = Font::load(&config.font_family, config.font_size, dpi).map(|font| {
        let metrics = font.metrics();
        format!(
            "\"{}\" at {} on {dpi}: {}x{} pixel cells",
            config.font_family, config.font_size, metrics.width, metrics.height
        )
    });
    let display = display.map(|(summary, _)| summary);
    vec![
        Requirement {
            what: "X libraries",
            found: libraries,
        },
        Requirement {
            what: "font",
            found: font,
        },
        Requirement {
            what: "display",
            found: display,
        },
    ]
}

/// The display a pane would open on, the monitors it would choose from, and
/// its resolution.
fn display_summary(display: Option<&str>) -> Result<(String, Dpi)> {
    let name = display
        .context(
            "$DISPLAY was not set when spokenpad started; on Wayland without Xwayland, \
             set nvim.mode = \"attach\" and run `spokenpad editor` in a terminal",
        )?
        .to_owned();
    xkb::load()?;
    let cstring = std::ffi::CString::new(name.clone()).context("the display name has a NUL")?;
    let (connection, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&cstring))
        .with_context(|| format!("connect to {name}"))?;
    let rect = place::target(&connection, screen)?.monitor;
    let resolution = x11::xft_dpi(&connection);
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    Ok((
        format!(
            "{name}, {}x{} usable, {resolution}{}",
            rect.width,
            rect.height,
            match wayland {
                true => " (through Xwayland: the pane opens in a corner, not at the pointer)",
                false => "",
            }
        ),
        resolution.dpi(),
    ))
}

/// Wait for X events on a thread, so the pane's loop can block on one channel.
///
/// libxcb is thread-safe: this thread blocks in `wait_for_event` while the
/// pane's own thread keeps sending requests over the same connection.
fn watch(
    window: &Window,
    events: Sender<Event>,
    stopping: Arc<AtomicBool>,
) -> Result<JoinHandle<()>> {
    let connection = window.shared_connection();
    std::thread::Builder::new()
        .name("spokenpad-pane-x11".to_owned())
        .spawn(move || {
            use x11rb::connection::Connection;
            loop {
                match connection.wait_for_event() {
                    Ok(_) if stopping.load(Ordering::Acquire) => return,
                    Ok(event) => {
                        if events.send(Event::Window(event)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        log::debug!("pane: the X connection ended: {error}");
                        return;
                    }
                }
            }
        })
        .context("start the pane's X11 watcher thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_a_colour_over_itself_changes_nothing() {
        let colour = Rgb(0x3f_7a_c2);
        assert_eq!(blend(colour.0, colour, 255), colour.0);
        assert_eq!(blend(colour.0, colour, 0), colour.0);
        assert_eq!(blend(colour.0, colour, 128), colour.0);
    }

    #[test]
    fn full_coverage_paints_the_colour_and_none_leaves_the_background() {
        assert_eq!(blend(0x000000, Rgb(0xffffff), 255), 0xffffff);
        assert_eq!(blend(0x123456, Rgb(0xffffff), 0), 0x123456);
        // Half coverage of white on black is mid grey in every channel.
        assert_eq!(blend(0x000000, Rgb(0xffffff), 128), 0x808080);
    }

    #[test]
    fn mouse_modifiers_are_named_the_way_neovim_reads_them() {
        assert_eq!(modifier_names(KeyButMask::from(0_u16)), "");
        assert_eq!(modifier_names(KeyButMask::SHIFT), "S");
        assert_eq!(modifier_names(KeyButMask::CONTROL), "C");
        assert_eq!(
            modifier_names(KeyButMask::CONTROL | KeyButMask::SHIFT | KeyButMask::MOD1),
            "C-S-A"
        );
        // Caps Lock and the mouse buttons are not modifiers Neovim names.
        assert_eq!(modifier_names(KeyButMask::LOCK | KeyButMask::BUTTON1), "");
    }

    #[test]
    fn damage_over_no_rows_is_nothing_to_draw() {
        assert!(everything(0).is_empty());
        assert_eq!(everything(3).range(), Some((0, 2)));
    }
}
