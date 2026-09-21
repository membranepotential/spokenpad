//! Open the dictation pane by hand, on whichever display you point it at.
//!
//! Nothing in the daemon does this yet; the example exists so the pane can be
//! looked at, and so a screenshot of it can be written out and judged.
//!
//! ```sh
//! # in your own session (the window will not take the focus)
//! cargo run --example pane -- --file /tmp/dictation.md
//! # headless, with a picture of the result
//! Xvfb :77 -screen 0 1280x800x24 &
//! cargo run --example pane -- --display :77 --screenshot /tmp/pane.png --quit-after 2
//! ```
//!
//! The screenshot is written by this example, not by the library: a PNG
//! encoder has no business in the daemon.
use anyhow::{Context, Result};
use clap::Parser;
use spokenpad::{
    config::{FontFamily, Nvim},
    shell::{
        nvim::pane_launch,
        pane::{Options, Pane, Sizing, Status},
    },
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(about = "Open spokenpad's dictation pane on an X display")]
struct Args {
    /// X display to open on, e.g. ":77". Defaults to $DISPLAY.
    #[arg(long, value_name = "DISPLAY")]
    display: Option<String>,
    /// The file the editor opens. A temporary one is used when this is unset.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    /// The socket the editor listens on, so `spokenpad` can append to it.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// A fontconfig family name.
    #[arg(long, default_value = "monospace")]
    family: String,
    /// Font size in pixels.
    #[arg(long, default_value_t = 16.0)]
    size: f32,
    #[arg(long, default_value_t = 72)]
    columns: u16,
    #[arg(long, default_value_t = 12)]
    rows: u16,
    /// Use spokenpad's bundled nvim configuration instead of the user's.
    #[arg(long)]
    bundled: bool,
    /// Write the window's pixels here as a PNG before exiting.
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,
    /// Exit after this many seconds instead of running until the window is
    /// closed.
    #[arg(long, value_name = "SECONDS")]
    quit_after: Option<f64>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let directory = tempfile::tempdir().context("make a scratch directory")?;
    let file = args
        .file
        .clone()
        .unwrap_or_else(|| directory.path().join("dictation.md"));
    if !file.exists() {
        std::fs::write(&file, "").with_context(|| format!("create {}", file.display()))?;
    }
    let config = Nvim {
        socket_path: args
            .socket
            .clone()
            .unwrap_or_else(|| directory.path().join("nvim.sock")),
        init: args.bundled.then(|| PathBuf::from("bundled")),
        ..Nvim::default()
    };
    let options = Options {
        // The example's own boundary: a display named on the command
        // line, or the one this shell is on.
        display: args
            .display
            .clone()
            .or_else(|| std::env::var("DISPLAY").ok())
            .filter(|name| !name.is_empty())
            .context("pass --display, or run this where $DISPLAY is set")?,
        family: FontFamily::try_from(args.family.clone())?,
        size: args.size,
        sizing: Sizing::Cells {
            columns: args.columns,
            rows: args.rows,
        },
        attach_timeout: Duration::from_secs_f64(config.startup_timeout_s),
        position: None,
        title: "spokenpad dictation".to_owned(),
    };
    let (command, marker) = pane_launch(&config, &file)?;
    let mut pane = Pane::open(&options, command)?;
    pane.show()?;
    marker.keep();
    println!(
        "pane window {} on {} listening at {}",
        pane.window().id(),
        args.display.as_deref().unwrap_or("$DISPLAY"),
        config.socket_path.display()
    );

    let deadline = args
        .quit_after
        .map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    loop {
        let timeout = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        if timeout.is_zero()
            || pane.step(timeout.min(Duration::from_millis(100)))? == Status::Finished
        {
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
    }

    if let Some(path) = &args.screenshot {
        let (pixels, width, height) = pane.framebuffer();
        write_png(path, pixels, width, height)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

/// The framebuffer as an 8-bit RGB PNG.
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
