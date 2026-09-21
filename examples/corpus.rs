//! Replays the local evaluation corpus through one spokenpad configuration.
//!
//! `examples/eval.rs` scores five committed clips: enough to notice a
//! regression, far too few to choose between two decoders. This runs the same
//! code over the whole local corpus (`eval-samples/local/references.json`,
//! built by `scripts/gladia-references.sh`) and reports, besides word error
//! rate, the failures WER hides: speech chunks that decode to nothing,
//! hypotheses that stop before the reference does, and the invented "Yeah."
//! See `docs/evaluation.md`.
//!
//! Two paths, because they fail differently:
//!
//! * `live` drives [`Worker`] as `shell/daemon.rs` does -- a preview tick every
//!   `preview.interval_ms` of audio, every settled chunk committed once, then
//!   the release decoding only the tail.
//! * `whole` decodes each capture in one pass with no segmenter, like
//!   `eval --whole`.
//!
//! Everything about the model comes from a config file, so another model, a
//! patched sherpa-onnx or another provider needs a config, not a code change.
//!
//! ```sh
//! cargo run --release --example=corpus -- --config eval.toml
//! cargo run --release --example=corpus -- --config eval.toml --path live \
//!     --trailing-silence-ms 0 --jsonl out.jsonl --label greedy-nopad
//! ```
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use spokenpad::{
    config::{Config, Decoding, Model},
    core::{
        decode::{Pipeline, Recognizer, Segment, Segmenter, TrailingSilence, Utterance, Worker},
        frames::Frames,
        text::Processor,
    },
    shell::{
        inference::{SpeechSegmenter, Transcriber},
        recorder::read_capture,
    },
};
use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum DecodePath {
    /// The daemon's own path: progressive commits, then the release tail.
    Live,
    /// One decode per capture, no VAD.
    Whole,
}
impl DecodePath {
    fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Whole => "whole",
        }
    }
}

#[derive(Parser)]
#[command(about = "Replay the local corpus through one spokenpad configuration")]
struct Args {
    /// Configuration file [default: $XDG_CONFIG_HOME/spokenpad/config.toml]
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Directory holding the ASR model, overriding asr.model_dir
    #[arg(long, value_name = "DIR")]
    model_dir: Option<PathBuf>,
    /// CPU threads per decode, overriding asr.num_threads
    #[arg(long, value_name = "N")]
    threads: Option<u16>,
    /// Corpus directory; needs a references.json
    #[arg(long, value_name = "DIR", default_value = "eval-samples/local")]
    corpus: PathBuf,
    /// Decode path to replay; repeat the flag for more [default: live, whole]
    #[arg(long, value_enum)]
    path: Vec<DecodePath>,
    /// Zeros appended before each decode, overriding the recognizer's second
    #[arg(long, value_name = "MS")]
    trailing_silence_ms: Option<u64>,
    /// Audio between preview ticks on the live path [default: preview.interval_ms]
    #[arg(long, value_name = "MS")]
    tick_ms: Option<u64>,
    /// Captures decoded at once; each job loads its own copy of the model
    #[arg(long, value_name = "N", default_value_t = 1)]
    jobs: usize,
    /// Score only captures whose reference is in this language; repeat for more
    #[arg(long, value_name = "CODE")]
    language: Vec<String>,
    /// Score only the first N captures of the corpus
    #[arg(long, value_name = "N")]
    limit: Option<usize>,
    /// Reference words a hypothesis may lose at its end before the file counts as one
    #[arg(long, value_name = "N", default_value_t = 5)]
    lost_tail: usize,
    /// Name for this run in the JSON lines
    #[arg(long, value_name = "NAME")]
    label: Option<String>,
    /// Append one JSON line per capture and a summary line to this file
    #[arg(long, value_name = "PATH")]
    jsonl: Option<PathBuf>,
    /// Put reference and hypothesis text in the JSON lines (private speech)
    #[arg(long)]
    with_text: bool,
    /// Decode the live preview too, as the daemon does; costs ~10x the time and
    /// cannot change a single word of the result
    #[arg(long)]
    previews: bool,
    /// Print every capture instead of the worst rows
    #[arg(long)]
    all: bool,
}

