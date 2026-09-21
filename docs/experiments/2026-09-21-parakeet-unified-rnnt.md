# Does beam search, and with it hotwords, work on a non-TDT NeMo RNNT?

_2026-09-21, worktree branch off `4be5947` with sherpa-onnx 1.13.8. Intel
i7-9850H, 6 threads, CPU provider. Other agents were building throughout, so
every timing here is under load; `docs/asr.md`'s idle figures are roughly 5×
better and the two sets are not comparable._

## Question

spokenpad decodes Parakeet TDT greedily because `modified_beam_search` on a
NeMo **TDT** model returns `""` or an invented "Yeah." for clear speech
([k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267)).
sherpa-onnx constructs its NeMo beam-search decoder with a TDT flag taken
from the encoder's own metadata, and every defect the open fix
[#3657](https://github.com/k2-fsa/sherpa-onnx/pull/3657) names sits on the
TDT side of that flag. So: on a NeMo transducer that is **not** TDT, does
beam search work — and do hotwords come back with it?

`parakeet-unified-en-0.6b` is that model: an RNNT, not a TDT, in the same
`nemo_transducer` file layout. Three further questions follow if it works:
what it costs in accuracy, in speed, and in lost speech.

## Method

- Downloaded `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2`
  from the k2-fsa/sherpa-onnx `asr-models` release:
  **501 350 460 bytes**, sha256
  `99f63605b3a85a54c250c0869670a687b7d6598a47bf2421515e1f839a76e150`.
  Unpacked to `~/.local/share/spokenpad/models-candidates/`.
- Wrote spokenpad config files that select it with nothing but
  `family = "parakeet"` and `model_dir`, and ran `spokenpad check`.
- Read both encoders' ONNX `metadata_props` (they sit at the tail of the
  protobuf) to find a reliable TDT signal.
- `cargo run --release --example=eval` and `-- --whole` on the five local
  clips, under `greedy_search`, under `modified_beam_search` with an empty
  vocabulary, and under beam search with a ten-word technical vocabulary.
- `spokenpad transcribe` on `eval-samples/shell-commands.wav` with
  `hotwords_score` swept over 1.5 / 3.0 / 6.0, to see whether the bias fires
  at all.
- Replayed every recovery capture through the daemon's VAD path with a
  scratch harness (`examples/model_replay.rs`, not kept), counting chunks,
  empty decodes, bare "Yeah." decodes and words — never text.
- Decoded the last 10.6 s of the capture whose tail TDT beam search loses.
- Decoded the Qwen3-ASR release's own `test_wavs/de.wav`, a public 6.7 s
  German clip with a published reference.

## Data

- `eval-samples/*.wav`: five clips, 100 s, English dictated with a German
  accent, hand-verified references.
- `~/.local/state/spokenpad/audio/`: 176 recovery captures, 4004 s of speech
  after VAD, 325 settled chunks. Private; counts and word totals only.
- `test_wavs/de.wav` from the Qwen3-ASR release (public, with a published
  German reference).

## Results

### It loads with no code change

`family = "parakeet"` plus `model_dir` is enough. The release ships
`encoder.int8.onnx`, `decoder.int8.onnx`, `joiner.int8.onnx` and
`tokens.txt`, exactly the roles `pick` looks for, and `spokenpad check`
prints `Configuration valid; CPU recognizer ready; VAD ready.` under greedy,
under beam search and with a vocabulary. `model_type = "nemo_transducer"` is
unchanged, and the adapter's default feature dimension is corrected from the
model's own metadata, as it already is for Parakeet TDT v3 (both declare
`feat_dim 128`).

### Beam search works, and hotwords work

The last 10.6 s of the capture whose tail TDT beam search loses:

| model | greedy `Padded` / `Bare` | beam `Padded` / `Bare` | beam + vocabulary |
|---|---|---|---|
| Parakeet TDT 0.6B v3 | 8 / 7 words | **empty / empty** | — |
| parakeet-unified-en-0.6b | 7 / 7 words | 7 / 7 words | 7 / 7 words |

