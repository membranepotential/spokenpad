//! A headless desktop the pane tests start for themselves: an `Xvfb` on a
//! free display number and an `i3` with a generated config and its own IPC
//! socket. [`desktops`] starts sway, Openbox and KWin (Wayland and X11) the
//! same way.
//!
//! Nothing here ever touches the user's session. An Xvfb's display number is
//! above `:50`, i3 gets `-c` and a private `ipc-socket`, and every process is
//! stopped on the way out, also when an assertion fails.
#![allow(dead_code)]

pub mod desktops;

use anyhow::{Context, Result};
use serde_json::Value;
use spokenpad::core::wm::{HEADER_LEN, Message, encode, reply_length};
use std::{
    io::{BufRead, Read, Write},
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
        xproto::{
            AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _,
            CreateWindowAux, KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, PropMode,
            Window, WindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

/// How long a window manager may take to settle after a map before a test
/// believes what its tree says.
pub const SETTLE: Duration = Duration::from_millis(500);
pub const START_TIMEOUT: Duration = Duration::from_secs(15);

pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
}

/// These tests exercise real programs. A missing one is a broken environment,
/// not a reason to report a green suite, so it fails unless the operator says
/// otherwise.
pub fn tools_or_skip(programs: &[&str]) -> bool {
    let missing: Vec<&str> = programs
        .iter()
        .copied()
        .filter(|program| !on_path(program))
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("SPOKENPAD_ALLOW_MISSING_X11").is_some(),
        "{} not installed; this test needs a headless X server, i3, nvim and \
         fontconfig, the HiDPI test Alacritty, and the window manager tests sway, \
         Xwayland, Openbox and KWin on Wayland and X11 (pacman: xorg-server-xvfb \
         i3-wm neovim xorg-setxkbmap alacritty sway xorg-xwayland openbox kwin \
         kwin-x11). \
         Set SPOKENPAD_ALLOW_MISSING_X11=1 to skip it deliberately.",
        missing.join(", ")
    );
    false
}

/// A child process that is stopped when the test leaves its scope, whether it
/// passed or panicked. `SIGTERM` first and `SIGKILL` only as a fallback: an X
/// server killed outright leaves `/tmp/.X<n>-lock` and its socket behind, and
/// every leftover costs the next run a display number.
pub struct Killed(pub Child);

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
    pub fn alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

/// Where an X server came from, which decides what "this test's own" means
/// for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// An Xvfb this test started, on a number above `:50`.
    Xvfb,
    /// The Xwayland of a Wayland compositor this test started. Its number is
    /// whichever one the compositor took, and the test learned it from that
    /// compositor, never from its own environment.
    Xwayland,
}

/// The X server this test owns, and the connection it watches it over. This
/// connection is the test's own: the pane opens a second one of its own.
pub struct XServer {
    number: u32,
    pub display: String,
    pub connection: RustConnection,
    pub root: Window,
    origin: Origin,
    /// The process this server lives and dies with: the Xvfb itself, or the
    /// compositor that runs Xwayland. Declared after the connection, so it is
    /// stopped after the connection closes.
    owner: Killed,
}

impl XServer {
    /// Start an X server of this test's own, above `:50`.
    ///
    /// The number is claimed by *starting* a server on it rather than by
    /// looking for a free one: an X server takes its lock atomically, so two
    /// test binaries asking at the same moment cannot both get it — one wins
    /// and the other's server exits, and this moves on to the next number.
    /// Looking first and starting afterwards is the race.
    ///
    /// `-displayfd` is the readiness signal: the server writes the number it
    /// took once it is listening, so there is nothing to poll. It does not
    /// choose the number, because a number given on the command line fixes
    /// it — which is what keeps this above the `:50` the user's session will
    /// never be on.
    pub fn start() -> Self {
        Self::start_with_screens(1)
    }

    /// The same, with this many X screens, so a test can open on `:N.1`.
    /// `display`, `connection` and `root` are always screen 0's.
    pub fn start_with_screens(screens: u32) -> Self {
        for number in 50..200 {
            if let Some(server) = Self::try_start(number, screens) {
                return server;
            }
        }
        panic!("no free X display number between :50 and :200");
    }

