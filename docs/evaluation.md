# Evaluation

← [docs index](README.md) | Scores decodes produced by the ASR setup
described in [asr.md](asr.md) — the `hotwords_score` tuning table there was
measured by eyeballing a single clip, which is the gap this harness closes.

`scripts/eval.py` answers, repeatably and numerically: *is a change to
`asr.vocabulary`, `asr.hotwords_score`, or `asr.decoding` making
transcription better or worse?*

## Running it

```
uv run scripts/eval.py
uv run scripts/eval.py --vocabulary mkdir --hotwords-score 1.5
uv run scripts/eval.py --sweep 0 1.5 3.0 6.0 --vocabulary mkdir
uv run scripts/eval.py --json > result.json
```

The model loads once and does a warmup decode before any sample is timed
(the first inference is much slower than the rest — the daemon does the
same thing at startup). `--sweep` reconstructs the model per
`hotwords_score` value, since the value is baked in at construction; it is
correspondingly slower and is meant for occasional tuning runs, not routine
use.

CLI flags: `--config PATH` (defaults to the XDG config path, same as the
daemon), `--samples-dir PATH` (defaults to `eval-samples/`), `--vocabulary`
(repeatable, comma-separated), `--hotwords-score`, `--decoding`, `--sweep
SCORE [SCORE...]`, `--json`. `--vocabulary`/`--hotwords-score` only have an
effect under `decoding = modified_beam_search` (the config default) — see
[asr.md](asr.md#hotwords-biasing-the-beam-not-rewriting-the-output).

The script exits non-zero if the 37 s long-clip check (below) fails, and
zero otherwise.

## What the numbers mean

For each sample: **WER** (word error rate) and **CER** (character error rate),
decode time, and real-time factor (clip duration ÷ decode time — higher is
faster), plus the **Handy 0.9.6 baseline WER** for the same clip from
`eval-samples/transcripts.json` for comparison. Handy is the *baseline to
beat*, not the target — do not read "beats Handy" as "is good."

All five samples have a verified reference and are scored. Current aggregate,
measured 2026-09-11: **17.6%** WER through `--vad` (the path the daemon uses)
and **13.9%** without it, against Handy's **48.7%** on the same five.

WER and CER need normalisation to mean anything; `scripts/eval.py` does,
exactly:

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
normalised text, divided by the reference's token count. CER is the same
edit distance over characters of the normalised text, divided by the
reference's character count. Both are implemented directly in
`scripts/eval.py` — no dependency was added for this. The **aggregate row**
is corpus-level (total edit distance over all scored samples, divided by
total reference length), not a mean of per-sample percentages.

**Exercises**: `references.json` names the specific error each sample
probes (e.g. `mkdir` → `mkir`, `set`/`reset` must not become `sed`/`rust`).
An aggregate WER can improve while a specific regression comes back, so
`scripts/eval.py` checks each of these directly — word-present /
word-absent — against the post-processed text and reports PASS/FAIL per
check, not folded into WER. Only exercises with a concrete before/after
word pair are checked this way; `test_file` → `test underscore file`
(spoken punctuation) is a known, undesigned-for limitation with no target
behaviour to assert, so it's excluded rather than given a check that can
never pass.

## Why one sample is also pass/fail, not only a WER row

`handy-1787827757.wav` (37 s) is the clip Handy discarded outright (`Timed out
waiting 30s for live transcription to finalize`). It had no reference for a
long time, because what was said was never recovered; since 2026-08-27 it has a
verified one and is scored like every other sample — which is what finally put
Handy's worst failure into the aggregate.

It also keeps a hard assertion of its own (`long_clip_check: true` in
`references.json`), because a WER row cannot express "returned something, in
time": a one-shot decode must return non-empty text comfortably inside the
clip's own duration. "Comfortably" is deliberately loose — `scripts/eval.py`
asserts decode time under 0.5× the clip's duration (18.5 s here), not a fixed
wall-clock threshold, because the eval machine may be under load. `docs/asr.md`
measures ~14.5x real-time warm and idle, degrading to roughly 3x under
heavy load (~13 s for this clip) — the 0.5x margin stays clear of that
without hardcoding a number that would make the harness flaky. The flag is
deliberately independent of whether the sample has a reference: inferring it
from `reference is None` silently disabled the check on the one sample it
exists for, the moment that clip was finally transcribed.

## What the references are worth

All five entries in `eval-samples/references.json` carry `"verified": true`.
They were reconstructed from the session in which the samples were recorded and
then confirmed against the audio by the speaker on 2026-08-27, with `uv run
scripts/verify_references.py`. Four were confirmed unchanged; the fifth
(`handy-1787827757`) had no reference at all and gained one. `scripts/eval.py`
tags any unverified sample with `[UNVERIFIED ref]` in the table, so the absence
of that marker is the check. (`--json` still reports
`"references_verified": false` on the aggregate: that field is hardcoded in
`scripts/eval.py` and has not been updated — the per-sample `"verified"` flags
are the true ones.)

That verification pass also discharged the two biases that used to make the
`handy WER` column unquotable: `handy-1787828395`'s reference is no longer
Handy's own output with a word corrected but independently confirmed text, and
the 37 s clip Handy discarded is scored rather than excluded — so the aggregate
finally contains Handy's worst failure instead of omitting it.

Two caveats remain, in our own disfavour:

- The `handy-1787827757` reference was drafted from **this project's** model
  output and then corrected by the speaker. The correction was substantial (it
  fixed set/sed, reset/rust and recovered a whole trailing passage), so it is
  not circular, but a residual bias toward our model on that one sample cannot
  be fully excluded. A from-scratch transcription would settle it if the number
  ever mattered that much.
- Five clips of one speaker on one microphone is a **regression proxy, not an
  accuracy measurement**. Read per-sample movement and the `exercises` checks as
  the signal; the aggregate is for noticing that something moved, not for
  quoting as this tool's word error rate.
