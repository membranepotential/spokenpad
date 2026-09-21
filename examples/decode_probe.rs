//! Decoder probe: how often does a decoding method lose a speech window?
//!
//! Two modes, both printing one JSON object on stdout:
//!
//! - *replay* (default): splits every given WAV with the VAD, decodes each
//!   window the way `Pipeline::transcribe_speech` does (one padded decode, a
//!   bare retry only when the first decode is empty) and counts the losses.
//! - *windows* (`--range`): decodes the named sample ranges of the given WAV
//!   twice, padded and bare, and prints both texts.
//!
//! It never prints the text of a replayed capture, only counts; `--range`
//! prints text, so point it at your own recordings.
//!
//! ```sh
//! cargo run --release --example decode_probe -- --decoding beam ~/.local/state/spokenpad/audio/*.wav
//! cargo run --release --example decode_probe -- --decoding greedy --range 28.0:38.6 one.wav
//! ```
use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use serde_json::json;
use spokenpad::{
    config::{Asr, Config, Decoding, Model, REQUIRED_SAMPLE_RATE},
    core::decode::{Recognizer, Segmenter, TrailingSilence},
    shell::{
        inference::{SpeechSegmenter, Transcriber},
        recorder::read_capture,
    },
};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Method {
    Greedy,
    Beam,
}

/// One `START:END` pair in seconds, as `--range` takes it.
#[derive(Clone, Copy)]
struct Range {
    start: f64,
    end: f64,
}
impl std::str::FromStr for Range {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (a, b) = s.split_once(':').context("expected START:END in seconds")?;
        let range = Self {
            start: a.trim().parse()?,
            end: b.trim().parse()?,
        };
        if !(range.start.is_finite() && range.end.is_finite() && range.start < range.end) {
            bail!("empty or non-finite range {s}");
        }
        Ok(range)
    }
}
impl Range {
    fn samples(self, len: usize) -> std::ops::Range<usize> {
        let rate = f64::from(REQUIRED_SAMPLE_RATE);
        let start = (self.start * rate) as usize;
        let end = ((self.end * rate) as usize).min(len);
        start.min(end)..end
    }
}

#[derive(Parser)]
#[command(about = "Replay captures or windows through one Parakeet decoding method")]
struct Args {
    /// Which sherpa-onnx search to run
    #[arg(long, value_enum, default_value = "beam")]
    decoding: Method,
    /// Hotword phrases for beam search, comma separated
    #[arg(long, value_delimiter = ',')]
    vocabulary: Vec<String>,
    /// Per-token hotword bias
    #[arg(long, default_value_t = 1.5)]
    hotwords_score: f32,
    /// Directory holding the ASR model, overriding asr.model_dir
    #[arg(long, value_name = "DIR")]
    model_dir: Option<PathBuf>,
    /// Decode this `START:END` second range of the one given WAV instead of
    /// replaying it through the VAD; repeatable
    #[arg(long, value_name = "START:END")]
    range: Vec<Range>,
    /// WAV files to read
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

/// What a decoded window is counted as.
struct Counts {
    windows: usize,
    empty_padded: usize,
    empty_after_retry: usize,
    yeah: usize,
    words: usize,
}

fn normalise(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = Config::default();
    let decoding = match args.decoding {
        Method::Greedy => Decoding::GreedySearch,
        Method::Beam => Decoding::ModifiedBeamSearch {
            vocabulary: args.vocabulary.clone(),
            hotwords_score: args.hotwords_score,
        },
    };
    config.asr = Asr {
        model_dir: args.model_dir.unwrap_or(config.asr.model_dir),
        num_threads: config.asr.num_threads,
        model: Model::Parakeet { decoding },
    };
    let mut recognizer = Transcriber::new(&config.asr, REQUIRED_SAMPLE_RATE)?;
    recognizer.warm_up()?;

    if !args.range.is_empty() {
        let [file] = args.files.as_slice() else {
            bail!("--range takes exactly one WAV");
        };
        let (samples, _) = read_capture(file)?;
        let mut rows = vec![];
        for range in &args.range {
            let window = &samples[range.samples(samples.len())];
            let padded = recognizer.transcribe(window, TrailingSilence::Padded)?;
            let bare = recognizer.transcribe(window, TrailingSilence::Bare)?;
            rows.push(json!({
                "start": range.start, "end": range.end,
                "padded": padded, "bare": bare,
            }));
        }
        println!("{}", json!({"ranges": rows}));
        return Ok(());
    }

    let mut segmenter = SpeechSegmenter::new(&config.vad, REQUIRED_SAMPLE_RATE)?;
    let mut counts = Counts {
        windows: 0,
        empty_padded: 0,
        empty_after_retry: 0,
        yeah: 0,
        words: 0,
    };
    let mut per_file = vec![];
    for file in &args.files {
        let (samples, _) = read_capture(file)?;
        let segments = segmenter.split(&samples)?.segments;
        let mut lost = 0;
        let mut rescued = 0;
        for segment in &segments {
            let window = &samples[segment.window.clone()];
            let padded = recognizer.transcribe(window, TrailingSilence::Padded)?;
            counts.windows += 1;
            let text = if padded.trim().is_empty() {
                counts.empty_padded += 1;
                let bare = recognizer.transcribe(window, TrailingSilence::Bare)?;
                if bare.trim().is_empty() {
                    counts.empty_after_retry += 1;
                    lost += 1;
                } else {
                    rescued += 1;
                }
                bare
            } else {
                padded
            };
            let normal = normalise(&text);
            if normal == "yeah" {
                counts.yeah += 1;
            }
            counts.words += normal.split_whitespace().count();
        }
        if lost > 0 || rescued > 0 {
            per_file.push(json!({
                "file": file.file_name().context("filename")?.to_string_lossy(),
                "windows": segments.len(), "lost": lost, "rescued": rescued,
            }));
        }
    }
    println!(
        "{}",
        json!({
            "files": args.files.len(),
            "windows": counts.windows,
            "empty_padded": counts.empty_padded,
            "empty_after_retry": counts.empty_after_retry,
            "yeah_only": counts.yeah,
            "words": counts.words,
            "affected": per_file,
        })
    );
    Ok(())
}
