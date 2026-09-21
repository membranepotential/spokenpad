# Evaluation

← [docs index](README.md) | Scores decodes produced by the ASR setup
described in [asr.md](asr.md).

`examples/eval.rs` answers, repeatably and numerically: *is a change to
`asr.vocabulary`, `asr.hotwords_score`, `asr.decoding`, or the decode
pipeline itself making transcription better or worse?* It decodes every
clip in `eval-samples/references.json` whose wav exists locally, through
the same [`core::decode::Pipeline`](../src/core/decode.rs) the `transcribe`
command and the daemon use, and reports word error rate (WER) against the
hand-verified reference text.

## Running it

```sh
cargo run --release --example=eval                  # VAD-segmented, the daemon's path
cargo run --release --example=eval -- --whole        # one decode per clip, no VAD
cargo run --release --example=eval -- --config PATH --model-dir DIR
```

(`--example=eval`, not `--example eval` — see the note on the `eval` argument
below if a shell wrapper rejects the bare word.)

The model loads once and does a warmup decode before any sample is timed
(the first inference is much slower than the rest — the daemon does the
same thing at startup). CLI flags: `--config PATH` (defaults to the XDG
config path, same as the daemon), `--model-dir DIR` (overrides
`asr.model_dir`), `--samples-dir DIR` (defaults to `eval-samples/`),
`--whole` (decode each clip in one pass instead of VAD-segmented).

The wavs themselves are never committed (biometric voice data, kept local —
see `.gitignore`); a clip missing on disk is skipped rather than failing the
run. The harness exits non-zero if the long-clip check (below) fails, and
zero otherwise.

## What the numbers mean

For each sample: **WER** (word error rate), decode time, and real-time
factor (clip duration ÷ decode time — higher is faster).

