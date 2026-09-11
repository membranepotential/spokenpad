use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use spokenpad::{
    config::Config,
    core::{decode::Pipeline, text::Processor},
    shell::inference::{Transcriber, load_segmenter},
};
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(version, about = "Local push-to-talk dictation for Linux/X11")]
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
    spokenpad::shell::logging::init(args.verbose, args.log_file.as_deref())?;
    let mut config = Config::load(args.config.as_deref())?;
    if let Some(p) = args.model_dir {
        config.asr.model_dir = Some(spokenpad::config::expand_path(&p)?);
    }
    config.validate()?;
    match args.command {
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
        }
        None => {
            if let Err(e) = config.asr.check_files() {
                log::error!("{e:#}");
                return Ok(2);
            }
            if let Some(p) = &args.dump_audio {
                std::fs::create_dir_all(p)?;
            }
            if let Err(e) = spokenpad::shell::daemon::run(config, args.dump_audio.as_deref()) {
                let Some(code) = permanent_failure(&e) else {
                    return Err(e);
                };
                log::error!("{e:#}");
                return Ok(code);
            }
        }
    }
    Ok(0)
}
/// Exit codes systemd must treat as permanent, selected by error type so a
/// reworded message can never silently turn into a restart loop.
fn permanent_failure(error: &anyhow::Error) -> Option<u8> {
    error
        .downcast_ref::<spokenpad::shell::daemon::HotkeyUnavailable>()
        .map(|_| 3)
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use spokenpad::shell::daemon::HotkeyUnavailable;

    #[test]
    fn only_the_typed_hotkey_failure_maps_to_exit_code_three() {
        let wrapped: anyhow::Error = Err::<(), _>(anyhow!("no permission on /dev/input/event3"))
            .context(HotkeyUnavailable)
            .unwrap_err();
        assert_eq!(permanent_failure(&wrapped), Some(3));
        assert!(
            wrapped
                .to_string()
                .contains("hotkey watcher could not start"),
            "{wrapped}"
        );
        assert!(
            format!("{wrapped:#}").contains("/dev/input/event3"),
            "the cause must stay in the message"
        );

        // A message that merely reads like it is not a permanent failure.
        let lookalike = anyhow!("hotkey watcher could not start");
        assert_eq!(permanent_failure(&lookalike), None);
        assert_eq!(permanent_failure(&anyhow!("editor died")), None);
    }
}
