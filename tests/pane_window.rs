//! P0 of the own-window plan: does [`shell::pane::x11::Window`] stay unfocused
//! when it appears, and still accept a click and keystrokes?
//!
//! The check runs against an X server and a window manager this test starts
//! itself: an `Xvfb` on a free display number and an `i3` with a generated
//! config and its own IPC socket. It never opens anything on the user's
//! display, never reads the user's i3 config, and stops every process it
//! started, also when an assertion fails.
//!
//! The window under test is the one the pane ships, created by the library in
//! this process. Beside the pass criteria the test records an ablation: the
//! same window with one property rewritten before the map, over a second X
//! connection, since a window manager only ever reads what is on the window at
//! map time. Those rows are printed (run with `-- --nocapture`) and are the
//! evidence in
//! `docs/experiments/2026-09-21-own-window-p0-properties.md`.

use anyhow::{Context, Result};
use serde_json::Value;
use spokenpad::{
    core::{
        geometry::Rect,
        wm::{HEADER_LEN, Message, encode, reply_length},
    },
    shell::pane::x11::Window as Pane,
};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent,
            ConnectionExt as _, CreateWindowAux, EventMask, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
            MOTION_NOTIFY_EVENT, PropMode, Window, WindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

/// How long the window manager may take to settle after a map before the test
/// believes what the tree says. The research spike used 0.8 s; the tree is
/// polled first, so this is only the margin on top of that.
const SETTLE: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- environment

fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
}

/// This test exercises a real X server and a real window manager. A missing
/// one is a broken environment, not a reason to report a green suite, so it
/// fails unless the operator says otherwise.
fn tools_or_skip() -> bool {
    let missing: Vec<&str> = ["Xvfb", "i3"]
        .into_iter()
        .filter(|program| !on_path(program))
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("SPOKENPAD_ALLOW_MISSING_X11").is_some(),
        "{} not installed; this test needs a headless X server and i3 \
         (pacman: xorg-server-xvfb i3-wm). \
         Set SPOKENPAD_ALLOW_MISSING_X11=1 to skip it deliberately.",
        missing.join(" and ")
    );
    false
}