    fn try_start(number: u32, screens: u32) -> Option<Self> {
        let display = format!(":{number}");
        let screen_args = (0..screens).flat_map(|screen| {
            [
                "-screen".to_owned(),
                screen.to_string(),
                "1280x800x24".to_owned(),
            ]
        });
        let mut child = Killed(
            Command::new("Xvfb")
                .args([&display, "-displayfd", "1"])
                .args(screen_args)
                .args(["-nolisten", "tcp"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("start Xvfb"),
        );
        // The server says it is listening by writing its number; one that
        // could not take the display closes its stdout instead.
        read_display_number(&mut child).filter(|taken| *taken == number)?;
        let (connection, screen_index) =
            wait_for(START_TIMEOUT, "the X server to accept connections", || {
                x11rb::connect(Some(&display)).ok()
            });
        let root = connection.setup().roots[screen_index].root;
        Some(Self {
            number,
            display,
            connection,
            root,
            origin: Origin::Xvfb,
            owner: child,
        })
    }

    /// The Xwayland of a compositor this test started, on the display that
    /// compositor named. `compositor` is owned from here on: dropping the
    /// server stops it. Stopping its Xwayland first is the desktop's job
    /// (`desktops::Sway`, `desktops::KwinWayland`).
    pub fn xwayland(display: String, compositor: Killed) -> Self {
        let number = display
            .trim_start_matches(':')
            .parse()
            .unwrap_or_else(|_| panic!("{display:?} is not a local X display"));
        let (connection, screen_index) =
            wait_for(START_TIMEOUT, "Xwayland to accept connections", || {
                x11rb::connect(Some(&display)).ok()
            });
        let root = connection.setup().roots[screen_index].root;
        Self {
            number,
            display,
            connection,
            root,
            origin: Origin::Xwayland,
            owner: compositor,
        }
    }

    /// The pid of the process this server lives with.
    pub fn owner_pid(&self) -> u32 {
        self.owner.0.id()
    }

    /// Load a keyboard layout into this server, the way a user would.
    pub fn set_layout(&self, layout: &str) {
        let status = Command::new("setxkbmap")
            .args(["-display", &self.display, layout])
            .status()
            .expect("run setxkbmap");
        assert!(status.success(), "setxkbmap {layout} failed");
    }

    /// Replace the X resources on this server, the way `xrdb -load` does:
    /// the text of the root window's `RESOURCE_MANAGER`. `Xft.dpi` in it is
    /// what the pane and Alacritty both take the display's resolution from.
    ///
    /// Written over the test's own connection rather than with `xrdb`: an
    /// Xvfb resets, and forgets every property, when its last client
    /// disconnects, and this connection lives as long as the server.
    pub fn set_resources(&self, resources: &str) {
        self.connection
            .change_property8(
                PropMode::REPLACE,
                self.root,
                AtomEnum::RESOURCE_MANAGER,
                AtomEnum::STRING,
                resources.as_bytes(),
            )
            .expect("write RESOURCE_MANAGER")
            .check()
            .expect("the server accepts RESOURCE_MANAGER");
    }

    /// Remove `RESOURCE_MANAGER`, as on a server where nobody ran `xrdb`.
    pub fn clear_resources(&self) {
        self.connection
            .delete_property(self.root, AtomEnum::RESOURCE_MANAGER.into())
            .expect("delete RESOURCE_MANAGER")
            .check()
            .expect("the server deletes RESOURCE_MANAGER");
    }

    /// Every synthetic event goes through this: it refuses any display this
    /// test did not start, and any display whose server has died.
    pub fn assert_is_ours(&mut self) {
        assert!(
            self.origin == Origin::Xwayland || self.number >= 50,
            "display :{} is not one this test started",
            self.number
        );
        assert!(
            self.owner.alive(),
            "the X server this test started is gone; refusing to send events anywhere else"
        );
    }

    /// Wait until the server has handled every request this connection sent.
    /// A property written here, then a map sent over another connection,
    /// reaches the server in that order only after this.
    pub fn sync(&self) {
        self.connection.flush().expect("flush");
        self.input_focus();
    }

    pub fn input_focus(&self) -> Window {
        self.connection
            .get_input_focus()
            .expect("ask for the input focus")
            .reply()
            .expect("the input focus")
            .focus
    }

    pub fn atom(&self, name: &str) -> u32 {
        self.connection
            .intern_atom(false, name.as_bytes())
            .expect("intern an atom")
            .reply()
            .expect("the atom")
            .atom
    }

    /// A plain window of the test's own, not mapped yet.
    pub fn unmapped_window(&self) -> Window {
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
        id
    }

    /// A plain, focusable window, so the workspace under test is not empty.
    /// It carries none of the pane's properties.
    pub fn plain_window(&self) -> Window {
        let id = self.unmapped_window();
        self.connection.map_window(id).expect("map it");
        self.connection.flush().expect("flush");
        id
    }
}

/// The display number Xvfb wrote to its stdout once it was ready, or `None`
/// when it gave up on that display — which is how a number already taken by
/// somebody else announces itself.
fn read_display_number(child: &mut Killed) -> Option<u32> {
    let stdout = child.0.stdout.take().expect("Xvfb's stdout");
    let mut reader = std::io::BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        // One `read_line` is normally enough; the loop is for the case where
        // the pipe hands over the digits and the newline separately.
        let read = reader.read_line(&mut line).ok()?;
        if let Some(number) = line.trim().parse().ok().filter(|_| line.contains('\n')) {
            return Some(number);
        }
        if read == 0 {
            return None;
        }
        assert!(
            Instant::now() < deadline,
            "Xvfb did not name a display within {START_TIMEOUT:?}"
        );
    }
}

/// What moves the focus in a test's i3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I3Focus {
    /// `focus_follows_mouse no`: only a click moves the focus. i3's own title
    /// font, which ignores `Xft.dpi`.
    Click,
    /// `focus_follows_mouse yes`, i3's default and the user's setting: the
    /// pointer entering a window's frame focuses it. With a Pango title font,
    /// as a desktop in daily use has, so the title bar grows with `Xft.dpi`.
    FollowsMouse,
}

