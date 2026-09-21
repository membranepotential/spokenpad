//! Where the pane opens, and how big.
//!
//! The same rule managed mode uses, and the same pure functions decide it:
//! [`geometry::pick_output`] chooses the monitor and [`geometry::placement`]
//! puts a window of `nvim.window_fraction` of that monitor at the pointer,
//! clamped fully on-screen. What is here is only the asking: the monitors
//! from RandR, and the pointer from the X server itself.
//!
//! Managed mode asks the window manager over its IPC socket, because it has
//! to talk to i3 or sway anyway to prove the `no_focus` rule. The pane needs
//! no rule and no window manager, so it asks X directly — which also means it
//! works under a window manager spokenpad has never heard of.
//!
//! **Under Xwayland the pointer is not usable.** `QueryPointer` answers with
//! the last position the pointer had over an X window, so on a Wayland
//! desktop it is stale the moment the pointer leaves one: opening there would
//! put the window under where the mouse used to be. When `WAYLAND_DISPLAY` is
//! set the pointer is not asked for, and the window goes to the corner
//! `geometry::placement` falls back to.
use crate::core::geometry::{self, Rect};
use anyhow::{Context, Result};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto::ConnectionExt as _},
    xcb_ffi::XCBConnection,
};

/// The rectangle a pane should open in.
pub fn window(connection: &XCBConnection, screen: usize, fraction: f64) -> Result<Rect> {
    let outputs = outputs(connection, screen)?;
    let pointer = pointer(connection, screen);
    let output = geometry::pick_output(&outputs, pointer)
        .context("the X server reports no usable screen")?;
    Ok(geometry::placement(output, pointer, fraction))
}

/// The monitors, from RandR.
///
/// A server without the extension, or one that lists no monitor, still has a
/// root window, and its size is the one rectangle that is always true.
fn outputs(connection: &XCBConnection, screen: usize) -> Result<Vec<geometry::Output>> {
    let root = connection.setup().roots[screen].root;
    let found = monitors(connection, root).unwrap_or_default();
    if !found.is_empty() {
        return Ok(found);
    }
    let geometry = connection
        .get_geometry(root)?
        .reply()
        .context("ask the X server how big its screen is")?;
    Ok(vec![geometry::Output {
        rect: Rect {
            x: 0,
            y: 0,
            width: geometry.width.into(),
            height: geometry.height.into(),
        },
        primary: true,
        focused: false,
    }])
}

/// RandR's monitor list, which is what a desktop means by "monitors": one
/// entry per screen the user sees, already merged where outputs mirror.
fn monitors(
    connection: &XCBConnection,
    root: x11rb::protocol::xproto::Window,
) -> Option<Vec<geometry::Output>> {
    let reply = connection
        .randr_get_monitors(root, true)
        .ok()?
        .reply()
        .ok()?;
    Some(
        reply
            .monitors
            .iter()
            .filter(|monitor| monitor.width > 0 && monitor.height > 0)
            .map(|monitor| geometry::Output {
                rect: Rect {
                    x: monitor.x.into(),
                    y: monitor.y.into(),
                    width: monitor.width.into(),
                    height: monitor.height.into(),
                },
                primary: monitor.primary,
                // X has no notion of which monitor holds the keyboard focus;
                // `pick_output` falls back to the primary, then to the first.
                focused: false,
            })
            .collect(),
    )
}

/// Where the pointer is, when that can be known.
fn pointer(connection: &XCBConnection, screen: usize) -> Option<(i32, i32)> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
        // Xwayland answers with wherever the pointer last was over an X
        // window, which is not where it is. A window at a stale position is
        // worse than one in the corner, because it looks deliberate.
        log::debug!("on Wayland: the pane opens in a corner, since Xwayland's pointer is stale");
        return None;
    }
    let root = connection.setup().roots[screen].root;
    let reply = connection.query_pointer(root).ok()?.reply().ok()?;
    reply
        .same_screen
        .then_some((reply.root_x.into(), reply.root_y.into()))
}
