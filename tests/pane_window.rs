//! P0 of the own-window plan: does an X11 window with the proposed properties
//! stay unfocused when it appears, and still accept a click and keystrokes?
//!
//! The check runs against an X server and a window manager this test starts
//! itself: an `Xvfb` on a free display number and an `i3` with a generated
//! config and its own IPC socket. It never opens anything on the user's
//! display, never reads the user's i3 config, and kills every process it
//! started, also when an assertion fails.
//!
//! The window under test is made by [`examples/pane_spike.rs`](../examples/pane_spike.rs),
//! which this test drives over stdin/stdout.
//!
//! Beside the pass criteria the test records an ablation: the same window with
//! one property dropped at a time. Those rows are printed (run with
//! `-- --nocapture`) and are the evidence in
//! `docs/experiments/2026-09-21-own-window-p0-properties.md`.

use anyhow::{Context, Result};
use serde_json::Value;
use spokenpad::core::wm::{HEADER_LEN, Message, encode, reply_length};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, channel},
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{
            BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
            KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Window,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
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

/// The X server this test owns, and the connection it talks to it over.
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
        let connection = wait_for(START_TIMEOUT, "the X server to accept connections", || {
            x11rb::connect(Some(&display)).ok()
        });
        let (connection, screen_index) = connection;
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

// -------------------------------------------------------------- the spike

/// One running `examples/pane_spike`, driven over its stdin and stdout.
struct Spike {
    stdin: ChildStdin,
    lines: Receiver<String>,
    seen: Vec<String>,
    child: Killed,
}

impl Spike {
    fn start(server: &XServer, arguments: &[&str]) -> Self {
        let mut child = Command::new(spike_binary())
            .arg("--display")
            .arg(&server.display)
            .args(arguments)
            .env("DISPLAY", &server.display)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the pane spike");
        let stdin = child.stdin.take().expect("the spike's stdin");
        let stdout = child.stdout.take().expect("the spike's stdout");
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            stdin,
            lines,
            seen: Vec::new(),
            child: Killed(child),
        }
    }

    fn send(&mut self, command: &str) {
        writeln!(self.stdin, "{command}").expect("write a command to the spike");
        self.stdin.flush().expect("flush the command");
    }

    fn pump(&mut self) {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
    }

    /// The first line starting with `prefix` that has not been taken yet.
    fn expect(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            self.pump();
            if let Some(index) = self
                .seen
                .iter()
                .position(|line| line.starts_with(prefix) || line == prefix)
            {
                return self.seen.remove(index);
            }
            assert!(
                Instant::now() < deadline,
                "the spike never said {prefix:?}; it said {:?}",
                self.seen
            );
            assert!(
                self.child.alive(),
                "the spike exited before saying {prefix:?}"
            );
            sleep(Duration::from_millis(20));
        }
    }

    fn count(&mut self, prefix: &str) -> usize {
        self.pump();
        self.seen
            .iter()
            .filter(|line| line.starts_with(prefix))
            .count()
    }

    fn window(&mut self) -> Window {
        let line = self.expect("window ");
        line["window ".len()..].parse().expect("a window id")
    }

    /// Position and size in root coordinates, read from the X server.
    fn geometry(&mut self) -> (i32, i32, u32, u32) {
        self.send("geometry");
        let line = self.expect("geometry ");
        let values: Vec<i64> = line["geometry ".len()..]
            .split_whitespace()
            .map(|word| word.parse().expect("a number"))
            .collect();
        let [x, y, width, height] = values[..] else {
            panic!("geometry needs four numbers, got {line:?}");
        };
        (x as i32, y as i32, width as u32, height as u32)
    }
}

/// `examples/pane_spike`, next to the test binary cargo just built.
fn spike_binary() -> PathBuf {
    let test = std::env::current_exe().expect("the test binary's path");
    let binary = test
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile>")
        .join("examples")
        .join("pane_spike");
    assert!(
        binary.is_file(),
        "{} is missing; build it with `cargo build --example pane_spike`",
        binary.display()
    );
    binary
}

// ------------------------------------------------------------ synthetic input

// spokenpad itself never synthesises input: `docs/constraints.md` forbids it
// in the program, because the tool this project replaced rewrote the core X
// keymap that way and corrupted keystrokes system-wide. The rule is about the
// program, not about a test. The three functions below fake pointer and key
// events through the XTEST extension, and they do it only against the Xvfb
// server this test started, which `assert_is_ours` re-checks on every call.
// Nothing here ever reaches the user's session.

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

/// A window made with the given properties, mapped once.
struct Mapped {
    spike: Spike,
    window: Window,
    observed: Observed,
}