/// A child process that is stopped when the test leaves its scope, whether it
/// passed or panicked. `SIGTERM` first and `SIGKILL` only as a fallback: an X
/// server killed outright leaves `/tmp/.X<n>-lock` and its socket behind, and
/// every leftover costs the next run a display number.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        // SAFETY: `id()` is this child's pid, and the child is not reaped
        // until the `wait` below, so the pid cannot have been reused.
        let _ = unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Killed {
    fn alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

/// The X server this test owns, and the connection it watches it over. This
/// connection is the test's own: the pane opens a second one of its own.
struct XServer {
    number: u32,
    display: String,
    connection: RustConnection,
    root: Window,
    child: Killed,
}

impl XServer {
    fn start() -> Self {
        let number = free_display_number();
        // The user dictates on :0. Nothing in this test may reach it.
        assert!(
            number >= 50,
            "refusing to run on display :{number}; the test picks its own, above :50"
        );
        let display = format!(":{number}");
        let child = Killed(
            Command::new("Xvfb")
                .args([&display, "-screen", "0", "1280x800x24", "-nolisten", "tcp"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start Xvfb"),
        );
        let (connection, screen_index) =
            wait_for(START_TIMEOUT, "the X server to accept connections", || {
                x11rb::connect(Some(&display)).ok()
            });
        let root = connection.setup().roots[screen_index].root;
        Self {
            number,
            display,
            connection,
            root,
            child,
        }
    }

    /// Every synthetic event goes through this: it refuses any display this
    /// test did not start, and any display whose server has died.
    fn assert_is_ours(&mut self) {
        assert!(
            self.number >= 50,
            "display :{} is not one this test started",
            self.number
        );
        assert!(
            self.child.alive(),
            "the Xvfb this test started is gone; refusing to send events anywhere else"
        );
    }

    fn input_focus(&self) -> Window {
        self.connection
            .get_input_focus()
            .expect("ask for the input focus")
            .reply()
            .expect("the input focus")
            .focus
    }

    fn atom(&self, name: &str) -> u32 {
        self.connection
            .intern_atom(false, name.as_bytes())
            .expect("intern an atom")
            .reply()
            .expect("the atom")
            .atom
    }

    /// A plain, focusable window, so the workspace under test is not empty.
    /// It carries none of the pane's properties.
    fn plain_window(&self) -> Window {
        let id = self.connection.generate_id().expect("a window id");
        self.connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                self.root,
                0,
                0,
                320,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .expect("create a plain window");
        self.connection.map_window(id).expect("map it");
        self.connection.flush().expect("flush");
        id
    }
}

/// The lowest display number above :50 that no X server holds.
fn free_display_number() -> u32 {
    (50..200)
        .find(|number| {
            !Path::new(&format!("/tmp/.X{number}-lock")).exists()
                && !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        })
        .expect("a free X display number")
}

/// i3 with a generated config: `focus_follows_mouse no`, so only a click can
/// move the focus, and a private IPC socket, so no client of this test can
/// reach the i3 the user is running.
struct I3 {
    socket: PathBuf,
    _directory: tempfile::TempDir,
    _child: Killed,
}

impl I3 {
    fn start(server: &XServer) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("i3.sock");
        let config = directory.path().join("i3.config");
        std::fs::write(
            &config,
            format!(
                "# generated by tests/pane_window.rs; never the user's config\n\
                 focus_follows_mouse no\n\
                 ipc-socket {}\n",
                socket.display()
            ),
        )
        .expect("write the i3 config");
        let child = Killed(
            Command::new("i3")
                .args(["-c".as_ref(), config.as_os_str()])
                .env("DISPLAY", &server.display)
                .env_remove("I3SOCK")
                .env_remove("SWAYSOCK")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start i3"),
        );
        let this = Self {
            socket,
            _directory: directory,
            _child: child,
        };
        wait_for(START_TIMEOUT, "i3 to answer on its IPC socket", || {
            this.request(Message::GetVersion, "").ok()
        });
        this
    }

    /// One i3 IPC request over its own socket, framed by the daemon's own
    /// protocol code in `core::wm`.
    fn request(&self, message: Message, payload: &str) -> Result<String> {
        let mut stream = UnixStream::connect(&self.socket).context("connect to the i3 socket")?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(&encode(message, payload)?)?;
        stream.shutdown(Shutdown::Write).ok();
        let mut header = [0u8; HEADER_LEN];
        stream.read_exact(&mut header)?;
        let length = reply_length(&header, message)?;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body)?;
        Ok(String::from_utf8(body)?)
    }

    fn tree(&self) -> Value {
        serde_json::from_str(&self.request(Message::GetTree, "").expect("the i3 tree"))
            .expect("the i3 tree is JSON")
    }

    fn command(&self, command: &str) {
        let reply = self.request(Message::RunCommand, command).expect("run it");
        spokenpad::core::wm::check_command_reply(&reply).expect("i3 accepted the command");
    }

    /// What i3 says about one X11 window, once it manages it.
    fn node(&self, window: Window) -> Option<Node> {
        fn walk(node: &Value, window: Window, found: &mut Option<Node>) {
            if node.get("window").and_then(Value::as_u64) == Some(u64::from(window)) {
                *found = Some(Node {
                    focused: node.get("focused").and_then(Value::as_bool) == Some(true),
                    floating: matches!(
                        node.get("floating").and_then(Value::as_str),
                        Some("auto_on" | "user_on")
                    ),
                });
                return;
            }
            for key in ["nodes", "floating_nodes"] {
                for child in node
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if found.is_none() {
                        walk(child, window, found);
                    }
                }
            }
        }
        let mut found = None;
        walk(&self.tree(), window, &mut found);
        found
    }

    fn wait_until_managed(&self, window: Window) {
        wait_for(START_TIMEOUT, "i3 to manage the window", || {
            self.node(window)
        });
    }
}

/// What the i3 tree says about the window under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Node {
    focused: bool,
    floating: bool,
}

/// One map, as the window manager and the X server report it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    focused: bool,
    floating: bool,
    stole_focus: bool,
}

