//! The pane's X11 window: created so that no window manager will focus it,
//! mapped only when spokenpad asks, and drawn into with `PutImage`.
//!
//! The window carries five properties, all of them set **before the first
//! `MapWindow`**, which is when a window manager reads them:
//!
//! | property | what it does |
//! |---|---|
//! | `_NET_WM_USER_TIME = 0` | "do not focus this window when it is mapped" (EWMH). On i3 this is the whole guarantee, and it holds even for the first window on an empty workspace. |
//! | `_NET_WM_WINDOW_TYPE_UTILITY` | floats the window on tiling window managers, and blocks focus on the ones that ignore user time (bspwm, Hyprland's Xwayland). |
//! | `_NET_WM_STATE_ABOVE` | stacks the window above the one the user is typing in. KWin stacks a window it refused focus *below* the active one otherwise. It gives no focus anywhere measured. |
//! | `WM_HINTS input = True` | the ICCCM "passive input" model: the window manager may give the window the focus *later*, when the user clicks it, so they can type into it. |
//! | `WM_CLASS = spokenpad-pane` | a name no rule written for the managed-mode terminal can match, and the name the `no_focus` rule matches that spokenpad adds to sway before the map (`shell::wm::Wm::refuse_focus`): sway reads none of the properties above. |
//!
//! **`_NET_WM_USER_TIME` is written once, as 0, and never again.** The EWMH
//! contract is that a toolkit updates it to the timestamp of the last user
//! interaction; a window manager re-reads it at every map, so a window that
//! updated it would steal the focus the next time it appeared. That was
//! measured on i3, together with everything in the table above:
//! `docs/experiments/2026-09-21-own-window-p0-properties.md`; the stacking
//! and sway in `docs/experiments/2026-09-22-pane-stacking-and-sway.md`.
//!
//! `WM_TAKE_FOCUS` is deliberately absent: with `input = True` and a user time
//! of 0 it changes nothing, and announcing it would oblige us to answer it.
use crate::core::{
    font::{Dpi, XftDpi},
    geometry::Rect,
};
use anyhow::{Context, Result, ensure};
use std::{ffi::CString, sync::Arc};
use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::{Connection, RequestConnection as _},
    properties::{WmHints, WmHintsState, WmSizeHints, WmSizeHintsSpecification},
    protocol::{
        Event,
        xproto::{
            AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
            CreateWindowAux, EventMask, Gcontext, Gravity, ImageFormat, ImageOrder, PropMode,
            WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
    xcb_ffi::XCBConnection,
};

/// `WM_CLASS`. Deliberately not `spokenpad`: a `no_focus` rule a user wrote
/// for the managed-mode terminal must not match this window, because this
/// window needs no rule at all.
pub const INSTANCE: &str = "spokenpad-pane";
/// `WM_CLASS` class name; see [`INSTANCE`].
pub const CLASS: &str = "spokenpad-pane";

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_USER_TIME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_STATE,
        _NET_WM_STATE_ABOVE,
        // Not a standard property: the pane sends itself a client message of
        // this type to wake the thread blocked waiting for X events, so that
        // thread can notice it should stop.
        SPOKENPAD_PANE_WAKE,
    }
}

/// One X11 window and the connection that owns it.
///
/// The connection is `XCBConnection` rather than x11rb's pure-Rust one because
/// `xkbcommon`'s X11 half — the only way to read the user's real keyboard
/// layout, dead keys and all — takes a raw `xcb_connection_t`.
pub struct Window {
    connection: Arc<XCBConnection>,
    screen: usize,
    id: x11rb::protocol::xproto::Window,
    atoms: Atoms,
    context: Gcontext,
    /// Whether this X server wants the low byte of a pixel first, which every
    /// server on a little-endian machine does.
    low_byte_first: bool,
}

/// A connection to an X display, before any window exists on it: what the
/// pane needs to know about the display to decide how big its window is.
pub struct Display {
    connection: XCBConnection,
    screen: usize,
}

impl Display {
    pub fn connect(display: &str) -> Result<Self> {
        let name = CString::new(display).context("the display name contains a NUL")?;
        let (connection, screen) = XCBConnection::connect(Some(&name))
            .with_context(|| format!("connect to the X display {display}"))?;
        Ok(Self { connection, screen })
    }

    /// The resolution the pane sizes its font by; see [`xft_dpi`].
    pub fn dpi(&self) -> Dpi {
        xft_dpi(&self.connection).dpi()
    }
}

