//! Spike: an X11 window carrying the properties that are meant to stop a
//! window manager from focusing it when it appears.
//!
//! Nothing in the daemon uses this. It exists so phase P0 of the own-window
//! plan can be measured: which property does what, on which window manager.
//! [`tests/pane_window.rs`](../tests/pane_window.rs) drives it over
//! stdin/stdout against an Xvfb display it starts itself; by hand it can be
//! pointed at any display with `--display`.
//!
//! Every property is set **before** the first `MapWindow`, which is what the
//! specifications require: the window manager reads them once, when it takes
//! the window over.
//!
//! Lines this program writes to stdout, one per line, flushed:
//!
//! | line | meaning |
//! |---|---|
//! | `window <id>` | the window exists, properties are set, it is not mapped yet |
//! | `mapped` / `unmapped` | a `map` / `unmap` command finished |
//! | `geometry <x> <y> <w> <h>` | answer to `geometry`, in root coordinates |
//! | `user-time <n>` \| `user-time none` | answer to `read-user-time` |
//! | `ok <command>` | any other command finished |
//! | `event key-press <keycode>` | a key reached this window |
//! | `event button-press <button>` | a pointer button reached this window |
//! | `event focus-in` / `event focus-out` | the X input focus entered or left |
//! | `event delete` | the window manager asked the window to close |
//!
//! Commands it reads from stdin, one per line:
//!
//! | command | effect |
//! |---|---|
//! | `map` / `unmap` | `MapWindow` / `UnmapWindow` |
//! | `user-time none` \| `user-time <n>` | rewrite `_NET_WM_USER_TIME` |
//! | `read-user-time` | read the property back from the server |
//! | `configure <x> <y> <w> <h>` | `ConfigureWindow` (an ICCCM move/resize) |
//! | `net-moveresize <x> <y> <w> <h>` | the EWMH `_NET_MOVERESIZE_WINDOW` message |
//! | `geometry` | report the window's position and size |
//! | `quit` | exit |

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::{
    io::{BufRead, Write},
    sync::mpsc::{Receiver, RecvTimeoutError, channel},
    time::Duration,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT,
    connection::Connection,
    properties::{WmHints, WmHintsState, WmSizeHints, WmSizeHintsSpecification},
    protocol::{
        Event,
        xproto::{
            AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
            EventMask, PropMode, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TAKE_FOCUS,
        _NET_WM_NAME,
        _NET_WM_USER_TIME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_MOVERESIZE_WINDOW,
    }
}

/// What `_NET_WM_WINDOW_TYPE` is set to before the first map.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum WindowType {
    /// Do not set the property at all.
    None,
    /// `_NET_WM_WINDOW_TYPE_NORMAL`.
    Normal,
    /// `_NET_WM_WINDOW_TYPE_UTILITY`: the type the plan proposes.
    Utility,
}

/// `_NET_WM_USER_TIME`: absent, or a timestamp. `0` is the EWMH value that
/// means "do not focus this window when it is mapped".
#[derive(Clone, Copy, PartialEq, Eq)]
struct UserTime(Option<u32>);

impl std::str::FromStr for UserTime {
    type Err = std::num::ParseIntError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == "none" {
            return Ok(Self(None));
        }
        text.parse().map(|time| Self(Some(time)))
    }
}