impl Observed {
    fn row(&self, label: &str) -> String {
        format!(
            "| {label} | {} | {} | {} |",
            yes_no(self.focused),
            yes_no(self.floating),
            if self.stole_focus {
                "moved"
            } else {
                "unchanged"
            }
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

// -------------------------------------------------- the window under test

/// The pane's window plus the events it has reported so far. The pane selects
/// its own events on its own connection, so this is what the real window sees.
struct Watched {
    window: Pane,
    key_press: usize,
    button_press: usize,
    focus_in: usize,
}

impl Watched {
    fn open(server: &XServer) -> Self {
        Self::at(
            server,
            Rect {
                x: 0,
                y: 0,
                width: 480,
                height: 240,
            },
        )
    }

    fn at(server: &XServer, rect: Rect) -> Self {
        let window = Pane::open(Some(&server.display), rect, "spokenpad dictation")
            .expect("open the pane window");
        Self {
            window,
            key_press: 0,
            button_press: 0,
            focus_in: 0,
        }
    }

    fn pump(&mut self) {
        while let Some(event) = self.window.poll().expect("poll the pane's events") {
            match event {
                Event::KeyPress(_) => self.key_press += 1,
                Event::ButtonPress(_) => self.button_press += 1,
                Event::FocusIn(_) => self.focus_in += 1,
                _ => {}
            }
        }
    }

    fn id(&self) -> Window {
        self.window.id()
    }
}

// ----------------------------------------------- rewriting properties to ablate

/// The ablation rewrites a property on the pane's window over the *test's*
/// connection, before the window is mapped. A window manager reads what is on
/// the window at map time and does not care which client put it there, so this
/// measures the shipped window with one property changed, rather than a
/// second, similar window the test built itself.
fn set_user_time(server: &XServer, window: Window, value: Option<u32>) {
    let atom = server.atom("_NET_WM_USER_TIME");
    match value {
        Some(time) => server
            .connection
            .change_property32(PropMode::REPLACE, window, atom, AtomEnum::CARDINAL, &[time])
            .expect("write _NET_WM_USER_TIME"),
        None => server
            .connection
            .delete_property(window, atom)
            .expect("delete _NET_WM_USER_TIME"),
    };
    server.connection.flush().expect("flush");
}

fn set_window_type(server: &XServer, window: Window, name: &str) {
    let property = server.atom("_NET_WM_WINDOW_TYPE");
    let value = server.atom(name);
    server
        .connection
        .change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::ATOM,
            &[value],
        )
        .expect("write _NET_WM_WINDOW_TYPE");
    server.connection.flush().expect("flush");
}

fn set_input_hint(server: &XServer, window: Window, input: bool) {
    x11rb::properties::WmHints {
        input: Some(input),
        initial_state: Some(x11rb::properties::WmHintsState::Normal),
        ..x11rb::properties::WmHints::new()
    }
    .set(&server.connection, window)
    .expect("write WM_HINTS");
    server.connection.flush().expect("flush");
}

fn announce_take_focus(server: &XServer, window: Window) {
    let protocols = server.atom("WM_PROTOCOLS");
    let delete = server.atom("WM_DELETE_WINDOW");
    let take_focus = server.atom("WM_TAKE_FOCUS");
    server
        .connection
        .change_property32(
            PropMode::REPLACE,
            window,
            protocols,
            AtomEnum::ATOM,
            &[delete, take_focus],
        )
        .expect("write WM_PROTOCOLS");
    server.connection.flush().expect("flush");
}

/// The EWMH way to move a window, as a client message to the root. The pane
/// itself uses `ConfigureWindow` ([`Pane::place`]); this is here to record
/// whether a window manager honours the other mechanism too.
fn net_moveresize(server: &XServer, window: Window, rect: Rect) {
    // Flags: window gravity in the low byte (0 = the gravity from
    // WM_NORMAL_HINTS), one bit each for x, y, width and height, then the
    // source indication (1 = a normal application).
    const FLAGS: u32 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 11) | (1 << 12);
    let message = ClientMessageEvent::new(
        32,
        window,
        server.atom("_NET_MOVERESIZE_WINDOW"),
        [FLAGS, rect.x as u32, rect.y as u32, rect.width, rect.height],
    );
    server
        .connection
        .send_event(
            false,
            server.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            message,
        )
        .expect("send _NET_MOVERESIZE_WINDOW");
    server.connection.flush().expect("flush");
}

// ------------------------------------------------------------ synthetic input

// spokenpad itself never synthesises input: `docs/constraints.md` forbids it
// in the program, because the tool this project replaced rewrote the core X
// keymap that way and corrupted keystrokes system-wide. The rule is about the
// program, not about a test. The functions below fake pointer and key events
// through the XTEST extension, and they do it only against the Xvfb server
// this test started, which `assert_is_ours` re-checks on every call. Nothing
// here ever reaches the user's session.

fn fake(server: &mut XServer, kind: u8, detail: u8, x: i16, y: i16) {
    server.assert_is_ours();
    server
        .connection
        .xtest_fake_input(kind, detail, 0, server.root, x, y, 0)
        .expect("fake an input event")
        .check()
        .expect("the X server accepted the faked event");
}

fn move_pointer(server: &mut XServer, x: i16, y: i16) {
    fake(server, MOTION_NOTIFY_EVENT, 0, x, y);
    server.connection.flush().expect("flush");
}

fn click(server: &mut XServer, x: i16, y: i16) {
    move_pointer(server, x, y);
    fake(server, BUTTON_PRESS_EVENT, 1, 0, 0);
    fake(server, BUTTON_RELEASE_EVENT, 1, 0, 0);
    server.connection.flush().expect("flush");
}

/// A keycode that produces `keysym` under the layout the server loaded.
fn keycode_for(server: &XServer, keysym: u32) -> u8 {
    let setup = server.connection.setup();
    let (first, last) = (setup.min_keycode, setup.max_keycode);
    let count = last - first + 1;
    let mapping = server
        .connection
        .get_keyboard_mapping(first, count)
        .expect("ask for the keyboard mapping")
        .reply()
        .expect("the keyboard mapping");
    let per_keycode = usize::from(mapping.keysyms_per_keycode);
    mapping
        .keysyms
        .chunks(per_keycode)
        .position(|symbols| symbols.contains(&keysym))
        .map(|index| first + u8::try_from(index).expect("a keycode fits a byte"))
        .unwrap_or_else(|| panic!("no keycode produces keysym {keysym:#x}"))
}

fn press_key(server: &mut XServer, keycode: u8) {
    fake(server, KEY_PRESS_EVENT, keycode, 0, 0);
    fake(server, KEY_RELEASE_EVENT, keycode, 0, 0);
    server.connection.flush().expect("flush");
}

// ------------------------------------------------------------------ utilities

fn wait_for<T>(timeout: Duration, what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = attempt() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(50));
    }
}

