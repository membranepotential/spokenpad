//! Composes the screenshot at the top of README.md: the dictation pane
//! floating above an ordinary editor window, mid-dictation, with committed
//! text, a live preview after it and the winbar showing a latched recording.
//!
//! Starts its own Xvfb (above `:70`) and i3 -- never the user's display or
//! window manager -- and opens a background `xterm` running a read-only
//! `nvim` on a short snippet, so there is something plausible for the pane to
//! float over. It then opens the pane exactly as `pane_launch` and `Pane`
//! open it for real, and drives the embedded editor over the pane's own RPC
//! channel with the same three Lua entry points the daemon calls
//! (`src/lua/spokenpad.lua`'s `Spokenpad.setup`, `.append_once` and `.push`):
//! not `NvimSession`, whose reattachment path falls back to the user's real
//! `~/.local/state/spokenpad/dictation/` the moment it cannot prove this
//! freshly-opened editor is its own -- exactly the file this script must
//! never go near. All of the text is written here, not read from the user's
//! dictation files.
//!
//! The picture is the composited screen, read with `GetImage` on the root
//! window, not the pane's own framebuffer: the point is to show the pane
//! floating over something, and the pane only knows its own pixels.
//!
//! To regenerate `docs/screenshot.png`:
//! ```sh
//! cargo run --release --example screenshot
//! optipng -quiet -o4 docs/screenshot.png
//! ```
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use rmpv::Value;
use spokenpad::{
    config::{FontFamily, Nvim, PaneLayout},
    core::{
        font::Points,
        geometry::{Dimensions, Rect},
        wm::{HEADER_LEN, Message, encode, reply_length},
    },
    shell::{
        nvim::{PreviewPlacement, STARTUP_TIMEOUT, pane_launch},
        pane::{Options, Pane, place::Target},
    },
};
use std::{
    io::{BufRead, BufReader, Read as _, Write as _},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, ImageFormat},
};

/// The nvim half of spokenpad, loaded into the embedded editor over RPC
/// exactly as `shell::nvim::NvimSession` loads it (that constant is private
/// to that module, so this is its own copy of the same `include_str!`).
const SPOKENPAD_LUA: &str = include_str!("../src/lua/spokenpad.lua");

#[derive(Parser)]
#[command(about = "Compose the README screenshot on a private headless X server")]
struct Args {
    /// Where to write the PNG.
    #[arg(long, default_value = "docs/screenshot.png")]
    out: PathBuf,
    /// The whole scene's width, which is also the image's.
    #[arg(long, default_value_t = 1200)]
    width: u16,
    /// The whole scene's height.
    #[arg(long, default_value_t = 640)]
    height: u16,
    /// The pane's width, in cells.
    #[arg(long, default_value_t = 58)]
    columns: u16,
    /// The pane's height, in cells.
    #[arg(long, default_value_t = 9)]
    rows: u16,
    /// The pane's font size in points, as `nvim.font_size`.
    #[arg(long, default_value_t = 14.0)]
    font_size: f32,
    /// Where the pane opens, as if this were the mouse pointer.
    #[arg(long, default_value_t = 460)]
    pane_x: i32,
    #[arg(long, default_value_t = 170)]
    pane_y: i32,
}

/// A synthetic snippet for the background editor. Thematically the code the
/// synthetic dictation below is about, so the screenshot tells one story.
const BACKGROUND_SNIPPET: &str = "\
/// Parses the next token, or `None` at the end of the input.
fn next_token(input: &[u8], pos: &mut usize) -> Option<Token> {
    if *pos >= input.len() {
        return None;
    }
    let start = *pos;
    while *pos < input.len() && !is_boundary(input[*pos]) {
        *pos += 1;
    }
    Some(Token::new(&input[start..*pos]))
}

fn is_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || byte == b';'
}
";

const COMMITTED_FIRST: &str = "Fixed the crash in the config parser: it now checks the token isn't empty before indexing into it.";
const COMMITTED_SECOND: &str =
    "Added a regression test in tests/config.rs and updated the changelog.";
