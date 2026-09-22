//! The dictation pane: an X11 window spokenpad draws itself, with an embedded
//! Neovim behind it.
//!
//! The window never takes keyboard focus when it appears ([`x11`] explains the
//! properties that make that true), floats on tiling window managers or tiles
//! there (`nvim.pane_layout`), and needs no rule in the user's configuration;
//! on sway the daemon adds one over IPC before it opens. It opens beside the
//! pointer, never under it ([`geometry::placement`]), so under
//! focus-follows-mouse only a deliberate move into it focuses it. The user can
//! click into it and type.
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
//! as it does for an editor the user opened. The pane draws; it never writes to the
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
    geometry::{self, Anchor, Dimensions, Extents, Gap, Rect},
    grid::{Cell, CursorShape, Damage, RedrawEvent, Rgb, Screen, Style, Underline},
};
use crate::shell::nvim::rpc::waiting_for_keys;
use anyhow::{Context, Result, bail, ensure};
use font::{CellMetrics, Coverage, Face, Font};
use keyboard::Keyboard;
use rmpv::Value;
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
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
    /// An X event, and when it arrived: the watcher stamps it the moment it
    /// reads it, before the loop, which may be busy drawing, gets to it.
    Window {
        event: x11rb::protocol::Event,
        at: Instant,
    },
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
    /// The monitor to open on and the pointer to open beside, or `None` to
    /// open at the origin and let the window manager decide.
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
    Finished(Ending),
}

/// Why a pane finished, which decides what becomes of the capture it showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The user closed it, at `at`: the window manager asked
    /// (`WM_DELETE_WINDOW`, dated when it arrived), or Neovim said it was
    /// quitting because it was told to (`:q`, `:q!`, `:wq`, `:qa`, `:cq`;
    /// dated by the user's last key or click in the window, which is what
    /// told it). The daemon cancels a capture that was running then.
    ByUser { at: Instant },
    /// Neovim went away without saying so — killed, crashed, a deadly
    /// signal, `:restart` — or the pane lost its sources of events. Nothing the user meant: the
    /// capture goes on, and its next text opens a new pane.
    EditorDied,
}

/// Registered in the embedded Neovim once it is attached: on a quit it was
/// told to do, it says so on the pane's own channel — its stdio, the one
/// `--embed` made — before any channel closes. `VimLeavePre` runs for `:q`,
/// `:wq`, `:qa`, `:q!` and `:cq` with `v:dying` at 0; a deadly signal sets
/// `v:dying`, and a kill or a crash runs nothing at all. So the notice is
/// independent of how long the process then takes to exit — Neovim waits up
/// to two seconds for its jobs, an LSP among them — and a crash can never
/// produce it.
///
/// `:restart` (Neovim 0.12) runs `VimLeavePre` too, with `v:dying` at 0, and
/// then the process exits; its new server waits for a UI that handles the
/// `restart` event, which the pane does not. That is not the user closing the
/// pane either, so the notice also needs `v:exitreason` at `quit`, which
/// every quit above sets and `:restart` does not. Before 0.12 there is
/// neither.
const ANNOUNCE_LEAVING: &str = r#"
local channel
for _, chan in ipairs(vim.api.nvim_list_chans()) do
  if chan.stream == "stdio" and chan.mode == "rpc" then
    channel = chan.id
  end
end
if not channel then
  return
end
local reasoned = vim.fn.has("nvim-0.12") == 1
vim.api.nvim_create_autocmd("VimLeavePre", {
  group = vim.api.nvim_create_augroup("SpokenpadPane", { clear = true }),
  callback = function()
    if vim.v.dying == 0 and (not reasoned or vim.v.exitreason == "quit") then
      pcall(vim.rpcnotify, channel, "spokenpad_leaving")
    end
  end,
})
"#;

