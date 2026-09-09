use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use spokenpad::{
    config::Config,
    decode::{Pipeline, Recognizer},
    inference::{Transcriber, load_segmenter},
    text::Processor,
};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(version, about = "Local push-to-talk dictation for Linux/X11")]
struct Args {
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    model_dir: Option<PathBuf>,
    #[arg(short, long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    log_file: Option<PathBuf>,
    #[arg(long)]
    dump_audio: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
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
    spokenpad::logging::init(args.verbose, args.log_file.as_deref())?;
    let mut config = Config::load(args.config.as_deref())?;
    if let Some(p) = args.model_dir {
        config.asr.model_dir = spokenpad::config::expand_path(&p)?;
    }
    config.validate()?;
    match args.command {
        Some(Action::Transcribe { wav, out }) => {
            let (samples, rate) = match spokenpad::recorder::read_capture(&wav) {
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
            if let Err(e) = config.asr.check_files() {
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
            if let Err(e) = config.asr.check_files() {
                log::error!("{e:#}");
                return Ok(2);
            }
            let mut model = Transcriber::new(&config.asr, config.audio.sample_rate)?;
            model.transcribe(&vec![0.; config.audio.sample_rate as usize])?;
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
        }
        None => {
            if let Err(e) = config.asr.check_files() {
                log::error!("{e:#}");
                return Ok(2);
            }
            if let Some(p) = &args.dump_audio {
                std::fs::create_dir_all(p)?;
            }
            if let Err(e) = spokenpad::daemon::run(config, args.dump_audio.as_deref()) {
                // Keep the established systemd permanent-failure exit code.
                if e.chain()
                    .any(|c| c.to_string() == "hotkey watcher could not start")
                {
                    log::error!("{e:#}");
                    return Ok(3);
                }
                return Err(e);
            }
        }
    }
    Ok(0)
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