#[derive(Parser)]
#[command(about = "P0 spike: an X11 window that must not take focus when it is mapped")]
struct Args {
    /// X display to open, e.g. ":57". Defaults to $DISPLAY.
    #[arg(long, value_name = "DISPLAY")]
    display: Option<String>,
    /// `_NET_WM_USER_TIME` before the first map: "none", or a timestamp.
    #[arg(long, value_name = "NONE|MILLIS", default_value = "0")]
    user_time: UserTime,
    /// `_NET_WM_WINDOW_TYPE` before the first map.
    #[arg(long, value_enum, default_value_t = WindowType::Utility)]
    window_type: WindowType,
    /// The ICCCM `WM_HINTS` input flag. False is the "No Input" model.
    #[arg(long, value_name = "BOOL", default_value_t = true, action = clap::ArgAction::Set)]
    input_hint: bool,
    /// Also announce `WM_TAKE_FOCUS` in `WM_PROTOCOLS` (the "globally active"
    /// model). This program never answers it.
    #[arg(long)]
    take_focus: bool,
    /// `WM_CLASS` instance name.
    #[arg(long, default_value = "spokenpad-pane")]
    instance: String,
    /// `WM_CLASS` class name.
    #[arg(long, default_value = "spokenpad-pane")]
    class: String,
    /// `WM_NAME` and `_NET_WM_NAME`.
    #[arg(long, default_value = "spokenpad dictation")]
    title: String,
    #[arg(long, default_value_t = 480)]
    width: u16,
    #[arg(long, default_value_t = 240)]
    height: u16,
    /// Requested position, also written into `WM_NORMAL_HINTS`.
    #[arg(long, allow_negative_numbers = true)]
    x: Option<i16>,
    /// Requested position, also written into `WM_NORMAL_HINTS`.
    #[arg(long, allow_negative_numbers = true)]
    y: Option<i16>,
    /// Map the window at once instead of waiting for a `map` command.
    #[arg(long)]
    map: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (connection, screen_index) =
        x11rb::connect(args.display.as_deref()).context("open the X display")?;
    let screen = &connection.setup().roots[screen_index];
    let root = screen.root;
    let black = screen.black_pixel;
    let atoms = Atoms::new(&connection)?.reply()?;
    let window = connection.generate_id()?;

    connection.create_window(
        COPY_DEPTH_FROM_PARENT,
        window,
        root,
        args.x.unwrap_or(0),
        args.y.unwrap_or(0),
        args.width,
        args.height,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(black).event_mask(
            EventMask::EXPOSURE
                | EventMask::STRUCTURE_NOTIFY
                | EventMask::KEY_PRESS
                | EventMask::BUTTON_PRESS
                | EventMask::FOCUS_CHANGE,
        ),
    )?;
    set_properties(&connection, &atoms, window, &args)?;
    connection.flush()?;
    say(&format!("window {window}"));

    if args.map {
        connection.map_window(window)?;
        connection.flush()?;
        say("mapped");
    }
    serve(&connection, &atoms, root, window, commands())
}

/// Everything the window manager reads when it takes the window over. All of
/// it is in place before the window is ever mapped.
fn set_properties(
    connection: &RustConnection,
    atoms: &Atoms,
    window: Window,
    args: &Args,
) -> Result<()> {
    if let UserTime(Some(time)) = args.user_time {
        connection.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_USER_TIME,
            AtomEnum::CARDINAL,
            &[time],
        )?;
    }
    let window_type = match args.window_type {
        WindowType::None => None,
        WindowType::Normal => Some(atoms._NET_WM_WINDOW_TYPE_NORMAL),
        WindowType::Utility => Some(atoms._NET_WM_WINDOW_TYPE_UTILITY),
    };
    if let Some(window_type) = window_type {
        connection.change_property32(
            PropMode::REPLACE,
            window,
            atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[window_type],
        )?;
    }
    WmHints {
        input: Some(args.input_hint),
        initial_state: Some(WmHintsState::Normal),
        ..WmHints::new()
    }
    .set(connection, window)?;

    let position = args.x.zip(args.y).map(|(x, y)| {
        (
            WmSizeHintsSpecification::ProgramSpecified,
            x.into(),
            y.into(),
        )
    });
    WmSizeHints {
        position,
        size: Some((
            WmSizeHintsSpecification::ProgramSpecified,
            args.width.into(),
            args.height.into(),
        )),
        min_size: Some((16, 16)),
        win_gravity: Some(x11rb::protocol::xproto::Gravity::NORTH_WEST),
        ..WmSizeHints::new()
    }
    .set_normal_hints(connection, window)?;

    // ICCCM wants instance and class as two NUL-terminated strings in one
    // property. The name is deliberately not "spokenpad": a user rule written
    // for the managed-mode terminal must not match this window.
    let mut class = Vec::new();
    class.extend_from_slice(args.instance.as_bytes());
    class.push(0);
    class.extend_from_slice(args.class.as_bytes());
    class.push(0);
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        &class,
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        args.title.as_bytes(),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        atoms._NET_WM_NAME,
        atoms.UTF8_STRING,
        args.title.as_bytes(),
    )?;
    let mut protocols = vec![atoms.WM_DELETE_WINDOW];
    if args.take_focus {
        protocols.push(atoms.WM_TAKE_FOCUS);
    }
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.WM_PROTOCOLS,
        AtomEnum::ATOM,
        &protocols,
    )?;
    Ok(())
}

