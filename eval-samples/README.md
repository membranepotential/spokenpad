# eval-samples

Two evaluation sets live here. **Neither audio set is committed** — the
recordings are one person's voice, which is biometric data, and the transcripts
are what they said. `.gitignore` keeps `eval-samples/*.wav` and the whole of
`eval-samples/local/` out of the repository; `references.json` in this
directory is the one exception, because its five references are
hand-checked text that the speaker chose to publish.

## The five committed clips

`references.json` holds five short recordings with references reconstructed
from the recording session and confirmed against the audio by the speaker
(`"verified": true` on each). The wavs sit beside it, untracked.

| clip | ~length | what it exercises |
|---|---|---|
| `cd-home.wav` | 1 s | the shortest useful utterance; the case where Parakeet returns `""` if the window is padded wrong |
| `keyboard-layout-reset.wav` | 14 s | technical vocabulary (`udev rules`) |
| `new-model-review.wav` | 27 s | ordinary connected prose, the easy control |
| `replacements-critique.wav` | 37 s | the longest clip, and spelled-out letters (`s e t`, `r e s e t`); also the pass/fail long-clip check |
| `shell-commands.wav` | 20 s | spoken punctuation and commands (`mkdir`, `test_file`, `rm -rf`) |

`references.json` has an `exercises` field per clip naming the wording it is
sensitive to. It is documentation for whoever reads a WER change, not something
the harness checks.

```sh
cargo run --release --example=eval            # VAD-segmented, the daemon's path
cargo run --release --example=eval -- --whole # one decode per clip
```

Five clips of 182 reference words are a **regression check, not a benchmark**.
Read per-clip movement; the aggregate is for noticing that something moved. See
[docs/evaluation.md](../docs/evaluation.md).

## The private corpus in `local/`

`local/` is a frozen dataset of the author's own dictation, git-ignored in
full: **181 recordings, 75 minutes, 16 kHz mono** — 124 English, 38 German and
19 that nobody spoke in. It is what the five clips cannot be: large enough to
choose between two decoders, and made of real captures with their real
failures.

The references are not human. They come from
[Gladia](https://gladia.io) through
`scripts/gladia-references.sh`, a local script that is not in the
repository and the one development tool that uploads these recordings anywhere, run by hand with the
speaker's consent. Measured against the five hand-checked references above,
that reference is itself **17.0% wrong**, almost entirely on technical
vocabulary — so the corpus compares systems well and states absolute accuracy
badly.

[`examples/corpus.rs`](../examples/corpus.rs) reads it:

```sh
cargo run --release --example=corpus -- --config C.toml --corpus eval-samples/local
```

### Language: what Gladia heard and what was spoken

Gladia assigned each capture one language. The speaker switches language
inside a dictation, so dataset version `2026-09-21.1` adds a hand-checked
`spoken` field per capture: `en`, `de`, `mixed` (both languages, or the
language could not be told) or `none`. Audio and references are unchanged from
`2026-09-21`, so numbers from either version compare.

| `spoken` | captures |
|---|---|
| `en` | 113 |
| `de` | 34 |
| `mixed` | 15 |
| `none` | 19 |

A `mixed` capture's reference is in one language only: Gladia dropped or
translated the other one. Greedy scores 21.4% on the 15 mixed captures and
9.9% on the 147 single-language ones, so the aggregate WER carries about a
point of reference error from them. The report prints WER per `spoken` group;
`--spoken en --spoken de` leaves the mixed captures out.

### The dev subset

The whole corpus takes 8–20 minutes per decode path. For iteration, `--subset
dev` runs 28 captures, about 18 minutes of audio, chosen for the failures the
full corpus has shown, and for the least private content:

```sh
cargo run --release --example=corpus -- --config C.toml --path live --subset dev --jobs 2
```

`--subset NAME` reads `local/subsets/NAME.txt`; `--ids FILE` reads any such
list (one capture file name per line, `#` starts a comment). A name the corpus
does not hold is an error. The full corpus stays the gate before a change
ships; the subset only says whether a change is worth that run.

The dev subset holds every capture on which today's `main` (greedy) leaves a
chunk empty at the first decode (4 captures, 5 chunks), the one chunk still
empty after the retry, all 4 chunk seams that wrote a word twice, all 3
captures that lose words at their end, both captures where beam search loses
speech after the retry, 16 of the 19 chunks beam search leaves empty, one
capture without speech that every decoder writes words into and one it does
not, the capture the lead-padding clamp changes, 4 captures where beam search
on the whole-file path loses the ending, and for coverage 4 German and 3 mixed
captures, the 7 shortest and the longest short of 5 minutes:

```
capture-2026-09-09-120010  capture-2026-09-09-165818  capture-2026-09-09-170202
capture-2026-09-09-170244  capture-2026-09-11-122658  capture-2026-09-11-122704
capture-2026-09-11-122704-1  capture-2026-09-14-110141  capture-2026-09-14-110247
capture-2026-09-14-120915  capture-2026-09-14-125747  capture-2026-09-14-140747
capture-2026-09-14-163026  capture-2026-09-16-122927  capture-2026-09-17-193035
capture-2026-09-17-200400  capture-2026-09-17-200626  capture-2026-09-18-205920
capture-2026-09-18-210153  capture-2026-09-18-221701  capture-2026-09-18-225753
capture-2026-09-18-231741  capture-2026-09-19-190152  capture-2026-09-21-122623
capture-2026-09-21-142322  capture-2026-09-21-170941  capture-2026-09-21-171005
cd-home
```

Why each one is in, and how private its content is, is recorded inside the
dataset (`local/subsets/dev-notes.tsv`), not here. How well the subset stands
in for the corpus: [the dev-subset experiment](../docs/experiments/2026-09-22-dev-subset.md).

`local/README.md` (inside the dataset, git-ignored with it) documents the
layout, the JSON schema, provenance and how to extend it. The experiments that
used it:

- [the references themselves](../docs/experiments/2026-09-21-gladia-reference-transcripts.md)
- [greedy against beam search](../docs/experiments/2026-09-21-greedy-vs-beam-corpus.md)
- [how much trailing silence a decode needs](../docs/experiments/2026-09-21-trailing-silence-padding.md)
- [Whisper tiny.en and SenseVoice](../docs/experiments/2026-09-21-model-families-corpus.md)
- [the live path against whole-file decoding](../docs/experiments/2026-09-21-live-path-against-whole-file.md)
- [the lead-padding clamp](../docs/experiments/2026-09-21-lead-padding-clamp-corpus.md)
- [parakeet-unified-en](../docs/experiments/2026-09-21-parakeet-unified-en-corpus.md)

## Rules for this data

- Never commit a wav, a Gladia response, a run file with `--with-text`, or any
  transcript of this speech.
- Never send it anywhere except Gladia through the script above, which deletes
  its copies afterwards.
- Never quote what is said in a commit message, a doc, an experiment file or a
  report. Counts, durations, word totals and error rates are fine, and are what
  every experiment file here contains.
