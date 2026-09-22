//! Window managers other than i3, each in a headless session of its own, so a
//! test can ask the same question of all of them: does the pane stay
//! unfocused?
//!
//! - **sway** runs on wlroots' headless backend with Xwayland, and is asked
//!   over its own IPC socket;
//! - **Openbox** runs on an Xvfb above `:50`, like i3;
//! - **KWin** runs as `kwin_wayland --virtual` with Xwayland, on a D-Bus
//!   session bus of its own;
//! - **KWin on X11** runs as `kwin_x11` (the separate `kwin-x11` package
//!   since KWin 6) on an Xvfb above `:50`, on a D-Bus session bus of its own.
//!
//! Every one of them runs in a [`Private`] environment: an empty
//! environment, a temporary `HOME`, `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`,
//! and a configuration file the test generated. None of them can read the
//! user's window manager configuration, find the user's Wayland or X display,
//! or reach the user's session bus.
//!
//! One thing is not private: an Xwayland takes the lowest free X display
//! number, and neither sway nor KWin can be told which one. That is a socket
//! in `/tmp/.X11-unix` for the life of the test, never `:0` (which is taken),
//! and the harness only ever talks to the number its own compositor reported.
use super::{Ipc, Killed, Node, START_TIMEOUT, XServer, click as fake_click, find_node, wait_for};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{JoinHandle, sleep},
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, Window},
    rust_connection::RustConnection,
};

/// How long one map of a test's own window is given to be handled before
/// [`Desktop::map_until_managed`] sends it again.
const MAP_PATIENCE: Duration = Duration::from_secs(2);
/// How long [`Desktop::map_until_managed`] tries in all: a loaded machine
/// running several window managers at once is slow, not broken.
const MANAGE_TIMEOUT: Duration = Duration::from_secs(40);

/// The size every desktop here gives its one screen.
pub const SCREEN: (u16, u16) = (1280, 800);

// --------------------------------------------------------- the environment

/// A private home for one window manager: an empty environment apart from
/// `PATH`, and every directory it could read a configuration from or leave a
/// socket in pointed into one temporary directory.
///
/// The directory lives under the system temporary directory rather than the
/// scratch space of the test, because a Unix socket path must fit in 108
/// bytes and every compositor puts its sockets in `XDG_RUNTIME_DIR`.
pub struct Private {
    directory: tempfile::TempDir,
}

impl Private {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        for name in ["home", "config", "data", "cache", "run"] {
            std::fs::create_dir(directory.path().join(name)).expect("make a private directory");
        }
        // XDG_RUNTIME_DIR must be private to its owner, or compositors refuse
        // it.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            directory.path().join("run"),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("make the runtime directory private");
        Self { directory }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    pub fn runtime(&self) -> PathBuf {
        self.path("run")
    }

    /// `program`, to be run in this environment and nowhere else.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_RUNTIME_DIR", self.runtime())
            .current_dir(self.directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Wait for a program started here to write a file here, and return
    /// what it wrote.
    fn read_when_written(&self, name: &str, what: &str) -> String {
        let path = self.path(name);
        wait_for(START_TIMEOUT, what, || {
            std::fs::read_to_string(&path)
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        })
    }
}

impl Default for Private {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------ what a test can ask

/// What a pane focus test needs from a desktop, whichever it is.
pub trait Desktop {
    /// The window manager's name and version, and the X display it serves.
    fn describe(&self) -> String;
    fn server(&self) -> &XServer;
    fn server_mut(&mut self) -> &mut XServer;
    /// The X11 window the window manager itself says has the focus.
    fn focused(&self) -> Option<Window>;
    /// Whether the window manager has taken `window` on.
    fn manages(&self, window: Window) -> bool;
    /// Whether the window manager floats `window`, when it can say.
    fn floating(&self, window: Window) -> Option<bool>;
    /// The user picks `window` — a left click at `at`, a point on it, where
    /// this desktop can deliver a click the way a user's would reach the
    /// window manager.
    fn select(&mut self, window: Window, at: (i16, i16));
    /// Type one `k` at whatever has the X input focus, where this desktop
    /// can deliver a key without disturbing the window manager. Returns
    /// whether it could.
    fn type_key(&mut self) -> bool;
    /// Move to a workspace or desktop with no window on it.
    fn go_to_empty_workspace(&mut self);
    /// How the focus sampler asks the window manager, from its own thread.
    fn view(&self) -> View;
    /// Whether the window manager shows `window` above `other`, as it draws
    /// them. On an X11 window manager that is the X server's own stacking
    /// order; a Wayland compositor decides it itself.
    fn above(&self, window: Window, other: Window) -> Option<bool> {
        x_stacking_above(self.server(), window, other)
    }
    /// sway's IPC socket, which a daemon under sway reads from `$SWAYSOCK`;
    /// `None` on every other desktop.
    fn sway_socket(&self) -> Option<PathBuf> {
        None
    }
    /// The `XDG_RUNTIME_DIR` the window manager runs with, where sway puts
    /// its socket; `None` on every desktop but sway.
    fn runtime_dir(&self) -> Option<PathBuf> {
        None
    }

