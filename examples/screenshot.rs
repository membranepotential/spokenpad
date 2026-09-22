//! Composes the screenshot at the top of README.md: the dictation pane
//! floating above an ordinary editor window, mid-dictation, with committed
//! text, a live preview below it and the winbar showing a latched recording.
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
        nvim::pane_launch,
        pane::{Options, Pane, place::Target},
    },
};
use std::{
    io::{BufRead, BufReader, Read as _, Write as _},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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

    let (_xvfb, display) = start_xvfb(args.width, args.height)?;
    let (_i3, _i3_dir) = start_i3(&display)?;

    let scratch = tempfile::tempdir().context("make a scratch directory")?;

    // ---------------------------------------------- an ordinary editor window
    let snippet = scratch.path().join("session_snippet.rs");
    std::fs::write(&snippet, BACKGROUND_SNIPPET).context("write the background snippet")?;
    let _background = Killed(
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
                "-R",
                "--cmd",
                "colorscheme habamax",
            ])
            .arg(&snippet)
            .env("DISPLAY", &display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start the background xterm (is xterm installed?)")?,
    );
    // nvim has to start, read its (clean) config and draw a screen's worth
    // of syntax highlighting inside it; there is no socket to poll for that.
    sleep(Duration::from_millis(1200));

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
            attach_timeout: Duration::from_secs_f64(config.startup_timeout_s),
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
    let timeout = Duration::from_secs_f64(config.startup_timeout_s);
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

    pump_until(&mut pane, Duration::from_secs(10), |pane| {
        grid_text(pane).contains("export path")
    })?;
    // A few more idle frames so the cursor's blink phase and the preview's
    // scroll settle before the picture is taken.
    for _ in 0..10 {
        let _ = pane.step(Duration::from_millis(50));
    }
    sleep(Duration::from_millis(300));

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
        .chunks_exact(4)
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

/// Pumps the pane's X event loop until `ready` is true or `timeout` passes.
fn pump_until(
    pane: &mut Pane,
    timeout: Duration,
    mut ready: impl FnMut(&Pane) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let _ = pane.step(Duration::from_millis(50));
        if ready(pane) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for the pane to draw the pushed text"
        );
    }
}

/// An `Xvfb` of this script's own, above `:70` so it can never be the user's
/// `:0`. `-displayfd` is the readiness signal: the server writes the number
/// it took once it is listening, and closes its stdout instead if another
/// process already holds that number.
fn start_xvfb(width: u16, height: u16) -> Result<(Killed, String)> {
    for number in 70..200 {
        let display = format!(":{number}");
        let mut child = Command::new("Xvfb")
            .args([display.as_str(), "-displayfd", "1", "-screen", "0"])
            .arg(format!("{width}x{height}x24"))
            .args(["-nolisten", "tcp"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("start Xvfb (is xorg-server-xvfb installed?)")?;
        let mut line = String::new();
        let took = BufReader::new(child.stdout.take().expect("Xvfb's stdout"))
            .read_line(&mut line)
            .ok()
            .filter(|read| *read > 0)
            .and_then(|_| line.trim().parse::<u32>().ok());
        if took == Some(number) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while x11rb::connect(Some(&display)).is_err() {
                ensure!(
                    Instant::now() < deadline,
                    "Xvfb did not accept connections on {display} within 10s"
                );
                sleep(Duration::from_millis(50));
            }
            return Ok((Killed(child), display));
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
fn start_i3(display: &str) -> Result<(Killed, tempfile::TempDir)> {
    let directory = tempfile::tempdir().context("make a scratch directory for i3")?;
    let socket = directory.path().join("i3.sock");
    let config = directory.path().join("i3.config");
    std::fs::write(
        &config,
        format!("focus_follows_mouse no\nipc-socket {}\n", socket.display()),
    )
    .context("write the i3 config")?;
    let child = Command::new("i3")
        .args(["-c".as_ref(), config.as_os_str()])
        .env("DISPLAY", display)
        .env_remove("I3SOCK")
        .env_remove("SWAYSOCK")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start i3 (is i3-wm installed?)")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ipc_request(&socket, Message::GetVersion, "").is_err() {
        ensure!(
            Instant::now() < deadline,
            "i3 did not answer on its IPC socket within 10s"
        );
        sleep(Duration::from_millis(50));
    }
    Ok((Killed(child), directory))
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

/// A child process stopped, gracefully first, when this drops.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        // SAFETY: `id()` is this child's pid, and it is not reaped until the
        // `wait` below, so the pid cannot have been reused by another
        // process in between.
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
