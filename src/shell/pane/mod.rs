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
//! Nothing in the daemon uses this yet: wiring `nvim.mode = "pane"` is the
//! next phase.
pub mod font;
pub mod keyboard;
pub mod ui;
pub mod x11;
pub mod xkb;

use crate::core::{
    geometry::Rect,
    grid::{Cell, CursorShape, Damage, Rgb, Screen, Style, Underline},
};
use anyhow::{Context, Result, bail, ensure};
use font::{CellMetrics, Coverage, Face, Font};
use keyboard::Keyboard;
use rmpv::Value;
use std::{
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
    /// The X display, or `None` for `$DISPLAY`.
    pub display: Option<String>,
    /// A fontconfig family name; `monospace` is the one every desktop has.
    pub family: String,
    /// Font size in pixels.
    pub size: f32,
    pub columns: u16,
    pub rows: u16,
    /// Where the top-left corner should go, or `None` to let the window
    /// manager decide.
    pub position: Option<(i32, i32)>,
    pub title: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            display: None,
            family: "monospace".to_owned(),
            size: 16.0,
            columns: 72,
            rows: 12,
            position: None,
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
    /// The window's pixels, `0x00RRGGBB`, row by row.
    pixels: Vec<u32>,
    width: u16,
    height: u16,
    columns: u16,
    rows: u16,
    /// Rows changed since the last frame was drawn.
    pending: Damage,
    focused: bool,
    /// Which mouse button is down, so motion can be reported as a drag.
    dragging: Option<&'static str>,
}

impl Pane {
    /// Open the window and start the editor. The window is **not** mapped yet:
    /// [`Self::show`] does that, once the caller has placed it.
    ///
    /// `command` must already carry `--embed`, and should carry `--listen` too
    /// so the daemon can append to the same editor.
    pub fn open(options: &Options, command: Command) -> Result<Self> {
        ensure!(
            options.columns > 0 && options.rows > 0,
            "the pane needs at least one cell"
        );
        let font = Font::load(&options.family, options.size)?;
        let metrics = font.metrics();
        let width = metrics.width * u32::from(options.columns);
        let height = metrics.height * u32::from(options.rows);
        // X11 counts a window's size in 16 bits, so a grid that would not fit
        // on any screen has to be refused rather than silently wrapped.
        let (width, height) = match (u16::try_from(width), u16::try_from(height)) {
            (Ok(width), Ok(height)) => (width, height),
            _ => bail!(
                "a {}x{} grid of {}x{} pixel cells is larger than any X11 window",
                options.columns,
                options.rows,
                metrics.width,
                metrics.height
            ),
        };
        let (x, y) = options.position.unwrap_or((0, 0));
        let window = Window::open(
            options.display.as_deref(),
            Rect {
                x,
                y,
                width: u32::from(width),
                height: u32::from(height),
            },
            &options.title,
        )?;
        let keyboard = Keyboard::new(window.connection())?;
        let (sender, events) = channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let watcher = watch(&window, sender.clone(), Arc::clone(&stopping))?;
        let mut editor = Editor::spawn(command, sender)?;
        let attach = editor.attach(options.columns, options.rows)?;
        let mut pane = Self {
            window,
            keyboard,
            font,
            metrics,
            screen: Screen::new(),
            editor,
            events,
            watcher: Some(watcher),
            stopping,
            pixels: vec![0; usize::from(width) * usize::from(height)],
            width,
            height,
            columns: options.columns,
            rows: options.rows,
            pending: Damage::NONE,
            focused: false,
            dragging: None,
        };
        pane.await_response(attach, Duration::from_secs(20))
            .context("attach to the embedded nvim as a UI")?;
        Ok(pane)
    }

