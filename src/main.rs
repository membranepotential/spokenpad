use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use spokenpad::{
    config::{self, Config},
    core::{control::Request, decode::Pipeline, models::files_to_ensure, text::Processor},
    shell::{
        inference::{Transcriber, load_segmenter, model_config},
        models::{FetchEvent, all_present, fetch_models},
        pane,
    },
};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(version, about = "Local push-to-talk dictation for Linux")]
struct Args {
    /// Configuration file; must exist if given [default: $XDG_CONFIG_HOME/spokenpad/config.toml]
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory holding the ASR model, overriding asr.model_dir
    #[arg(long, global = true, value_name = "DIR")]
    model_dir: Option<PathBuf>,
    /// Also write the log, at DEBUG level, to stderr
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Diagnostic log file, or "none" for no file [default: $XDG_STATE_HOME/spokenpad/spokenpad.log]
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,
    /// Write every capture into DIR as a WAV, for offline evaluation
    #[arg(long, value_name = "DIR")]
    dump_audio: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Control(Control),
    #[command(flatten)]
    Action(Action),
}
/// Requests to the running daemon, for key bindings. Each sends one request
/// over the control socket and exits: no config is read, no model loaded.
#[derive(Subcommand, Clone, Copy)]
enum Control {
    /// Begin a push-to-talk capture. Bind it to the key going down.
    Start,
    /// End a push-to-talk capture. Bind it to the key coming up.
    Stop,
    /// Begin a latched capture, or end the capture that is running.
    Toggle,
    /// Discard the capture being recorded. Text already in the file stays.
    Cancel,
}
impl From<Control> for Request {
    fn from(control: Control) -> Self {
        match control {
            Control::Start => Self::Start,
            Control::Stop => Self::Stop,
            Control::Toggle => Self::Toggle,
            Control::Cancel => Self::Cancel,
        }
    }
}
#[derive(Subcommand)]
enum Action {
    /// Recover a recording using the daemon's VAD and recognizer.
    Transcribe {
        wav: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate settings and load/warm the CPU models, without opening devices/windows.
    Check,
    /// Open the dictation editor in this terminal; the daemon writes into it.
    ///
    /// Runs nvim on the dictation socket, on the file holding anything
    /// dictated while no editor was open, or on a new dictation file.
    Editor,
    /// Download spokenpad's default models: Parakeet TDT 0.6B v3 (int8,
    /// ~670 MB) and the Silero VAD (~0.6 MB).
    ///
    /// Verifies every file against its pinned size and sha256, downloading
    /// only what is missing or does not match; a file that already
    /// verifies is left untouched. The daemon, `check` and `transcribe` do
    /// this on their own before loading a default model that is missing,
    /// so running this ahead of time is optional -- useful mainly to see
    /// the download happen, or to warm a fresh install without starting
    /// the service.
    FetchModels {
        /// Destination directory [default: $XDG_DATA_HOME/spokenpad/models]
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
}
fn write_transcript(text: &str, out: Option<&std::path::Path>) -> Result<()> {
    if let Some(path) = out {
        // Explicit --out authorizes replacement; refuse symlinks to unrelated files.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("write {}", path.display()))?;
        writeln!(f, "{text}")?;
    } else {
        writeln!(std::io::stdout().lock(), "{text}")?;
    }
    Ok(())
}
fn run(args: Args) -> Result<u8> {
    // First, and alone: a key binding spawns this on every press and release.
    let command = match args.command {
        Some(Command::Control(control)) => {
            let socket = spokenpad::config::control_socket();
            spokenpad::shell::control::send(&socket, control.into())?;
            return Ok(0);
        }
        Some(Command::Action(action)) => Some(action),
        None => None,
    };
    spokenpad::shell::logging::init(args.verbose, args.log_file.as_deref())?;
    let mut config = Config::load(args.config.as_deref())?;
    if let Some(p) = args.model_dir {
        config.asr.model_dir = spokenpad::config::expand_path(&p)?;
    }
    config.validate()?;
    match command {
        Some(Action::Editor) => match spokenpad::shell::nvim::open_editor(&config.nvim)? {},
        Some(Action::FetchModels { dir }) => {
            let dest = match dir {
                Some(d) => config::expand_path(&d)?,
                None => config::models_dir(),
            };
            let files: Vec<_> = spokenpad::core::models::DEFAULT_MODEL_FILES
                .iter()
                .collect();
            fetch_models(&dest, &files, report_fetch_event_stderr)?;
        }
        Some(Action::Transcribe { wav, out }) => {
            let (samples, rate) = match spokenpad::shell::recorder::read_capture(&wav) {
                Ok(audio) => audio,
                Err(e) => {
                    log::error!("{e:#}");
                    return Ok(4);
                }
            };
            if rate != config.audio.sample_rate {
                log::error!(
                    "recording is {rate}Hz; model requires {}Hz",
                    config.audio.sample_rate
                );
                return Ok(4);
            }
            ensure_default_models(&config);
            if let Err(e) = model_config(&config.asr) {
                log::error!("{e:#}");
                return Ok(2);
            }
            let transcriber = Transcriber::new(&config.asr, rate)?;
            let mut pipeline = Pipeline {
                recognizer: transcriber,
                segmenter: load_segmenter(&config.vad, rate),
            };
            log::info!(
                "decoding {:.1}s from {}",
                samples.len() as f64 / f64::from(rate),
                wav.display()
            );
            let raw = pipeline.decode(&samples, || false, |_| {})?;
            let text = Processor::new(&config.text)?.process(&raw);
            if text.trim().is_empty() {
                log::warn!("recording decoded to no text");
            }
            write_transcript(&text, out.as_deref())?;
        }
        Some(Action::Check) => {
            ensure_default_models(&config);
            if let Err(e) = model_config(&config.asr) {
                log::error!("{e:#}");
                return Ok(2);
            }
            let mut model = Transcriber::new(&config.asr, config.audio.sample_rate)?;
            model.warm_up()?;
            let segmenter = load_segmenter(&config.vad, config.audio.sample_rate);
            ensure!(
                !config.vad.enabled || segmenter.is_some(),
                "VAD is enabled but could not load"
            );
            println!(
                "Configuration valid; CPU recognizer ready; VAD {}.",
                if segmenter.is_some() {
                    "ready"
                } else {
                    "disabled"
                }
            );
            if config.nvim.mode == config::Mode::Pane {
                // The pane needs three things this machine may not have, and
                // finding that out at the first dictation — when the text
                // goes to a file instead of a window — is too late.
                let mut missing = false;
                println!("nvim.mode = \"pane\":");
                for requirement in pane::requirements(&config.nvim) {
                    match requirement.found {
                        Ok(detail) => println!("  {}: {detail}", requirement.what),
                        Err(error) => {
                            missing = true;
                            println!("  {}: NOT AVAILABLE: {error:#}", requirement.what);
                        }
                    }
                }
                if missing {
                    println!(
                        "  Dictation still works: text goes to the dictation file \
                         until a window can be opened."
                    );
                    return Ok(2);
                }
            }
        }
        None => {
            ensure_default_models(&config);
            if let Err(e) = model_config(&config.asr) {
                log::error!("{e:#}");
                return Ok(2);
            }
            if let Some(p) = &args.dump_audio {
                std::fs::create_dir_all(p)?;
            }
            spokenpad::shell::daemon::run(config, args.dump_audio.as_deref())?;
        }
    }
    Ok(0)
}
/// Downloads whichever default model files this configuration would load
/// but does not have yet: only the default Parakeet weights and/or the
/// default Silero VAD, and only when `asr`/`vad` are still pointed at them
/// (see `core::models::files_to_ensure`). A user-configured `model_dir` or
/// another `asr.family` is never touched, so its absence stays the plain
/// "missing ASR model" error `model_config` raises next.
///
/// Never fails the caller: a download that cannot complete (offline, DNS, a
/// corrupted mirror) is logged and left for that same "missing model" error
/// to report clearly with its own exit code, rather than adding a second
/// error path for the same underlying problem.
fn ensure_default_models(config: &Config) {
    let files = files_to_ensure(&config.asr, &config.vad);
    if files.is_empty() {
        return;
    }
    let dest = config::models_dir();
    match all_present(&dest, &files) {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            log::warn!(
                "could not check default models in {}: {e:#}",
                dest.display()
            );
            return;
        }
    }
    log::info!("downloading missing default models into {}", dest.display());
    if let Err(e) = fetch_models(&dest, &files, report_fetch_event_log) {
        log::error!("could not download default models: {e:#}");
    }
}
/// Progress for the daemon's own automatic download: `Info` for what
/// changed, `Debug` for the download bytes so `-v`/the log file can show it
/// without either flooding the journal by default.
fn report_fetch_event_log(event: FetchEvent<'_>) {
    match event {
        FetchEvent::Present(f) => log::debug!("model {} already present", f.relative_path),
        FetchEvent::Downloading(f) => {
            log::info!("downloading {} ({} bytes)", f.relative_path, f.size);
        }
        FetchEvent::Progress { file, downloaded } => {
            log::debug!("{}: {downloaded}/{} bytes", file.relative_path, file.size);
        }
        FetchEvent::Verified(f) => log::info!("verified {}", f.relative_path),
    }
}
/// Progress for `spokenpad fetch-models`: one line per file, plus an
/// in-place percentage while it downloads.
fn report_fetch_event_stderr(event: FetchEvent<'_>) {
    let mut err = std::io::stderr();
    match event {
        FetchEvent::Present(f) => eprintln!("  {}: present", f.relative_path),
        FetchEvent::Downloading(f) => {
            eprint!("  {}: downloading ({} bytes)", f.relative_path, f.size);
            let _ = err.flush();
        }
        FetchEvent::Progress { file, downloaded } => {
            eprint!(
                "\r  {}: downloading {:3}%",
                file.relative_path,
                downloaded.saturating_mul(100) / file.size.max(1)
            );
            let _ = err.flush();
        }
        FetchEvent::Verified(f) => eprintln!("\r  {}: done              ", f.relative_path),
    }
}
fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("spokenpad: {e:#}");
            ExitCode::FAILURE
        }
    }
}