/// i3 with a generated config: by default `focus_follows_mouse no`, so only a
/// click can move the focus, and a private IPC socket, so no client of this
/// test can reach the i3 the user is running.
pub struct I3 {
    ipc: Ipc,
    _directory: tempfile::TempDir,
    _child: Killed,
}

impl I3 {
    pub fn start(server: &XServer) -> Self {
        Self::start_with(server, I3Focus::Click)
    }

    /// i3 as [`Self::start`], with `focus` deciding what moves the focus. It
    /// reads `Xft.dpi` when it starts, so a test sets that first.
    pub fn start_with(server: &XServer, focus: I3Focus) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let socket = directory.path().join("i3.sock");
        let config = directory.path().join("i3.config");
        let focus = match focus {
            I3Focus::Click => "focus_follows_mouse no\n",
            I3Focus::FollowsMouse => "focus_follows_mouse yes\nfont pango:monospace 8\n",
        };
        std::fs::write(
            &config,
            format!(
                "# generated by the spokenpad pane tests; never the user's config\n\
                 {focus}\
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
            ipc: Ipc { socket },
            _directory: directory,
            _child: child,
        };
        wait_for(START_TIMEOUT, "i3 to answer on its IPC socket", || {
            this.request(Message::GetVersion, "").ok()
        });
        this
    }

    /// One i3 IPC request over its own socket.
    pub fn request(&self, message: Message, payload: &str) -> Result<String> {
        self.ipc.request(message, payload)
    }

    pub fn tree(&self) -> Value {
        self.ipc.tree()
    }

    pub fn command(&self, command: &str) {
        self.ipc.command(command);
    }

    pub fn version(&self) -> String {
        self.ipc.version()
    }

