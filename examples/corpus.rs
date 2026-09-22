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
//! * `live` drives [`Worker`] as `shell/daemon` does -- a tick every
//!   `preview.interval_seconds` of audio over the audio since the committed offset,
//!   bounded by `preview.max_seconds`, every settled chunk committed once, then
//!   the release decoding only what is still held.
//! * `whole` decodes each capture in one pass with no detector, like
//!   `eval --whole`.
//!
//! Everything about the model comes from a config file, so another model, a
//! patched sherpa-onnx or another provider needs a config, not a code change.
//!
//! ```sh
//! cargo run --release --example=corpus -- --config eval.toml
//! cargo run --release --example=corpus -- --config eval.toml --path live \
//!     --trailing-silence-ms 0 --jsonl out.jsonl --label greedy-nopad
//! # 28 captures picked for their failures: iterate here, gate on the corpus
//! cargo run --release --example=corpus -- --config eval.toml --path live --subset dev
//! ```
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use spokenpad::{
    config::{Config, Decoding},
    core::{
        decode::{Pipeline, Recognizer, TickKind, TrailingSilence, Utterance, Worker},
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
    path::{Path, PathBuf},
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
    /// Audio between preview ticks on the live path [default: preview.interval_seconds]
    #[arg(long, value_name = "MS")]
    tick_ms: Option<u64>,
    /// Captures decoded at once; each job loads its own copy of the model
    #[arg(long, value_name = "N", default_value_t = 1)]
    jobs: usize,
    /// Score only captures whose reference is in this language; repeat for more
    #[arg(long, value_name = "CODE")]
    language: Vec<String>,
    /// Score only captures whose hand-checked `spoken` language is this, `mixed`
    /// for captures holding both; repeat for more
    #[arg(long, value_name = "CODE")]
    spoken: Vec<String>,
    /// Score only the captures listed in <corpus>/subsets/NAME.txt
    #[arg(long, value_name = "NAME", conflicts_with = "ids")]
    subset: Option<String>,
    /// Score only the captures listed in this file: one wav file name per line,
    /// `#` starts a comment
    #[arg(long, value_name = "PATH")]
    ids: Option<PathBuf>,
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
    /// The wav, relative to the dataset directory in a frozen dataset and
    /// absolute in an index that points at recordings where they were made.
    path: PathBuf,
    reference: String,
    /// What the reference transcriber heard the capture in. Empty for a
    /// capture it found no speech in.
    #[serde(default)]
    languages: Vec<String>,
    /// The language actually spoken, checked by hand: a language code,
    /// `mixed` when the capture holds both languages (its reference is then
    /// right about one of them at most), `none` when nobody spoke. Absent from
    /// an index nobody has checked.
    #[serde(default)]
    spoken: Option<String>,
    /// Of the wav's bytes. A dataset carries it so a replay can prove it read
    /// the audio the references were made from.
    #[serde(default)]
    sha256: Option<String>,
}
impl Entry {
    /// One language per capture, which is how the references are built.
    fn language(&self) -> &str {
        self.languages.first().map_or("--", String::as_str)
    }
    fn spoken(&self) -> &str {
        self.spoken.as_deref().unwrap_or("--")
    }
    /// Where the wav is now: a relative `path` belongs to the dataset that
    /// names it, an absolute one speaks for itself.
    fn wav(&self, corpus: &Path) -> PathBuf {
        corpus.join(&self.path)
    }
}

/// Hex sha256 of a file's bytes.
fn digest(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The capture file names a subset lists: one per line, `#` to the end of a
/// line is a comment, blank lines are skipped.
fn parse_ids(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|id| !id.is_empty())
        .collect()
}

/// The entries a subset names, in corpus order. A name the corpus does not
/// hold is an error: a subset that silently shrinks measures something else.
fn select(entries: Vec<Entry>, ids: &[&str]) -> Result<Vec<Entry>> {
    let unknown: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| !entries.iter().any(|e| e.file == *id))
        .collect();
    ensure!(
        unknown.is_empty(),
        "the subset names captures the corpus does not hold: {}",
        unknown.join(", ")
    );
    Ok(entries
        .into_iter()
        .filter(|e| ids.contains(&e.file.as_str()))
        .collect())
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

/// Longest repeat a chunk seam is examined for. A trailing pad of 0.5 s cannot
/// carry more than a word or two of the next chunk's speech.
const SEAM_WINDOW: usize = 4;

/// Words that a chunk seam wrote twice: the tail of one committed chunk
/// repeated at the head of the next.
///
/// This is what an overlap between two decode windows looks like in the file.
/// Lead padding is clamped at the previous chunk's speech end so it cannot
/// reach back into committed speech, but a chunk's *trailing* pad still reaches
/// forward into the next chunk's speech, and nothing clamps that.
#[derive(Default, Clone, Copy)]
struct Seams {
    /// Seams where the next chunk began with words the previous one ended with.
    repeated: usize,
    /// Words repeated over all of them.
    words: usize,
}
fn seam_repeats(texts: &[String]) -> Seams {
    let chunks: Vec<Vec<String>> = texts
        .iter()
        .map(|t| tokenize(t))
        .filter(|t| !t.is_empty())
        .collect();
    let mut seams = Seams::default();
    for pair in chunks.windows(2) {
        let (before, after) = (&pair[0], &pair[1]);
        let overlap = (1..=SEAM_WINDOW.min(before.len()).min(after.len()))
            .rev()
            .find(|k| before[before.len() - k..] == after[..*k])
            .unwrap_or(0);
        if overlap > 0 {
            seams.repeated += 1;
            seams.words += overlap;
        }
    }
    seams
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
/// is what `shell::inference` does for a window it decodes in one piece.
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

type Engine = Worker<Probe<Transcriber>, SpeechSegmenter>;

fn engine(config: &Config, padding: Option<usize>) -> Result<Engine> {
    let rate = config.audio.sample_rate;
    let mut recognizer = Transcriber::new(&config.asr, rate)?;
    // Warm up before the probe, so lazy native initialisation is not counted.
    recognizer.warm_up()?;
    Ok(Worker::new(
        Pipeline {
            recognizer: Probe::new(recognizer, padding),
            segmenter: SpeechSegmenter::new(&config.vad, rate)?,
        },
        config.preview.window(rate),
    ))
}

// -- the two paths ----------------------------------------------------------------

/// Replays one capture through `shell/daemon`'s own loop: a tick every
/// `tick` samples over the audio since the committed offset, bounded by
/// `preview.max_seconds`, each settled chunk committed once, then the release
/// decoding only the audio still held. Returns the committed texts in order.
///
/// The clock is the capture's own, not the wall clock. The daemon ticks on wall
/// time, so a loaded machine ticks over a longer stretch of audio and lands on
/// different chunk boundaries. Ticking on audio time makes a replay reproducible
/// and models the idle machine, where a decode runs ~14x faster than real time.
///
/// The kind of each tick is the daemon's, from [`TickKind::for_tail`], except
/// that without `--previews` a tick the daemon would preview is
/// [`TickKind::Settled`]: it commits exactly what the preview tick commits,
/// without the cosmetic decode, which saves the replay about nine decodes in
/// ten.
fn replay_live(worker: &mut Engine, samples: &[f32], plan: Plan) -> Result<Vec<String>> {
    let utterance = Utterance::new(1);
    let window = worker.window();
    // `session.committed_hint`. Here it never lags: the replay is one thread,
    // so it is the worker's own offset.
    let mut hint = Frames::ZERO;
    let mut landed = vec![];
    let mut texts = vec![];
    let mut available = plan.tick;
    while available < samples.len() {
        // Past preview.max_seconds of uncommitted audio the daemon stops the
        // cosmetic decode but keeps ticking, a window at a time.
        let kind = tick_kind(available - hint.get(), window, plan.previews);
        // `snapshot_capture(committed_hint)`, truncated to one tick's work.
        let end = available.min(hint.get() + window);
        worker.tick(&samples[hint.get()..end], hint, &utterance, kind, |c| {
            landed.push((c.text, c.through))
        })?;
        for (text, through) in landed.drain(..) {
            hint = hint.max(through);
            texts.push(text);
        }
        available += plan.tick;
    }
    utterance.release();
    // `discard_before(committed_hint)` has dropped everything already
    // committed, so the release sees only the audio still held.
    worker.finish(&samples[hint.get()..], hint, &utterance, |c| {
        texts.push(c.text)
    })?;
    Ok(texts)
}

/// The kind of tick the daemon issues for a tail of `tail` samples, with a
/// preview tick left undecoded unless `previews` asks for it.
fn tick_kind(tail: usize, window: usize, previews: bool) -> TickKind {
    match TickKind::for_tail(tail, window) {
        TickKind::Preview if !previews => TickKind::Settled,
        kind => kind,
    }
}

/// One decode of the whole capture, with no detector in front of it.
fn decode_whole(worker: &mut Engine, samples: &[f32]) -> Result<Vec<String>> {
    if samples.is_empty() {
        return Ok(vec![]);
    }
    let text = worker
        .pipeline
        .recognizer
        .transcribe(samples, TrailingSilence::Padded)?;
    Ok(if text.trim().is_empty() {
        vec![]
    } else {
        vec![text]
    })
}

// -- results ------------------------------------------------------------------------

struct Row {
    file: String,
    language: String,
    spoken: String,
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
    seams: Seams,
    /// The committed chunk texts, in order. Private speech: written out only
    /// with `--with-text`.
    commits: Vec<String>,
    tally: Tally,
    decode_seconds: f64,
}

fn run_pass(
    config: &Config,
    entries: &[Entry],
    corpus: &Path,
    plan: Plan,
    jobs: usize,
) -> Result<Vec<Row>> {
    let processor = Processor::new(&config.text)?;
    let rate = config.audio.sample_rate;
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<(usize, Row)>> = Mutex::new(vec![]);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = vec![];
        for _ in 0..jobs {
            let (next, rows, processor) = (&next, &rows, &processor);
            let corpus = &*corpus;
            handles.push(scope.spawn(move || -> Result<()> {
                let mut worker = engine(config, plan.padding)?;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(entry) = entries.get(i) else {
                        return Ok(());
                    };
                    let wav = entry.wav(corpus);
                    if let Some(want) = &entry.sha256 {
                        let got = digest(&wav)?;
                        ensure!(
                            &got == want,
                            "{}: sha256 {got} does not match the dataset's {want}; \
                             the audio is not what the references were made from",
                            entry.file
                        );
                    }
                    let (samples, wav_rate) = read_capture(&wav)?;
                    ensure!(
                        wav_rate == rate,
                        "{}: {wav_rate}Hz, expected {rate}Hz",
                        entry.file
                    );
                    worker.pipeline.recognizer.take();
                    let started = Instant::now();
                    let texts = match plan.path {
                        DecodePath::Live => replay_live(&mut worker, &samples, plan)?,
                        DecodePath::Whole => decode_whole(&mut worker, &samples)?,
                    };
                    let decode_seconds = started.elapsed().as_secs_f64();
                    let yeah = texts.iter().filter(|t| is_yeah(t)).count();
                    let commits: Vec<String> =
                        texts.into_iter().filter(|t| !t.trim().is_empty()).collect();
                    let seams = seam_repeats(&commits);
                    let hypothesis = processor.process(&commits.join(" "));
                    let reference = tokenize(&entry.reference);
                    let hyp = tokenize(&hypothesis);
                    let alignment = align(&reference, &hyp);
                    rows.lock().expect("results lock").push((
                        i,
                        Row {
                            file: entry.file.clone(),
                            language: entry.language().to_owned(),
                            spoken: entry.spoken().to_owned(),
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
                            seams,
                            commits,
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
    seams: Seams,
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
        seams: Seams::default(),
        tally: Tally::default(),
        audio_seconds: 0.0,
        decode_seconds: 0.0,
    };
    for row in rows {
        s.audio_seconds += row.duration_s;
        s.decode_seconds += row.decode_seconds;
        s.hyp_words += row.hyp_words;
        s.yeah += row.yeah;
        s.seams.repeated += row.seams.repeated;
        s.seams.words += row.seams.words;
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
        "{:<34} {:>4} {:>6} {:>5} {:>5} {:>7} {:>5} {:>4} {:>4} {:>5} {:>4} {:>7}",
        "file", "lang", "dur", "ref", "hyp", "WER", "lost", "e1", "e2", "yeah", "dup", "decode"
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
            "{:<34} {:>4} {:>5.0}s {:>5} {:>5} {:>7} {:>5} {:>4} {:>4} {:>5} {:>4} {:>6.2}s",
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
            row.seams.repeated,
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
    if let Some(line) = breakdown(rows, |r| &r.language) {
        println!("by reference language  {line}");
    }
    if let Some(line) = breakdown(rows, |r| &r.spoken) {
        println!("by spoken language     {line}");
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
        "chunk seams that wrote a word twice: {} ({} words), over {} committed chunks",
        summary.seams.repeated,
        summary.seams.words,
        rows.iter().map(|r| r.commits.len()).sum::<usize>()
    );
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

/// Corpus-level WER per group of `key`, or `None` when every row falls into
/// one group and the aggregate already says it.
fn breakdown<'a>(rows: &'a [Row], key: impl Fn(&'a Row) -> &'a str) -> Option<String> {
    let mut groups: Vec<&str> = rows.iter().map(&key).collect();
    groups.sort_unstable();
    groups.dedup();
    (groups.len() > 1).then(|| {
        groups
            .iter()
            .map(|group| {
                let part: Vec<&Row> = rows.iter().filter(|r| key(r) == *group).collect();
                // A capture nobody spoke in has no WER: its edits are all
                // insertions, counted apart as `invented`.
                let (edits, words) = part
                    .iter()
                    .filter(|r| r.ref_words > 0)
                    .fold((0, 0), |(e, w), r| (e + r.edits, w + r.ref_words));
                let name = if group.is_empty() { "--" } else { group };
                match words {
                    0 => format!("{name} {} captures, no reference words", part.len()),
                    _ => format!(
                        "{name} {:.1}% ({} captures, {words} words)",
                        edits as f64 / words as f64 * 100.0,
                        part.len()
                    ),
                }
            })
            .collect::<Vec<_>>()
            .join("   ")
    })
}

/// Family and decoding method, as the report and the JSON lines name them.
fn describe(config: &Config) -> (&'static str, String) {
    (
        "parakeet",
        match &config.asr.decoding {
            Decoding::GreedySearch => "greedy_search".to_owned(),
            Decoding::ModifiedBeamSearch {
                vocabulary,
                hotwords_score,
            } => format!(
                "modified_beam_search ({} hotword phrases at {hotwords_score})",
                vocabulary.len()
            ),
        },
    )
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
            "file": row.file, "language": row.language, "spoken": row.spoken,
            "duration_s": row.duration_s,
            "ref_words": row.ref_words, "hyp_words": row.hyp_words,
            "edits": row.edits, "wer": row.wer, "lost_tail": row.lost_tail,
            "empty_first": row.tally.empty_first,
            "empty_after_retry": row.tally.empty_after_retry,
            "decodes": row.tally.decodes, "yeah": row.yeah,
            "seam_repeats": row.seams.repeated, "seam_words": row.seams.words,
            "chunks": row.commits.len(),
            "decode_seconds": row.decode_seconds,
        });
        if args.with_text {
            value["reference"] = row.reference.clone().into();
            value["hypothesis"] = row.hypothesis.clone().into();
            value["commits"] = row.commits.clone().into();
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
            "subset": args.subset.clone().or_else(|| args.ids.as_ref().map(|p| p.display().to_string())),
            "files": summary.files, "scored": summary.scored,
            "empty_reference": summary.empty_reference, "invented": summary.invented,
            "ref_words": summary.ref_words, "hyp_words": summary.hyp_words,
            "edits": summary.edits, "wer": summary.wer,
            "empty_first": summary.tally.empty_first,
            "empty_after_retry": summary.tally.empty_after_retry,
            "decodes": summary.tally.decodes, "yeah": summary.yeah,
            "seam_repeats": summary.seams.repeated, "seam_words": summary.seams.words,
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
    let list = args
        .subset
        .as_ref()
        .map(|name| args.corpus.join("subsets").join(format!("{name}.txt")))
        .or_else(|| args.ids.clone());
    let samples = match &list {
        Some(list) => {
            let text = std::fs::read_to_string(list)
                .with_context(|| format!("read the subset {}", list.display()))?;
            select(corpus.samples, &parse_ids(&text))
                .with_context(|| format!("subset {}", list.display()))?
        }
        None => corpus.samples,
    };
    let mut entries: Vec<Entry> = samples
        .into_iter()
        .filter(|e| e.wav(&args.corpus).is_file())
        .filter(|e| args.language.is_empty() || args.language.iter().any(|l| l == e.language()))
        .filter(|e| args.spoken.is_empty() || args.spoken.iter().any(|l| l == e.spoken()))
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
    let tick = args.tick_ms.map_or(
        (config.preview.interval_seconds * f64::from(rate)) as usize,
        |ms| (ms * u64::from(rate) / 1000) as usize,
    );
    ensure!(tick > 0, "--tick-ms must be at least 1");
    let plan = |path| Plan {
        path,
        padding: args
            .trailing_silence_ms
            .map(|ms| (ms * u64::from(rate) / 1000) as usize),
        tick,
        previews: args.previews,
        rate,
    };

    let (family, decoding) = describe(&config);
    println!(
        "corpus {} ({} captures{}{})",
        args.corpus.display(),
        entries.len(),
        list.as_ref()
            .map(|l| format!(" of the subset {}", l.display()))
            .unwrap_or_default(),
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
        let rows = run_pass(&config, &entries, &args.corpus, plan(path), args.jobs)?;
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
    fn a_relative_wav_belongs_to_its_dataset_and_an_absolute_one_does_not() {
        let entry = |path: &str| Entry {
            file: "c.wav".into(),
            path: path.into(),
            reference: String::new(),
            languages: vec![],
            spoken: None,
            sha256: None,
        };
        // A frozen dataset names its audio relative to itself, so the same
        // index works wherever the directory is moved or copied to.
        assert_eq!(
            entry("audio/c.wav").wav(Path::new("/data/corpus-2026-09-21")),
            PathBuf::from("/data/corpus-2026-09-21/audio/c.wav")
        );
        // An index that points at recordings where they were made keeps
        // working: joining an absolute path replaces the base.
        assert_eq!(
            entry("/var/state/c.wav").wav(Path::new("/data/corpus")),
            PathBuf::from("/var/state/c.wav")
        );
    }

    #[test]
    fn a_subset_lists_file_names_and_comments() {
        let text = "# the dev subset\nb.wav\n\n  a.wav  # the lost chunk\n#c.wav\n";
        assert_eq!(parse_ids(text), ["b.wav", "a.wav"]);
    }

    #[test]
    fn a_subset_keeps_corpus_order_and_refuses_a_capture_it_does_not_hold() {
        let entries: Vec<Entry> = ["a.wav", "b.wav", "c.wav"]
            .iter()
            .map(|f| Entry {
                file: (*f).into(),
                path: f.into(),
                reference: String::new(),
                languages: vec![],
                spoken: None,
                sha256: None,
            })
            .collect();
        let picked = select(entries.clone(), &["c.wav", "a.wav"]).unwrap();
        let names: Vec<&str> = picked.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(names, ["a.wav", "c.wav"]);
        let Err(error) = select(entries, &["a.wav", "gone.wav"]) else {
            panic!("a capture the corpus does not hold was accepted");
        };
        assert!(error.to_string().contains("gone.wav"), "{error}");
    }

    #[test]
    fn the_breakdown_scores_each_group_and_skips_a_single_one() {
        let mixed = Row {
            spoken: "mixed".into(),
            ..row(10, 10, 5, 0)
        };
        let silent = Row {
            spoken: "none".into(),
            ..row(0, 3, 3, 0)
        };
        let rows = [row(10, 10, 1, 0), mixed, silent];
        assert_eq!(
            breakdown(&rows, |r| &r.spoken).unwrap(),
            "en 10.0% (1 captures, 10 words)   mixed 50.0% (1 captures, 10 words)   \
             none 1 captures, no reference words"
        );
        assert_eq!(breakdown(&rows, |r| &r.language), None, "all en");
    }

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

    /// The replay issues the daemon's kind of tick: a tail longer than the
    /// window is read a window at a time, whatever `--previews` says, and a
    /// shorter one is previewed or, without `--previews`, only settled.
    /// Committing a window that settled nothing over a shorter tail would
    /// cut speech the daemon never cuts, and every number this harness
    /// produced would be of a decoder nobody runs.
    #[test]
    fn the_replay_ticks_as_the_daemon_does() {
        for previews in [false, true] {
            assert_eq!(tick_kind(11, 10, previews), TickKind::Window);
        }
        assert_eq!(tick_kind(10, 10, true), TickKind::Preview);
        assert_eq!(tick_kind(10, 10, false), TickKind::Settled);
        assert_eq!(tick_kind(0, 10, false), TickKind::Settled);
    }

    fn row(ref_words: usize, hyp_words: usize, edits: usize, lost_tail: usize) -> Row {
        Row {
            file: String::new(),
            language: "en".into(),
            spoken: "en".into(),
            duration_s: 1.0,
            reference: String::new(),
            hypothesis: String::new(),
            ref_words,
            hyp_words,
            edits,
            wer: (ref_words > 0).then(|| edits as f64 / ref_words as f64),
            lost_tail,
            yeah: 0,
            seams: Seams::default(),
            commits: vec![],
            tally: Tally::default(),
            decode_seconds: 0.1,
        }
    }

    #[test]
    fn a_seam_repeat_is_the_longest_tail_the_next_chunk_starts_with() {
        let texts = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let clean = seam_repeats(&texts(&["one two three", "four five"]));
        assert_eq!((clean.repeated, clean.words), (0, 0));
        // The trailing pad carried "three" into the next window as well.
        let once = seam_repeats(&texts(&["one two three", "Three, four five."]));
        assert_eq!(
            (once.repeated, once.words),
            (1, 1),
            "case and punctuation are normalised away"
        );
        let two = seam_repeats(&texts(&["one two three", "two three four", "four five"]));
        assert_eq!(
            (two.repeated, two.words),
            (2, 3),
            "two words at the first seam, one at the second"
        );
        // A word that merely recurs inside a chunk is not a seam repeat.
        let inside = seam_repeats(&texts(&["the cat sat", "on the mat"]));
        assert_eq!(inside.repeated, 0);
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
