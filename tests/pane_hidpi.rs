//! The pane's text is as big as Alacritty's, on any display.
//!
//! The pane once took `nvim.font_size` as pixels and ignored the display's
//! resolution, so on a 192 dpi screen its text was half the size of the same
//! number in Alacritty, and nothing noticed: every other pane test runs on an
//! X server without `Xft.dpi`, where a pixel and a point differ by a third and
//! the cells looked plausible. This test sets `Xft.dpi` on its own Xvfb and
//! checks the pane against the real thing: Alacritty, started on the same
//! server with the same family, size and grid, measured from its window.
//!
//! It needs Alacritty to run under Xvfb, which Mesa's software renderer
//! (llvmpipe) makes possible. Alacritty gets a private `HOME` and
//! configuration, so nothing of the user's is read; the server is the test's
//! own, above `:50`. Screenshots of both windows at every resolution go to
//! `SPOKENPAD_PANE_SCREENSHOTS` (the temporary directory by default).
mod harness;

use harness::{Killed, XServer, screenshot_dir, tools_or_skip, write_png};
use spokenpad::{
    config::{FontFamily, Nvim},
    core::font::{Dpi, Points},
    shell::{
        nvim::pane_launch,
        pane::{Options, Pane, Sizing, font::Font},
    },
};
use std::{
    path::Path,
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat, MapState, Window};

const COLUMNS: u16 = 40;
const ROWS: u16 = 10;
/// How long Alacritty may take to open its window and settle at the size it
/// asked for; llvmpipe on a loaded machine is slow to start.
const ALACRITTY_TIMEOUT: Duration = Duration::from_secs(20);

const TEXT: &str = "The quick brown fox jumps over the lazy dog.\n\
                    Über Größe: 0123456789 ()[]{} <=> -> |x|\n";

#[test]
fn the_pane_measures_its_cells_like_alacritty_at_every_resolution() {
    if !tools_or_skip(&["Xvfb", "nvim", "fc-match", "alacritty"]) {
        return;
    }
    let server = XServer::start();
    let directory = tempfile::tempdir().expect("a temporary directory");
    let file = directory.path().join("text.md");
    std::fs::write(&file, TEXT).expect("write the text both windows show");
    let family = FontFamily::default();
    let screenshots = screenshot_dir();

    for (dpi, points) in [(96.0, 12.0), (192.0, 12.0), (192.0, 11.25), (144.0, 11.25)] {
        server.set_resources(&format!("Xft.antialias:\t1\nXft.dpi:\t{dpi}\n"));
        let size = Points::try_from(points).expect("a point size");
        let label = format!("{dpi}dpi-{points}pt");

        // The pane, reading the resolution from the server.
        let (pane_cells, pane_window) = {
            let mut pane = open_pane(&server, directory.path(), &file, &family, size);
            let metrics = pane.metrics();
            // The resolution scales the size, exactly: 12 pt at 192 dpi is
            // 24 pt at 96, and that is what the user reported missing.
            let scaled = Points::try_from(points * dpi as f32 / 96.0).expect("a point size");
            let expected = Font::load(&family, scaled, Dpi::DEFAULT)
                .expect("load the font")
                .metrics();
            assert_eq!(metrics, expected, "{label}: the pane ignored Xft.dpi");
            // Let nvim draw, for the screenshot.
            let settle = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < settle {
                pane.step(Duration::from_millis(50)).expect("run the pane");
            }
            let (pixels, width, height) = pane.framebuffer();
            let path = screenshots.join(format!("pane-{label}.png"));
            write_png(&path, pixels, width, height).expect("write the pane's screenshot");
            println!(
                "{label}: pane cells {}x{}, {}",
                metrics.width,
                metrics.height,
                path.display()
            );
            let window = geometry(&server, pane.window().id());
            ((metrics.width, metrics.height), window)
        };
        assert_eq!(
            pane_window,
            (
                pane_cells.0 * u32::from(COLUMNS),
                pane_cells.1 * u32::from(ROWS)
            ),
            "{label}: the pane's window is not a whole grid of its cells"
        );

        // Alacritty, with the same font and grid, on the same server.
        let alacritty_window = alacritty(&server, directory.path(), &file, &family, points);
        let alacritty_cells = (
            alacritty_window.0 / u32::from(COLUMNS),
            alacritty_window.1 / u32::from(ROWS),
        );
        let path = screenshots.join(format!("alacritty-{label}.png"));
        println!(
            "{label}: alacritty cells {}x{}, {}",
            alacritty_cells.0,
            alacritty_cells.1,
            path.display()
        );
        assert_eq!(
            pane_window, alacritty_window,
            "{label}: a {COLUMNS}x{ROWS} pane is {pane_window:?} pixels and the same grid in \
             Alacritty is {alacritty_window:?}"
        );
    }
}