    /// What i3 says about one X11 window, once it manages it.
    pub fn node(&self, window: Window) -> Option<Node> {
        find_node(&self.tree(), window)
    }

    pub fn wait_until_managed(&self, window: Window) {
        wait_for(START_TIMEOUT, "i3 to manage the window", || {
            self.node(window)
        });
    }
}

/// A client of one i3-compatible IPC socket — i3's, or sway's, which speaks
/// the same protocol — framed by the daemon's own protocol code in
/// `core::wm`. It only ever connects to the path it was built with, which the
/// test generated; `I3SOCK` and `SWAYSOCK` are never read.
#[derive(Debug, Clone)]
pub struct Ipc {
    pub socket: PathBuf,
}

impl Ipc {
    pub fn request(&self, message: Message, payload: &str) -> Result<String> {
        let mut stream = UnixStream::connect(&self.socket)
            .context("connect to the window manager's IPC socket")?;
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

    pub fn tree(&self) -> Value {
        serde_json::from_str(&self.request(Message::GetTree, "").expect("the tree"))
            .expect("the tree is JSON")
    }

    pub fn command(&self, command: &str) {
        let reply = self.request(Message::RunCommand, command).expect("run it");
        spokenpad::core::wm::check_command_reply(&reply)
            .unwrap_or_else(|error| panic!("the window manager refused {command:?}: {error:#}"));
    }

    pub fn version(&self) -> String {
        let reply = self.request(Message::GetVersion, "").unwrap_or_default();
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
}

/// What an i3 or sway tree says about X11 window `window`, if it shows it.
pub fn find_node(tree: &Value, window: Window) -> Option<Node> {
    fn walk(node: &Value, window: Window, found: &mut Option<Node>) {
        if node.get("window").and_then(Value::as_u64) == Some(u64::from(window)) {
            *found = Some(Node {
                focused: node.get("focused").and_then(Value::as_bool) == Some(true),
                // i3 says `floating` on the window's node; sway says it by
                // the node's type.
                floating: matches!(
                    node.get("floating").and_then(Value::as_str),
                    Some("auto_on" | "user_on")
                ) || node.get("type").and_then(Value::as_str) == Some("floating_con"),
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
    walk(tree, window, &mut found);
    found
}

/// What the i3 tree says about a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub focused: bool,
    pub floating: bool,
}

// ------------------------------------------------------------ synthetic input

// spokenpad itself never synthesises input: `docs/constraints.md` forbids it
// in the program, because the tool this project replaced rewrote the core X
// keymap that way and corrupted keystrokes system-wide. The rule is about the
// program, not about a test. The functions below fake pointer and key events
// through the XTEST extension, and they do it only against the Xvfb server the
// test started, which `assert_is_ours` re-checks on every call. Nothing here
// ever reaches the user's session.

fn fake(server: &mut XServer, kind: u8, detail: u8, x: i16, y: i16) {
    server.assert_is_ours();
    server
        .connection
        .xtest_fake_input(kind, detail, 0, server.root, x, y, 0)
        .expect("fake an input event")
        .check()
        .expect("the X server accepted the faked event");
}

pub fn move_pointer(server: &mut XServer, x: i16, y: i16) {
    fake(server, MOTION_NOTIFY_EVENT, 0, x, y);
    server.connection.flush().expect("flush");
}

pub fn click(server: &mut XServer, x: i16, y: i16) {
    move_pointer(server, x, y);
    fake(server, BUTTON_PRESS_EVENT, 1, 0, 0);
    fake(server, BUTTON_RELEASE_EVENT, 1, 0, 0);
    server.connection.flush().expect("flush");
}

/// Where a keysym lives on the layout the server has loaded: its keycode and
/// whether Shift is needed for it.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub keycode: u8,
    pub shift: bool,
}

/// Find `keysym` on the loaded layout, at the unshifted or the shifted level.
pub fn find_key(server: &XServer, keysym: u32) -> Option<Key> {
    let setup = server.connection.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - first + 1;
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
        .enumerate()
        .find_map(|(index, symbols)| {
            // Level 0 is the plain key and level 1 is the shifted one; higher
            // levels need AltGr, which these tests do not use.
            let level = symbols.iter().take(2).position(|sym| *sym == keysym)?;
            Some(Key {
                keycode: first + u8::try_from(index).ok()?,
                shift: level == 1,
            })
        })
}

pub fn press_key(server: &mut XServer, key: Key) {
    let shift = find_key(server, 0xffe1).map(|key| key.keycode);
    if key.shift
        && let Some(shift) = shift
    {
        fake(server, KEY_PRESS_EVENT, shift, 0, 0);
    }
    fake(server, KEY_PRESS_EVENT, key.keycode, 0, 0);
    fake(server, KEY_RELEASE_EVENT, key.keycode, 0, 0);
    if key.shift
        && let Some(shift) = shift
    {
        fake(server, KEY_RELEASE_EVENT, shift, 0, 0);
    }
    server.connection.flush().expect("flush");
}

/// Type a keysym by number, so a test can name a dead key as well as a letter.
pub fn press_keysym(server: &mut XServer, keysym: u32) -> bool {
    match find_key(server, keysym) {
        Some(key) => {
            press_key(server, key);
            true
        }
        None => false,
    }
}

/// Ask a window to close, the way a window manager's close button does.
pub fn close_window(server: &XServer, window: Window) {
    let message = x11rb::protocol::xproto::ClientMessageEvent::new(
        32,
        window,
        server.atom("WM_PROTOCOLS"),
        [server.atom("WM_DELETE_WINDOW"), 0, 0, 0, 0],
    );
    server
        .connection
        .send_event(
            false,
            window,
            x11rb::protocol::xproto::EventMask::NO_EVENT,
            message,
        )
        .expect("send WM_DELETE_WINDOW");
    server.connection.flush().expect("flush");
}

/// `ISO_Level3_Shift`, which is what a German keyboard labels AltGr.
pub const ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// Press a key with AltGr held, naming the key by what it types *without* it.
///
/// AltGr is a level shift the layout applies, not a modifier Neovim names, so
/// this is how the test proves the pane does not report it as Alt: `AltGr`+`q`
/// on a German layout must arrive as the text `@`, not as `<M-q>`.
pub fn press_with_altgr(server: &mut XServer, unshifted: u32) -> bool {
    let (Some(level3), Some(key)) = (
        find_key(server, ISO_LEVEL3_SHIFT),
        find_key(server, unshifted),
    ) else {
        return false;
    };
    fake(server, KEY_PRESS_EVENT, level3.keycode, 0, 0);
    fake(server, KEY_PRESS_EVENT, key.keycode, 0, 0);
    fake(server, KEY_RELEASE_EVENT, key.keycode, 0, 0);
    fake(server, KEY_RELEASE_EVENT, level3.keycode, 0, 0);
    server.connection.flush().expect("flush");
    true
}

/// Type a run of text on the loaded layout. Returns false if any character is
/// not on it.
pub fn type_text(server: &mut XServer, text: &str) -> bool {
    text.chars().all(|character| {
        let keysym = match character {
            // Latin-1 keysyms are their own code points; everything else needs
            // the Unicode form.
            character if (character as u32) < 0x100 => character as u32,
            character => 0x0100_0000 | character as u32,
        };
        press_keysym(server, keysym)
    })
}

// ------------------------------------------------------------------ utilities

pub fn wait_for<T>(timeout: Duration, what: &str, mut attempt: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = attempt() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(50));
    }
}

/// The directory the pane tests drop screenshots in, for a human to judge.
pub fn screenshot_dir() -> PathBuf {
    let path = std::env::var_os("SPOKENPAD_PANE_SCREENSHOTS")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// A framebuffer as an 8-bit RGB PNG.
pub fn write_png(path: &Path, pixels: &[u32], width: u16, height: u16) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        u32::from(width),
        u32::from(height),
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let mut bytes = Vec::with_capacity(pixels.len() * 3);
    for pixel in pixels {
        bytes.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8]);
    }
    writer.write_image_data(&bytes)?;
    Ok(())
}