    /// Map `window`, a window of the test's own, until the window manager
    /// takes it on. A map a window manager dropped — Openbox drops one that
    /// arrives while it is still starting — is sent again, but only after
    /// the last one had [`MAP_PATIENCE`] to be handled: sending it again
    /// sooner would undo a map that was only slow, and under load could
    /// never let one through.
    fn map_until_managed(&self, window: Window) {
        let connection = &self.server().connection;
        wait_for(
            MANAGE_TIMEOUT,
            "the window manager to manage a window of the test's",
            || {
                connection.map_window(window).ok()?;
                connection.flush().ok()?;
                let sent = std::time::Instant::now();
                while sent.elapsed() < MAP_PATIENCE {
                    if self.manages(window) {
                        return Some(());
                    }
                    sleep(Duration::from_millis(50));
                }
                connection.unmap_window(window).ok()?;
                connection.flush().ok()?;
                None
            },
        );
    }

    fn wait_until_managed(&self, window: Window) {
        wait_for(
            START_TIMEOUT,
            "the window manager to manage the window",
            || self.manages(window).then_some(()),
        );
    }
}

/// How to ask a window manager what it focused, without borrowing the
/// desktop: over EWMH on a connection of the sampler's own, or over an IPC
/// socket.
#[derive(Debug, Clone)]
pub enum View {
    Ewmh,
    Tree(Ipc),
}

// ------------------------------------------------------------------- sway

/// sway on the headless backend, with Xwayland, a generated config and no
/// input devices. Its focus is read from its tree over its own IPC socket.
pub struct Sway {
    /// Owns sway itself: dropping the server stops the compositor, which
    /// stops its Xwayland.
    pub server: XServer,
    ipc: Ipc,
    version: String,
    private: Private,
}

impl Sway {
    pub fn start() -> Self {
        let private = Private::new();
        let config = private.path("sway.config");
        let display_file = private.path("display");
        std::fs::write(
            &config,
            format!(
                "# generated by the spokenpad pane tests; never the user's config\n\
                 xwayland enable\n\
                 focus_follows_mouse no\n\
                 output HEADLESS-1 resolution {}x{}\n\
                 exec sh -c 'printf %s \"$DISPLAY\" > {}'\n",
                SCREEN.0,
                SCREEN.1,
                display_file.display()
            ),
        )
        .expect("write the sway config");
        let compositor = Killed(
            private
                .command("sway")
                .args(["-c".as_ref(), config.as_os_str()])
                .env("WLR_BACKENDS", "headless")
                .env("WLR_LIBINPUT_NO_DEVICES", "1")
                .env("WLR_RENDERER", "pixman")
                .spawn()
                .expect("start sway"),
        );
        let socket = wait_for(START_TIMEOUT, "sway to open its IPC socket", || {
            std::fs::read_dir(private.runtime())
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("sway-ipc."))
                })
        });
        let ipc = Ipc { socket };
        wait_for(START_TIMEOUT, "sway to answer on its IPC socket", || {
            ipc.request(spokenpad::core::wm::Message::GetVersion, "")
                .ok()
        });
        let display = private.read_when_written("display", "sway to name its Xwayland display");
        let server = XServer::xwayland(display, compositor);
        let version = ipc.version();
        Self {
            server,
            ipc,
            version,
            private,
        }
    }

    pub fn command(&self, command: &str) {
        self.ipc.command(command);
    }

    pub fn node(&self, window: Window) -> Option<Node> {
        find_node(&self.ipc.tree(), window)
    }
}