/// The display's `Xft.dpi`, looked up exactly where winit, and so Alacritty,
/// looks it up: x11rb's default resource database, which is the
/// `RESOURCE_MANAGER` property of the *first* screen's root window whatever
/// screen the display names, or `~/.Xresources`, then `~/.Xdefaults`, when
/// nobody ran `xrdb`, merged with `$XENVIRONMENT` or `~/.Xdefaults-<host>`;
/// queried as `Xft.dpi`, so `*dpi` matches and the last of equal entries wins.
///
/// When the answer is not a usable resolution the pane uses 96, and says why
/// in the debug log; `spokenpad check` prints the same reason.
pub fn xft_dpi(connection: &impl Connection) -> XftDpi {
    let found = match x11rb::resource_manager::new_from_default(connection) {
        Ok(database) => XftDpi::parse(database.get_string("Xft.dpi", "")),
        Err(error) => {
            log::debug!("cannot read the X resource database: {error}");
            XftDpi::Unset
        }
    };
    if !matches!(found, XftDpi::Set(_)) {
        log::debug!("pane resolution: {found}");
    }
    found
}

impl Window {
    /// Create the window with the property set above. It is **not** mapped:
    /// the caller decides when it appears, and nothing before [`Self::map`]
    /// can change what the window manager will do with it.
    pub fn open(display: Display, rect: Rect, title: &str) -> Result<Self> {
        let Display { connection, screen } = display;
        let connection = Arc::new(connection);
        check_visual(&connection, screen)?;
        let low_byte_first = connection.setup().image_byte_order == ImageOrder::LSB_FIRST;
        let root = connection.setup().roots[screen].root;
        let background = connection.setup().roots[screen].black_pixel;
        let id = connection.generate_id()?;
        let (x, y, width, height) = clamp(rect);
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                id,
                root,
                x,
                y,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(background)
                    .event_mask(
                        EventMask::EXPOSURE
                            | EventMask::STRUCTURE_NOTIFY
                            | EventMask::KEY_PRESS
                            | EventMask::KEY_RELEASE
                            | EventMask::BUTTON_PRESS
                            | EventMask::BUTTON_RELEASE
                            // Motion only while a button is down. Plain
                            // `POINTER_MOTION` would wake the pane for every
                            // pixel the pointer crosses on its way somewhere
                            // else, and a drag is the only motion it acts on.
                            | EventMask::BUTTON1_MOTION
                            | EventMask::BUTTON2_MOTION
                            | EventMask::BUTTON3_MOTION
                            | EventMask::FOCUS_CHANGE,
                    ),
            )?
            .check()
            .context("create the pane window")?;
        let atoms = Atoms::new(&*connection)?.reply()?;
        let context = connection.generate_id()?;
        connection
            .create_gc(context, id, &CreateGCAux::new().graphics_exposures(0))?
            .check()
            .context("create the pane's graphics context")?;
        let window = Self {
            connection,
            screen,
            id,
            atoms,
            context,
            low_byte_first,
        };
        window.set_properties(rect, title)?;
        window.connection.flush()?;
        Ok(window)
    }

    fn set_properties(&self, rect: Rect, title: &str) -> Result<()> {
        let connection = &self.connection;
        // Written once, as zero, and never again: see the module comment.
        connection.change_property32(
            PropMode::REPLACE,
            self.id,
            self.atoms._NET_WM_USER_TIME,
            AtomEnum::CARDINAL,
            &[0],
        )?;
        connection.change_property32(
            PropMode::REPLACE,
            self.id,
            self.atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[self.atoms._NET_WM_WINDOW_TYPE_UTILITY],
        )?;
        // The initial state, which a window manager reads at the first map.
        // Changing it later would take a client message to the root; the
        // pane never sends one.
        connection.change_property32(
            PropMode::REPLACE,
            self.id,
            self.atoms._NET_WM_STATE,
            AtomEnum::ATOM,
            &[self.atoms._NET_WM_STATE_ABOVE],
        )?;
        WmHints {
            input: Some(true),
            initial_state: Some(WmHintsState::Normal),
            ..WmHints::new()
        }
        .set(connection, self.id)?;
        let (x, y, width, height) = clamp(rect);
        WmSizeHints {
            position: Some((
                WmSizeHintsSpecification::ProgramSpecified,
                x.into(),
                y.into(),
            )),
            size: Some((
                WmSizeHintsSpecification::ProgramSpecified,
                width.into(),
                height.into(),
            )),
            min_size: Some((16, 16)),
            win_gravity: Some(Gravity::NORTH_WEST),
            ..WmSizeHints::new()
        }
        .set_normal_hints(connection, self.id)?;
        // ICCCM wants instance and class as two NUL-terminated strings in one
        // property.
        let mut class = Vec::with_capacity(INSTANCE.len() + CLASS.len() + 2);
        class.extend_from_slice(INSTANCE.as_bytes());
        class.push(0);
        class.extend_from_slice(CLASS.as_bytes());
        class.push(0);
        connection.change_property8(
            PropMode::REPLACE,
            self.id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            &class,
        )?;
        connection.change_property8(
            PropMode::REPLACE,
            self.id,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        )?;
        connection.change_property8(
            PropMode::REPLACE,
            self.id,
            self.atoms._NET_WM_NAME,
            self.atoms.UTF8_STRING,
            title.as_bytes(),
        )?;
        connection.change_property32(
            PropMode::REPLACE,
            self.id,
            self.atoms.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &[self.atoms.WM_DELETE_WINDOW],
        )?;
        Ok(())
    }

    pub fn id(&self) -> x11rb::protocol::xproto::Window {
        self.id
    }

    pub fn connection(&self) -> &XCBConnection {
        &self.connection
    }

    /// A second handle on the same connection, for the thread that waits for
    /// events while the pane's loop draws. libxcb is thread-safe, so requests
    /// from one thread and `wait_for_event` on another are allowed.
    pub fn shared_connection(&self) -> Arc<XCBConnection> {
        Arc::clone(&self.connection)
    }

    pub fn map(&self) -> Result<()> {
        self.connection.map_window(self.id)?.check()?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn unmap(&self) -> Result<()> {
        self.connection.unmap_window(self.id)?.check()?;
        self.connection.flush()?;
        Ok(())
    }

    /// Move and resize. A window manager that floats the window honours this;
    /// one that tiles it may not, which is why the pane asks for the position
    /// in `WM_NORMAL_HINTS` first and only corrects it here.
    pub fn place(&self, rect: Rect) -> Result<()> {
        let (x, y, width, height) = clamp(rect);
        self.connection
            .configure_window(
                self.id,
                &ConfigureWindowAux::new()
                    .x(i32::from(x))
                    .y(i32::from(y))
                    .width(u32::from(width))
                    .height(u32::from(height)),
            )?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    /// The window's position in root coordinates and its current size.
    pub fn geometry(&self) -> Result<Rect> {
        let root = self.connection.setup().roots[self.screen].root;
        let geometry = self.connection.get_geometry(self.id)?.reply()?;
        let origin = self
            .connection
            .translate_coordinates(self.id, root, 0, 0)?
            .reply()?;
        Ok(Rect {
            x: origin.dst_x.into(),
            y: origin.dst_y.into(),
            width: geometry.width.into(),
            height: geometry.height.into(),
        })
    }

    /// The next event, or `None` when none is queued.
    pub fn poll(&self) -> Result<Option<Event>> {
        Ok(self.connection.poll_for_event()?)
    }

    /// Whether this client message is the window manager asking the window to
    /// close.
    pub fn is_close_request(&self, event: &ClientMessageEvent) -> bool {
        event.format == 32
            && event.window == self.id
            && event.type_ == self.atoms.WM_PROTOCOLS
            && event.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW
    }

    /// A handle another thread can use to make this window deliver an event.
    pub fn waker(&self) -> Waker {
        Waker {
            connection: Arc::clone(&self.connection),
            window: self.id,
            atom: self.atoms.SPOKENPAD_PANE_WAKE,
        }
    }

    /// Send that wake-up, so a thread blocked in `wait_for_event` returns.
    pub fn wake(&self) -> Result<()> {
        let message = ClientMessageEvent::new(32, self.id, self.atoms.SPOKENPAD_PANE_WAKE, [0; 5]);
        self.connection
            .send_event(false, self.id, EventMask::NO_EVENT, message)?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }

    /// Copy a block of pixels into the window.
    ///
    /// `pixels` is `0x00RRGGBB` per pixel, `width` wide, laid out row by row,
    /// and lands with its top-left corner at `x, y`. A `PutImage` request has
    /// a size limit, so this sends as many whole rows at a time as fit.
    pub fn present(&self, x: i16, y: i16, width: u16, pixels: &[u32]) -> Result<()> {
        if width == 0 || pixels.is_empty() {
            return Ok(());
        }
        ensure!(
            pixels.len().is_multiple_of(usize::from(width)),
            "the pixel block is not a whole number of {width}-pixel rows"
        );
        let rows = pixels.len() / usize::from(width);
        let row_bytes = usize::from(width) * 4;
        // Leave room for the request header itself.
        let budget = self.connection.maximum_request_bytes().saturating_sub(64);
        let rows_per_request = (budget / row_bytes).clamp(1, rows);
        let depth = self.connection.setup().roots[self.screen].root_depth;
        let mut bytes = Vec::with_capacity(rows_per_request * row_bytes);
        for band in (0..rows).step_by(rows_per_request) {
            let height = rows_per_request.min(rows - band);
            bytes.clear();
            for pixel in &pixels[band * usize::from(width)..(band + height) * usize::from(width)] {
                match self.low_byte_first {
                    true => bytes.extend_from_slice(&pixel.to_le_bytes()),
                    false => bytes.extend_from_slice(&pixel.to_be_bytes()),
                }
            }
            self.connection
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    self.id,
                    self.context,
                    width,
                    u16::try_from(height).unwrap_or(u16::MAX),
                    x,
                    y.saturating_add(i16::try_from(band).unwrap_or(i16::MAX)),
                    0,
                    depth,
                    &bytes,
                )?
                .check()
                .context("copy pixels into the pane window")?;
        }
        self.connection.flush()?;
        Ok(())
    }
}

