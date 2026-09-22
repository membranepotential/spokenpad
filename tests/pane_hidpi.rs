//! The pane's text is as big as Alacritty's, on any display.
//!
//! The pane once took `nvim.font_size` as pixels and ignored the display's
//! resolution, so on a 192 dpi screen its text was half the size of the same
//! number in Alacritty, and nothing noticed: every other pane test runs on an
//! X server without `Xft.dpi`, where a pixel and a point differ by a third and
//! the cells looked plausible. This test sets `Xft.dpi` on its own Xvfb, in
//! each of the ways a desktop sets it, and checks the pane against the real
//! thing: Alacritty, started on the same server with the same family, size
//! and grid, measured from its window.
//!
//! **Hermetic.** Before anything runs, this process's `HOME` and
//! `XDG_CONFIG_HOME` point at an empty temporary directory, and the variables
//! that redirect fontconfig or the X resources are removed. The pane's
//! `fc-match`, the reference `Font::load` and Alacritty all inherit that one
//! environment, so they read the same system fontconfig and no user
//! `fonts.conf`, `~/.Xresources` or Alacritty configuration.
//!
//! It needs Alacritty to run under Xvfb, which Mesa's software renderer
//! (llvmpipe) makes possible; the server is the test's own, above `:50`.
//! Screenshots of both windows for every case go to
//! `SPOKENPAD_PANE_SCREENSHOTS` (the temporary directory by default).
mod harness;