/// Map an unmapped window and record what the map did to the focus.
fn map_and_watch(server: &mut XServer, i3: &I3, pane: &mut Watched) -> Observed {
    // Keep the pointer in a corner: with focus_follows_mouse off it cannot
    // move the focus, but a window mapped under it would muddy the reading.
    move_pointer(server, 5, 5);
    let before = server.input_focus();
    pane.window.map().expect("map the pane");
    i3.wait_until_managed(pane.id());
    sleep(SETTLE);
    let node = i3.node(pane.id()).expect("i3 still manages the window");
    pane.pump();
    Observed {
        focused: node.focused,
        floating: node.floating,
        stole_focus: server.input_focus() != before,
    }
}

/// Unmap a window that is already managed, then map it again. The focus is
/// read after the unmap, since the unmap itself hands the focus back.
fn remap(server: &mut XServer, i3: &I3, pane: &mut Watched) -> Observed {
    pane.window.unmap().expect("unmap the pane");
    sleep(SETTLE);
    map_and_watch(server, i3, pane)
}

/// Open a fresh pane, let `ablate` rewrite its properties before the map, and
/// map it.
fn ablate(
    server: &mut XServer,
    i3: &I3,
    ablate: impl FnOnce(&XServer, Window),
) -> (Watched, Observed) {
    let mut pane = Watched::open(server);
    ablate(server, pane.id());
    let observed = map_and_watch(server, i3, &mut pane);
    (pane, observed)
}