/// How long [`Pane::show`] waits for the window manager to frame and map the
/// window before it gives up placing it by its frame. The window is up by
/// then either way; this only bounds how long the first text waits behind it.
const FRAME_WAIT: Duration = Duration::from_millis(500);

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
    /// The monitor and the anchor the pane was placed by before the map,
    /// which [`Self::show`] places it by again once the window manager has
    /// framed it; `None` without a target.
    spot: Option<(Rect, Anchor)>,
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
    /// When Neovim said it was quitting because it was told to
    /// ([`ANNOUNCE_LEAVING`]); its channel closes a moment later. Never
    /// cleared: `VimLeavePre` runs once the exit can no longer be cancelled.
    leaving: Option<Instant>,
    /// When the user last typed or clicked into the window, as the X watcher
    /// read it: what a quit Neovim announces is dated by.
    last_input: Option<Instant>,
    /// Whether the last row says that the daemon is waiting for a call
    /// Neovim holds behind a half-typed command ([`HELD_NOTICES`]).
    held: bool,
}

/// What the pane draws over the start of its last row while the daemon waits
/// for a call Neovim holds. Neovim's own `showcmd` keeps the right end, which
/// shows the keys it is waiting on.
///
/// The longest that fits is drawn, whole: half a sentence ending mid-word
/// says less than a shorter one.
const HELD_NOTICES: [&str; 3] = [
    " waiting for the editor: finish or <Esc> the pending command ",
    " waiting for the editor: finish or <Esc> it ",
    " waiting: <Esc> ",
];
/// The columns `showcmd` draws in at the right end of the last row.
const SHOWCMD_COLUMNS: usize = 11;

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
        let dpi = display.dpi();
        let font = Font::load(&options.family, options.size, dpi)?;
        let metrics = font.metrics();
        let padding = dpi.padding();
        // A window is a whole number of cells inside a fixed margin: the pane
        // draws cells, and a partial column at the right edge is a strip that
        // only the margin's colour would ever fill. On a monitor, no more
        // cells than fit on it with the margin.
        let dimensions = match &options.target {
            Some(target) => {
                options
                    .dimensions
                    .fit(target.monitor, (metrics.width, metrics.height), padding)
            }
            None => options.dimensions,
        };
        let (columns, rows) = (dimensions.columns.get(), dimensions.lines.get());
        let width = metrics.width * u32::from(columns) + 2 * padding;
        let height = metrics.height * u32::from(rows) + 2 * padding;
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
        let spot = options.target.as_ref().map(|target| {
            let anchor = match target.pointer {
                Some(at) => Anchor::Pointer {
                    at,
                    gap: Gap::at(dpi),
                },
                None => Anchor::Corner,
            };
            (target.monitor, anchor)
        });
        // Nobody has said how large the frame will be yet, nor on which side
        // of this position the window manager will draw it: leave room for
        // one on every side. `show` places the frame itself once it exists.
        let placed = spot.map(|(monitor, anchor)| {
            geometry::placement(monitor, anchor, size, Extents::assumed(dpi))
        });
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
            canvas: Canvas::new(width, height, metrics, padding),
            columns,
            rows,
            spot,
            pending: Damage::NONE,
            frame_ready: false,
            focused: false,
            dragging: None,
            editor_gone: false,
            leaving: None,
            last_input: None,
            held: false,
        };
        let attach = pane.editor.attach(columns, rows)?;
        pane.await_response(attach, options.attach_timeout)
            .context("attach to the embedded nvim as a UI")?;
        // Sent, not waited for: Neovim sources the user's configuration after
        // the attach, and a startup message there raises a hit-enter prompt
        // that holds every call until the user answers it — in the window
        // that has to be mapped first. The answer arrives whenever Neovim
        // gets to it; `handle` logs it if it failed. For the same reason the
        // script finds the pane's channel itself rather than being told the
        // id `nvim_get_chan_info` would have to answer first.
        pane.editor.request(
            "nvim_exec_lua",
            vec![Value::from(ANNOUNCE_LEAVING), Value::Array(Vec::new())],
        )?;
        pane.watcher = Some(watch(&pane.window, sender, Arc::clone(&pane.stopping))?);
        Ok(pane)
    }

    /// A handle another thread can use to make [`Self::step`] return.
    pub fn waker(&self) -> x11::Waker {
        self.window.waker()
    }

    /// Map the window. It will not take the focus; see [`x11`].
    ///
    /// With a target, the window is then placed again, by the frame the
    /// window manager drew around it: the position asked for before the map
    /// left room for any frame, and this puts the frame itself the gap from
    /// the pointer, or flush in the corner. It is one move, before Neovim has
    /// drawn anything.
    pub fn show(&mut self) -> Result<()> {
        self.window.map()?;
        if let Some((monitor, anchor)) = self.spot
            && let Err(error) = self.frame_at(monitor, anchor)
        {
            log::debug!("could not place the pane by its frame: {error:#}");
        }
        Ok(())
    }

    /// Place the mapped window so that its frame, as the window manager drew
    /// it, lies where [`geometry::placement`] puts it.
    ///
    /// A window manager that sized the window itself — a tiling one that
    /// tiled it — decides where it goes too, and is left to.
    fn frame_at(&self, monitor: Rect, anchor: Anchor) -> Result<()> {
        ensure!(
            self.window.await_viewable(FRAME_WAIT)?,
            "the window manager did not show the pane within {FRAME_WAIT:?}"
        );
        let (window, frame) = self.window.framed()?;
        let size = (u32::from(self.canvas.width), u32::from(self.canvas.height));
        if (window.width, window.height) != size {
            log::debug!(
                "the window manager sized the pane {}x{}, so it places it too",
                window.width,
                window.height
            );
            return Ok(());
        }
        let placed = geometry::placement(monitor, anchor, size, frame);
        if (placed.x, placed.y) != (window.x, window.y) {
            self.window.place(placed)?;
        }
        Ok(())
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

    /// The blank margin around the grid, in pixels on each side.
    pub fn padding(&self) -> u32 {
        self.canvas.padding
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
            Err(RecvTimeoutError::Disconnected) => {
                return Ok(Status::Finished(Ending::EditorDied));
            }
        };
        let queued: Vec<Event> = self.events.try_iter().collect();
        let mut status = Status::Running;
        for event in first.into_iter().chain(queued) {
            if let finished @ Status::Finished(_) = self.handle(event)? {
                status = finished;
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

    /// Say, or stop saying, that the daemon is waiting for a call Neovim holds
    /// behind a half-typed command. Drawn at once: Neovim, which would
    /// otherwise end the frame, is the one not running.
    pub fn show_held(&mut self, held: bool) -> Result<()> {
        if held == self.held {
            return Ok(());
        }
        self.held = held;
        self.damage(Damage::row(self.rows.saturating_sub(1)));
        self.draw()
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
            Event::Editor(FromEditor::Leaving) => {
                // The quit was typed or clicked into this window, so it is
                // dated by that input, not by when Neovim got round to saying
                // so: a slow `VimLeavePre` must not make a key pressed right
                // after `:q<CR>` look like it came first, and cancel the
                // capture it started.
                let told = self.last_input.unwrap_or_else(Instant::now);
                self.leaving.get_or_insert(told);
                Ok(Status::Running)
            }
            Event::Editor(FromEditor::Gone) => {
                self.editor_gone = true;
                Ok(Status::Finished(match self.leaving {
                    Some(at) => Ending::ByUser { at },
                    None => Ending::EditorDied,
                }))
            }
            Event::Window { event, at } => self.window_event(event, at),
        }
    }

    fn window_event(&mut self, event: x11rb::protocol::Event, at: Instant) -> Result<Status> {
        use x11rb::protocol::Event as X;
        match event {
            X::Expose(_) => self.damage_everything(),
            X::ConfigureNotify(event) => self.resize(event.width, event.height)?,
            X::KeyPress(event) => {
                if let Some(keys) = self.keyboard.press(event.detail, event.state) {
                    self.last_input = Some(at);
                    self.editor.input(&keys)?;
                }
            }
            X::ButtonPress(event) => {
                self.last_input = Some(at);
                self.button(&event, true)?;
            }
            X::ButtonRelease(event) => {
                self.last_input = Some(at);
                self.button(&event, false)?;
            }
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
                return Ok(Status::Finished(Ending::ByUser { at }));
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
        let inside = |extent: u16| u32::from(extent).saturating_sub(2 * self.canvas.padding);
        let columns = (inside(width) / self.metrics.width).max(1) as u16;
        let rows = (inside(height) / self.metrics.height).max(1) as u16;
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

    /// Which cell a window coordinate is in, clamped to the grid: a click in
    /// the margin goes to the cell nearest it.
    fn cell_at(&self, x: i16, y: i16) -> (u16, u16) {
        let inside = |at: i16| (at.max(0) as u32).saturating_sub(self.canvas.padding);
        let column = (inside(x) / self.metrics.width).min(u32::from(u16::MAX)) as u16;
        let row = (inside(y) / self.metrics.height).min(u32::from(u16::MAX)) as u16;
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
        // The margin is Neovim's default background, like a terminal's
        // padding. Each row's band already covers its left and right margin
        // (`paint_row`); the strips above the first row and below the last
        // are painted with them. Below the last is also whatever a window
        // that is not a whole number of cells tall has left over.
        let background = self.screen.defaults().background;
        let height = usize::from(self.canvas.height);
        let padding = self.canvas.padding as usize;
        let cell = self.metrics.height as usize;
        let mut top = padding + usize::from(first) * cell;
        let mut bottom = (padding + (usize::from(last) + 1) * cell).min(height);
        if first == 0 {
            self.canvas.fill(
                0,
                0,
                u32::from(self.canvas.width),
                padding as u32,
                background,
            );
            top = 0;
        }
        if last == self.rows.saturating_sub(1) {
            let leftover = height.saturating_sub(bottom) as u32;
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
        let (origin, top) = canvas.origin(row, 0);
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
            let left = origin + column as u32 * metrics.width;
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
        if self.held && row == self.rows.saturating_sub(1) {
            self.paint_held_notice(row);
        }
    }

    /// The longest of [`HELD_NOTICES`] that leaves `showcmd` its columns, over
    /// the start of `row`, in the default colours swapped.
    fn paint_held_notice(&mut self, row: u16) {
        let defaults = self.screen.defaults();
        let style = Style {
            foreground: defaults.background,
            background: defaults.foreground,
            special: defaults.special,
            bold: false,
            italic: false,
            strikethrough: false,
            underline: None,
        };
        let room = usize::from(self.columns).saturating_sub(SHOWCMD_COLUMNS);
        let mut buffer = [0; 4];
        let Some(notice) = HELD_NOTICES
            .into_iter()
            .find(|notice| notice.chars().count() <= room)
        else {
            return;
        };
        for (column, character) in notice.chars().enumerate() {
            let (left, top) = self.canvas.origin(row, column as u16);
            let metrics = self.metrics;
            self.canvas
                .fill(left, top, metrics.width, metrics.height, style.background);
            let text: &str = character.encode_utf8(&mut buffer);
            self.canvas.cell(&mut self.font, text, style, left, top);
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
    let (left, top) = canvas.origin(row, position.column);
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
    /// The blank margin around the grid, in pixels on each side.
    padding: u32,
}

impl Canvas {
    fn new(width: u16, height: u16, metrics: CellMetrics, padding: u32) -> Self {
        Self {
            pixels: vec![0; usize::from(width) * usize::from(height)],
            width,
            height,
            metrics,
            padding,
        }
    }

    /// The top-left pixel of a cell.
    fn origin(&self, row: u16, column: u16) -> (u32, u32) {
        (
            self.padding + u32::from(column) * self.metrics.width,
            self.padding + u32::from(row) * self.metrics.height,
        )
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
/// then, and closing the window must not be the thing that loses it. An
/// editor the user opened in attach mode outlives the daemon; a pane's editor
/// does not, so it writes first.
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
/// Before either, a command the user left half typed is cancelled, one
/// `<Esc>` at a time, asking each time with `nvim_get_mode` -- which Neovim
/// answers at once even while it waits for a key -- and once more after the
/// last, then a call that takes back what that `<Esc>` typed, if it typed
/// anything. Each gets this long.
const MODE_TIMEOUT: Duration = Duration::from_millis(100);
const CANCEL_ATTEMPTS: u32 = 3;
const _: () = assert!(
    (CANCEL_ATTEMPTS as u128 + 2) * MODE_TIMEOUT.as_millis()
        + WRITE_TIMEOUT.as_millis()
        + QUIT_GRACE.as_millis()
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
        self.cancel_half_typed_command();
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

impl Pane {
    /// Cancel a command the user left half typed in the window: a count, `g`,
    /// `"`, `f`. Neovim runs no call while one is pending, the write below
    /// included, and a window closing mid-command would otherwise lose
    /// everything typed since the last utterance. Nothing is lost by
    /// cancelling it: the window is going away, and the command with it.
    fn cancel_half_typed_command(&mut self) {
        let was = match self.call("nvim_get_mode", Vec::new(), MODE_TIMEOUT) {
            Ok(mode) if waiting_for_keys(&mode) => mode_name(&mode).to_owned(),
            _ => return,
        };
        // What Neovim shows at the cursor while it waits, read from the
        // pane's own grid, which is current: Neovim draws before it waits,
        // and the answer above came after that. After `<C-v>` it is `^`,
        // after `<C-r>` a `"`, after `<C-k>` a `?`; after `<C-v>` and digits
        // it is the cell's own text again.
        let cursor = self.screen.cursor();
        let marker = self
            .screen
            .cell(cursor.row, cursor.column)
            .map(|cell| cell.text.clone())
            .unwrap_or_default();
        log::info!("cancelling a command left half typed in the pane, to write it");
        for _ in 0..CANCEL_ATTEMPTS {
            if self.editor.input("<Esc>").is_err() {
                return;
            }
            // Not `nvim_get_mode`: Neovim answers that the moment it arrives,
            // possibly before the `<Esc>` ahead of it is read. This call runs
            // only once the `<Esc>` is, so an answer means Neovim is free,
            // and says what the `<Esc>` did. No answer means it is still
            // waiting for keys, and the next `<Esc>` goes in; the query left
            // behind changes nothing when it runs.
            if let Ok(trace) = self.call(
                "nvim_exec_lua",
                vec![
                    Value::from(ESCAPE_TRACE),
                    Value::Array(vec![
                        Value::from(was.as_str()),
                        Value::from(marker.as_str()),
                        Value::from(cursor.row + 1),
                        Value::from(cursor.column + 1),
                    ]),
                ],
                MODE_TIMEOUT,
            ) {
                self.take_back_escape(&trace);
                return;
            }
        }
    }

    /// Take back what the `<Esc>` that cancelled a pending command typed,
    /// as [`ESCAPE_TRACE`] found it.
    ///
    /// In Insert or Replace mode an `<Esc>` is not always a cancel: after
    /// `<C-v>` it goes into the text as a literal ESC and Insert mode goes on,
    /// and after `<C-v>` and some digits it ends the number, whose character
    /// goes in, and leaves Insert mode. Either would be written into the
    /// file. The first is taken back with `<BS>`, which in Replace mode also
    /// puts back the character it replaced; the second is deleted.
    fn take_back_escape(&mut self, trace: &Value) {
        let field = |index: usize| trace.as_array().and_then(|parts| parts.get(index));
        match field(0).and_then(Value::as_str) {
            Some("backspace") => {
                log::info!("the <Esc> went into the text after <C-v>; taking it back");
                if let Err(error) = self.editor.input("<BS>") {
                    log::warn!("could not take back the <Esc>: {error:#}");
                }
            }
            Some("delete") => {
                log::info!("the <Esc> ended a <C-v> number; taking its character back");
                let (Some(row), Some(column)) = (
                    field(1).and_then(Value::as_i64),
                    field(2).and_then(Value::as_i64),
                ) else {
                    return;
                };
                let deleted = self.call(
                    "nvim_buf_set_text",
                    vec![
                        Value::from(0),
                        Value::from(row),
                        Value::from(column),
                        Value::from(row),
                        Value::from(column + 1),
                        Value::Array(Vec::new()),
                    ],
                    MODE_TIMEOUT,
                );
                if let Err(error) = deleted {
                    log::warn!("could not take back the <C-v> number: {error:#}");
                }
            }
            _ => {}
        }
    }
}

/// The `mode` of an `nvim_get_mode` answer, or "" without one.
fn mode_name(mode: &Value) -> &str {
    mode.as_map()
        .and_then(|fields| {
            fields
                .iter()
                .find(|(key, _)| key.as_str() == Some("mode"))
                .and_then(|(_, value)| value.as_str())
        })
        .unwrap_or("")
}

/// What a cancelling `<Esc>` typed: `{ "backspace" }`, `{ "delete", row,
/// column }` (0-based, the character to delete) or `{ "none" }`. Only
/// looks; see [`Pane::take_back_escape`].
///
/// Its arguments are what was true before the `<Esc>`: the mode, the text
/// Neovim showed at the cursor, and where the cursor was on the screen
/// (1-based). Each answer rests on those facts, never on the text alone,
/// which may hold an ESC or a control character of the user's own:
///
/// - `backspace`: `<C-v>` was pending (its `^` marker was showing), and
///   Insert mode goes on with an ESC before the cursor. That ESC is the
///   `<Esc>`.
/// - `delete`: Insert mode has ended, and the cursor is on a control
///   character at the very screen cell it was on before. Leaving Insert mode
///   moves the cursor left, so only a character inserted there by the
///   `<Esc>` ending a `<C-v>` number puts it back where it was. At the start
///   of a line the cursor cannot move left, and a control character of the
///   user's under it would look the same, so there no `^` may have been
///   showing: a `<C-v>` number typed right before one of the user's own
///   control characters at the start of a line is left in the file, since
///   the two cannot be told apart.
const ESCAPE_TRACE: &str = r#"
local was, marker, screen_row, screen_col = ...
local mode = vim.api.nvim_get_mode().mode
local row, col = unpack(vim.api.nvim_win_get_cursor(0))
local line = vim.api.nvim_get_current_line()
if mode:match("^[iR]") then
  if marker == "^" and col > 0 and line:byte(col) == 27 then
    return { "backspace" }
  end
elseif was:match("^[iR]") and (col > 0 or marker ~= "^") then
  local byte = line:byte(col + 1)
  local at = vim.fn.screenpos(0, row, col + 1)
  if byte and byte < 32 and byte ~= 9 and at.row == screen_row and at.col == screen_col then
    return { "delete", row - 1, col }
  end
end
return { "none" }
"#;

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
            // Its text is not logged: no transcript goes to the log.
            log::error!(
                "the pane could not write {named} ({why}) and could not keep its {} \
                 characters either ({error:#}); they are lost",
                text.chars().count()
            );
        }
    }
}

/// Write rescued text next to the file it came from, or into the state
/// directory when the buffer had no file.
fn keep_beside(name: &str, text: &str) -> Result<PathBuf> {
    let (stem, extension) = match name.is_empty() {
        false => (PathBuf::from(format!("{name}.unsaved")), ""),
        true => {
            let directory = crate::config::state_dir();
            std::fs::create_dir_all(&directory)?;
            let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S");
            (directory.join(format!("unsaved-{stamp}")), ".md")
        }
    };
    write_new(&stem, extension, text)
}

/// Write `text` to `<stem><extension>`, or `<stem>-1<extension>` and so on:
/// the first of those names nothing is at yet. So an earlier rescue is never
/// overwritten, a symlink planted at the name is never followed (a new file
/// is created with `O_EXCL`, which refuses one), and the file is always
/// created 0600, like the dictation files.
fn write_new(stem: &Path, extension: &str, text: &str) -> Result<PathBuf> {
    for collision in 0_u32..1000 {
        let mut name = stem.as_os_str().to_owned();
        if collision > 0 {
            name.push(format!("-{collision}"));
        }
        name.push(extension);
        let path = PathBuf::from(name);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        };
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        return Ok(path);
    }
    bail!("{} and 999 names after it are taken", stem.display())
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
                true => " (through Xwayland: the pane opens in a corner, not beside the pointer)",
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
                        let at = Instant::now();
                        if events.send(Event::Window { event, at }).is_err() {
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

    /// A rescue never replaces an earlier one, never writes through a
    /// symlink someone left at its name, and is private whatever was there.
    #[test]
    fn a_rescue_takes_a_new_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes.md");
        let earlier = directory.path().join("notes.md.unsaved");
        std::fs::write(&earlier, "the earlier rescue\n").unwrap();
        std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o644)).unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, "not ours\n").unwrap();
        std::os::unix::fs::symlink(&target, directory.path().join("notes.md.unsaved-1")).unwrap();

        let kept = keep_beside(notes.to_str().unwrap(), "dictated").unwrap();
        assert_eq!(kept, directory.path().join("notes.md.unsaved-2"));
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "dictated\n");
        let mode = std::fs::metadata(&kept).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&earlier).unwrap(),
            "the earlier rescue\n"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "not ours\n");
    }
}