impl Desktop for Sway {
    fn describe(&self) -> String {
        format!(
            "sway {} (headless, Xwayland {})",
            self.version, self.server.display
        )
    }

    fn server(&self) -> &XServer {
        &self.server
    }

    fn server_mut(&mut self) -> &mut XServer {
        &mut self.server
    }

    fn focused(&self) -> Option<Window> {
        tree_focus(&self.ipc.tree())
    }

    fn manages(&self, window: Window) -> bool {
        self.node(window).is_some()
    }

    fn floating(&self, window: Window) -> Option<bool> {
        self.node(window).map(|node| node.floating)
    }

    /// A click through sway's own seat, since a headless sway has no input
    /// device and Xwayland's XTEST would stay inside the X server.
    fn select(&mut self, _window: Window, (x, y): (i16, i16)) {
        self.command(&format!("seat - cursor set {x} {y}"));
        self.command("seat - cursor press button1");
        self.command("seat - cursor release button1");
    }

    /// XTEST inside Xwayland: the key goes to the X input focus without
    /// passing through sway, which is enough to see where the X focus is.
    fn type_key(&mut self) -> bool {
        fake_key(&mut self.server)
    }

    fn go_to_empty_workspace(&mut self) {
        self.command("workspace pane-empty");
    }

    fn view(&self) -> View {
        View::Tree(self.ipc.clone())
    }

    /// sway draws every floating window of a workspace above its tiled ones,
    /// whatever order its Xwayland keeps. Two floating windows are not
    /// compared here.
    fn above(&self, window: Window, other: Window) -> Option<bool> {
        let (window, other) = (self.node(window)?, self.node(other)?);
        (window.floating != other.floating).then_some(window.floating)
    }

    fn sway_socket(&self) -> Option<PathBuf> {
        Some(self.ipc.socket.clone())
    }

    fn runtime_dir(&self) -> Option<PathBuf> {
        Some(self.private.runtime())
    }
}

