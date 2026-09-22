# ASR

← [docs index](README.md) | The decode-time-biasing rule this implements is
justified in [constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement);
the implementation is in [`shell/inference.rs`](../src/shell/inference.rs),
the `[asr]` settings in [`config.rs`](../src/config.rs).

## The model

spokenpad runs a NeMo transducer — Parakeet TDT by default — offline through
[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) 1.13.8, CPU only
(see [constraints.md](constraints.md#cpu-only)). The functional core sees only
the `Recognizer` trait.

`asr.model_dir` holds four files, each found by its role: `encoder`,
`decoder` and `joiner`, where `<role>.int8.onnx` is preferred over
`<role>.onnx`, and `tokens.txt`. A missing file makes `check` and
`transcribe` exit with code 2 and name the directory; the daemon says so in
the dictation window and tries again at the next press. The model reads
16 kHz mono, the only rate `audio.sample_rate` accepts.

Until 2026-09-22 `asr.family` also offered Whisper and SenseVoice, with
`asr.language`. Both were removed: Parakeet was better than either on every
measurement below, and each family was a code path of its own (Whisper's
30-second window was cut into pieces). A configuration that still sets
either key is refused with a message that says so
([decisions.md](decisions.md#one-model-family-vad-and-preview-always-on-2026-09-22)).

### Verified models

Measured on one example machine (Intel i7-9850H, 6 threads) over the five
local eval clips, with the VAD on and an empty vocabulary. WER is the
aggregate word error rate against hand-checked references, ignoring case and
punctuation. The Whisper and SenseVoice rows are kept as the record of why
they were removed.

| model | size | WER | wall time, 100 s of audio |
|---|---|---|---|
| `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8` (default) | 670 MB | 18.7% | ~20 s |
| `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming` | 633 MB | 8.2% | ~20 s |
| `sherpa-onnx-whisper-tiny.en` (removed) | 100 MB (int8) | 23.0% | ~9 s |
| `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09`, `language = "en"` (removed) | 240 MB | 29.4% | ~10 s |

Parakeet's row is greedy decoding against the corrected `shell-commands`
reference (2026-09-21); the Whisper and SenseVoice rows were measured the same
day before that correction, against a reference with one sentence the clip
does not contain, so they read slightly high.

`parakeet-unified-en-0.6b` is another NeMo transducer in another
`model_dir`, and it is the one model measured here whose beam search and
hotwords work (next section). **Read its WER with the next paragraph, not on
its own.** It is English only, and it does not fail gracefully on another
language: it transliterates German into English words, or returns nothing at
all. Replaying the author's 176 recovery captures, it left 16 speech chunks
empty where the default left none, and returned 20% fewer words. Use it only
if you dictate English and nothing else. Measurements:
[experiments/2026-09-21-parakeet-unified-rnnt.md](experiments/2026-09-21-parakeet-unified-rnnt.md).

Wall time is five `spokenpad transcribe` runs, each loading the model, on a
machine busy with parallel builds; it compares the models, not the decode
speed below. Whisper tiny.en punctuated but missed technical words (`uda
rules` for `udev rules`). SenseVoice wrote English in capitals.

Moonshine is not offered. Measured on sherpa-onnx 1.13.6, it failed on every
Moonshine v2 window longer than about ten seconds (an onnxruntime broadcast
error, then an empty result), and VAD windows are often longer. Not re-measured
on 1.13.8.

### Using another transducer

1. Download a sherpa-onnx release of a NeMo transducer, for example
   `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming` from the
   [asr-models release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models).
2. Unpack it, for example into `~/.local/share/spokenpad/models/`.
3. In `~/.config/spokenpad/config.toml`, set `[asr] model_dir` to it.
4. Run `spokenpad check`. It prints `Configuration valid; CPU recognizer
   ready` when the files load.
5. Run `spokenpad transcribe some.wav` on a 16 kHz recording to judge the
   quality, then restart the service.
## The default: Parakeet TDT 0.6B v3, int8

Run as a NeMo transducer (`model_type = "nemo_transducer"`). The files
(`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`,
~670 MB) are not committed. `spokenpad fetch-models` downloads them from a
pinned Hugging Face revision, verifying the pinned size and sha256 of every
file, and puts them in
`$XDG_DATA_HOME/spokenpad/models/parakeet-tdt-0.6b-v3-int8/`, the default
`asr.model_dir`. `check` and `transcribe` do this on their own the first
time they find the default directory missing or incomplete, before loading
the model, and the daemon does it in the background while it already records
— see [decisions.md](decisions.md#model-download-moves-into-the-binary-2026-09-21).

### Speed on a CPU

Int8 weights on 6 CPU threads are fast enough that a GPU buys nothing for a
decode fired once per utterance. The table below was taken with
`modified_beam_search`, before `greedy_search` became the default. Greedy is
10–15 % faster than beam search on the same machine
([1.13.6](experiments/2026-09-21-beam-search-upstream-fix.md#cost),
[1.13.8](experiments/2026-09-21-beam-search-public-repro.md#cost)), so read
these figures as an upper bound for what the default costs. On the
example machine (i7-9850H, idle, warm model, best of 3; measured before the
Rust port with the same model, thread count and provider):

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
open). Replaying the author's whole local corpus — 181 captures, 75 minutes —
through the daemon's VAD path, beam search left 19 speech chunks empty (2 still
empty after the retry without trailing silence) and greedy 5 (all recovered by
the retry); beam lost 57 reference words off the ends of captures against
greedy's 5, and invented three "Yeah." commits against none. Word error rate
alone cannot tell the two apart there, which is why those counts decide it; see
[the experiment](experiments/2026-09-21-greedy-vs-beam-corpus.md). Setting a
non-empty `vocabulary` switches to beam search and accepts that cost.

The defect is **TDT-only**, and spokenpad cannot detect that for you.
sherpa-onnx builds its NeMo beam-search decoder with a TDT flag it takes from
the encoder's `url` metadata, and every remaining defect sits on the TDT side
of that flag. Measured on 1.13.8, the same 10.6 s of clear speech that TDT
beam search decodes to `""` decodes to its words under beam search on
`parakeet-unified-en-0.6b`, a NeMo RNNT that is not TDT, and hotwords bias
that model's output as the tuning table below describes. Which side of the
flag a directory holds sits in an ONNX metadata entry that sherpa-onnx does
not expose, so spokenpad does not read it: `decoding` says what you want, and
the model you point `model_dir` at decides whether beam search is safe.

## Hotwords: biasing the beam, not rewriting the output

sherpa-onnx's `modified_beam_search` decoder supports *hotwords*: a list of
phrases with a per-token bias score, applied during beam search so that
matching tokens are more likely to survive, rather than being pattern-matched
against the output text afterward. This is what makes decode-time biasing
possible instead of the fuzzy string replacement rejected in
[constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement).
sherpa-onnx applies hotwords only in a transducer's beam search, which is why
`vocabulary` needs `decoding = "modified_beam_search"`.

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
