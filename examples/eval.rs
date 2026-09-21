//! Word-error-rate regression harness for `eval-samples/*.wav`.
//!
//! For every clip in `eval-samples/references.json` whose wav exists
//! locally, decodes it through the same [`Pipeline`] the `transcribe`
//! command and the daemon use, post-processes the text the same way
//! [`Processor`] does, and reports per-clip and aggregate WER by
//! word-level Levenshtein distance. See `docs/evaluation.md`.
//!
//! Usage:
//! ```sh
//! cargo run --release --example eval
//! cargo run --release --example eval -- --whole
//! cargo run --release --example eval -- --config PATH --model-dir DIR
//! ```
use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;
use spokenpad::{
    config::Config,
    core::{decode::Pipeline, text::Processor},
    shell::{
        inference::{Transcriber, load_segmenter},
        recorder::read_capture,
    },
};
use std::{path::PathBuf, time::Instant};

/// The long-clip regression check must decode well inside the clip's own
/// duration. Not a tight bound -- docs/asr.md measures ~14.5x real-time warm
/// and idle, degrading to roughly 3x under heavy load, i.e. ~13s for a 37s
/// clip. A 0.5x margin stays clear of that without going flaky on a busy
/// machine.
const LONG_CLIP_MARGIN: f64 = 0.5;

#[derive(Parser)]
#[command(about = "WER regression harness for eval-samples/*.wav")]
struct Args {
    /// Configuration file; must exist if given [default: $XDG_CONFIG_HOME/spokenpad/config.toml]
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory holding the ASR model, overriding asr.model_dir
    #[arg(long, value_name = "DIR")]
    model_dir: Option<PathBuf>,
    /// Directory with *.wav and references.json
    #[arg(long, value_name = "DIR", default_value = "eval-samples")]
    samples_dir: PathBuf,
    /// Decode each clip in one pass instead of VAD-segmented (the daemon's default path)
    #[arg(long)]
    whole: bool,
}

#[derive(Deserialize)]
struct References {
    samples: Vec<Reference>,
}
#[derive(Deserialize)]
struct Reference {
    file: String,
    duration_s: f64,
    verified: bool,
    reference: String,
    #[serde(default)]
    long_clip_check: bool,
}

// -- normalisation & WER -------------------------------------------------------

/// Casefold, replace punctuation with a space, collapse whitespace.
///
/// Apostrophes are kept (`let's` stays one token). Underscores are `\w` and
/// so are *not* stripped: `test_file` and `test file` are genuinely different
/// transcriptions here -- one symbolised spoken punctuation, one didn't.
fn normalize(text: &str) -> String {
    let punctuation = regex::Regex::new(r"[^\w\s']").unwrap();
    let whitespace = regex::Regex::new(r"\s+").unwrap();
    let lowered = text.to_lowercase();
    let stripped = punctuation.replace_all(&lowered, " ");
    whitespace.replace_all(&stripped, " ").trim().to_owned()
}

