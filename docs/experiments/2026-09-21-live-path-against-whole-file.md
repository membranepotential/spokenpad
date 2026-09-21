# The live path against whole-file decoding, over the corpus

_2026-09-21, `examples/corpus.rs` at ce311f6. sherpa-onnx 1.13.6, CPU,
6 threads, 1 s trailing silence. Timings taken under load from other agents._

## Question

On the five eval clips, decoding a clip whole scores better than the daemon's
VAD-segmented path: 14.3% against 18.7% for greedy
([evaluation.md](../evaluation.md#what-the-numbers-mean)). Progressive commit
has therefore always been defended as a latency feature that costs a little
accuracy. Does an hour of real dictation agree?

## Method

Every configuration of the 2026-09-21 batch ran both paths in the same process,
on the same captures, with the same references — the only difference is
`--path live` against `--path whole`. This is a by-product of those runs, not a
separate experiment; see
[greedy against beam](2026-09-21-greedy-vs-beam-corpus.md),
[trailing silence](2026-09-21-trailing-silence-padding.md) and
[model families](2026-09-21-model-families-corpus.md).

## Data

The local corpus of 2026-09-21: 181 captures, 75.1 minutes, 162 with a
reference (6832 words, 124 English and 38 German). References are ASR, not
ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)).

## Results

| configuration | live WER | whole WER | live words lost at the ends | whole words lost at the ends |
|---|---|---|---|---|
| Parakeet greedy, 1 s | **11.2%** | 13.2% | **5** | 95 |
| Parakeet greedy, 0.5 s | **11.6%** | 13.5% | **9** | 27 |
| Parakeet greedy, no padding | **10.9%** | 13.8% | **11** | 66 |
| Parakeet beam, 1 s | **11.5%** | 18.6% | **57** | 352 |
| Parakeet beam, 0.5 s | **12.7%** | 22.0% | **35** | 614 |
| Parakeet beam, no padding | **15.3%** | 18.7% | **175** | 445 |
| Whisper tiny.en | 38.5% | 38.7% | **4** | 2 |
| SenseVoice | **39.7%** | 40.1% | **42** | 672 |

(The Whisper and SenseVoice aggregates mix English and German and mean little
on their own; the comparison between their two columns still holds.)

Captures nobody spoke in that the model wrote words into, out of 19:

| configuration | live | whole |
|---|---|---|
| Parakeet greedy, 1 s | **2** | 3 |
| Parakeet beam, 1 s | **2** | 12 |
| Whisper tiny.en | **2** | 18 |
| SenseVoice | **2** | 19 |

## Conclusion

**The live path is not a trade — it is better on this corpus, on every
configuration.** It wins WER in 8 of 8, by 0.2 to 9.3 points, and loses far
fewer reference words off the ends of captures (5 against 95 for the default
configuration).

Two reasons, both visible in the counts:

- **A VAD chunk bounds the damage.** When Parakeet returns nothing or truncates,
  it loses one chunk of up to `vad.chunk_seconds`, not the rest of the capture.
  Whole-file decoding puts a whole five-minute dictation on one decode.
- **Silence is not decoded.** The VAD hands the recognizer only what it heard
  speech in, so a capture of room noise produces no decode at all. Decoded
  whole, every model in the batch invented words from silence, Whisper and
  SenseVoice in nearly every such capture.

So the five clips were misleading in both directions, and for the same reason:
182 reference words of clean, short, deliberate speech cannot contain the
failure — a lost chunk or a lost ending — that decides between these two paths.
Whole-file decoding looks better there because segmentation costs a little
context on a clip that never breaks; on real captures that cost is repaid many
times over.

This does not change any behaviour: the daemon already uses the live path, and
`vad.enabled = true` is already the default for the reasons
[decisions.md](../decisions.md) records. It removes the standing caveat that
progressive commit costs accuracy. `docs/evaluation.md` and
`config.example.toml` still quote the five-clip figures for that comparison and
should be read with this file beside them.