fn map_window(server: &mut XServer, i3: &I3, arguments: &[&str]) -> Mapped {
    let mut spike = Spike::start(server, arguments);
    let window = spike.window();
    let observed = map_and_watch(server, i3, &mut spike, window);
    Mapped {
        spike,
        window,
        observed,
    }
}

/// Unmap a window that is already managed, then map it again. The focus is
/// read after the unmap, since the unmap itself hands the focus back.
fn remap(server: &mut XServer, i3: &I3, mapped: &mut Mapped) -> Observed {
    mapped.spike.send("unmap");
    mapped.spike.expect("unmapped");
    sleep(SETTLE);
    map_and_watch(server, i3, &mut mapped.spike, mapped.window)
}

/// Map an unmapped window and record what the map did to the focus.
fn map_and_watch(server: &mut XServer, i3: &I3, spike: &mut Spike, window: Window) -> Observed {
    // Keep the pointer in a corner: with focus_follows_mouse off it cannot
    // move the focus, but a window mapped under it would muddy the reading.
    move_pointer(server, 5, 5);
    let before = server.input_focus();
    spike.send("map");
    spike.expect("mapped");
    i3.wait_until_managed(window);
    sleep(SETTLE);
    let node = i3.node(window).expect("i3 still manages the window");
    Observed {
        focused: node.focused,
        floating: node.floating,
        stole_focus: server.input_focus() != before,
    }
}

/// The properties the plan proposes. `WM_CLASS` is not on the list because
/// the spike always sets it.
const PROPOSED: &[&str] = &[
    "--user-time",
    "0",
    "--window-type",
    "utility",
    "--input-hint",
    "true",
];

// ----------------------------------------------------------------- the check