// ----------------------------------------------------------------- the check

#[test]
fn the_pane_window_never_takes_focus_on_i3() {
    if !tools_or_skip() {
        return;
    }
    let mut server = XServer::start();
    let i3 = I3::start(&server);
    let mut report: Vec<String> = Vec::new();

    // A plain window, so the workspace under test is never empty and the focus
    // has somewhere to be before each map.
    let base = server.plain_window();
    i3.wait_until_managed(base);
    sleep(SETTLE);
    assert!(
        i3.node(base).expect("base window").focused,
        "the plain window should hold the focus before each case"
    );

    // (a) not focused on map, on a workspace that already has a focused window
    let mut pane = Watched::open(&server);
    let observed = map_and_watch(&mut server, &i3, &mut pane);
    assert!(!observed.focused, "i3 focused the pane on map (tree)");
    assert!(
        !observed.stole_focus,
        "the X input focus moved when the pane was mapped"
    );
    assert_eq!(pane.focus_in, 0, "the window itself saw a FocusIn on map");
    // (b) floating
    assert!(observed.floating, "i3 did not float the pane");
    report.push(observed.row("the properties the pane ships"));

    // Placement: P2 must be able to put the pane where the pointer is.
    let created = pane.window.geometry().expect("geometry");
    pane.window
        .place(Rect {
            x: 40,
            y: 60,
            width: 320,
            height: 200,
        })
        .expect("place the pane");
    sleep(Duration::from_millis(300));
    let configured = pane.window.geometry().expect("geometry");
    net_moveresize(
        &server,
        pane.id(),
        Rect {
            x: 700,
            y: 420,
            width: 300,
            height: 180,
        },
    );
    sleep(Duration::from_millis(300));
    let moveresized = pane.window.geometry().expect("geometry");
    report.push(format!(
        "| placement | created at {},{} | `Window::place` -> {:?} | `_NET_MOVERESIZE_WINDOW` -> {:?} |",
        created.x,
        created.y,
        (configured.x, configured.y, configured.width, configured.height),
        (
            moveresized.x,
            moveresized.y,
            moveresized.width,
            moveresized.height
        )
    ));
    assert_eq!(
        (
            configured.x,
            configured.y,
            configured.width,
            configured.height
        ),
        (40, 60, 320, 200),
        "`Window::place` did not place the floating pane where it was told"
    );

    // (c) focused after a click inside it
    let rect = pane.window.geometry().expect("geometry");
    let centre = (
        i16::try_from(rect.x + rect.width as i32 / 2).expect("on screen"),
        i16::try_from(rect.y + rect.height as i32 / 2).expect("on screen"),
    );
    click(&mut server, centre.0, centre.1);
    sleep(SETTLE);
    pane.pump();
    assert!(
        i3.node(pane.id()).expect("the pane").focused,
        "a click did not focus the pane"
    );
    assert_eq!(
        server.input_focus(),
        pane.id(),
        "the X input focus is not on the pane after a click"
    );
    assert!(pane.button_press >= 1, "the click never reached the window");

    // (d) typed keys arrive
    let keycode = keycode_for(&server, u32::from(b'a'));
    press_key(&mut server, keycode);
    press_key(&mut server, keycode);
    sleep(Duration::from_millis(300));
    pane.pump();
    assert_eq!(
        pane.key_press, 2,
        "typed keys did not reach the focused pane"
    );

    // (e) re-map after unmap is unfocused again, and the property that makes
    // that true is still 0 after the user has clicked and typed: the pane
    // never rewrites it, so no interaction can turn it into a real timestamp.
    assert_eq!(
        read_user_time(&server, pane.id()),
        Some(0),
        "_NET_WM_USER_TIME changed while the window was used"
    );
    let again = remap(&mut server, &i3, &mut pane);
    assert!(
        !again.focused,
        "the pane took focus when it was mapped a second time"
    );
    assert!(
        !again.stole_focus,
        "the X input focus moved on the second map"
    );
    report.push(again.row("as shipped, re-mapped after a click and two keys"));
    drop(pane);
    sleep(SETTLE);

    // Ablation: a real timestamp in the same property is enough to lose it.
    // A toolkit would write one on every keystroke; the pane must not.
    let (timestamped, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, Some(1))
    });
    report.push(observed.row("`_NET_WM_USER_TIME` rewritten to 1 before the map"));
    drop(timestamped);
    sleep(SETTLE);

    // Ablation, and the positive control for every check above: without the
    // user time, i3 focuses the same window on map. If this ever stops being
    // true, the test is measuring nothing.
    let (control, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, None)
    });
    assert!(
        observed.focused && observed.stole_focus,
        "without _NET_WM_USER_TIME the window was still not focused: the test \
         cannot tell focus from no focus, so the other rows prove nothing"
    );
    report.push(observed.row("no `_NET_WM_USER_TIME`"));
    drop(control);
    sleep(SETTLE);

    // Ablation: the type only decides floating on i3.
    let (normal, observed) = ablate(&mut server, &i3, |server, window| {
        set_window_type(server, window, "_NET_WM_WINDOW_TYPE_NORMAL")
    });
    assert!(!observed.focused, "user time 0 needs no window type on i3");
    assert!(
        !observed.floating,
        "a normal window floated without being asked to"
    );
    report.push(observed.row("`_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`"));
    drop(normal);
    sleep(SETTLE);

    // Ablation: `WM_TAKE_FOCUS` beside the shipped set.
    let (take_focus, observed) = ablate(&mut server, &i3, announce_take_focus);
    report.push(observed.row("as shipped, plus `WM_TAKE_FOCUS`"));
    drop(take_focus);
    sleep(SETTLE);

    // Ablation: the ICCCM "No Input" model, with no user time.
    let (no_input, observed) = ablate(&mut server, &i3, |server, window| {
        set_user_time(server, window, None);
        set_input_hint(server, window, false);
    });
    report.push(observed.row("`WM_HINTS input = False`, no user time"));
    drop(no_input);
    sleep(SETTLE);

    // Placement asked for before the map: P2 wants the pane to appear where
    // the pointer is, not to jump there a frame later.
    let mut placed = Watched::at(
        &server,
        Rect {
            x: 220,
            y: 140,
            width: 400,
            height: 200,
        },
    );
    let observed = map_and_watch(&mut server, &i3, &mut placed);
    assert!(!observed.focused, "the placed pane took focus");
    let landed = placed.window.geometry().expect("geometry");
    report.push(format!(
        "| placement asked for before the map (`WM_NORMAL_HINTS` position 220,140, size 400x200) | landed at {:?} | | |",
        (landed.x, landed.y, landed.width, landed.height)
    ));
    drop(placed);
    sleep(SETTLE);

    // (a, second half) not focused on map on an empty workspace, where i3's
    // `no_focus` rule does not apply.
    i3.command("workspace pane-empty");
    sleep(SETTLE);
    let mut empty = Watched::open(&server);
    let observed = map_and_watch(&mut server, &i3, &mut empty);
    assert!(
        !observed.focused,
        "the pane took focus as the first window on an empty workspace (tree)"
    );
    assert!(
        !observed.stole_focus,
        "the X input focus moved when the pane was mapped on an empty workspace"
    );
    report.push(observed.row("as shipped, empty workspace"));
    drop(empty);

    println!("\ni3 {}, display {}", i3_version(&i3), server.display);
    println!("| window properties | focused on map | floating | X input focus |");
    println!("|---|---|---|---|");
    for line in report {
        println!("{line}");
    }
}

fn read_user_time(server: &XServer, window: Window) -> Option<u32> {
    let atom = server.atom("_NET_WM_USER_TIME");
    server
        .connection
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
        .expect("ask for _NET_WM_USER_TIME")
        .reply()
        .expect("the property")
        .value32()
        .and_then(|mut values| values.next())
}

fn i3_version(i3: &I3) -> String {
    let reply = i3.request(Message::GetVersion, "").unwrap_or_default();
    serde_json::from_str::<Value>(&reply)
        .ok()
        .and_then(|value| {
            value
                .get("human_readable")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}
