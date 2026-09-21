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
[`scripts/gladia-references.sh`](../scripts/gladia-references.sh), the one
development tool that uploads these recordings anywhere, run by hand with the
speaker's consent. Measured against the five hand-checked references above,
that reference is itself **17.0% wrong**, almost entirely on technical
vocabulary — so the corpus compares systems well and states absolute accuracy
badly.

[`examples/corpus.rs`](../examples/corpus.rs) reads it:

```sh
cargo run --release --example=corpus -- --config C.toml --corpus eval-samples/local
```

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
