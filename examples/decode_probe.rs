//! Decoder probe: how often does a decoding method lose a speech window?
//!
//! Two modes, both printing one JSON object on stdout:
//!
//! - *replay* (default): splits every given WAV with the VAD, decodes each
//!   window the way `Pipeline::transcribe_speech` does (one padded decode, a
//!   bare retry only when the first decode is empty) and counts the losses.
//! - *windows* (`--range`): decodes the named sample ranges of the given WAV
//!   twice, padded and bare, and prints both texts.
//! - *whole* (`--whole`): decodes every given WAV end to end, padded and
//!   bare, and prints both texts. One model load for the whole list, which is
//!   what makes a grid of constructed clips cheap to sweep.
//!
//! Replay prints counts only, never the text of a private recording;
//! `--range` and `--whole` print text, so point them at recordings you may
//! read out loud.
//!
//! ```sh
//! cargo run --release --example decode_probe -- --decoding beam ~/.local/state/spokenpad/audio/*.wav
//! cargo run --release --example decode_probe -- --decoding greedy --range 28.0:38.6 one.wav
//! cargo run --release --example decode_probe -- --decoding beam --whole clips/*.wav
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
    /// Decode every given WAV end to end and print its text, instead of
    /// replaying it through the VAD
    #[arg(long, conflicts_with = "range")]
    whole: bool,
    /// WAV files to read
    #[arg(required = true)]
    files: Vec<PathBuf>,
}

/// What the run does with the files it was given.
enum Mode {
    /// Named second ranges of the one file, printing text.
    Ranges(Vec<Range>),
    /// Every file end to end, printing text.
    Whole,
    /// Every file split by the VAD, printing counts only.
    Replay,
}
impl Args {
    fn mode(&self) -> Result<Mode> {
        match (self.range.as_slice(), self.whole) {
            ([], false) => Ok(Mode::Replay),
            ([], true) => Ok(Mode::Whole),
            (ranges, false) if self.files.len() == 1 => Ok(Mode::Ranges(ranges.to_vec())),
            _ => bail!("--range takes exactly one WAV and cannot be combined with --whole"),
        }
    }
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
        model_dir: args.model_dir.clone().unwrap_or(config.asr.model_dir),
        num_threads: config.asr.num_threads,
        model: Model::Parakeet { decoding },
    };
    let mode = args.mode()?;
    let mut recognizer = Transcriber::new(&config.asr, REQUIRED_SAMPLE_RATE)?;
    recognizer.warm_up()?;

    match mode {
        Mode::Ranges(ranges) => {
            let [file] = args.files.as_slice() else {
                bail!("--range takes exactly one WAV");
            };
            let (samples, _) = read_capture(file)?;
            let mut rows = vec![];
            for range in &ranges {
                let window = &samples[range.samples(samples.len())];
                rows.push(json!({
                    "start": range.start, "end": range.end,
                    "padded": recognizer.transcribe(window, TrailingSilence::Padded)?,
                    "bare": recognizer.transcribe(window, TrailingSilence::Bare)?,
                }));
            }
            println!("{}", json!({"ranges": rows}));
            return Ok(());
        }
        Mode::Whole => {
            let mut rows = vec![];
            for file in &args.files {
                let (samples, _) = read_capture(file)?;
                rows.push(json!({
                    "file": file.file_name().context("filename")?.to_string_lossy(),
                    "padded": recognizer.transcribe(&samples, TrailingSilence::Padded)?,
                    "bare": recognizer.transcribe(&samples, TrailingSilence::Bare)?,
                }));
            }
            println!("{}", json!({"clips": rows}));
            return Ok(());
        }
        Mode::Replay => {}
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
        let mut lost_windows = vec![];
        let mut rescued = 0;
        let mut yeah_windows = vec![];
        for segment in &segments {
            let window = &samples[segment.window.clone()];
            let padded = recognizer.transcribe(window, TrailingSilence::Padded)?;
            counts.windows += 1;
            let text = if padded.trim().is_empty() {
                counts.empty_padded += 1;
                let bare = recognizer.transcribe(window, TrailingSilence::Bare)?;
                if bare.trim().is_empty() {
                    counts.empty_after_retry += 1;
                    lost_windows.push(json!([segment.window.start, segment.window.end]));
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
                yeah_windows.push(json!([segment.window.start, segment.window.end]));
            }
            counts.words += normal.split_whitespace().count();
        }
        if !lost_windows.is_empty() || rescued > 0 || !yeah_windows.is_empty() {
            per_file.push(json!({
                "file": file.file_name().context("filename")?.to_string_lossy(),
                "windows": segments.len(), "rescued": rescued,
                "lost": lost_windows, "yeah": yeah_windows,
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