use harness::{Killed, XServer, screenshot_dir, tools_or_skip, write_png};
use spokenpad::{
    config::{FontFamily, Nvim},
    core::{
        font::{Dpi, Points},
        geometry::Dimensions,
    },
    shell::{
        nvim::pane_launch,
        pane::{Options, Pane, font::Font, x11::Display},
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

/// Where a case puts its resources.
enum Resources {
    /// The root window's `RESOURCE_MANAGER`, as `xrdb -load` writes it.
    Property(&'static str),
    /// Only `~/.Xresources`, on a server where nobody ran `xrdb`.
    HomeFile(&'static str),
}

struct Case {
    label: &'static str,
    resources: Resources,
    /// The resolution the resources state.
    dpi: f32,
    points: f32,
}

const CASES: [Case; 6] = [
    Case {
        label: "96dpi-12pt",
        resources: Resources::Property("Xft.antialias:\t1\nXft.dpi:\t96\n"),
        dpi: 96.0,
        points: 12.0,
    },
    Case {
        label: "192dpi-12pt",
        resources: Resources::Property("Xft.antialias:\t1\nXft.dpi:\t192\n"),
        dpi: 192.0,
        points: 12.0,
    },
    Case {
        label: "192dpi-11.25pt",
        // The last of two equal entries wins, as in every X resource reader.
        resources: Resources::Property("Xft.dpi:\t120\nXft.dpi:\t192\n"),
        dpi: 192.0,
        points: 11.25,
    },
    Case {
        label: "144dpi-11.25pt",
        resources: Resources::Property("Xft.dpi:\t144\n"),
        dpi: 144.0,
        points: 11.25,
    },
    Case {
        label: "wildcard-192dpi-12pt",
        resources: Resources::Property("*dpi:\t192\n"),
        dpi: 192.0,
        points: 12.0,
    },
    Case {
        label: "xresources-192dpi-12pt",
        resources: Resources::HomeFile("Xft.dpi: 192\n"),
        dpi: 192.0,
        points: 12.0,
    },
];

#[test]
fn the_pane_measures_its_cells_like_alacritty_at_every_resolution() {
    if !tools_or_skip(&["Xvfb", "nvim", "fc-match", "alacritty"]) {
        return;
    }
    let directory = tempfile::tempdir().expect("a temporary directory");
    let home = directory.path().join("home");
    std::fs::create_dir_all(&home).expect("make the private home");
    isolate(&home);
    // Two screens, so the last check can open on `:N.1`.
    let server = XServer::start_with_screens(2);
    let file = directory.path().join("text.md");
    std::fs::write(&file, TEXT).expect("write the text both windows show");
    let family = FontFamily::default();
    let screenshots = screenshot_dir();
    let xresources = home.join(".Xresources");

    for case in CASES {
        let label = case.label;
        match case.resources {
            Resources::Property(text) => {
                let _ = std::fs::remove_file(&xresources);
                server.set_resources(text);
            }
            Resources::HomeFile(text) => {
                server.clear_resources();
                std::fs::write(&xresources, text).expect("write ~/.Xresources");
            }
        }
        let size = Points::try_from(case.points).expect("a point size");

        // The pane, reading the resolution from the server.
        let (pane_cells, pane_window) = {
            let mut pane = open_pane(&server.display, directory.path(), &file, &family, size);
            let metrics = pane.metrics();
            assert_eq!(
                metrics,
                scaled(&family, case.points, case.dpi),
                "{label}: the pane did not find Xft.dpi"
            );
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
            ((metrics.width, metrics.height, pane.padding()), window)
        };
        let padding = (4.0 * case.dpi / 96.0).floor() as u32;
        assert_eq!(
            pane_cells.2, padding,
            "{label}: the margin is not four pixels at 96 dpi, scaled"
        );
        assert_eq!(
            pane_window,
            (
                pane_cells.0 * u32::from(COLUMNS) + 2 * padding,
                pane_cells.1 * u32::from(ROWS) + 2 * padding
            ),
            "{label}: the pane's window is not a whole grid of its cells and its margin"
        );

        // Alacritty, with the same font and grid, on the same server.
        let alacritty_window = alacritty(
            &server,
            directory.path(),
            &file,
            &family,
            case.points,
            label,
        );
        println!(
            "{label}: alacritty cells {}x{}",
            (alacritty_window.0 - 2 * padding) / u32::from(COLUMNS),
            (alacritty_window.1 - 2 * padding) / u32::from(ROWS),
        );
        assert_eq!(
            pane_window, alacritty_window,
            "{label}: a {COLUMNS}x{ROWS} pane is {pane_window:?} pixels and the same grid in \
             Alacritty is {alacritty_window:?}"
        );
    }

    // A display that names the second screen still takes its resources from
    // the first screen's root window, as winit's database does.
    let _ = std::fs::remove_file(&xresources);
    server.set_resources("Xft.dpi:\t192\n");
    let second = format!("{}.1", server.display);
    let dpi = Display::connect(&second)
        .expect("connect to the second screen")
        .dpi();
    assert_eq!(dpi, Dpi::new(192.0).unwrap(), "{second}");
    let size = Points::try_from(12.0).expect("a point size");
    let pane = open_pane(&second, directory.path(), &file, &family, size);
    assert_eq!(pane.metrics(), scaled(&family, 12.0, 192.0), "{second}");
}

/// Point this process, and everything it starts, at an empty home and the
/// system's own fontconfig and X resources.
fn isolate(home: &Path) {
    // SAFETY: this test binary holds one test, and this runs at its start,
    // before it starts an X server, a pane or any other thread; libtest's
    // main thread only waits for it. Nothing reads the environment
    // concurrently with these writes.
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

/// The cells of `points` at `dpi`, computed as that many points at 96 dpi:
/// the resolution scales the size exactly, and that is what the user reported
/// missing.
fn scaled(family: &FontFamily, points: f32, dpi: f32) -> spokenpad::core::font::CellMetrics {
    let points = Points::try_from(points * dpi / 96.0).expect("a point size");
    Font::load(family, points, Dpi::DEFAULT)
        .expect("load the font")
        .metrics()
}

fn open_pane(display: &str, root: &Path, file: &Path, family: &FontFamily, size: Points) -> Pane {
    let config = Nvim {
        socket_path: root.join("nvim.sock"),
        dictation_dir: root.to_owned(),
        // The editor Alacritty runs too, so the two screenshots show the same.
        editor: vec!["nvim".to_owned(), "--clean".to_owned()],
        display: Some(display.to_owned()),
        ..Nvim::default()
    };
    let (command, marker) = pane_launch(&config, file).expect("build the nvim command");
    let mut pane = Pane::open(
        &Options {
            display: display.to_owned(),
            family: family.clone(),
            size,
            dimensions: Dimensions::new(COLUMNS, ROWS).expect("a grid of at least one cell"),
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
    label: &str,
) -> (u32, u32) {
    let config = root.join("alacritty.toml");
    std::fs::write(
        &config,
        format!(
            "[window]\n\
             dimensions = {{ columns = {COLUMNS}, lines = {ROWS} }}\n\
             padding = {{ x = 4, y = 4 }}\n\
             [font]\n\
             size = {}\n\
             normal = {{ family = \"{family}\" }}\n",
            points
        ),
    )
    .expect("write Alacritty's configuration");
    // Everything else, `HOME` included, is this process's isolated
    // environment, the same one the pane's `fc-match` sees.
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
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
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
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| u32::from_le_bytes([pixel[0], pixel[1], pixel[2], 0]))
        .collect();
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
