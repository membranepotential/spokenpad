//! The pane's X11 window: created so that no window manager will focus it,
//! mapped only when spokenpad asks, and drawn into with `PutImage`.
//!
//! The window carries four properties, all of them set **before the first
//! `MapWindow`**, which is when a window manager reads them:
//!
//! | property | what it does |
//! |---|---|
//! | `_NET_WM_USER_TIME = 0` | "do not focus this window when it is mapped" (EWMH). On i3 this is the whole guarantee, and it holds even for the first window on an empty workspace. |
//! | `_NET_WM_WINDOW_TYPE_UTILITY` | floats the window on tiling window managers, and blocks focus on the ones that ignore user time (bspwm, Hyprland's Xwayland). |
//! | `WM_HINTS input = True` | the ICCCM "passive input" model: the window manager may give the window the focus *later*, so the user can click in and type. |
//! | `WM_CLASS = spokenpad-pane` | a name no rule written for the managed-mode terminal can match. |
//!
//! **`_NET_WM_USER_TIME` is written once, as 0, and never again.** The EWMH
//! contract is that a toolkit updates it to the timestamp of the last user
//! interaction; a window manager re-reads it at every map, so a window that
//! updated it would steal the focus the next time it appeared. That was
//! measured on i3, together with everything in the table above:
//! `docs/experiments/2026-09-21-own-window-p0-properties.md`.
//!
//! `WM_TAKE_FOCUS` is deliberately absent: with `input = True` and a user time
//! of 0 it changes nothing, and announcing it would oblige us to answer it.
use crate::core::geometry::Rect;
use anyhow::{Context, Result};
use std::ffi::CString;
use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::Connection,
    properties::{WmHints, WmHintsState, WmSizeHints, WmSizeHintsSpecification},
    protocol::{
        Event,
        xproto::{
            AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, Gravity,
            PropMode, WindowClass,
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
    }
}

/// One X11 window and the connection that owns it.
///
/// The connection is `XCBConnection` rather than x11rb's pure-Rust one because
/// `xkbcommon`'s X11 half — the only way to read the user's real keyboard
/// layout, dead keys and all — takes a raw `xcb_connection_t`.
pub struct Window {
    connection: XCBConnection,
    screen: usize,
    id: x11rb::protocol::xproto::Window,
    atoms: Atoms,
}

impl Window {
    /// Create the window with the property set above. It is **not** mapped:
    /// the caller decides when it appears, and nothing before [`Self::map`]
    /// can change what the window manager will do with it.
    pub fn open(display: Option<&str>, rect: Rect, title: &str) -> Result<Self> {
        let display = display
            .map(CString::new)
            .transpose()
            .context("the display name contains a NUL")?;
        let (connection, screen) =
            XCBConnection::connect(display.as_deref()).context("connect to the X display")?;
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
                            | EventMask::POINTER_MOTION
                            | EventMask::FOCUS_CHANGE,
                    ),
            )?
            .check()
            .context("create the pane window")?;
        let atoms = Atoms::new(&connection)?.reply()?;
        let window = Self {
            connection,
            screen,
            id,
            atoms,
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

    pub fn screen(&self) -> usize {
        self.screen
    }

    pub fn atoms(&self) -> &Atoms {
        &self.atoms
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
    pub fn is_close_request(&self, event: &x11rb::protocol::xproto::ClientMessageEvent) -> bool {
        event.format == 32
            && event.window == self.id
            && event.type_ == self.atoms.WM_PROTOCOLS
            && event.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW
    }

    pub fn flush(&self) -> Result<()> {
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
