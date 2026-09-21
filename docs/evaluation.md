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

Current aggregate, measured 2026-09-21 on the five local clips:
**17.6%** WER through the VAD-segmented path (the one the daemon uses) —
reproduces the historical figure exactly. The `--whole` (no-VAD) run
currently measures **18.7%**, not the previously reported 13.9%; see
[decisions.md](decisions.md#the-python-reference-implementation-is-dropped)
for why that number moved and why it is not a regression in this harness.

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