const PREVIEW: &str =
    "Next I want to check whether the same bounds issue shows up in the export path";

fn main() -> Result<()> {
    let args = Args::parse();
    let home = tempfile::tempdir().context("make an isolated HOME")?;
    isolate(home.path());

    // Every `Killed` below reaps its whole process tree on drop, but a raw
    // SIGTERM (what `timeout` and a plain `kill` send) or SIGINT (Ctrl-C)
    // ends a Rust process without running any destructor at all -- nothing
    // here would be reaped, only orphaned. This turns both into an ordinary
    // flag the waits below check, so the signal instead becomes a `main`
    // that returns, which runs every `Drop` exactly as an error return does.
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))
        .context("install a SIGINT handler")?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))
        .context("install a SIGTERM handler")?;

    let (_xvfb, display) = start_xvfb(args.width, args.height, &stopping)?;
    let (_i3, _i3_dir) = start_i3(&display, &stopping)?;

    let scratch = tempfile::tempdir().context("make a scratch directory")?;

    // ---------------------------------------------- an ordinary editor window
    let snippet = scratch.path().join("session_snippet.rs");
    std::fs::write(&snippet, BACKGROUND_SNIPPET).context("write the background snippet")?;
    let _background = Killed::spawn(
        Command::new("xterm")
            .args([
                "-fa",
                "monospace",
                "-fs",
                "13",
                "-bg",
                "#1d2021",
                "-fg",
                "#ebdbb2",
                "-T",
                "session_snippet.rs - nvim",
                "-e",
                "nvim",
                // Never the operator's `~/.config/nvim` or its plugins
                // (rust-analyzer among them, on this machine): `isolate`
                // already points `$HOME` elsewhere, but `--clean` refuses
                // them regardless of the environment, so this background
                // window carries the same guarantee on its own.
                "--clean",
                "-R",
                "--cmd",
                "colorscheme habamax",
            ])
            .arg(&snippet)
            .env("DISPLAY", &display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .context("start the background xterm (is xterm installed?)")?;
    // nvim has to start, read its (clean) config and draw a screen's worth
    // of syntax highlighting inside it; there is no socket to poll for that.
    interruptible_sleep(Duration::from_millis(1200), &stopping)?;

    // -------------------------------------------------------- the pane itself
    let file = scratch.path().join("dictation-2026-09-22-091500.md");
    std::fs::write(&file, "").context("create the dictation file")?;
    let config = Nvim {
        socket_path: scratch.path().join("nvim.sock"),
        // Not `"bundled"`: materialising it writes into
        // `$HOME/.local/state/spokenpad/private`, which `isolate` leaves
        // without even a `.local/state` to hold it. The file it would write
        // is this same one, read directly instead.
        init: Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        ))),
        colorscheme: Some("habamax".to_owned()),
        display: Some(display.clone()),
        ..Nvim::default()
    };
    let (command, marker) = pane_launch(&config, &file)?;
    let mut pane = Pane::open(
        &Options {
            display: display.clone(),
            family: FontFamily::default(),
            size: Points::try_from(args.font_size)?,
            dimensions: Dimensions::new(args.columns, args.rows)
                .expect("a grid of at least one cell"),
            layout: PaneLayout::Floating,
            attach_timeout: STARTUP_TIMEOUT,
            target: Some(Target {
                monitor: Rect {
                    x: 0,
                    y: 0,
                    width: u32::from(args.width),
                    height: u32::from(args.height),
                },
                pointer: Some((args.pane_x, args.pane_y)),
            }),
            title: "spokenpad dictation".to_owned(),
        },
        command,
    )
    .context("open the pane")?;
    pane.show().context("map the pane")?;
    // Nothing reattaches to this editor, but a marker-less socket is not
    // what a live spokenpad editor looks like, so it stays truthful anyway.
    marker.keep();

    // -------------------- drive it over the pane's own RPC channel, the way
    // -------------------- `src/lua/spokenpad.lua`'s own doc comment says the
    // -------------------- daemon does: `Spokenpad.setup`, `.append_once`,
    // -------------------- `.push`.
    let timeout = STARTUP_TIMEOUT;
    pane.call(
        "nvim_exec_lua",
        vec![Value::from(SPOKENPAD_LUA), Value::Array(Vec::new())],
        timeout,
    )
    .context("load the spokenpad Lua module")?;
    let buffer = pane
        .call(
            "nvim_exec_lua",
            vec![
                Value::from("return vim.api.nvim_get_current_buf()"),
                Value::Array(Vec::new()),
            ],
            timeout,
        )
        .context("ask for the current buffer")?
        .as_i64()
        .context("nvim returned a non-integer buffer handle")?;
    pane.call(
        "nvim_exec_lua",
        vec![
            Value::from("Spokenpad.setup(...)"),
            Value::Array(vec![Value::from(buffer), Value::from(true)]),
        ],
        timeout,
    )
    .context("Spokenpad.setup")?;
    for (id, text, continued) in [
        ("shot:1", COMMITTED_FIRST, false),
        ("shot:2", COMMITTED_SECOND, true),
    ] {
        pane.call(
            "nvim_exec_lua",
            vec![
                Value::from("return Spokenpad.append_once(...)"),
                Value::Array(vec![
                    Value::from(id),
                    Value::from(text),
                    Value::from(continued),
                ]),
            ],
            timeout,
        )
        .context("Spokenpad.append_once")?;
    }
    let indicator = Value::Map(vec![
        (Value::from("phase"), Value::from("recording")),
        (Value::from("level"), Value::F64(0.55)),
        (Value::from("preview"), Value::from(PREVIEW)),
        // The capture already wrote both commits, so its tail continues them.
        (
            Value::from("preview_placement"),
            Value::from(PreviewPlacement::Continuation.as_str()),
        ),
        (Value::from("notice"), Value::from("")),
        (Value::from("notice_detail"), Value::from("")),
        (Value::from("latched"), Value::from(true)),
        (Value::from("previewing"), Value::from(true)),
    ]);
    pane.call(
        "nvim_exec_lua",
        vec![
            Value::from("Spokenpad.push(...)"),
            Value::Array(vec![indicator]),
        ],
        timeout,
    )
    .context("Spokenpad.push")?;

    pump_until(&mut pane, Duration::from_secs(10), &stopping, |pane| {
        grid_text(pane).contains("export path")
    })?;
    // A few more idle frames so the cursor's blink phase and the preview's
    // scroll settle before the picture is taken.
    for _ in 0..10 {
        let _ = pane.step(Duration::from_millis(50));
    }
    interruptible_sleep(Duration::from_millis(300), &stopping)?;

    // --------------- the composited screen, not just the pane's framebuffer
    let (connection, screen) =
        x11rb::connect(Some(&display)).context("open a second connection to the display")?;
    let root = connection.setup().roots[screen].root;
    let image = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            0,
            0,
            args.width,
            args.height,
            !0,
        )
        .context("ask for the screen's pixels")?
        .reply()
        .context("the screen's pixels")?;
    let pixels: Vec<u32> = image
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]))
        .collect();
    write_png(&args.out, &pixels, args.width, args.height)?;
    println!("wrote {}", args.out.display());
    Ok(())
}