#[test]
fn pane_window_never_takes_focus_on_i3() {
    if !tools_or_skip() {
        return;
    }
    let mut server = XServer::start();
    let i3 = I3::start(&server);
    let mut report: Vec<String> = Vec::new();

    // A normal window, so the workspace under test is never empty and the
    // focus has somewhere to be before each map.
    let mut base = Spike::start(
        &server,
        &[
            "--window-type",
            "normal",
            "--user-time",
            "none",
            "--instance",
            "pane-spike-base",
            "--map",
        ],
    );
    let base_window = base.window();
    base.expect("mapped");
    i3.wait_until_managed(base_window);
    sleep(SETTLE);
    assert!(
        i3.node(base_window).expect("base window").focused,
        "the plain window should hold the focus before each case"
    );

    // (a) not focused on map, on a workspace that already has a focused window
    let mut pane = map_window(&mut server, &i3, PROPOSED);
    assert!(
        !pane.observed.focused,
        "i3 focused the proposed window on map (tree)"
    );
    assert!(
        !pane.observed.stole_focus,
        "the X input focus moved when the proposed window was mapped"
    );
    assert_eq!(
        pane.spike.count("event focus-in"),
        0,
        "the window itself saw a FocusIn on map"
    );
    // (b) floating
    assert!(
        pane.observed.floating,
        "i3 did not float the utility window"
    );
    report.push(pane.observed.row("the proposed set"));

    // Placement: P2 must be able to put the pane where the pointer is.
    let (created_x, created_y, _, _) = pane.spike.geometry();
    pane.spike.send("configure 40 60 320 200");
    pane.spike.expect("ok configure");
    sleep(Duration::from_millis(300));
    let configured = pane.spike.geometry();
    pane.spike.send("net-moveresize 700 420 300 180");
    pane.spike.expect("ok net-moveresize");
    sleep(Duration::from_millis(300));
    let moveresized = pane.spike.geometry();
    report.push(format!(
        "| placement | created at {created_x},{created_y} | ConfigureWindow -> {:?} | _NET_MOVERESIZE_WINDOW -> {:?} |",
        configured, moveresized
    ));
    assert!(
        configured.0 == 40 && configured.1 == 60 || moveresized.0 == 700 && moveresized.1 == 420,
        "neither ConfigureWindow nor _NET_MOVERESIZE_WINDOW placed the floating window; \
         got {configured:?} and {moveresized:?}"
    );

    // (c) focused after a click inside it
    let (x, y, width, height) = pane.spike.geometry();
    let centre = (
        i16::try_from(x + width as i32 / 2).expect("on screen"),
        i16::try_from(y + height as i32 / 2).expect("on screen"),
    );
    click(&mut server, centre.0, centre.1);
    sleep(SETTLE);
    assert!(
        i3.node(pane.window).expect("the pane").focused,
        "a click did not focus the pane"
    );
    assert_eq!(
        server.input_focus(),
        pane.window,
        "the X input focus is not on the pane after a click"
    );
    assert!(
        pane.spike.count("event button-press") >= 1,
        "the click never reached the window"
    );

    // (d) typed keys arrive
    let keycode = keycode_for(&server, u32::from(b'a'));
    press_key(&mut server, keycode);
    press_key(&mut server, keycode);
    sleep(Duration::from_millis(300));
    assert_eq!(
        pane.spike.count("event key-press"),
        2,
        "typed keys did not reach the focused pane"
    );

    // (e) re-map after unmap is unfocused again, and the property that makes
    // that true is still 0 after the user has clicked and typed: the spike
    // never rewrites it, so no interaction can turn it into a real timestamp.
    pane.spike.send("read-user-time");
    assert_eq!(
        pane.spike.expect("user-time "),
        "user-time 0",
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
    report.push(again.row("the proposed set, re-mapped after a click and two keys"));

    // Ablation: a real timestamp in the same property is enough to lose it.
    // A toolkit would write one on every keystroke; spokenpad must not.
    pane.spike.send("user-time 1");
    pane.spike.expect("ok user-time");
    let with_timestamp = remap(&mut server, &i3, &mut pane);
    report.push(
        with_timestamp
            .row("the proposed set, `_NET_WM_USER_TIME` rewritten to 1 before the re-map"),
    );
    drop(pane.spike);
    sleep(SETTLE);

    // Ablation, and the positive control for every check above: without the
    // user time, i3 focuses the same window on map. If this ever stops being
    // true, the test is measuring nothing.
    let ablation = map_window(
        &mut server,
        &i3,
        &["--user-time", "none", "--window-type", "utility"],
    );
    assert!(
        ablation.observed.focused && ablation.observed.stole_focus,
        "without _NET_WM_USER_TIME the window was still not focused: the test \
         cannot tell focus from no focus, so the other rows prove nothing"
    );
    report.push(ablation.observed.row("no `_NET_WM_USER_TIME`"));
    drop(ablation.spike);
    sleep(SETTLE);

    // Ablation: the type only decides floating on i3.
    let normal = map_window(
        &mut server,
        &i3,
        &["--user-time", "0", "--window-type", "normal"],
    );
    report.push(
        normal
            .observed
            .row("`_NET_WM_WINDOW_TYPE_NORMAL` instead of `_UTILITY`"),
    );
    assert!(
        !normal.observed.focused,
        "user time 0 needs no window type on i3"
    );
    assert!(
        !normal.observed.floating,
        "a normal window floated without being asked to"
    );
    drop(normal.spike);
    sleep(SETTLE);

    // Ablation: `WM_TAKE_FOCUS` beside the proposed set.
    let mut take_focus_arguments = PROPOSED.to_vec();
    take_focus_arguments.push("--take-focus");
    let take_focus = map_window(&mut server, &i3, &take_focus_arguments);
    report.push(
        take_focus
            .observed
            .row("the proposed set plus `WM_TAKE_FOCUS`"),
    );
    drop(take_focus.spike);
    sleep(SETTLE);

    // Ablation: the ICCCM "No Input" model, with no user time.
    let no_input = map_window(
        &mut server,
        &i3,
        &[
            "--user-time",
            "none",
            "--window-type",
            "utility",
            "--input-hint",
            "false",
        ],
    );
    report.push(
        no_input
            .observed
            .row("`WM_HINTS input = False`, no user time"),
    );
    drop(no_input.spike);
    sleep(SETTLE);

    // Placement before the map: P2 wants the pane to appear where the pointer
    // is, not to jump there a frame later.
    let mut requested = PROPOSED.to_vec();
    requested.extend_from_slice(&[
        "--x", "220", "--y", "140", "--width", "400", "--height", "200",
    ]);
    let mut placed = map_window(&mut server, &i3, &requested);
    assert!(!placed.observed.focused, "the placed pane took focus");
    let at_map = placed.spike.geometry();
    report.push(format!(
        "| placement asked for before the map (`WM_NORMAL_HINTS` position 220,140, size 400x200) | landed at {:?} | | |",
        at_map
    ));
    drop(placed.spike);
    sleep(SETTLE);

    // (a, second half) not focused on map on an empty workspace, where i3's
    // `no_focus` rule does not apply.
    i3.command("workspace pane-spike-empty");
    sleep(SETTLE);
    let empty = map_window(&mut server, &i3, PROPOSED);
    assert!(
        !empty.observed.focused,
        "the pane took focus as the first window on an empty workspace (tree)"
    );
    assert!(
        !empty.observed.stole_focus,
        "the X input focus moved when the pane was mapped on an empty workspace"
    );
    report.push(empty.observed.row("the proposed set, empty workspace"));
    drop(empty.spike);

    println!("\ni3 {}, display {}", i3_version(&i3), server.display);
    println!("| window properties | focused on map | floating | X input focus |");
    println!("|---|---|---|---|");
    for line in report {
        println!("{line}");
    }
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