fn tokenize(text: &str) -> Vec<String> {
    normalize(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Levenshtein distance (substitution/insertion/deletion cost 1).
fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, item_a) in a.iter().enumerate() {
        let mut current = vec![0; b.len() + 1];
        current[0] = i + 1;
        for (j, item_b) in b.iter().enumerate() {
            let cost = usize::from(item_a != item_b);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        previous = current;
    }
    previous[b.len()]
}

fn word_error_rate(reference: &str, hypothesis: &str) -> f64 {
    let ref_tokens = tokenize(reference);
    let hyp_tokens = tokenize(hypothesis);
    if ref_tokens.is_empty() {
        return if hyp_tokens.is_empty() { 0.0 } else { 1.0 };
    }
    edit_distance(&ref_tokens, &hyp_tokens) as f64 / ref_tokens.len() as f64
}

// -- evaluation -----------------------------------------------------------------

struct SampleResult {
    file: String,
    verified: bool,
    duration_s: f64,
    reference: String,
    hypothesis: String,
    decode_seconds: f64,
    wer: f64,
}

struct LongClipCheck {
    file: String,
    threshold_s: f64,
    decode_seconds: f64,
    hypothesis: String,
}
impl LongClipCheck {
    fn passed(&self) -> bool {
        !self.hypothesis.trim().is_empty() && self.decode_seconds < self.threshold_s
    }
}

/// Corpus-level WER: total edit distance over all scored samples, divided by
/// total reference token count -- not a mean of per-sample percentages.
fn aggregate_wer(results: &[SampleResult]) -> f64 {
    let mut total_ref_tokens = 0usize;
    let mut total_edit = 0usize;
    for r in results {
        let ref_tokens = tokenize(&r.reference);
        total_edit += edit_distance(&ref_tokens, &tokenize(&r.hypothesis));
        total_ref_tokens += ref_tokens.len();
    }
    if total_ref_tokens == 0 {
        0.0
    } else {
        total_edit as f64 / total_ref_tokens as f64
    }
}

fn print_report(results: &[SampleResult], whole: bool) {
    println!(
        "decoding: {}",
        if whole {
            "whole-buffer"
        } else {
            "VAD-segmented"
        }
    );
    println!();
    let header = format!("{:<28} {:>7} {:>8} {:>7}", "file", "WER", "decode", "RTF");
    println!("{header}");
    println!("{}", "-".repeat(header.len()));
    for r in results {
        let rtf = if r.decode_seconds > 0.0 {
            r.duration_s / r.decode_seconds
        } else {
            f64::INFINITY
        };
        let tag = if r.verified { "" } else { " [UNVERIFIED ref]" };
        println!(
            "{:<28} {:>6.1}% {:>7.2}s {:>6.1}x{tag}",
            r.file,
            r.wer * 100.0,
            r.decode_seconds,
            rtf
        );
    }
    println!("{}", "-".repeat(header.len()));
    println!(
        "{:<28} {:>6.1}%",
        format!("AGGREGATE ({} samples)", results.len()),
        aggregate_wer(results) * 100.0
    );
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = Config::load(args.config.as_deref())?;
    if let Some(p) = args.model_dir {
        config.asr.model_dir = spokenpad::config::expand_path(&p)?;
    }
    config.validate()?;

    let references: References = serde_json::from_str(
        &std::fs::read_to_string(args.samples_dir.join("references.json"))
            .context("read references.json")?,
    )
    .context("parse references.json")?;

    eprintln!("loading model and warming up ...");
    let mut recognizer = Transcriber::new(&config.asr, config.audio.sample_rate)?;
    recognizer.warm_up()?;
    let segmenter = if args.whole {
        None
    } else {
        load_segmenter(&config.vad, config.audio.sample_rate)
    };
    let mut pipeline = Pipeline {
        recognizer,
        segmenter,
    };
    let processor = Processor::new(&config.text)?;

    let mut results = vec![];
    let mut long_clip = None;
    let mut skipped = 0;

    for r in &references.samples {
        let wav = args.samples_dir.join(&r.file);
        if !wav.is_file() {
            skipped += 1;
            continue;
        }
        let (samples, rate) = read_capture(&wav)?;
        anyhow::ensure!(
            rate == config.audio.sample_rate,
            "{}: {rate}Hz, expected {}Hz",
            r.file,
            config.audio.sample_rate
        );
        let wav_duration_s = samples.len() as f64 / f64::from(rate);
        if (wav_duration_s - r.duration_s).abs() > 1.0 {
            eprintln!(
                "warning: {}: references.json says duration_s={}, actual wav is {wav_duration_s:.1}s",
                r.file, r.duration_s
            );
        }

        let started = Instant::now();
        let raw = pipeline.decode(&samples, || false, |_| {})?;
        let decode_seconds = started.elapsed().as_secs_f64();
        let hypothesis = processor.process(&raw);

        if r.long_clip_check {
            long_clip = Some(LongClipCheck {
                file: r.file.clone(),
                threshold_s: wav_duration_s * LONG_CLIP_MARGIN,
                decode_seconds,
                hypothesis: hypothesis.clone(),
            });
        }
        results.push(SampleResult {
            file: r.file.clone(),
            verified: r.verified,
            duration_s: wav_duration_s,
            wer: word_error_rate(&r.reference, &hypothesis),
            reference: r.reference.clone(),
            hypothesis,
            decode_seconds,
        });
    }

    if skipped > 0 {
        eprintln!("skipped {skipped} sample(s) with no local wav");
    }
    anyhow::ensure!(!results.is_empty(), "no local wav files to evaluate");

    print_report(&results, args.whole);

    if let Some(lc) = &long_clip {
        let passed = lc.passed();
        let status = if passed { "PASS" } else { "FAIL" };
        println!(
            "\nLong-clip check: [{status}] {}: decoded in {:.2}s (threshold {:.1}s)",
            lc.file, lc.decode_seconds, lc.threshold_s
        );
        if !passed {
            println!("  hypothesis: {:?}", lc.hypothesis);
            anyhow::bail!("long-clip check failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_preserves_symbolized_underscores() {
        assert_eq!(normalize(" Test_file, RM-RF! "), "test_file rm rf");
    }

    #[test]
    fn normalization_keeps_apostrophes() {
        assert_eq!(normalize("Let's GO."), "let's go");
    }

    #[test]
    fn word_error_rate_handles_edits_and_empty_references() {
        assert_eq!(word_error_rate("one two", "one too"), 0.5);
        assert_eq!(word_error_rate("", ""), 0.0);
        assert_eq!(word_error_rate("", "word"), 1.0);
        assert_eq!(word_error_rate("a b c", "a b c"), 0.0);
    }

    #[test]
    fn edit_distance_matches_known_values() {
        let a = tokenize("a b c");
        let b = tokenize("a x c d");
        assert_eq!(edit_distance(&a, &b), 2);
    }

    #[test]
    fn aggregate_is_corpus_level_not_a_mean_of_percentages() {
        let make = |reference: &str, hypothesis: &str| SampleResult {
            file: String::new(),
            verified: true,
            duration_s: 1.0,
            wer: word_error_rate(reference, hypothesis),
            reference: reference.into(),
            hypothesis: hypothesis.into(),
            decode_seconds: 0.1,
        };
        // 1 error / 1 ref token (100%) and 0 errors / 9 ref tokens (0%);
        // corpus-level is 1/10, not the 50% a naive mean would give.
        let results = [
            make("a", "b"),
            make(
                "one two three four five six seven eight nine",
                "one two three four five six seven eight nine",
            ),
        ];
        assert!((aggregate_wer(&results) - 0.1).abs() < 1e-9);
    }
}