/// The screen must be one `PutImage` can feed without a conversion table:
/// 24 or 32 bits of colour in 32-bit pixels, with the usual channel masks.
fn check_visual(connection: &XCBConnection, screen: usize) -> Result<()> {
    let setup = connection.setup();
    let screen = &setup.roots[screen];
    let depth = screen.root_depth;
    ensure!(
        depth == 24 || depth == 32,
        "the pane needs a 24- or 32-bit display; this one is {depth}-bit"
    );
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == depth)
        .with_context(|| format!("the X server lists no pixmap format for depth {depth}"))?;
    ensure!(
        format.bits_per_pixel == 32,
        "the pane needs 32 bits per pixel; this display packs {}",
        format.bits_per_pixel
    );
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|allowed| allowed.visuals.iter())
        .find(|visual| visual.visual_id == screen.root_visual)
        .context("the X server does not describe its own root visual")?;
    ensure!(
        (visual.red_mask, visual.green_mask, visual.blue_mask) == (0xff0000, 0x00ff00, 0x0000ff),
        "the pane needs an RGB visual; this one masks {:#x}/{:#x}/{:#x}",
        visual.red_mask,
        visual.green_mask,
        visual.blue_mask
    );
    Ok(())
}

/// Wakes a pane's event loop from another thread.
///
/// It sends the window a client message of a type nothing else uses, which is
/// enough to make a thread blocked in `wait_for_event` return. That is what
/// lets the pane's loop block with no deadline at all and still answer a
/// close or a shutdown at once: the thread that asks also knocks.
///
/// libxcb is thread-safe, so this needs no lock of its own.
pub struct Waker {
    connection: Arc<XCBConnection>,
    window: x11rb::protocol::xproto::Window,
    atom: u32,
}

impl Waker {
    pub fn wake(&self) -> Result<()> {
        let message = ClientMessageEvent::new(32, self.window, self.atom, [0; 5]);
        self.connection
            .send_event(false, self.window, EventMask::NO_EVENT, message)?
            .check()?;
        self.connection.flush()?;
        Ok(())
    }
}

/// X11 speaks `i16` positions and `u16` sizes, and refuses a zero extent.
fn clamp(rect: Rect) -> (i16, i16, u16, u16) {
    let coordinate = |value: i32| value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    let extent = |value: u32| value.clamp(1, u32::from(i16::MAX as u16)) as u16;
    (
        coordinate(rect.x),
        coordinate(rect.y),
        extent(rect.width),
        extent(rect.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rectangle_is_clamped_into_what_x11_can_express() {
        assert_eq!(
            clamp(Rect {
                x: 10,
                y: 20,
                width: 300,
                height: 200
            }),
            (10, 20, 300, 200)
        );
        let huge = clamp(Rect {
            x: i32::MAX,
            y: i32::MIN,
            width: u32::MAX,
            height: 0,
        });
        assert_eq!(huge, (i16::MAX, i16::MIN, i16::MAX as u16, 1));
    }
}