    /// Map the window. It will not take the focus; see [`x11`].
    pub fn show(&mut self) -> Result<()> {
        self.window.map()
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
        (&self.pixels, self.width, self.height)
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
        self.draw()?;
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
            if let Event::Editor(FromEditor::Response {
                id: answered,
                error,
                result,
            }) = &event
                && *answered == id
            {
                ensure!(error.is_nil(), "nvim RPC {id} failed: {error}");
                return Ok(result.clone());
            }
            self.handle(event)?;
        }
    }

    fn handle(&mut self, event: Event) -> Result<Status> {
        match event {
            Event::Editor(FromEditor::Redraw(events)) => {
                for event in &events {
                    self.pending = self.pending.union(self.screen.apply(event));
                }
                Ok(Status::Running)
            }
            Event::Editor(FromEditor::Response { id, error, .. }) => {
                if !error.is_nil() {
                    log::warn!("pane: nvim reported an error for request {id}: {error}");
                }
                Ok(Status::Running)
            }
            Event::Editor(FromEditor::Gone) => Ok(Status::Finished),
            Event::Window(event) => self.window_event(event),
        }
    }

    fn window_event(&mut self, event: x11rb::protocol::Event) -> Result<Status> {
        use x11rb::protocol::Event as X;
        match event {
            X::Expose(_) => self.pending = self.pending.union(everything(self.rows)),
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
                self.pending = self.pending.union(Damage::row(self.screen.cursor().row));
            }
            X::FocusOut(_) => {
                self.focused = false;
                self.editor.set_focus(false)?;
                self.pending = self.pending.union(Damage::row(self.screen.cursor().row));
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

    /// A new window size means a new number of cells, which only Neovim can
    /// act on: it answers with a `grid_resize` and repaints.
    fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        self.width = width;
        self.height = height;
        self.pixels = vec![0; usize::from(width) * usize::from(height)];
        let columns = (u32::from(width) / self.metrics.width).max(1) as u16;
        let rows = (u32::from(height) / self.metrics.height).max(1) as u16;
        if (columns, rows) != (self.columns, self.rows) {
            self.columns = columns;
            self.rows = rows;
            self.editor.try_resize(columns, rows)?;
        }
        self.pending = self.pending.union(everything(self.rows));
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
        let Some((first, last)) = self.pending.range() else {
            return Ok(());
        };
        self.pending = Damage::NONE;
        let last = last.min(self.rows.saturating_sub(1));
        if first > last || self.width == 0 || self.height == 0 {
            return Ok(());
        }
        for row in first..=last {
            self.paint_row(row);
        }
        let top = usize::from(first) * self.metrics.height as usize;
        let mut bottom =
            ((usize::from(last) + 1) * self.metrics.height as usize).min(usize::from(self.height));
        if last == self.rows.saturating_sub(1) {
            // A window is rarely a whole number of cells tall. The strip under
            // the last row belongs to no cell, so nothing would ever paint it
            // and it would stay black under a light colourscheme.
            let background = self.screen.defaults().background;
            let leftover = u32::from(self.height) - bottom as u32;
            self.fill(
                0,
                bottom as u32,
                u32::from(self.width),
                leftover,
                background,
            );
            bottom = usize::from(self.height);
        }
        if top >= bottom {
            return Ok(());
        }
        let stride = usize::from(self.width);
        self.window.present(
            0,
            i16::try_from(top).unwrap_or(i16::MAX),
            self.width,
            &self.pixels[top * stride..bottom * stride],
        )
    }

    fn paint_row(&mut self, row: u16) {
        let background = self.screen.defaults().background;
        let (cell_width, cell_height) = (self.metrics.width, self.metrics.height);
        let top = u32::from(row) * cell_height;
        self.fill(0, top, u32::from(self.width), cell_height, background);
        let cells: Vec<Cell> = self.screen.row(row).to_vec();
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
            let style = self.screen.style(cell.highlight);
            let left = column as u32 * cell_width;
            self.fill(left, top, cell_width * span, cell_height, style.background);
            self.paint_cell(&cell.text, style, left, top);
            column += span as usize;
        }
        if self.screen.cursor().row == row {
            self.paint_cursor(row);
        }
    }

    fn paint_cell(&mut self, text: &str, style: Style, left: u32, top: u32) {
        let metrics = self.metrics;
        let baseline = top + metrics.baseline;
        if let Some(glyph) = self
            .font
            .glyph(text, Face::of(style.bold, style.italic))
            .cloned()
        {
            self.blit(&glyph, left, baseline, style.foreground);
        }
        if let Some(underline) = style.underline {
            let y = baseline.saturating_add_signed(metrics.underline);
            self.rule(underline, left, y, metrics.width, style.special);
        }
        if style.strikethrough {
            self.fill(
                left,
                top + metrics.strikethrough,
                metrics.width,
                metrics.thickness,
                style.special,
            );
        }
    }

    /// The cursor: filled while the pane has the focus, an outline when it
    /// does not, so the window says plainly that typing would go elsewhere.
    fn paint_cursor(&mut self, row: u16) {
        let position = self.screen.cursor();
        if position.column >= self.columns {
            return;
        }
        if self.screen.busy() {
            // Neovim is working, and where its cursor sits means nothing until
            // it is done. A terminal hides the cursor then, and so does this.
            return;
        }
        let style = self.screen.cursor_style();
        let cell = self
            .screen
            .cell(row, position.column)
            .cloned()
            .unwrap_or_default();
        let under = self.screen.style(cell.highlight);
        let (foreground, background) = match style.attribute {
            Some(highlight) => {
                let cursor = self.screen.style(highlight);
                (cursor.foreground, cursor.background)
            }
            // Attribute 0 means "swap the cell's own colours".
            None => (under.background, under.foreground),
        };
        let metrics = self.metrics;
        let left = u32::from(position.column) * metrics.width;
        let top = u32::from(row) * metrics.height;
        let fraction = |whole: u32| (whole * u32::from(style.percentage) / 100).max(1);
        if !self.focused {
            self.outline(left, top, metrics.width, metrics.height, background);
            return;
        }
        match style.shape {
            CursorShape::Block => {
                self.fill(left, top, metrics.width, metrics.height, background);
                self.paint_cell(
                    &cell.text,
                    Style {
                        foreground,
                        ..under
                    },
                    left,
                    top,
                );
            }
            CursorShape::Vertical => self.fill(
                left,
                top,
                fraction(metrics.width),
                metrics.height,
                background,
            ),
            CursorShape::Horizontal => {
                let thickness = fraction(metrics.height);
                self.fill(
                    left,
                    top + metrics.height - thickness,
                    metrics.width,
                    thickness,
                    background,
                );
            }
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

    /// A one-pixel frame, for the cursor of an unfocused pane.
    fn outline(&mut self, left: u32, top: u32, width: u32, height: u32, colour: Rgb) {
        self.fill(left, top, width, 1, colour);
        self.fill(left, top + height.saturating_sub(1), width, 1, colour);
        self.fill(left, top, 1, height, colour);
        self.fill(left + width.saturating_sub(1), top, 1, height, colour);
    }

    /// An underline in one of Neovim's styles.
    fn rule(&mut self, style: Underline, left: u32, y: u32, width: u32, colour: Rgb) {
        let thickness = self.metrics.thickness;
        match style {
            Underline::Single => self.fill(left, y, width, thickness, colour),
            Underline::Double => {
                self.fill(left, y, width, thickness, colour);
                self.fill(left, y + thickness * 2, width, thickness, colour);
            }
            Underline::Dotted => {
                for x in (left..left + width).step_by(2) {
                    self.fill(x, y, 1, thickness, colour);
                }
            }
            Underline::Dashed => {
                for x in (left..left + width).step_by(4) {
                    self.fill(x, y, 2, thickness, colour);
                }
            }
            Underline::Curl => {
                // A two-pixel zigzag reads as a wave at text sizes and needs
                // no curve rasteriser.
                for (step, x) in (left..left + width).enumerate() {
                    let offset = u32::from(step as u32 % 4 < 2);
                    self.fill(x, y + offset, 1, thickness, colour);
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

impl Drop for Pane {
    fn drop(&mut self) {
        self.editor.shutdown();
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

/// The command that starts the Neovim behind a pane.
///
/// It is the same argv every other spokenpad editor gets — the bundled or
/// configured init, the ownership marker, `--listen` on the dictation socket,
/// the file — with `--embed` added, which is what turns stdin and stdout into
/// the UI channel. The marker matters: it is how an `NvimSession` in attach
/// mode recognises this editor as spokenpad's and appends to it.
pub fn editor_command(config: &crate::config::Nvim, target: &std::path::Path) -> Result<Command> {
    use crate::shell::nvim;
    let marker = nvim::new_marker()?;
    nvim::write_marker(&nvim::marker_path(&config.socket_path), &marker)?;
    let init = nvim::resolve_init(config)?;
    let mut argv = nvim::editor_argv(config, target, &marker, init.as_deref())?;
    // After the program and whatever the user configured with it, before the
    // flags this build added.
    argv.insert(config.editor.len(), "--embed".to_owned());
    let (program, arguments) = argv.split_first().context("the nvim command is empty")?;
    let mut command = Command::new(program);
    command.args(arguments);
    Ok(command)
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