fn open_pane(
    server: &XServer,
    root: &Path,
    file: &Path,
    family: &FontFamily,
    size: Points,
) -> Pane {
    let config = Nvim {
        socket_path: root.join("nvim.sock"),
        dictation_dir: root.to_owned(),
        // The editor Alacritty runs too, so the two screenshots show the same.
        editor: vec!["nvim".to_owned(), "--clean".to_owned()],
        display: Some(server.display.clone()),
        ..Nvim::default()
    };
    let (command, marker) = pane_launch(&config, file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: server.display.clone(),
            family: family.clone(),
            size,
            sizing: Sizing::Cells {
                columns: COLUMNS,
                rows: ROWS,
            },
            ..Options::default()
        },
        command,
    )
    .expect("open the pane");
    pane.show().expect("map the pane");
    marker.keep();
    pane
}

/// Start Alacritty with this family, size and grid, wait until its window has
/// the size it asked for, write a screenshot, and return that size.
fn alacritty(
    server: &XServer,
    root: &Path,
    file: &Path,
    family: &FontFamily,
    points: f32,
) -> (u32, u32) {
    let home = root.join("alacritty-home");
    std::fs::create_dir_all(&home).expect("make Alacritty's home");
    let config = home.join("alacritty.toml");
    std::fs::write(
        &config,
        format!(
            "[window]\n\
             dimensions = {{ columns = {COLUMNS}, lines = {ROWS} }}\n\
             padding = {{ x = 0, y = 0 }}\n\
             [font]\n\
             size = {points}\n\
             normal = {{ family = \"{family}\" }}\n"
        ),
    )
    .expect("write Alacritty's configuration");
    let _alacritty = Killed(
        Command::new("alacritty")
            .arg("--config-file")
            .arg(&config)
            .args([
                "--class",
                "spokenpad-test-alacritty",
                "-e",
                "nvim",
                "--clean",
            ])
            .arg(file)
            .env("DISPLAY", &server.display)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WINIT_X11_SCALE_FACTOR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start alacritty"),
    );
    // Alacritty opens at winit's default size and then asks for the one its
    // grid needs; the answer is the size it keeps.
    let deadline = Instant::now() + ALACRITTY_TIMEOUT;
    let mut last = None;
    let window = loop {
        let found = alacritty_window(server);
        if let Some(window) = found {
            let size = geometry(server, window);
            if last == Some(size) {
                break window;
            }
            last = Some(size);
        }
        assert!(
            Instant::now() < deadline,
            "Alacritty did not open a window on {} within {ALACRITTY_TIMEOUT:?} (last size \
             {last:?}); it needs OpenGL, which Mesa's llvmpipe provides under Xvfb",
            server.display
        );
        sleep(Duration::from_millis(700));
    };
    // Let nvim draw inside it before the picture is taken.
    sleep(Duration::from_millis(1500));
    let (width, height) = geometry(server, window);
    let image = server
        .connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            window,
            0,
            0,
            width as u16,
            height as u16,
            !0,
        )
        .expect("ask for Alacritty's pixels")
        .reply()
        .expect("Alacritty's pixels");
    let pixels: Vec<u32> = image
        .data
        .chunks_exact(4)
        .map(|pixel| u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]))
        .collect();
    let label = format!(
        "{}dpi-{points}pt",
        dpi_of(server).expect("the test set Xft.dpi")
    );
    write_png(
        &screenshot_dir().join(format!("alacritty-{label}.png")),
        &pixels,
        width as u16,
        height as u16,
    )
    .expect("write Alacritty's screenshot");
    (width, height)
}

/// The mapped top-level window whose `WM_CLASS` is the one this test gave
/// Alacritty.
fn alacritty_window(server: &XServer) -> Option<Window> {
    let tree = server
        .connection
        .query_tree(server.root)
        .ok()?
        .reply()
        .ok()?;
    tree.children.into_iter().find(|window| {
        let viewable = server
            .connection
            .get_window_attributes(*window)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE);
        let class = server
            .connection
            .get_property(false, *window, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.value)
            .unwrap_or_default();
        viewable
            && class
                .split(|byte| *byte == 0)
                .any(|name| name == b"spokenpad-test-alacritty")
    })
}

fn geometry(server: &XServer, window: Window) -> (u32, u32) {
    let reply = server
        .connection
        .get_geometry(window)
        .expect("ask for a window's size")
        .reply()
        .expect("a window's size");
    (u32::from(reply.width), u32::from(reply.height))
}

/// The `Xft.dpi` this test put on the server, read back.
fn dpi_of(server: &XServer) -> Option<f64> {
    let reply = server
        .connection
        .get_property(
            false,
            server.root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1024,
        )
        .ok()?
        .reply()
        .ok()?;
    Dpi::from_resources(&String::from_utf8_lossy(&reply.value)).map(Dpi::get)
}
