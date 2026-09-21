# Greedy against modified_beam_search over the whole local corpus

_2026-09-21, `examples/corpus.rs` at ce311f6. Parakeet TDT 0.6B v3 int8,
sherpa-onnx 1.13.6, CPU, 6 threads. Timings taken while other agents built and
decoded on the same machine (load average 20–45) and compare only runs of this
batch._

## Question

On 2026-09-21 Parakeet's decoding was switched from `modified_beam_search` to
`greedy_search` because beam search returned `""` or an invented "Yeah." for
clear speech and lost a whole sentence of the author's
([decisions.md](../decisions.md#greedy-decoding-by-default-beam-search-drops-speech-2026-09-21)).
On the five eval clips beam scores *better* (15.4% against greedy's 18.7%), so
the decision rested on one capture and a window sweep. Does an hour of real
dictation support it?

## Method

```sh
cargo run --release --example=corpus -- --config greedy.toml \
    --corpus eval-samples/local --jsonl results.jsonl --label greedy-pad1000 --jobs 2
cargo run --release --example=corpus -- --config beam.toml \
    --corpus eval-samples/local --jsonl results.jsonl --label beam-pad1000 --jobs 2
```

The two configs differ in one line:

```toml
[asr]
family = "parakeet"
model_dir = "~/.local/share/spokenpad/models/parakeet-tdt-0.6b-v3-int8"
num_threads = 6
decoding = "greedy_search"          # or "modified_beam_search"
```

**No hotwords.** The author's `~/.config/spokenpad/config.toml` has no `[asr]`
table at all, so `asr.vocabulary` is empty and there is no configured
vocabulary to test beam search with. Beam search is therefore compared as it
would actually run today: as a decoder, buying nothing.

Both decode paths ran (`live` and `whole`; see
[evaluation.md](../evaluation.md#the-two-paths)).

## Data

The local corpus of 2026-09-21: 181 captures, 75.1 minutes, 162 of them with a
reference (124 English, 38 German, 6832 reference words) and 19 that Gladia
heard no speech in. References are ASR, not ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)).

## Results

| run | path | WER | en | de | hyp words | e1 | e2 | "Yeah." | captures losing >5 words at the end | words lost | empty ref, words anyway | wall |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| greedy | live | **11.2%** | 9.6% | 17.3% | 7008 | 5 | 0 | 0 | 0 | **5** | 2 | 1019 s |
| beam | live | 11.5% | 10.2% | 16.4% | 6894 | 19 | 2 | 3 | 2 | **57** | 2 | 1357 s |
| greedy | whole | **13.2%** | 11.9% | 18.2% | 6790 | – | – | 1 | 3 | **95** | 3 | 624 s |
| beam | whole | 18.6% | 19.0% | 17.5% | 6321 | – | – | 5 | 9 | **352** | 12 | 1286 s |

`e1` is speech chunks whose padded decode returned nothing, `e2` those the bare
retry could not rescue either; both are structurally zero on the `whole` path.

Per-file WER, live path: greedy median 8.3%, p75 18.5%, p90 33.3%; beam median
8.3%, p75 16.7%, p90 33.3%. The distributions are the same shape.

**Paired bootstrap over captures** (5000 resamples, greedy minus beam):

| path | WER difference | 95% interval | greedy better / worse / equal |
|---|---|---|---|
| live | −0.32 points | [−1.22, +0.47] | 21 / 33 / 108 |
| whole | −5.42 points | [−10.47, −1.52] | 34 / 23 / 105 |

So on the live path the WER difference is **inside the noise** — and the
counts beside it are not:

- Beam left 19 speech chunks empty against greedy's 5, and 2 of those survived
  the retry without trailing silence. Those 2 are speech that is simply gone.
- Beam lost 57 reference words off the ends of captures; greedy lost 5.
- Beam produced 3 commits that were nothing but "Yeah."; greedy produced none.

On the whole path, where no VAD boundary limits the damage, the same failure
takes whole captures. Ten captures where greedy lost nothing at the end and
beam lost nearly everything:

| reference words | words beam lost at the end |
|---|---|
| 244 | 106 |
| 77 | 30 |
| 67 | 44 |
| 36 | 36 |
| 22 | 22 |
| 17 | 17 |

Beam also wrote words into 12 of the 19 captures nobody spoke in, against
greedy's 3.

Beam is slower as well: 2686 s of decode against 2006 s on the live path, 2527 s
against 1227 s on the whole path.

## Conclusion

**Keep `greedy_search` as the default.** The corpus confirms the decision the
single lost sentence prompted, and shows why the five eval clips said the
opposite: with 182 reference words they cannot hold a capture whose last
sentence never arrived, which is the failure that matters.

The WER numbers alone would call the live path a tie. Word error rate charges
one edit per missing word, so a hundred captures nearly right drown out two
that lost a sentence — exactly the trade a dictation tool must not make. The
`lost`, `e1`/`e2` and `yeah` counts are what separates the two decoders, and
they do not need the reference to be right about the words.

What this does **not** show: how beam search behaves with a non-empty
`asr.vocabulary`, which is the only reason it exists. sherpa-onnx has hotwords
only in beam search, so a user who wants vocabulary biasing still accepts these
numbers. Whether that trade is worth it is open, and needs a corpus run with a
real vocabulary — the author configures none.

The upstream bug is [k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267)
(open).
