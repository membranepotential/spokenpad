use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use spokenpad::{
    config::{self, Config, Source},
    core::{control::Request, decode::Pipeline, text::Processor},
    shell::{
        control::Socket,
        inference::{SpeechSegmenter, Transcriber, model_config},
        models::{FetchEvent, ensure_defaults, fetch_models},
        pane,
    },
};
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Local push-to-talk dictation for Linux",
    long_about = "Local push-to-talk dictation for Linux.\n\n\
        Run without a command, spokenpad is the daemon: it listens for the \
        commands below, records while a key is held, and writes what you \
        said into a Neovim window. systemd's spokenpad.socket starts it on \
        the first press; nothing else needs to run it.",
    after_help = "Exit codes: 0 success, 1 any other error, 2 a model file is missing, \
        3 another daemon is running, 4 the recording given to transcribe is unreadable \
        or not 16 kHz, 5 check found that the pane cannot open here."
)]
struct Cli {
    #[command(flatten)]
    settings: Settings,
    #[command(flatten)]
    logging: Logging,
    /// Write every capture into DIR as a WAV, for offline evaluation
    #[arg(long, value_name = "DIR")]
    dump_audio: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Where the configuration comes from. Given before the command or after
/// it; after it wins.
#[derive(Args, Clone, Default)]
struct Settings {
    /// Configuration file; must exist if given [default: $XDG_CONFIG_HOME/spokenpad/config.toml]
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory holding the speech model, overriding asr.model_dir
    #[arg(long, value_name = "DIR")]
    model_dir: Option<PathBuf>,
}

impl Settings {
    fn or(self, outer: Self) -> Self {
        Self {
            config: self.config.or(outer.config),
            model_dir: self.model_dir.or(outer.model_dir),
        }
    }
    fn source(self) -> Source {
        Source {
            path: self.config,
            model_dir: self.model_dir,
        }
    }
}

/// Where the log goes. Given before the command or after it; after it wins.
#[derive(Args, Clone, Default)]
struct Logging {
    /// Also write the log, at DEBUG level, to stderr
    #[arg(short, long)]
    verbose: bool,
    /// Diagnostic log file, or "none" for no file [default: $XDG_STATE_HOME/spokenpad/spokenpad.log]
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

impl Logging {
    fn or(self, outer: Self) -> Self {
        Self {
            verbose: self.verbose || outer.verbose,
            log_file: self.log_file.or(outer.log_file),
        }
    }
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Control(Control),
    /// Recover a recording: decode a WAV the way the daemon does and print
    /// the text.
    Transcribe {
        /// The recording, a 16 kHz mono WAV such as the daemon's own
        /// recovery WAVs
        wav: PathBuf,
        /// Write the text to this file instead of standard output
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Start this many seconds into the recording: where the daemon's own
        /// transcription of it stopped, as its log says
        #[arg(long, value_name = "SECONDS", default_value_t = 0.0)]
        from: f64,
        #[command(flatten)]
        settings: Settings,
        #[command(flatten)]
        logging: Logging,
    },
    /// Validate the configuration, load the models, list the input devices
    /// with the one the daemon opens marked, and say whether the pane can
    /// open here; records nothing and opens no window
    Check {
        #[command(flatten)]
        settings: Settings,
        #[command(flatten)]
        logging: Logging,
    },
    /// Open the dictation editor in this terminal; the daemon writes into it.
    ///
    /// Runs nvim on the dictation socket, on the file holding anything
    /// dictated while no editor was open, or on a new dictation file.
    Editor {
        /// Configuration file; must exist if given [default: $XDG_CONFIG_HOME/spokenpad/config.toml]
        #[arg(short, long, value_name = "PATH")]
        config: Option<PathBuf>,
        #[command(flatten)]
        logging: Logging,
    },
    /// Download spokenpad's default models: Parakeet TDT 0.6B v3 (int8,
    /// ~670 MB) and the Silero VAD (~0.6 MB).
    ///
    /// Verifies every file against its pinned size and sha256, downloading
    /// only what is missing or does not match; a file that already
    /// verifies is left untouched. The daemon, `check` and `transcribe` do
    /// this on their own before loading a default model that is missing,
    /// so running this ahead of time is optional -- useful mainly to see
    /// the download happen, or to warm a fresh install without starting
    /// the service. Reads no configuration.
    FetchModels {
        /// Destination directory [default: $XDG_DATA_HOME/spokenpad/models]
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },
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

/// Why spokenpad exited, as its exit code says. Chosen by what went wrong,
/// never by an error's message, so a reworded error cannot change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Exit {
    Success = 0,
    /// A model file is missing (`check`, `transcribe`).
    ModelMissing = 2,
    /// Another daemon holds the lock: a daemon started by hand.
    AnotherDaemon = 3,
    /// The recording given to `transcribe` is unreadable or not 16 kHz.
    BadRecording = 4,
    /// `check`: the pane this configuration opens cannot open here.
    PaneUnavailable = 5,
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        Self::from(exit as u8)
    }
}

fn run(cli: Cli) -> Result<Exit> {
    let Cli {
        settings: outer,
        logging: outer_logging,
        dump_audio,
        command,
    } = cli;
    match command {
        // First, and alone: a key binding spawns this on every press and
        // release.
        Some(Command::Control(control)) => {
            spokenpad::shell::control::send(&config::control_socket(), control.into())?;
            Ok(Exit::Success)
        }
        Some(Command::FetchModels { dir }) => fetch(dir),
        Some(Command::Editor { config, logging }) => {
            start_logging(logging.or(outer_logging))?;
            let settings = Settings {
                config,
                model_dir: None,
            }
            .or(outer);
            let config = settings.source().load()?;
            match spokenpad::shell::nvim::open_editor(&config.nvim)? {}
        }
        Some(Command::Transcribe {
            wav,
            out,
            from,
            settings,
            logging,
        }) => {
            start_logging(logging.or(outer_logging))?;
            let config = settings.or(outer).source().load()?;
            transcribe(&config, &wav, out.as_deref(), from)
        }
        Some(Command::Check { settings, logging }) => {
            start_logging(logging.or(outer_logging))?;
            // An invalid configuration is an error, with its whole text.
            let config = settings.or(outer).source().load()?;
            check(&config)
        }
        None => {
            // SAFETY: nothing so far has started a thread: only the
            // arguments have been parsed.
            let inherited = unsafe { Socket::from_systemd(&config::control_socket()) }?;
            start_logging(outer_logging)?;
            serve(outer.source(), inherited, dump_audio)
        }
    }
}

/// Makes the state directory private and opens the log in it.
fn start_logging(logging: Logging) -> Result<()> {
    // Before the log, which is the first to write into it.
    let state = config::state_dir();
    let narrowed = spokenpad::shell::dirs::secure_own(&state);
    spokenpad::shell::logging::init(logging.verbose, logging.log_file.as_deref())?;
    match narrowed {
        Ok(true) => log::info!(
            "{} could be listed by other users; narrowed it to 0700",
            state.display()
        ),
        Ok(false) => {}
        Err(e) => log::warn!("could not make {} private: {e}", state.display()),
    }
    Ok(())
}

/// The daemon. Nothing here may end it over a setting: it has taken presses
/// on its socket, and one that exits leaves them unanswered while systemd
/// starts it again.
fn serve(source: Source, inherited: Option<Socket>, dump_audio: Option<PathBuf>) -> Result<Exit> {
    let (config, invalid) = source.load_or_defaults();
    if let Some(error) = invalid {
        log::error!("config not loaded, running on the defaults: {error:#}");
    }
    let dump = dump_audio.filter(|dir| match spokenpad::shell::dirs::create_private(dir) {
        Ok(()) => true,
        Err(e) => {
            log::error!("not dumping audio: create {}: {e}", dir.display());
            false
        }
    });
    match spokenpad::shell::daemon::run(config, source, inherited, dump.as_deref()) {
        Err(e) if e.is::<spokenpad::shell::daemon::AnotherDaemon>() => {
            eprintln!("spokenpad: {e:#}");
            Ok(Exit::AnotherDaemon)
        }
        result => result.map(|()| Exit::Success),
    }
}

/// `spokenpad fetch-models`: the pinned default files, wherever `dir`
/// says, whatever the configuration says.
fn fetch(dir: Option<PathBuf>) -> Result<Exit> {
    let dest = match dir {
        Some(d) => config::expand_path(&d)?,
        None => config::models_dir(),
    };
    let files: Vec<_> = spokenpad::core::models::DEFAULT_MODEL_FILES
        .iter()
        .collect();
    fetch_models(&dest, &files, report_fetch_event_stderr)?;
    Ok(Exit::Success)
}

/// `spokenpad transcribe`: one recording, from `from` seconds on, through
/// the daemon's detector and recognizer, to `out` or standard output.
fn transcribe(config: &Config, wav: &Path, out: Option<&Path>, from: f64) -> Result<Exit> {
    ensure!(
        from.is_finite() && from >= 0.0,
        "--from must be a number of seconds, 0 or more"
    );
    let (mut samples, rate) = match spokenpad::shell::recorder::read_capture(wav) {
        Ok(audio) => audio,
        Err(e) => {
            log::error!("{e:#}");
            return Ok(Exit::BadRecording);
        }
    };
    if rate != config.audio.sample_rate {
        log::error!(
            "recording is {rate}Hz; model requires {}Hz",
            config.audio.sample_rate
        );
        return Ok(Exit::BadRecording);
    }
    samples.drain(..((from * f64::from(rate)) as usize).min(samples.len()));
    ensure_default_models(config);
    if let Err(e) = models_present(config) {
        log::error!("{e:#}");
        return Ok(Exit::ModelMissing);
    }
    let mut pipeline = Pipeline {
        recognizer: Transcriber::new(&config.asr, rate)?,
        segmenter: SpeechSegmenter::new(&config.vad, rate)?,
    };
    log::info!(
        "decoding {:.1}s from {}",
        samples.len() as f64 / f64::from(rate),
        wav.display()
    );
    let raw = pipeline.decode(&samples, || false, |_, _| {})?;
    let text = Processor::new(&config.text)?.process(&raw);
    if text.trim().is_empty() {
        log::warn!("recording decoded to no text");
    }
    write_transcript(&text, out)?;
    Ok(Exit::Success)
}

/// `spokenpad check`: loads what the daemon would load, and says whether the
/// pane can open here.
fn check(config: &Config) -> Result<Exit> {
    ensure_default_models(config);
    if let Err(e) = models_present(config) {
        log::error!("{e:#}");
        return Ok(Exit::ModelMissing);
    }
    let mut model = Transcriber::new(&config.asr, config.audio.sample_rate)?;
    model.warm_up()?;
    SpeechSegmenter::new(&config.vad, config.audio.sample_rate)?;
    println!("Configuration valid; CPU recognizer ready; VAD ready.");
    print_input_devices(config.audio.device.as_deref());
    println!(
        "A running daemon applies [nvim] changes to the next dictation window it opens; \
         every other section takes effect after `systemctl --user restart spokenpad`."
    );
    if config.nvim.mode != config::Mode::Pane {
        return Ok(Exit::Success);
    }
    // The pane needs three things this machine may not have, and finding
    // that out at the first dictation — when the text goes to a file instead
    // of a window — is too late.
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
    if !missing {
        return Ok(Exit::Success);
    }
    println!(
        "  Dictation still works: text goes to the dictation file \
         until a window can be opened."
    );
    Ok(Exit::PaneUnavailable)
}

/// The input devices, with the one the daemon opens for `audio.device`
/// (`query`) marked. Lists them without opening any.
fn print_input_devices(query: Option<&str>) {
    let which = query.map_or_else(
        || "the default input device".to_owned(),
        |query| format!("audio.device = {query:?}"),
    );
    match spokenpad::shell::audio::input_devices(query) {
        Ok((devices, why)) => {
            println!("Input devices (* marks the one the daemon opens, for {which}):");
            if devices.is_empty() {
                println!("  none");
            }
            for device in &devices {
                let mark = if device.chosen { '*' } else { ' ' };
                println!("  {mark} {} ({})", device.name, device.host_api);
            }
            if let Some(why) = why {
                println!("  The daemon opens none of them: {why}");
            }
        }
        Err(error) => println!("Input devices: cannot list them: {error:#}"),
    }
}

fn write_transcript(text: &str, out: Option<&Path>) -> Result<()> {
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

/// Downloads whichever default model files this configuration would load
/// but does not have yet (see `shell::models::ensure_defaults`). A
/// user-configured `model_dir` is never touched, so its absence stays the
/// plain "missing ASR model" error `models_present` raises next.
///
/// Never fails the caller: a download that cannot complete (offline, DNS, a
/// corrupted mirror) is logged and left for that same "missing model" error
/// to report clearly with its own exit code, rather than adding a second
/// error path for the same underlying problem.
fn ensure_default_models(config: &Config) {
    if let Err(e) = ensure_defaults(&config.asr, &config.vad, |done, total| {
        log::debug!("downloaded {done} of {total} bytes");
    }) {
        log::error!("could not download default models: {e:#}");
    }
}

/// Whether the files of both models are where the configuration says. A
/// missing one is [`Exit::ModelMissing`], whichever model it belongs to.
fn models_present(config: &Config) -> Result<()> {
    model_config(&config.asr)?;
    ensure!(
        config.vad.model.is_file(),
        "missing VAD model {} (run `spokenpad fetch-models`)",
        config.vad.model.display()
    );
    Ok(())
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
    match run(Cli::parse()) {
        Ok(exit) => exit.into(),
        Err(e) => {
            eprintln!("spokenpad: {e:#}");
            ExitCode::FAILURE
        }
    }
}