Current aggregate, measured 2026-09-21 on the five local clips with the
default `greedy_search` and the corrected `shell-commands` reference:
**18.7%** WER through the VAD-segmented path (the one the daemon uses) and
**14.3%** through `--whole`. With `decoding = "modified_beam_search"` the
same clips score 15.4% and 16.5%: beam search gets a few words right that
greedy misses here, but it drops whole speech chunks on real captures, which
these five clips do not show — see
[decisions.md](decisions.md#greedy-decoding-by-default-beam-search-drops-speech-2026-09-21).
Figures before 2026-09-21 were measured with beam search and the old
reference, and are not comparable.

WER needs normalisation to mean anything; `examples/eval.rs` does, exactly:

1. Casefold.
2. Replace every character that is not a word character (`\w`, which
   includes underscore), whitespace, or an apostrophe with a space — so
   `rm-rf` and `rm -rf` both become the two tokens `rm rf`, distinguishable
   from the collapsed error `rmrf`, and `let's` survives as one token.
   Underscores are **not** stripped: `test_file` and `test file` are a real
   distinction here (symbolised vs. spoken punctuation), and collapsing them
   would hide it.
3. Collapse repeated whitespace and strip the ends.

WER is Levenshtein edit distance over whitespace-split tokens of the
normalised text, divided by the reference's token count. The **aggregate
row** is corpus-level (total edit distance over all scored samples, divided
by total reference token count), not a mean of per-sample percentages —
`examples/eval.rs` has a unit test asserting that distinction directly.

`references.json`'s `exercises` field names the specific wording each clip
is known to be sensitive to (e.g. `mkdir` → `mkir`, `set`/`reset` must not
become `sed`/`rust`). It is documentation for a human reading a WER change,
not something the harness checks programmatically — see
[decisions.md](decisions.md#the-python-reference-implementation-is-dropped)
for why the older per-clip regex checks were dropped along with the
hotword-sweep tooling they were built for.

## Why one sample is also pass/fail, not only a WER row

`replacements-critique.wav` (37 s) is the longest clip. A WER row cannot
express "returned something, in time", so it carries a hard assertion of its
own (`long_clip_check: true` in `references.json`): a one-shot decode must
return non-empty text comfortably inside the clip's own duration.
"Comfortably" is deliberately loose — the harness asserts decode time under
0.5× the clip's duration (18.5 s here), not a fixed wall-clock threshold,
because the eval machine may be under load. `docs/asr.md` measures ~14.5x
real-time warm and idle, degrading to roughly 3x under heavy load (~13 s for
this clip) — the 0.5x margin stays clear of that without hardcoding a number
that would make the harness flaky.

## What the references are worth

All five entries in `eval-samples/references.json` carry `"verified": true`:
reconstructed from the recording session and confirmed against the audio by
the speaker. The harness tags any unverified sample with `[UNVERIFIED ref]`
in the table, so the absence of that marker is the check.

Five clips of one speaker on one microphone is a **regression proxy, not an
accuracy measurement**. Read per-sample movement as the signal; the
aggregate is for noticing that something moved, not for quoting as this
tool's word error rate.

## The local corpus

Five clips cannot decide between two decoders: a whole sentence lost on a real
capture moves the aggregate by a couple of points, which is inside the noise of
five samples. `examples/corpus.rs` replays every recovery capture in
`$XDG_STATE_HOME/spokenpad/audio` instead — over an hour of real dictation —
and reports what WER hides.

```sh
# every capture, both paths, the committed configuration
cargo run --release --example=corpus -- --config eval.toml

# one path, no trailing silence, machine-readable output under a name
cargo run --release --example=corpus -- --config eval.toml --path live \
    --trailing-silence-ms 0 --jsonl results.jsonl --label greedy-nopad

# only the German captures, every row, two captures decoded at a time
cargo run --release --example=corpus -- --config eval.toml \
    --language de --all --jobs 2
```

Nothing about the model is hard-wired: `--config PATH` is a full spokenpad
configuration, so another model family, a patched sherpa-onnx or another
provider is a config file rather than a change here. `--model-dir` and
`--threads` override single keys of it.

### What it measures

Per capture and in aggregate: WER, reference and hypothesis words, decode
time, and four counts WER cannot express.

| Column | Meaning |
|---|---|
| `lost` | Reference words at the very end the hypothesis has nothing opposite. The dictation whose last sentence never arrived. |
| `e1` | Speech chunks whose padded decode returned nothing, so the pipeline decoded them again bare. |
| `e2` | Of those, the ones the bare retry returned nothing for either: speech that is simply gone. |
| `yeah` | Commits that are nothing but "Yeah." — what beam search invents for clear speech ([k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267)). |

`e1` and `e2` are structurally zero on the `whole` path: with no segmenter
nothing claims a window holds speech, so `Pipeline::transcribe_speech` never
retries. Read them only on the `live` path.

The aggregate is corpus-level (all edits over all reference words), and the
per-file distribution is printed beside it, because one capture that loses
everything and a hundred that are nearly right average to something that
describes neither. Captures whose reference is empty — an empty press, a
cancelled take — are counted apart, along with how many of them decoded to
words anyway.

### The two paths

`--path live` drives `core::decode::Worker` the way `shell/daemon.rs` does: a
preview tick every `preview.interval_ms`, every settled chunk committed once,
then the release decoding only the tail. `--path whole` decodes each capture in
one pass with no segmenter. Both run by default.

The live replay's clock is the capture's own, not the wall clock. The daemon
ticks on wall time, so a loaded machine ticks over a longer stretch of audio and
lands on different chunk boundaries; ticking on audio time makes a replay
reproducible and models an idle machine, where a decode runs ~14x faster than
real time.

The preview decode is skipped unless `--previews` is given. The preview is
cosmetic — `Worker::tick` neither commits it nor advances the offset by it — so
the committed text is identical word for word, while the replay does about a
tenth of the decodes. With `--previews` the wall time is the daemon's real
workload; without it, only the decodes that produce text.

### Where the corpus comes from

`scripts/gladia-references.sh` builds `eval-samples/local/references.json` by
sending each recording to [Gladia](https://gladia.io) and keeping the
transcript. The author's corpus of 2026-09-21 is 181 captures and 73 minutes:
124 English, 38 German and 19 that nobody spoke in
([the experiment](experiments/2026-09-21-gladia-reference-transcripts.md)).
The harness prints a WER per reference language, and `--language de` scores
only one of them; a corpus of two languages has no single aggregate worth
quoting. It is a development tool: **it uploads audio to a third party**, no
code under `src/` calls it, and the user granting that has to mean it. The raw
response per capture and the index it builds stay in the git-ignored
`eval-samples/local/` — the recordings are the author's own speech, and so are
the transcripts. It is idempotent (a capture already fetched is skipped) and
deletes each job and its uploaded audio from Gladia after reading the result.

### What an ASR reference is worth

**It is not ground truth.** Gladia's transcripts of the five hand-checked clips
score 17.0% WER against the human references — those five are the
technical-vocabulary clips, and Gladia misses exactly what they exist to
exercise (`udev` as `udef`, `rm -rf` as one word, a spelled-out `s e t` as
`zset`). A reference like this is systematically friendly to a system that
writes what Gladia writes.

So:

- A difference of a point or two between two configurations is not a result.
  Look at whether it moves the same way on both paths, and at the counts
  beside it — chunks lost, tails lost — which are not reference-relative at all.
- Absolute WER over this corpus is an upper bound on the error, not the error.
- A change that improves technical vocabulary will look *worse* here.

The counts (`lost`, `e1`, `e2`, `yeah`) are the reason the corpus is worth
having: they say what was lost without asking the reference to be right about
the words.

### What it has decided so far

| experiment | question | answer |
|---|---|---|
| [greedy against beam](experiments/2026-09-21-greedy-vs-beam-corpus.md) | is `greedy_search` the right default? | yes; the WER difference is noise, the lost chunks and lost endings are not |
| [trailing silence](experiments/2026-09-21-trailing-silence-padding.md) | how long should the zero padding be? | one second; without it the bare retry is the same decode and every empty chunk is lost |
| [model families](experiments/2026-09-21-model-families-corpus.md) | is Whisper tiny.en or SenseVoice a serious alternative? | no; and both invent words from silence |
| [live against whole](experiments/2026-09-21-live-path-against-whole-file.md) | does progressive commit cost accuracy? | no; it wins on all eight configurations tried |
| [the references](experiments/2026-09-21-gladia-reference-transcripts.md) | what is an ASR reference worth? | enough to compare, not enough to quote |

The runs behind those files are kept beside the corpus, in the git-ignored
`eval-samples/local/`: `results-2026-09-21.jsonl` (one JSON line per capture
per run, plus a summary line), `configs/` and the console log.