/// How one pass decodes, once the flags are resolved against the config.
#[derive(Clone, Copy)]
struct Plan {
    path: DecodePath,
    /// Zeros to append before each decode, or `None` to leave the recognizer
    /// its own second.
    padding: Option<usize>,
    /// Audio between preview ticks, in samples.
    tick: usize,
    /// `preview.max_seconds` in samples: past this the daemon stops previewing.
    max_tail: usize,
    previews: bool,
    rate: u32,
}
impl Plan {
    fn padding_ms(self) -> u64 {
        let samples = self.padding.unwrap_or(self.rate as usize);
        (samples as u64) * 1000 / u64::from(self.rate)
    }
    fn tick_ms(self) -> u64 {
        (self.tick as u64) * 1000 / u64::from(self.rate)
    }
}

// -- the corpus ----------------------------------------------------------------

#[derive(Deserialize)]
struct Corpus {
    samples: Vec<Entry>,
}
#[derive(Deserialize, Clone)]
struct Entry {
    file: String,
    path: PathBuf,
    reference: String,
    /// What the reference transcriber heard the capture in. Empty for a
    /// capture it found no speech in.
    #[serde(default)]
    languages: Vec<String>,
}
impl Entry {
    /// One language per capture, which is how the references are built.
    fn language(&self) -> &str {
        self.languages.first().map_or("--", String::as_str)
    }
}

// -- normalisation, WER and the lost tail --------------------------------------

