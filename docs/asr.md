# ASR

← [docs index](README.md) | The decode-time-biasing rule this implements is
justified in [constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement);
`asr.py` (not yet implemented) is the shell module positioned in
[architecture.md](architecture.md#event-flow); config fields referenced below
are defined in [`config.py`](../src/voice_kb/config.py).

## Model

**Parakeet TDT 0.6B v3, int8**, run through
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) `>=1.13.6` as a NeMo
transducer (`model_type="nemo_transducer"` in
[`scripts/spike_decode.py`](../scripts/spike_decode.py)), CPU only, 6 threads
(see [constraints.md](constraints.md#cpu-only)). The model files
(`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`)
are fetched by `scripts/fetch_model.py` — not committed, ~630 MB — into
`models/parakeet-tdt-0.6b-v3-int8/` by default
(`AsrConfig.model_dir` in [`config.py`](../src/voice_kb/config.py)).

## Why int8 CPU, not a GPU model

The GPU on this machine is a 4 GB GTX 1650, deliberately kept free (see
[constraints.md](constraints.md#cpu-only)). Int8 quantization on 6 CPU
threads (i7-9850H) measures fast enough that this isn't a compromise:

| | |
|---|---|
| 20.4 s utterance, `greedy_search` | 2.12 s decode (**9.7× real-time**) |
| 20.4 s utterance, `modified_beam_search` | 2.38 s decode (8.6× real-time) |
| VRAM used | none |
| Handy 0.9.6, same clip | 1.37× real-time, fails past 30 s |

(README.md, STATUS.md). `modified_beam_search` is ~10% slower than
`greedy_search` on this checkpoint but is required for hotword biasing (next
section), and 8.6x real-time is still comfortably faster than needed for a
one-shot decode fired once per utterance — so it's the default
(`AsrConfig.decoding` in [`config.py`](../src/voice_kb/config.py)).

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

`scripts/build_hotwords.py` (not yet implemented) is responsible for this
reconstruction; the spike that proved it works is
[`scripts/spike_hotwords.py`](../scripts/spike_hotwords.py), which builds the
recognizer with `bpe_vocab=str(MODEL / "bpe.vocab")` and
`modeling_unit="bpe"` alongside a `hotwords_file`.

### Measured tuning: `hotwords_score`

`AsrConfig.hotwords_score` is a single global per-token bias applied to every
configured hotword (`AsrConfig.vocabulary`). It only takes effect with
`decoding_method="modified_beam_search"` — `AsrConfig.__post_init__` raises a
`ConfigError` if `vocabulary` is set under `greedy_search`
([`config.py`](../src/voice_kb/config.py)).

Measured against the same eval clip, biasing `mkdir` after the model
mis-decoded it as `mkir`:

| `hotwords_score` | Result |
|---|---|
| no hotwords | `mkir` (uncorrected) |
| **1.5** | `mkdir` — correct, no side effects observed |
| 3.0 | starts over-firing on unrelated audio |
| 6.0 | rewrites ordinary, unrelated words |

(README.md, STATUS.md, `scripts/spike_hotwords.py`.) `1.5` is the default in
[`config.py`](../src/voice_kb/config.py), with a warning in the docstring to
re-run `scripts/eval.py` before raising it.

## What's not yet answered

Whether hotword biasing alone closes the technical-vocabulary gap (e.g.
`dir` → `there`) or whether an LLM cleanup pass is still needed is an open
question — see [decisions.md](decisions.md#hotwords-only-cleanup-for-v1). It
needs `eval-samples/transcripts.json` hand-corrected to ground truth before
the eval harness can answer it (STATUS.md).