/// Alternate between X events and commands until `quit` or a broken stdin.
fn serve(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    window: Window,
    commands: Receiver<String>,
) -> Result<()> {
    loop {
        while let Some(event) = connection.poll_for_event()? {
            report(atoms, window, &event);
        }
        match commands.recv_timeout(Duration::from_millis(10)) {
            Ok(line) => {
                if !obey(connection, atoms, root, window, line.trim())? {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Run one command. `false` means the program should exit.
fn obey(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    window: Window,
    line: &str,
) -> Result<bool> {
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else {
        return Ok(true);
    };
    let numbers = |words: std::str::SplitWhitespace<'_>| -> Result<Vec<i32>> {
        words
            .map(|word| word.parse::<i32>().context("a coordinate"))
            .collect()
    };
    match command {
        "map" => {
            connection.map_window(window)?;
            connection.flush()?;
            say("mapped");
        }
        "unmap" => {
            connection.unmap_window(window)?;
            connection.flush()?;
            say("unmapped");
        }
        "user-time" => {
            let value: UserTime = words.next().unwrap_or("none").parse()?;
            match value {
                UserTime(Some(time)) => connection.change_property32(
                    PropMode::REPLACE,
                    window,
                    atoms._NET_WM_USER_TIME,
                    AtomEnum::CARDINAL,
                    &[time],
                )?,
                UserTime(None) => connection.delete_property(window, atoms._NET_WM_USER_TIME)?,
            };
            connection.flush()?;
            say("ok user-time");
        }
        "read-user-time" => {
            let reply = connection
                .get_property(
                    false,
                    window,
                    atoms._NET_WM_USER_TIME,
                    AtomEnum::CARDINAL,
                    0,
                    1,
                )?
                .reply()?;
            match reply.value32().and_then(|mut values| values.next()) {
                Some(time) => say(&format!("user-time {time}")),
                None => say("user-time none"),
            }
        }
        "configure" => {
            let values = numbers(words)?;
            let [x, y, width, height] = values[..] else {
                bail!("configure needs x y width height");
            };
            connection.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(u32::try_from(width)?)
                    .height(u32::try_from(height)?),
            )?;
            connection.flush()?;
            say("ok configure");
        }
        "net-moveresize" => {
            let values = numbers(words)?;
            let [x, y, width, height] = values[..] else {
                bail!("net-moveresize needs x y width height");
            };
            // EWMH _NET_MOVERESIZE_WINDOW flags: window gravity in the low
            // byte (0 = the gravity from WM_NORMAL_HINTS), then one bit each
            // for x, y, width, height, then the source indication
            // (1 = a normal application).
            const FLAGS: u32 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 12);
            let message = ClientMessageEvent::new(
                32,
                window,
                atoms._NET_MOVERESIZE_WINDOW,
                [FLAGS, x as u32, y as u32, width as u32, height as u32],
            );
            connection.send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                message,
            )?;
            connection.flush()?;
            say("ok net-moveresize");
        }
        "geometry" => {
            let geometry = connection.get_geometry(window)?.reply()?;
            let origin = connection
                .translate_coordinates(window, root, 0, 0)?
                .reply()?;
            say(&format!(
                "geometry {} {} {} {}",
                origin.dst_x, origin.dst_y, geometry.width, geometry.height
            ));
        }
        "quit" => return Ok(false),
        other => bail!("unknown command {other:?}"),
    }
    Ok(true)
}

fn report(atoms: &Atoms, window: Window, event: &Event) {
    match event {
        Event::KeyPress(event) => say(&format!("event key-press {}", event.detail)),
        Event::ButtonPress(event) => say(&format!("event button-press {}", event.detail)),
        Event::FocusIn(_) => say("event focus-in"),
        Event::FocusOut(_) => say("event focus-out"),
        Event::ClientMessage(event) => {
            let asks_to_close = event.format == 32
                && event.window == window
                && event.type_ == atoms.WM_PROTOCOLS
                && event.data.as_data32()[0] == atoms.WM_DELETE_WINDOW;
            if asks_to_close {
                say("event delete");
            }
        }
        _ => {}
    }
}

/// Read stdin on its own thread, so the X event loop never blocks on it.
fn commands() -> Receiver<String> {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    receiver
}

/// One line of the driver protocol, flushed: the test reads it as it arrives.
fn say(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