/// The X11 window of the focused node in an i3 or sway tree, if the focused
/// node shows one.
pub fn tree_focus(tree: &Value) -> Option<Window> {
    fn walk(node: &Value) -> Option<Option<Window>> {
        if node.get("focused").and_then(Value::as_bool) == Some(true) {
            return Some(
                node.get("window")
                    .and_then(Value::as_u64)
                    .and_then(|id| Window::try_from(id).ok()),
            );
        }
        ["nodes", "floating_nodes"]
            .iter()
            .flat_map(|key| {
                node.get(*key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .find_map(walk)
    }
    walk(tree).flatten()
}

// ---------------------------------------------------------------- Openbox

/// Openbox on an Xvfb of the test's own, with a generated `rc.xml`:
/// `focusNew` on (Openbox's default, and the setting under which it focuses
/// new windows), no focus-follows-mouse, and two desktops so one can be
/// empty.
pub struct Openbox {
    // Declared first, so Openbox stops before its X server does.
    _openbox: Killed,
    pub server: XServer,
    _private: Private,
}

impl Openbox {
    pub fn start() -> Self {
        let server = XServer::start();
        let private = Private::new();
        let config = private.path("rc.xml");
        std::fs::write(
            &config,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- generated by the spokenpad pane tests; never the user's config -->
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <focus>
    <focusNew>yes</focusNew>
    <followMouse>no</followMouse>
  </focus>
  <placement><policy>Smart</policy></placement>
  <desktops><number>2</number></desktops>
</openbox_config>
"#,
        )
        .expect("write the Openbox config");
        let openbox = Killed(
            private
                .command("openbox")
                .args([
                    "--sm-disable".as_ref(),
                    "--config-file".as_ref(),
                    config.as_os_str(),
                ])
                .env("DISPLAY", &server.display)
                .spawn()
                .expect("start openbox"),
        );
        wait_for_ewmh_wm(&server, "Openbox");
        let openbox = Self {
            _openbox: openbox,
            server,
            _private: private,
        };
        // Openbox publishes its check window before it handles map requests,
        // and a window mapped in between stays unmapped and unmanaged: seen
        // in about one parallel run in three. A throwaway window, mapped
        // until Openbox takes it, proves it is past that.
        let probe = openbox.server.unmapped_window();
        openbox.map_until_managed(probe);
        openbox
            .server
            .connection
            .destroy_window(probe)
            .expect("destroy the probe");
        openbox.server.sync();
        openbox
    }
}

impl Desktop for Openbox {
    fn describe(&self) -> String {
        format!(
            "{} (Xvfb {})",
            program_version(&["openbox", "--version"]),
            self.server.display
        )
    }

    fn server(&self) -> &XServer {
        &self.server
    }

    fn server_mut(&mut self) -> &mut XServer {
        &mut self.server
    }

    fn focused(&self) -> Option<Window> {
        active_window(&self.server.connection, self.server.root)
    }

    fn manages(&self, window: Window) -> bool {
        client_list(&self.server, "_NET_CLIENT_LIST").contains(&window)
    }

    fn floating(&self, _window: Window) -> Option<bool> {
        // Every window floats on a stacking window manager.
        Some(true)
    }

    /// A click through XTEST on the test's own Xvfb.
    fn select(&mut self, _window: Window, (x, y): (i16, i16)) {
        fake_click(&mut self.server, x, y);
    }

    fn type_key(&mut self) -> bool {
        fake_key(&mut self.server)
    }

    fn go_to_empty_workspace(&mut self) {
        switch_desktop(&self.server, 1);
    }

    fn view(&self) -> View {
        View::Ewmh
    }
}

// ------------------------------------------------------------------- KWin

/// KWin's focus stealing prevention level, `[Windows]
/// FocusStealingPreventionLevel` in `kwinrc`: the two a test runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusStealingPrevention {
    /// `1`, KWin's default.
    Low,
    /// `0`, the most permissive level a user can choose.
    None,
}

/// `kwin_wayland --virtual` with Xwayland, on a session bus of its own, with
/// a generated `kwinrc`. Its focus is read over EWMH on its Xwayland, where
/// KWin's X11 window manager publishes it for X11 windows.
pub struct KwinWayland {
    /// Owns KWin itself; see [`XServer::xwayland`].
    pub server: XServer,
    // After the server, so the bus outlives KWin.
    _bus: Killed,
    bus_address: String,
    level: FocusStealingPrevention,
    /// How many scripts [`Desktop::select`] has loaded, so each gets a name
    /// of its own.
    scripts: u32,
    private: Private,
}

impl KwinWayland {
    /// Start KWin with its focus stealing prevention at the default (`Low`)
    /// or at `None`, the most permissive level a user can pick.
    pub fn start(level: FocusStealingPrevention) -> Self {
        let private = Private::new();
        write_kwinrc(&private, level);
        let (bus, bus_address) = private_bus(&private);
        let display_file = private.path("display");
        let compositor = Killed(
            private
                .command("kwin_wayland")
                .env("DBUS_SESSION_BUS_ADDRESS", &bus_address)
                .args(["--virtual", "--xwayland", "--socket", "spokenpad-test"])
                .args(["--width", &SCREEN.0.to_string()])
                .args(["--height", &SCREEN.1.to_string()])
                .args([
                    "--no-lockscreen",
                    "--no-global-shortcuts",
                    "--no-kactivities",
                ])
                // KWin runs this once its Xwayland is up, with DISPLAY set to
                // it: the only reliable way to learn the number it took.
                .arg(format!(
                    "sh -c 'printf %s \"$DISPLAY\" > {}'",
                    display_file.display()
                ))
                .spawn()
                .expect("start kwin_wayland"),
        );
        let display = private.read_when_written("display", "KWin to name its Xwayland display");
        let server = XServer::xwayland(display, compositor);
        wait_for_ewmh_wm(&server, "KWin");
        Self {
            server,
            _bus: bus,
            bus_address,
            level,
            scripts: 0,
            private,
        }
    }

    /// One D-Bus method call to KWin, on its private bus.
    fn kwin_call(&self, arguments: &[&str]) {
        let output = self
            .private
            .command("dbus-send")
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus_address)
            .args(["--session", "--dest=org.kde.KWin", "--print-reply"])
            .args(arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run dbus-send");
        assert!(
            output.status.success(),
            "KWin refused {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// A generated `kwinrc`: click to focus, the focus stealing prevention level
/// under test, and two desktops so one can be empty.
fn write_kwinrc(private: &Private, level: FocusStealingPrevention) {
    std::fs::write(
        private.path("config").join("kwinrc"),
        format!(
            "# generated by the spokenpad pane tests; never the user's config\n\
             [Windows]\n\
             FocusStealingPreventionLevel={}\n\
             FocusPolicy=ClickToFocus\n\
             \n\
             [Desktops]\n\
             Number=2\n\
             Rows=1\n",
            match level {
                FocusStealingPrevention::Low => 1,
                FocusStealingPrevention::None => 0,
            }
        ),
    )
    .expect("write the kwinrc");
}

/// A D-Bus session bus of the test's own, so KWin never reaches the user's.
/// Returns the daemon and its address.
fn private_bus(private: &Private) -> (Killed, String) {
    let socket = private.runtime().join("bus");
    let bus = Killed(
        private
            .command("dbus-daemon")
            .arg("--session")
            .arg("--nofork")
            .arg(format!("--address=unix:path={}", socket.display()))
            .spawn()
            .expect("start a private session bus"),
    );
    wait_for(START_TIMEOUT, "the private session bus", || {
        socket.exists().then_some(())
    });
    (bus, format!("unix:path={}", socket.display()))
}

/// `kwin_x11` on an Xvfb of the test's own, on a session bus of its own,
/// with a generated `kwinrc`. An X11 window manager takes XTEST clicks and
/// keys like any other, so, unlike [`KwinWayland`], the user's click here is
/// a real click.
pub struct KwinX11 {
    // Declared first, so KWin stops before its X server and its bus.
    _kwin: Killed,
    pub server: XServer,
    _bus: Killed,
    level: FocusStealingPrevention,
    _private: Private,
}

impl KwinX11 {
    pub fn start(level: FocusStealingPrevention) -> Self {
        let server = XServer::start();
        let private = Private::new();
        write_kwinrc(&private, level);
        let (bus, bus_address) = private_bus(&private);
        let kwin = Killed(
            private
                .command("kwin_x11")
                .env("DISPLAY", &server.display)
                .env("QT_QPA_PLATFORM", "xcb")
                .env("DBUS_SESSION_BUS_ADDRESS", &bus_address)
                .arg("--no-kactivities")
                .spawn()
                .expect("start kwin_x11"),
        );
        wait_for_ewmh_wm(&server, "KWin");
        Self {
            _kwin: kwin,
            server,
            _bus: bus,
            level,
            _private: private,
        }
    }
}

impl Desktop for KwinX11 {
    fn describe(&self) -> String {
        format!(
            "{} (Xvfb {}, focus stealing prevention {})",
            program_version(&["kwin_x11", "--version"]),
            self.server.display,
            match self.level {
                FocusStealingPrevention::Low => "Low (the default)",
                FocusStealingPrevention::None => "None",
            }
        )
    }

    fn server(&self) -> &XServer {
        &self.server
    }

    fn server_mut(&mut self) -> &mut XServer {
        &mut self.server
    }

    fn focused(&self) -> Option<Window> {
        active_window(&self.server.connection, self.server.root)
    }

    fn manages(&self, window: Window) -> bool {
        client_list(&self.server, "_NET_CLIENT_LIST").contains(&window)
    }

    fn floating(&self, _window: Window) -> Option<bool> {
        Some(true)
    }

    /// A click through XTEST on the test's own Xvfb.
    fn select(&mut self, _window: Window, (x, y): (i16, i16)) {
        fake_click(&mut self.server, x, y);
    }

    fn type_key(&mut self) -> bool {
        fake_key(&mut self.server)
    }

    fn go_to_empty_workspace(&mut self) {
        switch_desktop(&self.server, 1);
    }

    fn view(&self) -> View {
        View::Ewmh
    }
}

/// The instance half of a window's `WM_CLASS`.
fn instance_of(server: &XServer, window: Window) -> Option<String> {
    let class = server
        .connection
        .get_property(false, window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    let instance = class.value.split(|byte| *byte == 0).next()?;
    String::from_utf8(instance.to_vec()).ok()
}

impl Desktop for KwinWayland {
    fn describe(&self) -> String {
        format!(
            "{} (virtual, Xwayland {}, focus stealing prevention {})",
            program_version(&["kwin_wayland", "--version"]),
            self.server.display,
            match self.level {
                FocusStealingPrevention::Low => "Low (the default)",
                FocusStealingPrevention::None => "None",
            }
        )
    }

    fn server(&self) -> &XServer {
        &self.server
    }

    fn server_mut(&mut self) -> &mut XServer {
        &mut self.server
    }

    fn focused(&self) -> Option<Window> {
        active_window(&self.server.connection, self.server.root)
    }

    fn manages(&self, window: Window) -> bool {
        client_list(&self.server, "_NET_CLIENT_LIST").contains(&window)
    }

    fn floating(&self, _window: Window) -> Option<bool> {
        Some(true)
    }

    /// Not a click: KWin receives XTEST from its Xwayland over libei only
    /// after a user approves it in a prompt, so a click cannot reach it
    /// headless. A KWin script activates the window instead, which is what
    /// KWin does itself on a click with its default click-to-focus policy.
    fn select(&mut self, window: Window, _at: (i16, i16)) {
        let instance = instance_of(&self.server, window).expect("the window names its class");
        self.scripts += 1;
        let script = self.private.path(&format!("select-{}.js", self.scripts));
        std::fs::write(
            &script,
            format!(
                "for (const window of workspace.windowList()) {{\n\
                 \x20   if (window.resourceName === {instance:?}) {{ workspace.activeWindow = window; }}\n\
                 }}\n"
            ),
        )
        .expect("write the KWin script");
        let name = format!("spokenpad-select-{}", self.scripts);
        self.kwin_call(&[
            "/Scripting",
            "org.kde.kwin.Scripting.loadScript",
            &format!("string:{}", script.display()),
            &format!("string:{name}"),
        ]);
        self.kwin_call(&["/Scripting", "org.kde.kwin.Scripting.start"]);
    }

    /// Nothing reaches KWin headless: XTEST on its Xwayland goes to KWin
    /// over libei, which KWin refuses without a user's approval — and the
    /// attempt deactivates the focused window.
    fn type_key(&mut self) -> bool {
        false
    }

    fn go_to_empty_workspace(&mut self) {
        switch_desktop(&self.server, 1);
    }

    fn view(&self) -> View {
        View::Ewmh
    }

    /// KWin composites the windows itself, in its own stacking order, and
    /// publishes that order for X11 windows as `_NET_CLIENT_LIST_STACKING`.
    fn above(&self, window: Window, other: Window) -> Option<bool> {
        ewmh_stacking_above(&self.server, window, other)
    }
}

// -------------------------------------------------------------- EWMH, read

/// Whether `_NET_CLIENT_LIST_STACKING` (bottom to top) lists `window` above
/// `other`, when it lists both.
pub fn ewmh_stacking_above(server: &XServer, window: Window, other: Window) -> Option<bool> {
    let stacking = client_list(server, "_NET_CLIENT_LIST_STACKING");
    let position = |target: Window| stacking.iter().position(|listed| *listed == target);
    Some(position(window)? > position(other)?)
}

/// Whether the X server stacks `window` above `other`: the order of their
/// top-level ancestors — the frames a reparenting window manager put them
/// in — among the root's children, bottom to top.
pub fn x_stacking_above(server: &XServer, window: Window, other: Window) -> Option<bool> {
    let top = |window: Window| with_frames(server, window).last().copied();
    let (window, other) = (top(window)?, top(other)?);
    let children = server
        .connection
        .query_tree(server.root)
        .ok()?
        .reply()
        .ok()?
        .children;
    let position = |target: Window| children.iter().position(|child| *child == target);
    Some(position(window)? > position(other)?)
}

fn wait_for_ewmh_wm(server: &XServer, name: &str) {
    let atom = server.atom("_NET_SUPPORTING_WM_CHECK");
    wait_for(
        START_TIMEOUT,
        &format!("{name} to manage the screen"),
        || {
            server
                .connection
                .get_property(false, server.root, atom, AtomEnum::WINDOW, 0, 1)
                .ok()?
                .reply()
                .ok()?
                .value32()?
                .next()
                .filter(|window| *window != 0)
        },
    );
}

fn atom_on(connection: &RustConnection, name: &str) -> Option<u32> {
    Some(
        connection
            .intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()?
            .atom,
    )
}

/// `_NET_ACTIVE_WINDOW` on the root: the window the window manager says is
/// active, or `None` when it says none is.
pub fn active_window(connection: &RustConnection, root: Window) -> Option<Window> {
    read_active_window(connection, root).ok().flatten()
}

/// The same, telling a window manager that names no window apart from a
/// connection that could not ask.
fn read_active_window(connection: &RustConnection, root: Window) -> anyhow::Result<Option<Window>> {
    let atom = connection
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let reply = connection
        .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)?
        .reply()?;
    Ok(reply
        .value32()
        .and_then(|mut values| values.next())
        .filter(|window| *window != 0))
}

/// `_NET_CLIENT_LIST` or `_NET_CLIENT_LIST_STACKING` (bottom to top).
pub fn client_list(server: &XServer, property: &str) -> Vec<Window> {
    let atom = server.atom(property);
    server
        .connection
        .get_property(false, server.root, atom, AtomEnum::WINDOW, 0, 4096)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(Iterator::collect))
        .unwrap_or_default()
}

fn switch_desktop(server: &XServer, desktop: u32) {
    let message = ClientMessageEvent::new(
        32,
        server.root,
        server.atom("_NET_CURRENT_DESKTOP"),
        [desktop, 0, 0, 0, 0],
    );
    server
        .connection
        .send_event(
            false,
            server.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            message,
        )
        .expect("ask for another desktop");
    server.connection.flush().expect("flush");
}

fn program_version(argv: &[&str]) -> String {
    Command::new(argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| argv[0].to_owned())
}

/// One `k` through XTEST, on a server the test owns.
fn fake_key(server: &mut XServer) -> bool {
    let key = super::find_key(server, u32::from(b'k')).expect("the layout has a `k`");
    super::press_key(server, key);
    true
}

/// Stop the Xwayland a compositor started, before the compositor itself.
/// Xwayland runs with `-terminate`, and left to itself outlives a stopped
/// compositor by several seconds; a test that has finished must leave no
/// process behind.
fn stop_xwayland(compositor: u32) {
    let children = std::fs::read_dir(format!("/proc/{compositor}/task"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|pid| pid.parse::<i32>().ok())
                .collect::<Vec<_>>()
        })
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|name| name.trim() == "Xwayland")
        })
        .collect::<Vec<_>>();
    for pid in &children {
        // SAFETY: `pid` was read from the compositor's own list of children
        // a moment ago, and the compositor, which reaps them, is still
        // running, so the pid cannot have been reused.
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while children
        .iter()
        .any(|pid| PathBuf::from(format!("/proc/{pid}")).exists())
        && std::time::Instant::now() < deadline
    {
        sleep(Duration::from_millis(20));
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        stop_xwayland(self.server.owner_pid());
    }
}

impl Drop for KwinWayland {
    fn drop(&mut self) {
        stop_xwayland(self.server.owner_pid());
    }
}

// ------------------------------------------------------------ the sampler

/// Samples the focus every few milliseconds on a thread of its own, for as
/// long as it runs: the X input focus on a connection of its own, and what
/// the window manager says. A focus change that lasted longer than one
/// sampling period cannot hide between two assertions.
pub struct FocusSampler {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Samples>>,
}

/// Every focus value seen, with how often.
#[derive(Debug, Default, Clone)]
pub struct Samples {
    pub count: usize,
    /// `GetInputFocus` answers: a window, or 0 (`None`) / 1 (`PointerRoot`).
    pub input: BTreeMap<Window, usize>,
    /// What the window manager called focused.
    pub manager: BTreeMap<Option<Window>, usize>,
    /// Samples that could not be taken: a failed request to the X server or
    /// the window manager. Any is a measurement that did not happen.
    pub errors: usize,
}

impl Samples {
    /// Fails unless the sampler actually measured: at least one sample, and
    /// no request that failed. A dead connection must not read as "never
    /// focused".
    pub fn assert_measured(&self, stage: &str) {
        assert!(
            self.count > 0 && self.errors == 0,
            "{stage}: the focus sampler took {} samples and failed {} times, so it measured \
             nothing",
            self.count,
            self.errors
        );
    }

    /// Whether any sample put the focus on one of `windows`.
    pub fn touched(&self, windows: &[Window]) -> bool {
        windows.iter().any(|window| {
            self.input.contains_key(window) || self.manager.contains_key(&Some(*window))
        })
    }
}

const SAMPLE_PERIOD: Duration = Duration::from_millis(5);

impl FocusSampler {
    pub fn start(display: &str, view: View) -> Self {
        let (connection, screen) = x11rb::connect(Some(display)).expect("connect the sampler");
        let root = connection.setup().roots[screen].root;
        let stop = Arc::new(AtomicBool::new(false));
        let theirs = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut samples = Samples::default();
            while !theirs.load(Ordering::Acquire) {
                let input = connection
                    .get_input_focus()
                    .map_err(anyhow::Error::from)
                    .and_then(|cookie| Ok(cookie.reply()?.focus));
                let manager = match &view {
                    View::Ewmh => read_active_window(&connection, root),
                    View::Tree(ipc) => ipc
                        .request(spokenpad::core::wm::Message::GetTree, "")
                        .and_then(|text| Ok(serde_json::from_str::<Value>(&text)?))
                        .map(|tree| tree_focus(&tree)),
                };
                match (input, manager) {
                    (Ok(input), Ok(manager)) => {
                        *samples.input.entry(input).or_default() += 1;
                        *samples.manager.entry(manager).or_default() += 1;
                        samples.count += 1;
                    }
                    _ => samples.errors += 1,
                }
                sleep(SAMPLE_PERIOD);
            }
            samples
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    pub fn finish(mut self) -> Samples {
        self.stop.store(true, Ordering::Release);
        self.thread
            .take()
            .expect("the sampler thread")
            .join()
            .expect("the sampler thread panicked")
    }
}

impl Drop for FocusSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ------------------------------------------------------------ X11 helpers

/// The first window below `root` whose `WM_CLASS` instance is `instance`,
/// wherever a reparenting window manager put it.
pub fn find_by_instance(server: &XServer, instance: &str) -> Option<Window> {
    /// KWin on X11 puts a client two levels down, in a wrapper inside its
    /// frame; no window manager here goes deeper.
    const DEPTH: usize = 3;
    fn search(server: &XServer, window: Window, instance: &str, depth: usize) -> Option<Window> {
        if instance_of(server, window).as_deref() == Some(instance) {
            return Some(window);
        }
        if depth == 0 {
            return None;
        }
        server
            .connection
            .query_tree(window)
            .ok()?
            .reply()
            .ok()?
            .children
            .into_iter()
            .find_map(|child| search(server, child, instance, depth - 1))
    }
    search(server, server.root, instance, DEPTH)
}

/// `window` and every ancestor of it below the root: the client window and
/// whatever frames a window manager wrapped around it. A focus on any of them
/// is a focus on the window.
pub fn with_frames(server: &XServer, window: Window) -> Vec<Window> {
    let mut chain = vec![window];
    let mut current = window;
    while let Some(parent) = server
        .connection
        .query_tree(current)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|tree| tree.parent)
        .filter(|parent| *parent != server.root && *parent != 0)
    {
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Where `window` is on the screen, in root coordinates.
pub fn screen_rect(server: &XServer, window: Window) -> (i32, i32, u32, u32) {
    let geometry = server
        .connection
        .get_geometry(window)
        .expect("ask for the geometry")
        .reply()
        .expect("the geometry");
    let origin = server
        .connection
        .translate_coordinates(window, server.root, 0, 0)
        .expect("translate to the root")
        .reply()
        .expect("the translation");
    (
        origin.dst_x.into(),
        origin.dst_y.into(),
        geometry.width.into(),
        geometry.height.into(),
    )
}
