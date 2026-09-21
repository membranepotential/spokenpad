# ASR

← [docs index](README.md) | The decode-time-biasing rule this implements is
justified in [constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement);
the implementation is in [`shell/inference.rs`](../src/shell/inference.rs),
the `[asr]` settings in [`config.rs`](../src/config.rs).

## Model families

spokenpad runs offline models through
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) 1.13.8, CPU only
(see [constraints.md](constraints.md#cpu-only)). `asr.family` selects how the
files are loaded; the functional core sees only the `Recognizer` trait.

| `family` | sherpa-onnx config | files in `model_dir` | hotwords | language |
|---|---|---|---|---|
| `parakeet` (default) | NeMo transducer | `encoder`, `decoder`, `joiner`, `tokens` | yes | fixed by the model |
| `whisper` | Whisper | `encoder`, `decoder`, `tokens` | no | `asr.language`, or detected |
| `sense_voice` | SenseVoice, with inverse text normalisation | `model`, `tokens` | no | `asr.language`, or detected |

Each file is found by its role: `<role>.int8.onnx` is preferred over
`<role>.onnx`, and a `<prefix>-` is allowed before the role, as in Whisper's
`tiny.en-encoder.int8.onnx`; `tokens` is `tokens.txt` or
`<prefix>-tokens.txt`. A missing file, or two files for one role, stops
start-up with exit code 2 and names the directory.

Every family reads 16 kHz mono, the only rate `audio.sample_rate` accepts.

Keys that do not apply to the chosen family are rejected when the config is
read, not ignored: `vocabulary`, `hotwords_score` and `decoding` belong to
`parakeet`, `language` to `whisper` and `sense_voice`, and every family but
`parakeet` needs an explicit `model_dir`.

### Verified models

Measured on one example machine (Intel i7-9850H, 6 threads) over the five
local eval clips, with the VAD on and an empty vocabulary. WER is the
aggregate word error rate against hand-checked references, ignoring case and
punctuation.

| model | size | WER | wall time, 100 s of audio |
|---|---|---|---|
| `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` (default) | 670 MB | 18.7% | ~20 s |
| `sherpa-onnx-whisper-tiny.en` | 100 MB (int8) | 23.0% | ~9 s |
| `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09`, `language = "en"` | 240 MB | 29.4% | ~10 s |

Parakeet's row is greedy decoding against the corrected `shell-commands`
reference (2026-09-21); the Whisper and SenseVoice rows were measured the same
day before that correction, against a reference with one sentence the clip
does not contain, so they read slightly high.

Wall time is five `spokenpad transcribe` runs, each loading the model, on a
machine busy with parallel builds; it compares the models, not the decode
speed below. Whisper tiny.en punctuates but misses technical words (`uda
rules` for `udev rules`). SenseVoice writes English in capitals.

Moonshine is not offered. Measured on sherpa-onnx 1.13.6, it failed on every
Moonshine v2 window longer than about ten seconds (an onnxruntime broadcast
error, then an empty result), and VAD windows are often longer. Not re-measured
on 1.13.8.

### Whisper's 30-second window

Whisper reads at most 30 s per decode, and sherpa-onnx drops the rest with
only a log line. A VAD window has no length limit: pauses shorter than
`2 × vad.edge_pad_seconds` stay inside one window. So for Whisper the adapter
cuts a window longer than 28 s into equal consecutive pieces, decodes each
with its second of trailing silence, and joins the texts. Every sample is
still decoded once; a cut can split a word.

### Using another model

1. Download a sherpa-onnx release of a supported family, for example
   `sherpa-onnx-whisper-base.en.tar.bz2` from the
   [asr-models release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models).
2. Unpack it, for example into `~/.local/share/spokenpad/models/`.
3. In `~/.config/spokenpad/config.toml`, set `[asr] family` and `model_dir`,
   and remove `decoding`, `hotwords_score` and `vocabulary` if the family is
   not `parakeet`.
4. Run `spokenpad check`. It prints `Configuration valid; CPU recognizer
   ready` when the files load.
5. Run `spokenpad transcribe some.wav` on a 16 kHz recording to judge the
   quality, then restart the service.

A family sherpa-onnx supports but spokenpad does not list (Paraformer,
Zipformer CTC, Canary, …) needs a new `Model` variant in `config.rs` and one
match arm in `shell/inference.rs`.

## The default: Parakeet TDT 0.6B v3, int8

Run as a NeMo transducer (`model_type = "nemo_transducer"`). The files
(`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`,
~670 MB) are not committed. `spokenpad fetch-models` downloads them from a
pinned Hugging Face revision, verifying the pinned size and sha256 of every
file, and puts them in
`$XDG_DATA_HOME/spokenpad/models/parakeet-tdt-0.6b-v3-int8/`, the default
`asr.model_dir`. The daemon, `check` and `transcribe` do this on their own
the first time they find the default directory missing or incomplete, before
loading the model — see [decisions.md](decisions.md#model-download-moves-into-the-binary).

### Speed on a CPU

Int8 weights on 6 CPU threads are fast enough that a GPU buys nothing for a
decode fired once per utterance. On the example machine (i7-9850H, idle, warm
model, `modified_beam_search`, best of 3; measured before the Rust port with
the same model, thread count and provider):

| audio | decode | real-time factor |
|---|---|---|
| 2 s | 0.22 s | 9.1× |
| 5 s | 0.38 s | 13.0× |
| 10 s | 0.62 s | 16.2× |
| 20 s | 1.19 s | **16.8×** |
| 30 s | 1.89 s | 15.8× |
| 37 s | 2.55 s | 14.5× |

Scaling is linear; the short clips are dominated by fixed per-decode
overhead. Measure on an idle machine: under load (a parallel build, load
average ~19) these figures degrade by roughly 5×, which is easy to mistake
for a scaling problem in the model.

`greedy_search` is the default (`asr.decoding`). `modified_beam_search` is
only needed for hotword biasing (next section), and on Parakeet TDT it is
unreliable: it returns `""` or an invented "Yeah." for clear speech in about
one request in five (upstream
[k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267),
open). Replaying the author's 170 recovery captures through the daemon's VAD
path, beam search left 19 speech chunks empty (2 still empty after the
retry without trailing silence) and greedy 4 (all recovered by the retry).
Setting a non-empty `vocabulary` switches to beam search and accepts that
cost.

## Hotwords: biasing the beam, not rewriting the output

sherpa-onnx's `modified_beam_search` decoder supports *hotwords*: a list of
phrases with a per-token bias score, applied during beam search so that
matching tokens are more likely to survive, rather than being pattern-matched
against the output text afterward. This is what makes decode-time biasing
possible instead of the fuzzy string replacement rejected in
[constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement).
sherpa-onnx applies hotwords only in a transducer's beam search, which is why
`vocabulary` exists only for `family = "parakeet"`.

Hotwords require BPE-level scoring, which means the decoder needs a
`bpe_vocab` file mapping subword pieces to scores.

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

`generate_bpe` in `shell/inference.rs` performs this reconstruction into a
temporary file whenever `asr.vocabulary` is non-empty. The recogniser is
built with that `bpe_vocab`, `modeling_unit = "bpe"` and a `hotwords_file`.

### Measured tuning: `hotwords_score`

`asr.hotwords_score` is a single global per-token bias applied to every
configured hotword (`asr.vocabulary`). It only takes effect with
`modified_beam_search`, which a non-empty vocabulary selects when `decoding`
is left out; the config rejects a vocabulary under an explicit
`greedy_search`.

Measured against the same eval clip, biasing `mkdir` after the model
mis-decoded it as `mkir`:

| `hotwords_score` | Result |
|---|---|
| no hotwords | `mkir` (uncorrected) |
| **1.5** | `mkdir` — correct; one observed side effect, `comment` -> `comman` |
| 3.0 | starts over-firing on unrelated audio |
| 6.0 | rewrites ordinary, unrelated words |

`1.5` is the default. Re-run the evaluation ([evaluation.md](evaluation.md))
before raising it.

## What's not yet answered

Whether hotword biasing alone closes the technical-vocabulary gap (e.g.
`dir` → `there`) or whether an LLM cleanup pass is still needed is an open
question — see [decisions.md](decisions.md#hotwords-only-cleanup-for-v1).
