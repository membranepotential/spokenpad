# Whisper tiny.en and SenseVoice over the local corpus

_2026-09-21, `examples/corpus.rs` at ce311f6. sherpa-onnx 1.13.6, CPU,
6 threads, 1 s trailing silence. Timings taken under load from other agents
(load average 20–45) and compare only runs of this batch._

## Question

`docs/asr.md` lists Whisper tiny.en at 23.0% WER and SenseVoice at 29.4%
against Parakeet's 18.7%, measured on five clips of 182 words. Over an hour of
real dictation, is either of them a serious alternative?

## Method

One run each, both decode paths, alongside the Parakeet runs of the same batch
([greedy against beam](2026-09-21-greedy-vs-beam-corpus.md)).

```toml
# whisper.toml
[asr]
family = "whisper"
model_dir = "~/.local/share/spokenpad/models/sherpa-onnx-whisper-tiny.en"
num_threads = 6

# sensevoice.toml
[asr]
family = "sense_voice"
model_dir = "~/.local/share/spokenpad/models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09"
num_threads = 6
language = "en"
```

```sh
cargo run --release --example=corpus -- --config whisper.toml \
    --corpus eval-samples/local --jsonl results.jsonl --label whisper-tiny-en --jobs 2
```

## Data

The local corpus of 2026-09-21: 181 captures, 75.1 minutes, 162 with a
reference (124 English with 5383 words, 38 German with 1449, and 19 captures
nobody spoke in). References are ASR, not ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)).

## Results

Read the **English** column. Whisper tiny.en is English-only and SenseVoice was
given `language = "en"`, so their German rows measure what an English model
does to German speech, not how well they transcribe it.

Live path:

| model | English WER | German WER | "Yeah." | words lost at the ends | wall | real time |
|---|---|---|---|---|---|---|
| Parakeet, greedy | **9.6%** | **17.3%** | 0 | 5 | 1019 s | 4.4× |
| Parakeet, beam | 10.2% | 16.4% | 3 | 57 | 1357 s | 3.3× |
| Whisper tiny.en | 15.8% | 122.9% | 0 | 4 | 417 s | 10.8× |
| SenseVoice, en | 23.3% | 100.5% | 0 | 42 | 359 s | 12.6× |

Whole path:

| model | English WER | German WER | words lost at the ends | empty reference, words anyway (of 19) |
|---|---|---|---|---|
| Parakeet, greedy | **11.9%** | **18.2%** | 95 | 3 |
| Whisper tiny.en | 15.0% | 126.7% | 2 | **18** |
| SenseVoice, en | 23.4% | 101.9% | 672 | **19** |

Per-file WER, live path: Parakeet greedy median 8.3%, 55 of 162 captures under
5%; Whisper median 19.4%, 53 captures at or above 40%; SenseVoice median 28.6%,
58 captures at or above 40%.

## Conclusion

**Parakeet stays the default.** Whisper tiny.en costs 6 WER points of English
and cannot transcribe the author's German at all; SenseVoice costs 14 and is
worse than that on every distribution measure. Neither is close enough for the
2–3× speed to matter for a decode fired once per utterance.

Two things the five clips could not show:

- **Both alternatives hallucinate into silence.** Decoded whole, Whisper wrote
  words into 18 of the 19 captures nobody spoke in and SenseVoice into all 19,
  against Parakeet's 3. The VAD keeps most of that out on the live path (2 each),
  which is the reason the VAD exists
  ([decisions.md](../decisions.md)), but a model that invents text from room
  noise is a poor fit for a tool whose output goes straight into a file.
- **SenseVoice loses the ends of captures.** 672 reference words off the ends of
  its whole-path hypotheses, against Parakeet's 95.

Whisper's 15.0–15.8% on English is respectable for 100 MB, and its lost-tail
count is the lowest of any model here (2 and 4 words). If a small model is ever
wanted, it is the one to look at again — but not as the default.

The figures in `docs/asr.md` (23.0% and 29.4%) came from five clips heavy in
technical vocabulary and are not comparable with these; both sets now exist, and
this one is over 37 times as many reference words.