The hypothesis holds: the defect is TDT-only, and this model's beam search
returns the words.

Hotword biasing fires, and behaves like the tuning table in
[asr.md](../asr.md#measured-tuning-hotwords_score). Biasing `commands` and
`dir` on `shell-commands.wav`:

| `hotwords_score` | effect |
|---|---|
| 1.5 | output unchanged |
| 3.0 | one word changed, for the worse: `mkdir` became `mk dir` |
| 6.0 | the output collapses into the hotwords repeated hundreds of times |

The collapse at 6.0 is the proof that the bias reaches the beam: it is the
same over-firing `asr.md` records for TDT at that score. It also means
biasing is available but needs the same careful tuning; 1.5 changed nothing
on this clip because the model already got these words right.

The `bpe.vocab` reconstruction this needs works unchanged: the release ships
a `tokens.txt` of 1025 SentencePiece pieces in the usual `▁piece id` form,
which is all `generate_bpe` reads.

### Accuracy on the five clips

| clip | TDT v3 greedy | unified greedy | unified beam | unified beam + vocabulary |
|---|---|---|---|---|
| shell-commands | 51.7% | 34.5% | 34.5% | 34.5% |
| cd-home | 100.0% | **0.0%** | 0.0% | 0.0% |
| keyboard-layout-reset | 21.1% | **0.0%** | 0.0% | 0.0% |
| replacements-critique | 17.8% | 6.8% | 6.8% | 6.8% |
| new-model-review | 0.0% | 0.0% | 0.0% | 0.0% |
| **aggregate, VAD** | **18.7%** | **8.2%** | **8.2%** | **8.2%** |
| **aggregate, `--whole`** | **14.3%** | **8.2%** | — | — |

Five clips of one speaker are a regression proxy, not an accuracy
measurement — but the movement is large and in one direction, and two clips
go from wrong to exact. The three decoding settings score the same, so on
these clips beam search neither helps nor hurts, and the vocabulary does not
bite.

### Lost speech and speed over 176 captures

Whole-file VAD replay, 176 captures, 325 chunks, 4004 s of speech. The same
chunks for every row, because the VAD settings are the same:

| model / decoding | empty after padding | empty after the `Bare` retry | bare "Yeah." | words | replay speed |
|---|---|---|---|---|---|
| TDT v3, greedy | 5 | **0** | 0 | **7023** | 2.5× |
| unified, greedy | 17 | **16** | 0 | **5648** | 2.2× |
| unified, beam | 34 | **29** | 0 | **5520** | 1.9× |

**This is the result that matters, and it is the opposite of the eval
table.** On the author's own captures the unified model loses 16 chunks
outright where TDT v3 loses none, and returns 1375 fewer words — 20% of the
corpus. 16 lost chunks account for about 350 of those words at the corpus
average, so most of the gap is not lost chunks but chunks that came back
shorter: German rendered as a few English words instead of a sentence.

**Beam search costs chunks here too**, even without the TDT defect: 29 lost
against greedy's 16 on the same audio, and 128 fewer words. It is healthy in
the sense that matters — it returns text for the tail TDT beam search loses,
and it is not the one-in-five collapse TDT shows — but it is not free, and
greedy remains the safer search on this model as well.

The same pattern on the first 30 captures alone (53 chunks, 670 s), where
Qwen3-ASR was also measured
([2026-09-21-qwen3-asr.md](2026-09-21-qwen3-asr.md)):

| model | empty after the retry | words |
|---|---|---|
| TDT v3, greedy | 0 | 1088 |
| unified, greedy | 1 | 993 |
| Qwen3-ASR | 0 | 1054 |

Peak resident memory over a decode run: 1440 MB against TDT v3's 1360 MB.

Digital silence of 0.2 / 0.5 / 1 / 3 / 10 s decodes to `""` in every case,
as TDT v3 does.

Decode speed, best of three on slices of `replacements-critique.wav`, a
committed English clip. Measuring on English matters: on the long German
capture the unified model emits nothing at all, so its decoder loop never
runs and its timings look better than they are.

| audio | TDT v3 greedy | unified greedy |
|---|---|---|
| 2 s | 1.60 s (1.2×) | 1.59 s (1.3×) |
| 5 s | 2.32 s (2.2×) | 4.13 s (1.2×) |
| 10 s | 4.69 s (2.1×) | 4.05 s (2.5×) |
| 20 s | 7.17 s (2.8×) | 7.64 s (2.6×) |

The two are the same speed within the scatter this load produces. Over the
whole corpus replay, 1845 s of decode against TDT v3's 1607 s, i.e. about
15% slower for the same audio.

### Why: German is gone

`test_wavs/de.wav`, against its published reference
"Raptorium Bergbau scheint profitierter als Monroe als Reaktion auf die
wirtschaftlichen Ausfälle zu sein.":

| model | output |
|---|---|
| Qwen3-ASR 0.6B | exact |
| Parakeet TDT v3 | exact but for one hyphen and its case |
| **parakeet-unified-en-0.6b** | "Rapturiumberg Bauschein Provita Els Monroe alreaction of the Wirtschaften Ausfellow Susan" |

The model is English-only and it does not degrade gracefully: it either
transliterates German into English words or returns nothing at all. On the
first 20 s of one capture — German, clear, one speaker — Parakeet TDT v3
returns 51 words and `parakeet-unified` returns the empty string.

The author's own captures contain German. That is what the 16 lost chunks and
the 1375 missing words above are.

### Should the greedy default change when the model is not TDT?

**No, and not by detection.** sherpa-onnx decides TDT by reading the
encoder's `url` metadata and looking for the string `tdt`
(`offline-transducer-nemo-model.cc`). Both models' metadata otherwise agree —
both say `model_type = EncDecRNNTBPEModel`, `feat_dim 128`,
`subsampling_factor 8` — and differ only in `vocab_size` (8192 against 1024)
and that `url`:

| key | Parakeet TDT v3 | parakeet-unified |
|---|---|---|
| `url` | `…/nvidia/parakeet-tdt-0.6b-v3` | `…/nvidia/parakeet-unified-en-0.6b` |
| `vocab_size` | 8192 | 1024 |

So a reliable signal exists, but it is an ONNX `metadata_props` entry at the
tail of a 654 MB protobuf, and sherpa-onnx exposes no accessor for it through
the C API. Reading it would mean spokenpad parsing ONNX itself, to decide a
default the config can already state in one line. That is not small, so it is
not implemented.

Nothing else needs to change either. The existing rule — a non-empty
`vocabulary` selects `modified_beam_search`, since that is the only search
with hotwords — is already the right rule for both models. It is the model
choice that decides whether that search is safe, and the model is the user's
to choose.

## Conclusion

**The question this experiment asked is answered yes, and the model it
answered it with cannot be the default.**

The hypothesis held completely: the beam-search defect is TDT-only. On a NeMo
RNNT that is not TDT, `modified_beam_search` returns text where TDT's returns
nothing, hotwords demonstrably bias the output, and the model loads through
the existing `parakeet` family with one config line and no code change. On
the five local clips it more than halves the aggregate word error rate,
18.7% to 8.2%.

Then the corpus replay contradicted it. Over the author's 176 real captures
the same model loses 16 chunks outright — TDT v3 loses none — and returns
20% fewer words, because a share of those captures is German and this model
is English-only. The user asked for the best accuracy **with no lost
sentences**; this model buys the first by giving up the second.

So it is a candidate only under a condition the user has to state: that
dictation is English, always. If it is, it is a strong one and the corpus
run should include it. If it is not, the interesting result here is the
general one — that leaving TDT is what brings hotwords back — and the
search should be for a **multilingual** non-TDT transducer, which this
experiment did not find.

Three things this did not measure: word error rate against real references
(the corpus harness with Gladia transcripts will), how much of the corpus is
German, and anything on an idle machine.

This experiment changes no default and adds no `decisions.md` entry about
one.

The config file for a corpus run is
`~/.cache/spokenpad-dev/models/configs/parakeet-unified-greedy.toml`, with
`-beam` and `-hotwords` beside it.
