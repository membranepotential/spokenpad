# parakeet-unified-en-0.6b over the English half of the corpus

_2026-09-21, `examples/corpus.rs` at 24f42df. sherpa-onnx 1.13.8, CPU,
6 threads, 1 s trailing silence, live path. Timings taken while other agents
ran on the machine and compare only runs of this batch._

## Question

[parakeet-unified-en-0.6b](2026-09-21-parakeet-unified-rnnt.md) is an English-only
NeMo RNNT, not a TDT, so sherpa-onnx's beam search works on it and hotwords
bias it. Measured on five clips it looked good. What would a user who only ever
dictates English actually gain by switching to it — and what would they lose?

## Method

The English captures of the corpus only (`--language en`), live path, against
the committed default on the same 124 captures.

```sh
corpus --config ~/.cache/spokenpad-dev/models/configs/parakeet-unified-greedy.toml \
       --corpus eval-samples/local --path live --language en \
       --jsonl english.jsonl --label unified-greedy --jobs 2
```

`parakeet-unified-beam.toml` is the same model with
`decoding = "modified_beam_search"` and an empty vocabulary; `greedy.toml` is
the committed default (Parakeet TDT 0.6B v3 int8, `greedy_search`).

Qwen3-ASR was skipped: it needs a family patch that is deliberately not merged,
and it decodes at 0.4–0.7× real time.

## Data

The 124 English captures of the local corpus of 2026-09-21: 5383 reference
words. References are ASR, not ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)), which
matters more than usual here — see the conclusion.

## Results

| model | decoding | WER | e1 | e2 | "Yeah." | captures losing >5 words at the end | words lost | wall |
|---|---|---|---|---|---|---|---|---|
| Parakeet TDT v3 (default) | greedy | 9.31% | 3 | 1 | 0 | **0** | **3** | 1100 s |
| parakeet-unified-en | greedy | **7.91%** | 1 | 1 | 0 | 1 | **41** | 547 s |
| parakeet-unified-en | beam | **7.71%** | 2 | 1 | 0 | 1 | **41** | 756 s |

Paired bootstrap over captures, 5000 resamples, against the default:

| model | WER difference | 95% interval | captures better / worse / equal |
|---|---|---|---|
| unified greedy | −1.39 points | [−2.51, −0.33] | 52 / 27 / 45 |
| unified beam | −1.60 points | [−2.75, −0.54] | 51 / 24 / 49 |

Per-file WER: the default's median is 8.3% with 15 captures at or above 40%;
unified greedy's median is 5.3% with 5.

Beam search on this model behaves: no "Yeah." commits, one extra chunk empty at
the first decode, and the retry rescues it. That is the opposite of beam search
on TDT ([greedy against beam](2026-09-21-greedy-vs-beam-corpus.md)), and it
matches what the five-clip study found.

**The whole lost-tail difference is one capture.** On a 23 s English capture of
39 reference words, both unified runs returned **nothing at all** — the one
chunk decoded empty and the retry without trailing silence did not rescue it —
while the default returned six words for the same audio, wrong enough to score
95%. Neither model transcribed that capture; one at least left something in the
file. Every other English capture kept its ending under all three.

## Conclusion

**An English-only user would gain about 1.5 WER points**, an interval that
excludes zero, better on twice as many captures as it is worse on, a third of
the very bad captures, and roughly half the decode time. Beam search is usable
on it, which the default cannot offer, so hotword biasing becomes available
without the defect that makes it unusable on TDT.

**It is still not the default**, for the reason the corpus exists to show: 24%
of the author's reference words are German, and this model has no German at
all. Offering it means offering a choice, not changing one.

Read the 1.5 points with the reference's bias in mind. Gladia's transcripts are
themselves 17.0% wrong on the five hand-checked clips, almost entirely on
technical vocabulary, so part of any gain here may be a model that writes more
like Gladia rather than one that hears better. The gain is large enough to
survive that doubt; it is not large enough to quote to three digits.

Open: whether `asr.vocabulary` on this model actually fixes the technical words
the corpus penalises — the one thing beam search is for, and the one thing this
run did not use, because the author configures no vocabulary.