/// Casefold, punctuation to spaces, collapse whitespace -- the normalisation
/// `examples/eval.rs` documents, so the two harnesses score alike.
fn tokenize(text: &str) -> Vec<String> {
    let punctuation = regex::Regex::new(r"[^\w\s']").unwrap();
    let whitespace = regex::Regex::new(r"\s+").unwrap();
    let lowered = text.to_lowercase();
    let stripped = punctuation.replace_all(&lowered, " ");
    whitespace
        .replace_all(&stripped, " ")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

struct Alignment {
    edits: usize,
    /// Reference words at the very end with nothing opposite them: the capture
    /// whose last sentence never arrived.
    lost_tail: usize,
}

/// Levenshtein distance over tokens, plus the length of the run of deletions
/// the optimal alignment ends with. Ties in the backtrace go to the diagonal,
/// so a word the recognizer got wrong counts as wrong rather than as lost.
fn align(reference: &[String], hypothesis: &[String]) -> Alignment {
    let (m, n) = (reference.len(), hypothesis.len());
    let mut d = vec![vec![0u32; n + 1]; m + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i as u32;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j as u32;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = u32::from(reference[i - 1] != hypothesis[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
        }
    }
    let (mut i, mut j, mut lost_tail) = (m, n, 0usize);
    while i > 0 {
        if j > 0 && d[i][j] == d[i - 1][j - 1] + u32::from(reference[i - 1] != hypothesis[j - 1]) {
            break;
        }
        if d[i][j] == d[i - 1][j] + 1 {
            i -= 1;
            lost_tail += 1;
        } else if j > 0 {
            j -= 1;
        } else {
            break;
        }
    }
    Alignment {
        edits: d[m][n] as usize,
        lost_tail,
    }
}

/// A commit that is nothing but the interjection beam search invents for clear
/// speech (k2-fsa/sherpa-onnx#3267).
fn is_yeah(text: &str) -> bool {
    let letters: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    letters.eq_ignore_ascii_case("yeah")
}

// -- the recognizer under measurement -------------------------------------------

#[derive(Default, Clone, Copy)]
struct Tally {
    decodes: usize,
    /// Speech windows whose padded decode returned nothing, so the pipeline
    /// decoded them once more without the trailing silence.
    empty_first: usize,
    /// Of those, the ones the bare decode returned nothing for either.
    empty_after_retry: usize,
}
impl std::ops::AddAssign for Tally {
    fn add_assign(&mut self, other: Self) {
        self.decodes += other.decodes;
        self.empty_first += other.empty_first;
        self.empty_after_retry += other.empty_after_retry;
    }
}

/// The recognizer as the harness needs to see it: every decode is counted, and
/// the second of zeros `TrailingSilence::Padded` appends can be replaced with
/// another length. That length is a measurement, not a setting -- there is
/// deliberately no config key for it.
///
/// With `padding` set, this pads here and asks the recognizer for `Bare`, which
/// is what `shell::inference` does for a window it decodes in one piece. A
/// Whisper window over 28 s is cut into pieces *inside* the recognizer, which
/// pads each piece; padding out here puts the zeros after the last piece only.
/// So leave `padding` unset when measuring Whisper.
struct Probe<R> {
    inner: R,
    padding: Option<usize>,
    buffer: Vec<f32>,
    /// Window length of the last padded decode that came back empty. A bare
    /// decode of a window that long is the pipeline's one retry.
    retried: Option<usize>,
    tally: Tally,
}
impl<R: Recognizer> Probe<R> {
    fn new(inner: R, padding: Option<usize>) -> Self {
        Self {
            inner,
            padding,
            buffer: vec![],
            retried: None,
            tally: Tally::default(),
        }
    }
    /// The counts since the last call, and starts counting again.
    fn take(&mut self) -> Tally {
        std::mem::take(&mut self.tally)
    }
}
impl<R: Recognizer> Recognizer for Probe<R> {
    fn transcribe(&mut self, samples: &[f32], trailing: TrailingSilence) -> Result<String> {
        self.tally.decodes += 1;
        let text = match (self.padding, trailing) {
            (Some(pad), TrailingSilence::Padded) => {
                self.buffer.clear();
                self.buffer.extend_from_slice(samples);
                self.buffer.resize(samples.len() + pad, 0.0);
                self.inner.transcribe(&self.buffer, TrailingSilence::Bare)?
            }
            _ => self.inner.transcribe(samples, trailing)?,
        };
        let empty = text.trim().is_empty();
        match trailing {
            TrailingSilence::Padded => self.retried = empty.then_some(samples.len()),
            TrailingSilence::Bare => {
                if self.retried == Some(samples.len()) {
                    self.tally.empty_first += 1;
                    self.tally.empty_after_retry += usize::from(empty);
                }
                self.retried = None;
            }
        }
        Ok(text)
    }
}

/// The segmenter a preview tick sees. The preview is cosmetic -- its text can
/// never reach the file, and `Worker::tick` neither commits it nor advances
/// the offset by it -- so with `hide` set the trailing unsettled chunk is
/// dropped before the tick sees it. The committed text is identical word for
/// word, and the replay saves the great majority of its decodes: a tick then
/// decodes only what it commits. `Worker::finish` must see that tail, so
/// [`replay_live`] clears the flag before it.
struct TailHider<S> {
    inner: S,
    hide: bool,
}
impl<S: Segmenter> Segmenter for TailHider<S> {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>> {
        let mut segments = self.inner.split(samples)?;
        if self.hide {
            while segments.last().is_some_and(|s| !s.settled) {
                segments.pop();
            }
        }
        Ok(segments)
    }
}

type Engine = Worker<Probe<Transcriber>, TailHider<SpeechSegmenter>>;

fn engine(config: &Config, padding: Option<usize>, path: DecodePath) -> Result<Engine> {
    let rate = config.audio.sample_rate;
    let mut recognizer = Transcriber::new(&config.asr, rate)?;
    // Warm up before the probe, so lazy native initialisation is not counted.
    recognizer.warm_up()?;
    let segmenter = match path {
        DecodePath::Live => Some(TailHider {
            inner: SpeechSegmenter::new(&config.vad, rate)?,
            hide: false,
        }),
        DecodePath::Whole => None,
    };
    Ok(Worker::new(Pipeline {
        recognizer: Probe::new(recognizer, padding),
        segmenter,
    }))
}

// -- the two paths ----------------------------------------------------------------

/// Replays one capture as the daemon drives it: a preview tick every `tick`
/// samples, each settled chunk committed once, then the release decode of the
/// tail. Returns the committed texts in order.
///
/// The clock is the capture's own, not the wall clock. The daemon ticks on wall
/// time, so a loaded machine ticks over a longer stretch of audio and lands on
/// different chunk boundaries. Ticking on audio time makes a replay reproducible
/// and models the idle machine, where a decode runs ~14x faster than real time.
fn replay_live(
    worker: &mut Engine,
    samples: &[f32],
    tick: usize,
    max_tail: usize,
    previews: bool,
) -> Result<Vec<String>> {
    if let Some(segmenter) = worker.pipeline.segmenter.as_mut() {
        segmenter.hide = !previews;
    }
    let utterance = Utterance::new(1);
    let mut hint = Frames::ZERO;
    let mut landed = vec![];
    let mut texts = vec![];
    let mut available = tick;
    while available < samples.len() {
        // Past preview.max_seconds of uncommitted audio the daemon pauses
        // previews and tries again one interval later.
        if available - hint.get() <= max_tail {
            worker.tick(&samples[hint.get()..available], hint, &utterance, |c| {
                landed.push((c.text, c.through));
            })?;
            for (text, through) in landed.drain(..) {
                hint = hint.max(through);
                texts.push(text);
            }
        }
        available += tick;
    }
    utterance.release();
    // The release decodes the open tail, so the tail must be visible again.
    if let Some(segmenter) = worker.pipeline.segmenter.as_mut() {
        segmenter.hide = false;
    }
    worker.finish(samples, &utterance, |c| texts.push(c.text))?;
    Ok(texts)
}

fn decode_whole(worker: &mut Engine, samples: &[f32]) -> Result<Vec<String>> {
    let mut texts = vec![];
    worker
        .pipeline
        .decode(samples, || false, |text| texts.push(text))?;
    Ok(texts)
}

// -- results ------------------------------------------------------------------------

struct Row {
    file: String,
    language: String,
    duration_s: f64,
    reference: String,
    hypothesis: String,
    ref_words: usize,
    hyp_words: usize,
    edits: usize,
    /// `None` when the reference is empty: nothing to divide by, and a capture
    /// nobody spoke in belongs in its own count.
    wer: Option<f64>,
    lost_tail: usize,
    yeah: usize,
    tally: Tally,
    decode_seconds: f64,
}

fn run_pass(config: &Config, entries: &[Entry], plan: Plan, jobs: usize) -> Result<Vec<Row>> {
    let processor = Processor::new(&config.text)?;
    let rate = config.audio.sample_rate;
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<(usize, Row)>> = Mutex::new(vec![]);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = vec![];
        for _ in 0..jobs {
            let (next, rows, processor) = (&next, &rows, &processor);
            handles.push(scope.spawn(move || -> Result<()> {
                let mut worker = engine(config, plan.padding, plan.path)?;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = entries.get(i) else {
                        return Ok(());
                    };
                    let (samples, wav_rate) = read_capture(&entry.path)?;
                    ensure!(
                        wav_rate == rate,
                        "{}: {wav_rate}Hz, expected {rate}Hz",
                        entry.file
                    );
                    worker.pipeline.recognizer.take();
                    let started = Instant::now();
                    let texts = match plan.path {
                        DecodePath::Live => replay_live(
                            &mut worker,
                            &samples,
                            plan.tick,
                            plan.max_tail,
                            plan.previews,
                        )?,
                        DecodePath::Whole => decode_whole(&mut worker, &samples)?,
                    };
                    let decode_seconds = started.elapsed().as_secs_f64();
                    let yeah = texts.iter().filter(|t| is_yeah(t)).count();
                    let hypothesis = processor.process(
                        &texts
                            .iter()
                            .filter(|t| !t.trim().is_empty())
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                    let reference = tokenize(&entry.reference);
                    let hyp = tokenize(&hypothesis);
                    let alignment = align(&reference, &hyp);
                    rows.lock().expect("results lock").push((
                        i,
                        Row {
                            file: entry.file.clone(),
                            language: entry.language().to_owned(),
                            duration_s: Frames(samples.len()).seconds(rate),
                            reference: entry.reference.clone(),
                            hypothesis,
                            ref_words: reference.len(),
                            hyp_words: hyp.len(),
                            edits: alignment.edits,
                            wer: (!reference.is_empty())
                                .then(|| alignment.edits as f64 / reference.len() as f64),
                            lost_tail: alignment.lost_tail,
                            yeah,
                            tally: worker.pipeline.recognizer.take(),
                            decode_seconds,
                        },
                    ));
                }
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => bail!("a decode thread panicked"),
            }
        }
        Ok(())
    })?;
    let mut rows = rows.into_inner().expect("results lock");
    rows.sort_by_key(|(i, _)| *i);
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

// -- reporting -------------------------------------------------------------------------

struct Summary {
    files: usize,
    scored: usize,
    empty_reference: usize,
    /// Captures with an empty reference that the recognizer put words into.
    invented: usize,
    ref_words: usize,
    hyp_words: usize,
    edits: usize,
    wer: f64,
    lost_tail_files: usize,
    lost_tail_words: usize,
    yeah: usize,
    tally: Tally,
    audio_seconds: f64,
    decode_seconds: f64,
}

fn summarize(rows: &[Row], lost_tail: usize) -> Summary {
    let mut s = Summary {
        files: rows.len(),
        scored: 0,
        empty_reference: 0,
        invented: 0,
        ref_words: 0,
        hyp_words: 0,
        edits: 0,
        wer: 0.0,
        lost_tail_files: 0,
        lost_tail_words: 0,
        yeah: 0,
        tally: Tally::default(),
        audio_seconds: 0.0,
        decode_seconds: 0.0,
    };
    for row in rows {
        s.audio_seconds += row.duration_s;
        s.decode_seconds += row.decode_seconds;
        s.hyp_words += row.hyp_words;
        s.yeah += row.yeah;
        s.tally += row.tally;
        if row.ref_words == 0 {
            s.empty_reference += 1;
            s.invented += usize::from(row.hyp_words > 0);
            continue;
        }
        s.scored += 1;
        s.ref_words += row.ref_words;
        s.edits += row.edits;
        s.lost_tail_words += row.lost_tail;
        s.lost_tail_files += usize::from(row.lost_tail > lost_tail);
    }
    if s.ref_words > 0 {
        s.wer = s.edits as f64 / s.ref_words as f64;
    }
    s
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

fn print_report(rows: &[Row], summary: &Summary, args: &Args, plan: Plan, wall: f64) {
    let header = format!(
        "{:<34} {:>4} {:>6} {:>5} {:>5} {:>7} {:>5} {:>4} {:>4} {:>5} {:>7}",
        "file", "lang", "dur", "ref", "hyp", "WER", "lost", "e1", "e2", "yeah", "decode"
    );
    let mut shown: Vec<&Row> = rows.iter().collect();
    if !args.all {
        shown.sort_by(|a, b| {
            b.wer
                .unwrap_or(0.0)
                .total_cmp(&a.wer.unwrap_or(0.0))
                .then(b.lost_tail.cmp(&a.lost_tail))
        });
        shown.truncate(15);
    }
    println!("\n== path: {} ==", plan.path.name());
    println!(
        "{}  (e1 = speech chunks empty on the first decode, e2 = still empty after the bare retry)",
        if args.all {
            "every capture, in corpus order"
        } else {
            "the 15 worst captures by WER; --all prints every row"
        }
    );
    println!("{header}");
    println!("{}", "-".repeat(header.len()));
    for row in shown {
        let wer = match row.wer {
            Some(w) => format!("{:.1}%", w * 100.0),
            None => "n/a".into(),
        };
        println!(
            "{:<34} {:>4} {:>5.0}s {:>5} {:>5} {:>7} {:>5} {:>4} {:>4} {:>5} {:>6.2}s",
            row.file,
            row.language,
            row.duration_s,
            row.ref_words,
            row.hyp_words,
            wer,
            row.lost_tail,
            row.tally.empty_first,
            row.tally.empty_after_retry,
            row.yeah,
            row.decode_seconds
        );
    }
    println!("{}", "-".repeat(header.len()));

    let mut wers: Vec<f64> = rows.iter().filter_map(|r| r.wer).collect();
    wers.sort_by(f64::total_cmp);
    let buckets = [0.05, 0.10, 0.20, 0.40, f64::INFINITY];
    let labels = ["<5%", "<10%", "<20%", "<40%", ">=40%"];
    let mut counts = [0usize; 5];
    for w in &wers {
        for (i, bucket) in buckets.iter().enumerate() {
            if w < bucket {
                counts[i] += 1;
                break;
            }
        }
    }
    println!(
        "aggregate WER {:.1}%  ({} edits / {} reference words over {} captures)",
        summary.wer * 100.0,
        summary.edits,
        summary.ref_words,
        summary.scored
    );
    println!(
        "per-file WER  median {:.1}%  p75 {:.1}%  p90 {:.1}%  max {:.1}%   [{}]",
        quantile(&wers, 0.5) * 100.0,
        quantile(&wers, 0.75) * 100.0,
        quantile(&wers, 0.9) * 100.0,
        wers.last().copied().unwrap_or(0.0) * 100.0,
        labels
            .iter()
            .zip(counts)
            .map(|(l, c)| format!("{l} {c}"))
            .collect::<Vec<_>>()
            .join("  ")
    );
    println!(
        "hypothesis words {} against {} reference words",
        summary.hyp_words, summary.ref_words
    );
    let mut languages: Vec<&str> = rows.iter().map(|r| r.language.as_str()).collect();
    languages.sort_unstable();
    languages.dedup();
    if languages.len() > 1 {
        let per_language: Vec<String> = languages
            .iter()
            .map(|language| {
                let part: Vec<&Row> = rows.iter().filter(|r| r.language == *language).collect();
                let (edits, words) = part.iter().fold((0, 0), |(e, w), r| {
                    (
                        e + if r.ref_words > 0 { r.edits } else { 0 },
                        w + r.ref_words,
                    )
                });
                let name = if language.is_empty() { "--" } else { language };
                match words {
                    0 => format!("{name} {} captures, no reference words", part.len()),
                    _ => format!(
                        "{name} {:.1}% ({} captures, {words} words)",
                        edits as f64 / words as f64 * 100.0,
                        part.len()
                    ),
                }
            })
            .collect();
        println!("by reference language  {}", per_language.join("   "));
    }
    println!(
        "speech chunks empty on the first decode {}, still empty after the bare retry {}  ({} decodes)",
        summary.tally.empty_first, summary.tally.empty_after_retry, summary.tally.decodes
    );
    println!(
        "captures losing more than {} reference words at the end: {}  ({} words lost at the ends in all)",
        args.lost_tail, summary.lost_tail_files, summary.lost_tail_words
    );
    println!("commits that are nothing but \"Yeah.\": {}", summary.yeah);
    println!(
        "captures with an empty reference: {}  ({} of them decoded to words anyway)",
        summary.empty_reference, summary.invented
    );
    println!(
        "audio {:.1} min, decode {:.0}s summed over {} job(s), {:.0}s wall, {:.1}x real time{}",
        summary.audio_seconds / 60.0,
        summary.decode_seconds,
        args.jobs,
        wall,
        summary.audio_seconds / wall.max(f64::MIN_POSITIVE),
        match (plan.path, plan.previews) {
            (DecodePath::Live, false) => " -- committed decodes only, no previews",
            _ => "",
        }
    );
}

/// Family and decoding method, as the report and the JSON lines name them.
fn describe(config: &Config) -> (&'static str, String) {
    match &config.asr.model {
        Model::Parakeet { decoding } => (
            "parakeet",
            match decoding {
                Decoding::GreedySearch => "greedy_search".to_owned(),
                Decoding::ModifiedBeamSearch {
                    vocabulary,
                    hotwords_score,
                } => format!(
                    "modified_beam_search ({} hotword phrases at {hotwords_score})",
                    vocabulary.len()
                ),
            },
        ),
        Model::Whisper { .. } => ("whisper", "greedy_search".to_owned()),
        Model::SenseVoice { .. } => ("sense_voice", "greedy_search".to_owned()),
    }
}

fn write_jsonl(
    out: &mut impl Write,
    rows: &[Row],
    summary: &Summary,
    args: &Args,
    config: &Config,
    plan: Plan,
    wall: f64,
) -> Result<()> {
    let label = args
        .label
        .clone()
        .unwrap_or_else(|| plan.path.name().to_owned());
    let (family, decoding) = describe(config);
    for row in rows {
        let mut value = serde_json::json!({
            "kind": "sample", "label": label, "path": plan.path.name(),
            "file": row.file, "language": row.language, "duration_s": row.duration_s,
            "ref_words": row.ref_words, "hyp_words": row.hyp_words,
            "edits": row.edits, "wer": row.wer, "lost_tail": row.lost_tail,
            "empty_first": row.tally.empty_first,
            "empty_after_retry": row.tally.empty_after_retry,
            "decodes": row.tally.decodes, "yeah": row.yeah,
            "decode_seconds": row.decode_seconds,
        });
        if args.with_text {
            value["reference"] = row.reference.clone().into();
            value["hypothesis"] = row.hypothesis.clone().into();
        }
        writeln!(out, "{value}")?;
    }
    writeln!(
        out,
        "{}",
        serde_json::json!({
            "kind": "summary", "label": label, "path": plan.path.name(),
            "family": family, "decoding": decoding,
            "model_dir": config.asr.model_dir, "num_threads": config.asr.num_threads,
            "sherpa_onnx": sherpa_onnx::version(),
            "trailing_silence_ms": plan.padding_ms(), "tick_ms": plan.tick_ms(),
            "previews": plan.previews, "jobs": args.jobs,
            "files": summary.files, "scored": summary.scored,
            "empty_reference": summary.empty_reference, "invented": summary.invented,
            "ref_words": summary.ref_words, "hyp_words": summary.hyp_words,
            "edits": summary.edits, "wer": summary.wer,
            "empty_first": summary.tally.empty_first,
            "empty_after_retry": summary.tally.empty_after_retry,
            "decodes": summary.tally.decodes, "yeah": summary.yeah,
            "lost_tail_threshold": args.lost_tail,
            "lost_tail_files": summary.lost_tail_files,
            "lost_tail_words": summary.lost_tail_words,
            "audio_seconds": summary.audio_seconds,
            "decode_seconds": summary.decode_seconds, "wall_seconds": wall,
        })
    )?;
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = Config::load(args.config.as_deref())?;
    if let Some(dir) = &args.model_dir {
        config.asr.model_dir = spokenpad::config::expand_path(dir)?;
    }
    if let Some(threads) = args.threads {
        config.asr.num_threads = threads;
    }
    config.validate()?;
    ensure!(args.jobs >= 1, "--jobs must be at least 1");

    let index = args.corpus.join("references.json");
    let corpus: Corpus =
        serde_json::from_str(&std::fs::read_to_string(&index).with_context(|| {
            format!(
                "read {} (scripts/gladia-references.sh builds it)",
                index.display()
            )
        })?)
        .with_context(|| format!("parse {}", index.display()))?;
    let mut entries: Vec<Entry> = corpus
        .samples
        .into_iter()
        .filter(|e| e.path.is_file())
        .filter(|e| args.language.is_empty() || args.language.iter().any(|l| l == e.language()))
        .collect();
    let dropped = args.limit.map_or(0, |n| entries.len().saturating_sub(n));
    if let Some(n) = args.limit {
        entries.truncate(n);
    }
    ensure!(
        !entries.is_empty(),
        "no local wav files in {}",
        index.display()
    );

    let rate = config.audio.sample_rate;
    let tick =
        ((args.tick_ms.unwrap_or(config.preview.interval_ms)) * u64::from(rate) / 1000) as usize;
    ensure!(tick > 0, "--tick-ms must be at least 1");
    let plan = |path| Plan {
        path,
        padding: args
            .trailing_silence_ms
            .map(|ms| (ms * u64::from(rate) / 1000) as usize),
        tick,
        max_tail: (config.preview.max_seconds * f64::from(rate)) as usize,
        previews: args.previews,
        rate,
    };

    let (family, decoding) = describe(&config);
    println!(
        "corpus {} ({} captures{})",
        args.corpus.display(),
        entries.len(),
        if dropped > 0 {
            format!(", {dropped} left out by --limit")
        } else {
            String::new()
        },
    );
    println!(
        "model {} [{family}, {decoding}], sherpa-onnx {}, {} threads",
        config.asr.model_dir.display(),
        sherpa_onnx::version(),
        config.asr.num_threads,
    );
    let shape = plan(DecodePath::Live);
    println!(
        "trailing silence {} ms, preview tick {} ms of audio, previews {}, {} job(s)",
        shape.padding_ms(),
        shape.tick_ms(),
        if args.previews { "decoded" } else { "skipped" },
        args.jobs
    );

    let mut jsonl = args
        .jsonl
        .as_ref()
        .map(|p| -> Result<_> {
            Ok(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .with_context(|| format!("open {}", p.display()))?,
            ))
        })
        .transpose()?;

    let mut paths = args.path.clone();
    if paths.is_empty() {
        paths = vec![DecodePath::Live, DecodePath::Whole];
    }
    paths.dedup();
    for path in paths {
        eprintln!("loading the model for the {} path ...", path.name());
        let started = Instant::now();
        let rows = run_pass(&config, &entries, plan(path), args.jobs)?;
        let wall = started.elapsed().as_secs_f64();
        let summary = summarize(&rows, args.lost_tail);
        print_report(&rows, &summary, &args, plan(path), wall);
        if let Some(out) = &mut jsonl {
            write_jsonl(out, &rows, &summary, &args, &config, plan(path), wall)?;
        }
    }
    if let Some(mut out) = jsonl {
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenization_matches_the_eval_harness() {
        assert_eq!(tokenize(" Test_file, RM-RF! "), ["test_file", "rm", "rf"]);
        assert_eq!(tokenize("Let's GO."), ["let's", "go"]);
    }

    #[test]
    fn alignment_counts_edits_and_the_trailing_deletions() {
        let reference = tokenize("one two three four five");
        assert_eq!(
            align(&reference, &tokenize("one two three four five")).lost_tail,
            0
        );
        let cut = align(&reference, &tokenize("one two three"));
        assert_eq!((cut.edits, cut.lost_tail), (2, 2));
        // A word the recognizer got wrong is wrong, not lost.
        let wrong = align(&reference, &tokenize("one two three four fife"));
        assert_eq!((wrong.edits, wrong.lost_tail), (1, 0));
        // Words lost in the middle are not a lost tail.
        let middle = align(&reference, &tokenize("one five"));
        assert_eq!((middle.edits, middle.lost_tail), (3, 0));
        // Nothing came back at all.
        let nothing = align(&reference, &[]);
        assert_eq!((nothing.edits, nothing.lost_tail), (5, 5));
    }

    #[test]
    fn yeah_is_recognized_whatever_the_punctuation() {
        assert!(is_yeah("Yeah."));
        assert!(is_yeah(" yeah "));
        assert!(!is_yeah("Yeah, right."));
        assert!(!is_yeah(""));
    }

    #[test]
    fn the_probe_counts_the_retry_and_repads() {
        struct Deaf {
            lengths: Vec<usize>,
        }
        impl Recognizer for Deaf {
            fn transcribe(&mut self, samples: &[f32], _: TrailingSilence) -> Result<String> {
                self.lengths.push(samples.len());
                Ok(String::new())
            }
        }
        let mut probe = Probe::new(Deaf { lengths: vec![] }, Some(3));
        probe
            .transcribe(&[1.0; 4], TrailingSilence::Padded)
            .unwrap();
        probe.transcribe(&[1.0; 4], TrailingSilence::Bare).unwrap();
        assert_eq!(
            probe.inner.lengths,
            [7, 4],
            "padded out here, bare passed through"
        );
        let tally = probe.take();
        assert_eq!(
            (tally.decodes, tally.empty_first, tally.empty_after_retry),
            (2, 1, 1)
        );
        assert_eq!(probe.take().decodes, 0, "take resets the counts");
    }

    #[test]
    fn a_padded_decode_that_returned_text_is_not_a_retry() {
        struct Heard;
        impl Recognizer for Heard {
            fn transcribe(&mut self, _: &[f32], _: TrailingSilence) -> Result<String> {
                Ok("words".into())
            }
        }
        let mut probe = Probe::new(Heard, None);
        probe
            .transcribe(&[1.0; 4], TrailingSilence::Padded)
            .unwrap();
        probe.transcribe(&[1.0; 4], TrailingSilence::Bare).unwrap();
        let tally = probe.take();
        assert_eq!((tally.empty_first, tally.empty_after_retry), (0, 0));
    }

    #[test]
    fn hiding_the_open_tail_drops_only_the_unsettled_end() {
        struct Three;
        impl Segmenter for Three {
            fn split(&mut self, _: &[f32]) -> Result<Vec<Segment>> {
                Ok([(0..4, true), (4..8, true), (8..10, false)]
                    .into_iter()
                    .map(|(window, settled)| Segment {
                        speech_end: window.end,
                        window,
                        settled,
                    })
                    .collect())
            }
        }
        let mut hider = TailHider {
            inner: Three,
            hide: false,
        };
        assert_eq!(hider.split(&[]).unwrap().len(), 3);
        hider.hide = true;
        let kept = hider.split(&[]).unwrap();
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|s| s.settled));
    }

    fn row(ref_words: usize, hyp_words: usize, edits: usize, lost_tail: usize) -> Row {
        Row {
            file: String::new(),
            language: "en".into(),
            duration_s: 1.0,
            reference: String::new(),
            hypothesis: String::new(),
            ref_words,
            hyp_words,
            edits,
            wer: (ref_words > 0).then(|| edits as f64 / ref_words as f64),
            lost_tail,
            yeah: 0,
            tally: Tally::default(),
            decode_seconds: 0.1,
        }
    }

    #[test]
    fn aggregate_is_corpus_level_not_a_mean_of_percentages() {
        // 1 error over 1 word (100%) and 0 over 9 (0%): corpus-level is 10%,
        // not the 50% a mean of the two percentages would give.
        let summary = summarize(&[row(1, 1, 1, 0), row(9, 9, 0, 0)], 5);
        assert!((summary.wer - 0.1).abs() < 1e-9);
        assert_eq!(summary.scored, 2);
    }

    #[test]
    fn an_empty_reference_is_counted_apart_from_the_word_error_rate() {
        let summary = summarize(&[row(0, 2, 2, 0)], 5);
        assert_eq!(
            (summary.scored, summary.empty_reference, summary.invented),
            (0, 1, 1)
        );
        assert_eq!(summary.wer, 0.0, "a capture nobody spoke in has no WER");
    }

    #[test]
    fn only_a_tail_longer_than_the_threshold_counts_as_a_lost_tail() {
        let summary = summarize(&[row(20, 15, 5, 5), row(20, 14, 6, 6)], 5);
        assert_eq!(summary.lost_tail_files, 1);
        assert_eq!(summary.lost_tail_words, 11);
    }
}
