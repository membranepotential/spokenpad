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

The script exits non-zero if the 37 s long-clip check (below) fails; a
normal run with all references unscored-but-decoded exits 0.

## What the numbers mean

For each sample with a reference: **WER** (word error rate) and **CER**
(character error rate), decode time, and real-time factor (clip duration ÷
decode time — higher is faster), plus the **Handy 0.9.6 baseline WER** for
the same clip from `eval-samples/transcripts.json` for comparison. Handy is
the *baseline to beat*, not the target — do not read "beats Handy" as "is
good."

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

## Why one sample is pass/fail, not a WER row

`handy-1787827757.wav` (37 s) has `reference: null` — it's the clip Handy
discarded outright (`Timed out waiting 30s for live transcription to
finalize`), so what was actually said was never recovered and there is
nothing to score WER against. It is still the single most important sample
in the set, so it gets a hard assertion instead: a one-shot decode must
return non-empty text, comfortably inside the clip's own duration.
"Comfortably" is deliberately loose — `scripts/eval.py` asserts decode time
under 0.5× the clip's duration (18.5 s here), not a fixed wall-clock
threshold, because the eval machine may be under load. `docs/asr.md`
measures ~14.5x real-time warm and idle, degrading to roughly 3x under
heavy load (~13 s for this clip) — the 0.5x margin stays clear of that
without hardcoding a number that would make the harness flaky.

## Standing caveat: references are unverified

Every entry in `eval-samples/references.json` has `"verified": false`. They
were reconstructed from the chat session in which the samples were
recorded — including the speaker's own typed corrections — not by someone
listening to the audio afterward. `scripts/eval.py` tags every WER/CER
figure derived from them accordingly (`[UNVERIFIED ref]` in the table,
`"verified": false` per sample and `"references_verified": false` on the
aggregate in `--json` output). Treat WER/CER numbers from this harness as
directional until someone listens to the five clips and flips the flags —
they are not yet established fact.


## The Handy baseline column is biased — do not quote it

`scripts/eval.py` prints a `handy WER` column from `eval-samples/transcripts.json`.
It is useful for spotting per-sample regressions and misleading as a verdict,
for two independent reasons:

1. **Some references were derived from Handy's own output.**
   `handy-1787828395`'s reference is Handy's transcript with a single word
   corrected, so Handy is scored against a reference produced by Handy. Its
   1.7% there is close to meaningless.
2. **Handy's worst failure is excluded from the aggregate.**
   `handy-1787827757` (37 s) has `reference: null` because Handy produced an
   empty transcript and the spoken content was never recovered. Scoring it
   would give Handy roughly 100% WER on 37 seconds of speech. It is instead a
   pass/fail check that Handy would fail outright and voice-kb passes in ~3 s.

So the current aggregate — voice-kb 18.4% vs Handy 15.8% — is not evidence that
Handy transcribes better. It is evidence that the reference set is small,
unverified, partly circular, and excludes the case that motivated this project.
Fixing that means listening to the audio and marking references `verified: true`.
Until then, treat per-sample movement and the `exercises` checks as the signal
and the aggregate as noise.
