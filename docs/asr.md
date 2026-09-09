# ASR

← [docs index](README.md) | The decode-time-biasing rule this implements is
justified in [constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement);
[`asr.py`](../src/spokenpad/asr.py) is the shell module positioned in
[architecture.md](architecture.md#event-flow); config fields referenced below
are defined in [`config.py`](../src/spokenpad/config.py).

## Model

**Parakeet TDT 0.6B v3, int8**, run through
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) `>=1.13.6` as a NeMo
transducer (`model_type="nemo_transducer"` in
[`src/spokenpad/asr.py`](../src/spokenpad/asr.py)), CPU only, 6 threads
(see [constraints.md](constraints.md#cpu-only)). The model files
(`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`)
are fetched by `scripts/fetch_model.py` — not committed, ~630 MB — into
`models/parakeet-tdt-0.6b-v3-int8/` by default
(`AsrConfig.model_dir` in [`config.py`](../src/spokenpad/config.py)).

## Why int8 CPU, not a GPU model

The GPU on this machine is a 4 GB GTX 1650, deliberately kept free (see
[constraints.md](constraints.md#cpu-only)). Int8 quantization on 6 CPU
threads (i7-9850H) measures fast enough that this isn't a compromise:

Idle machine, warm model, `modified_beam_search`, best of 3:

| audio | decode | real-time factor |
|---|---|---|
| 2 s | 0.22 s | 9.1× |
| 5 s | 0.38 s | 13.0× |
| 10 s | 0.62 s | 16.2× |
| 20 s | 1.19 s | **16.8×** |
| 30 s | 1.89 s | 15.8× |
| 37 s | 2.55 s | 14.5× |
| VRAM used | none | |
| Handy 0.9.6 | 1.37× real-time, discards past 30 s | |

Scaling is linear — the short clips are dominated by fixed per-decode overhead,
not the reverse. Measure on an idle machine: under load (a parallel build,
load average ~19) these figures degrade by roughly 5×, which is easy to
mistake for a scaling problem in the model.

`modified_beam_search` is marginally slower than `greedy_search` on this
checkpoint but is required for hotword biasing (next section), and the margin
is irrelevant for a one-shot decode fired once per utterance — so it's the
default
(`AsrConfig.decoding` in [`config.py`](../src/spokenpad/config.py)).

## Hotwords: biasing the beam, not rewriting the output

sherpa-onnx's `modified_beam_search` decoder supports *hotwords*: a list of
phrases with a per-token bias score, applied during beam search so that
matching tokens are more likely to survive, rather than being pattern-matched
against the output text afterward. This is what makes decode-time biasing
possible instead of the fuzzy string replacement rejected in
[constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement).

Hotwords require BPE-level scoring, which means the decoder needs a
`bpe_vocab` file mapping subword pieces to scores. This is where things get
non-obvious.

### The `bpe.vocab` reconstruction

sherpa-onnx's `bpe_vocab` parameter expects the **two-column SentencePiece
`.vocab` text file** — one `piece<TAB>log-probability` pair per line — not
the `bpe.model` **protobuf** that SentencePiece normally ships. These are
easy to conflate because both come from the same SentencePiece tokenizer and
share the `bpe.*` naming pattern, but they're structurally different files
and only one of them works here.

The published Parakeet TDT v3 model ships **neither** file. What it does
ship is `tokens.txt` — the flat token-to-ID table every sherpa-onnx model
needs regardless of hotwords. The `.vocab` file is reconstructible from that
table alone, using the standard SentencePiece BPE convention that a piece's
score is the negative of its index in the vocabulary:

```
score = -index
```

[`scripts/build_hotwords.py`](../scripts/build_hotwords.py) performs this
reconstruction (the generator itself lives in
[`src/spokenpad/asr.py`](../src/spokenpad/asr.py) so the daemon and the script
cannot drift apart). The recogniser is built with
`bpe_vocab=<model_dir>/bpe.vocab` and `modeling_unit="bpe"` alongside a
`hotwords_file`. Verified working -- and now guarded by
[the eval harness](evaluation.md).

### Measured tuning: `hotwords_score`

`AsrConfig.hotwords_score` is a single global per-token bias applied to every
configured hotword (`AsrConfig.vocabulary`). It only takes effect with
`decoding_method="modified_beam_search"` — `AsrConfig.__post_init__` raises a
`ConfigError` if `vocabulary` is set under `greedy_search`
([`config.py`](../src/spokenpad/config.py)).

Measured against the same eval clip, biasing `mkdir` after the model
mis-decoded it as `mkir`:

| `hotwords_score` | Result |
|---|---|
| no hotwords | `mkir` (uncorrected) |
| **1.5** | `mkdir` — correct; one observed side effect, `comment` -> `comman` |
| 3.0 | starts over-firing on unrelated audio |
| 6.0 | rewrites ordinary, unrelated words |

(Reproducible via `uv run scripts/eval.py --sweep 0 1.5 3.0 --vocabulary mkdir`.) `1.5` is the default in
[`config.py`](../src/spokenpad/config.py), with a warning in the docstring to
re-run `scripts/eval.py` before raising it.

## What's not yet answered

Whether hotword biasing alone closes the technical-vocabulary gap (e.g.
`dir` → `there`) or whether an LLM cleanup pass is still needed is an open
question — see [decisions.md](decisions.md#hotwords-only-cleanup-for-v1). It
needs `eval-samples/transcripts.json` hand-corrected to ground truth before
the eval harness can answer it (STATUS.md).