/// Every cell of every row, so a caller can look for text anywhere on the
/// grid without caring which row it landed on.
fn grid_text(pane: &Pane) -> String {
    let (_, rows) = pane.size();
    (0..rows)
        .map(|row| pane.screen().line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pumps the pane's X event loop until `ready` is true, `timeout` passes, or
/// `stopping` is set (Ctrl-C/SIGTERM): a wait that never checked it would
/// hold the whole scene, background nvim included, open for the full ten
/// seconds after the signal that was supposed to end this.
fn pump_until(
    pane: &mut Pane,
    timeout: Duration,
    stopping: &AtomicBool,
    mut ready: impl FnMut(&Pane) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let _ = pane.step(Duration::from_millis(50));
        if ready(pane) {
            return Ok(());
        }
        ensure!(!stopping.load(Ordering::Relaxed), "interrupted");
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for the pane to draw the pushed text"
        );
    }
}

/// `sleep`, but ended early by `stopping` instead of only by the clock.
fn interruptible_sleep(duration: Duration, stopping: &AtomicBool) -> Result<()> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        ensure!(!stopping.load(Ordering::Relaxed), "interrupted");
        sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())));
    }
    Ok(())
}

/// An `Xvfb` of this script's own, above `:70` so it can never be the user's
/// `:0`. `-displayfd` is the readiness signal: the server writes the number
/// it took once it is listening, and closes its stdout instead if another
/// process already holds that number.
fn start_xvfb(width: u16, height: u16, stopping: &AtomicBool) -> Result<(Killed, String)> {
    for number in 70..200 {
        let display = format!(":{number}");
        let mut command = Command::new("Xvfb");
        command
            .args([display.as_str(), "-displayfd", "1", "-screen", "0"])
            .arg(format!("{width}x{height}x24"))
            .args(["-nolisten", "tcp"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .context("start Xvfb (is xorg-server-xvfb installed?)")?;
        let mut line = String::new();
        let took = BufReader::new(child.stdout.take().expect("Xvfb's stdout"))
            .read_line(&mut line)
            .ok()
            .filter(|read| *read > 0)
            .and_then(|_| line.trim().parse::<u32>().ok());
        if took == Some(number) {
            let killed = Killed::adopt(child);
            let deadline = Instant::now() + Duration::from_secs(10);
            while x11rb::connect(Some(&display)).is_err() {
                ensure!(!stopping.load(Ordering::Relaxed), "interrupted");
                ensure!(
                    Instant::now() < deadline,
                    "Xvfb did not accept connections on {display} within 10s"
                );
                sleep(Duration::from_millis(50));
            }
            return Ok((killed, display));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    bail!("no free X display number between :70 and :200")
}

/// A private `i3`, with its own generated config and IPC socket, so it never
/// touches the user's. Waits for i3 to answer `GET_VERSION` on that socket
/// before returning: i3 only auto-tiles a window that maps after it has
/// taken `SubstructureRedirect` on the root window, and a fixed sleep before
/// mapping the background editor is exactly the race that once left it
/// floating, tiny and unmanaged in a corner, with the rest of the screen
/// showing nothing but i3's black default background.
fn start_i3(display: &str, stopping: &AtomicBool) -> Result<(Killed, tempfile::TempDir)> {
    let directory = tempfile::tempdir().context("make a scratch directory for i3")?;
    let socket = directory.path().join("i3.sock");
    let config = directory.path().join("i3.config");
    std::fs::write(
        &config,
        format!("focus_follows_mouse no\nipc-socket {}\n", socket.display()),
    )
    .context("write the i3 config")?;
    let mut command = Command::new("i3");
    command
        .args(["-c".as_ref(), config.as_os_str()])
        .env("DISPLAY", display)
        .env_remove("I3SOCK")
        .env_remove("SWAYSOCK")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let killed = Killed::spawn(&mut command).context("start i3 (is i3-wm installed?)")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ipc_request(&socket, Message::GetVersion, "").is_err() {
        ensure!(!stopping.load(Ordering::Relaxed), "interrupted");
        ensure!(
            Instant::now() < deadline,
            "i3 did not answer on its IPC socket within 10s"
        );
        sleep(Duration::from_millis(50));
    }
    Ok((killed, directory))
}

/// One i3 IPC request, the same wire format `tests/harness` speaks
/// (`core::wm`'s codec, shared with the daemon's own sway client).
fn ipc_request(socket: &Path, message: Message, payload: &str) -> Result<String> {
    let mut stream = UnixStream::connect(socket).context("connect to i3's IPC socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(&encode(message, payload)?)?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    let length = reply_length(&header, message)?;
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok(String::from_utf8(body)?)
}

/// Points this process, and everything it spawns, at an empty home and the
/// system's own fontconfig and X resources -- never the operator's own
/// `~/.Xresources`. Skipping this once left `Xft.dpi` at whatever the
/// operator's desktop had set (192, here), which doubled every cell, and at
/// that size the background xterm's Xft glyphs hit a real Xvfb/XRender bug
/// (`RenderAddGlyphs`, `BadLength`) and rendered nothing at all: a screen
/// that was 78% flat black instead of a background editor. This is the same
/// isolation `tests/pane_hidpi.rs` does for the same reason.
fn isolate(home: &Path) {
    // SAFETY: called once, at the very start of `main`, before any thread
    // or child process exists to read the environment concurrently.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        for variable in [
            "XDG_DATA_HOME",
            "FONTCONFIG_FILE",
            "FONTCONFIG_PATH",
            "XENVIRONMENT",
            "WINIT_X11_SCALE_FACTOR",
            "WAYLAND_DISPLAY",
        ] {
            std::env::remove_var(variable);
        }
    }
}

/// A child process, and every descendant it has when this drops, stopped:
/// `SIGTERM` to the whole tree, then `SIGKILL` for whatever is still there
/// after a grace period.
///
/// Not just this child's own pid or process group. A terminal emulator's own
/// job control makes the program it execs into a pty its own session leader
/// the moment it starts, so `xterm`'s background `nvim` was never in
/// `xterm`'s process group; a signal aimed only at `xterm` orphaned it
/// instead of reaching it, and once orphaned, `try_wait` on `xterm` has
/// nothing left to say about it either. Walking `/proc` for the whole
/// descendant tree, fresh, every time, is what actually reaches a process
/// like that. `SIGKILL` is unconditional at the end, not only for whatever
/// `try_wait` still calls running: it is what reaches an `nvim` stuck
/// spinning after an X error and ignoring `SIGTERM`, the one case that
/// prompted this, and a `kill` on a pid already gone is a harmless `ESRCH`.
struct Killed(Child);

impl Killed {
    fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self(command.spawn()?))
    }

    /// Wraps a child this already spawned and decided to keep, such as
    /// [`start_xvfb`]'s retry loop, which must read a child's stdout before
    /// it knows whether that child is the one to keep.
    fn adopt(child: Child) -> Self {
        Self(child)
    }
}

impl Drop for Killed {
    fn drop(&mut self) {
        let pid = self.0.id() as libc::pid_t;
        let tree = descendant_tree(pid);
        signal_all(&tree, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && matches!(self.0.try_wait(), Ok(None)) {
            sleep(Duration::from_millis(20));
        }
        signal_all(&tree, libc::SIGKILL);
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `pid` and every process descended from it at the moment this is called,
/// found by walking `/proc/<pid>/task/<pid>/children` (Linux-only, and only
/// ever asked about this process's own descendants, all of them this same
/// user's).
fn descendant_tree(pid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut found = vec![pid];
    let mut frontier = vec![pid];
    while let Some(parent) = frontier.pop() {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
        else {
            continue;
        };
        for child in text
            .split_whitespace()
            .filter_map(|field| field.parse::<libc::pid_t>().ok())
        {
            found.push(child);
            frontier.push(child);
        }
    }
    found
}

/// `signal` to every pid in `tree`, ignoring `ESRCH` from one already gone.
fn signal_all(tree: &[libc::pid_t], signal: libc::c_int) {
    for &pid in tree {
        // SAFETY: every pid here was read moments ago from this process's
        // own descendants under `/proc`, this user's throughout; signalling
        // one that has since exited is the documented, harmless `ESRCH`.
        let _ = unsafe { libc::kill(pid, signal) };
    }
}

/// A framebuffer as an 8-bit RGB PNG. Not shared with `examples/pane.rs`: a
/// PNG encoder has no business in the library, and two examples each owning
/// a dozen lines of it is cheaper than a shared module for just that.
fn write_png(path: &std::path::Path, pixels: &[u32], width: u16, height: u16) -> Result<()> {
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
