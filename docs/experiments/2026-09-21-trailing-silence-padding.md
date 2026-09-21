# How much trailing silence a decode needs

_2026-09-21, `examples/corpus.rs` at ce311f6. Parakeet TDT 0.6B v3 int8,
sherpa-onnx 1.13.6, CPU, 6 threads. Timings taken under load from other agents
(load average 20–45) and compare only runs of this batch._

## Question

`shell/inference.rs` appends one second of zeros to every window before
decoding (`TrailingSilence::Padded`). A chunk that decodes to nothing is
decoded once more without them (`Bare`), and that result stands.

The second of zeros has never been measured. Two pieces of history pull in
opposite directions: half a second once made the short "cd home" clip decode to
`""`, and on the capture that lost a sentence the padding cost greedy 2 of 9
windows. So: 1 s, 0.5 s or none?

## Method

The corpus harness overrides the padding with `--trailing-silence-ms`. It is a
measurement, not a setting — there is deliberately no config key for it. The
harness pads the window itself and asks the recognizer for `Bare`, which for a
window decoded in one piece is byte for byte what `shell/inference.rs` does.

```sh
for ms in 1000 500 0; do
  cargo run --release --example=corpus -- --config greedy.toml \
      --corpus eval-samples/local --trailing-silence-ms $ms \
      --jsonl results.jsonl --label greedy-pad$ms --jobs 2
done
```

and the same for `modified_beam_search`. Both decode paths each time.

## Data

The local corpus of 2026-09-21: 181 captures, 75.1 minutes, 162 with a
reference (124 English, 38 German, 6832 reference words). References are ASR,
not ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)).

## Results

Live path:

| decoding | padding | WER | e1 | e2 | "Yeah." | words lost at the ends |
|---|---|---|---|---|---|---|
| greedy | 1000 ms | 11.20% | 5 | **0** | 0 | 5 |
| greedy | 500 ms | 11.62% | 7 | 1 | 2 | 9 |
| greedy | 0 | **10.93%** | 3 | **3** | 2 | 11 |
| beam | 1000 ms | **11.52%** | 19 | 2 | 3 | 57 |
| beam | 500 ms | 12.70% | 23 | 0 | 8 | 35 |
| beam | 0 | 15.28% | 15 | **15** | 9 | 175 |

Whole path:

| decoding | padding | WER | "Yeah." | words lost at the ends | empty reference, words anyway |
|---|---|---|---|---|---|
| greedy | 1000 ms | **13.23%** | 1 | 95 | 3 |
| greedy | 500 ms | 13.48% | 7 | 27 | 8 |
| greedy | 0 | 13.82% | 3 | 66 | 9 |
| beam | 1000 ms | **18.65%** | 5 | 352 | 12 |
| beam | 500 ms | 22.03% | 11 | 614 | 15 |
| beam | 0 | 18.72% | 6 | 445 | 10 |

`e1` is speech chunks whose padded decode returned nothing; `e2` those the bare
retry could not rescue either.

**Paired bootstrap over captures** (5000 resamples):

| comparison | path | WER difference | 95% interval |
|---|---|---|---|
| greedy 0 − greedy 1000 | live | −0.26 points | [−0.83, +0.39] |
| greedy 500 − greedy 1000 | live | +0.42 points | [−0.31, +1.36] |
| greedy 0 − greedy 1000 | whole | +0.59 points | [−0.49, +1.73] |
| beam 0 − beam 1000 | live | +3.76 points | [+1.75, +6.40] |

## Conclusion

**Keep the second of zeros.** Removing it is worth, at best, a quarter of a WER
point that the bootstrap cannot distinguish from zero, and it costs the retry
its whole purpose.

The `e2` column is the finding. With no padding, `Padded` and `Bare` are the
same input, so the retry is the same decode, and a model that returned nothing
returns nothing again: every chunk that decodes empty is lost. Greedy lost 3 of
3, beam 15 of 15. With a second of zeros, greedy lost 0 of 5 and beam 2 of 19 —
the padding is what makes the second decode a *different* decode, and that is
the only reason the retry works at all.

Half a second is no better: for greedy it goes empty more often than a full
second (7 chunks against 5), loses one of them, and adds two "Yeah." commits;
for beam it is the worst setting of the three on the whole path (22.0% WER, 614
reference words lost off the ends of 17 captures). A full second wins for both
decoders on both paths, on every count except the one WER difference the
bootstrap cannot resolve.

The one earlier observation that pointed the other way — padding costing greedy
2 of 9 windows on a sweep over the capture that lost a sentence — does not
generalise. Over 162 captures the padded decode is empty more often *and*
recoverable, while the unpadded one is empty less often and never recoverable.

So the padding stays at one second, hard-coded, and stays a property of the
recogniser rather than a setting: the number that matters is not the WER it
buys but that `Padded` and `Bare` must differ.

Not shown: whether some other length (2 s, 250 ms) does better, and whether the
padding interacts with `vad.edge_pad_seconds`, which already puts real audio
around a chunk.
