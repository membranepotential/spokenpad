# How often the end-of-slice chunk close flips a chunk between empty and not

_2026-09-22, `main` at fd36708 against the same with one rule reverted.
Parakeet TDT 0.6B v3 int8, greedy, sherpa-onnx 1.13.8, 1 s trailing silence,
live path, 1.1 s tick. Timings taken while another agent built on the machine
(load average 11–14)._

## Question

[The lead-padding clamp experiment](2026-09-21-lead-padding-clamp-corpus.md#which-change-loses-the-chunk-2026-09-22)
traced the one chunk `main` loses (`capture-2026-09-18-231741`) to the
**end-of-slice chunk close** from 9f632f9: a pending chunk followed by a pause
of `settling_silence` at the end of the slice is closed there, instead of
waiting for the next span. Reverting that one rule restored the chunk. It left
open how often the rule changes whether a chunk decodes to nothing, in either
direction, over the whole corpus. One capture cannot say whether the rule loses
speech more often than it saves it.

**What would change a decision**, written before the run: if the revert loses
fewer chunks than `main` over 181 captures, and loses no chunk of its own, the
rule costs speech on real dictation and a guard is worth designing (settle a
chunk only once it holds some minimum of speech). If the revert loses chunks
`main` keeps, or the counts tie at one, the rule is a wash and stays as it is.

## Method

A scratch tree from `git archive fd36708`, not kept, with one line removed
from `merge_spans` in `src/core/segments.rs`:

```rust
c.closed |= len.saturating_sub(c.end) >= split_silence;
```

(and the `mut` on the binding it needed). `diff` against the tree shows those
two lines and nothing else. Built against the same sherpa-onnx 1.13.8 static
library as `main`. One run over the whole corpus:

```sh
corpus --config greedy.toml --corpus eval-samples/local --path live \
       --jobs 2 --all --with-text --jsonl runs/flips.jsonl --label no-slice-close
```

`main`'s side is the recorded `new-clamp` run in `runs/clamp.jsonl`; the
[dev subset run](2026-09-22-dev-subset.md) reproduced that recording word for
word on its 28 captures today, so it stands for `main`. The two runs were
compared capture by capture on the counts, and word by word on the committed
text: a word diff of the two hypotheses, counting the words only one build
wrote. Chunk boundaries differ between the builds, so a chunk of one build has
no partner in the other; the flip counts below are per capture.

## Data

The local corpus, dataset `2026-09-21.1`: 181 captures, 75.1 minutes, 6832
reference words.

## Results

### Totals

| | WER | edits | hyp words | e1 | e2 | lost at ends | dup (words) | committed chunks | wall |
|---|---|---|---|---|---|---|---|---|---|
| `main` | 10.85% | 741 | 7028 | 5 | **1** | 5 | 4 (6) | 333 | – |
| end-of-slice close reverted | 11.20% | 765 | 7008 | 6 | **0** | 5 | 4 (5) | 336 | 456 s |
| old code (`ce311f6`, sherpa 1.13.6), recorded | 11.20% | 765 | 7008 | 5 | 0 | 5 | – | – | – |

### The revert is the old code, word for word

With only this rule reverted, the committed text is **identical to the old
code's on all 181 captures**: same WER, same edits, same hypothesis on every
capture. The one count that differs is a first-decode empty on
`capture-2026-09-18-225810` that the retry rescues, so no word differs.

So the whole difference between the old code and `main`, the 0.35-point gain
and the one lost chunk alike, comes from the end-of-slice close. sherpa-onnx
1.13.8, the `preview.max_seconds` tick bound, the settled-silence advance and
the lead clamp change no committed word on this corpus once the close is
reverted. That answers the question the clamp experiment left open ("which of
the remaining differences causes the extra lost chunk, or the WER gain").

### Flips

Captures where a chunk is empty in one build and not in the other:

| capture | dur | e1 `main` → revert | e2 `main` → revert | edits `main` → revert |
|---|---|---|---|---|
| `capture-2026-09-18-231741` | 232 s | 2 → 1 | **1 → 0** | 13 → 19 |
| `capture-2026-09-14-140747` | 91 s | 0 → 1 | 0 → 0 | 17 → 15 |
| `capture-2026-09-18-225810` | 130 s | 0 → 1 | 0 → 0 | 5 → 6 |

- **Chunks lost after the retry:** `main` loses 1 the revert keeps; the revert
  loses none that `main` keeps.
- **Chunks empty at the first decode:** `main` has 1 the revert does not
  (231741); the revert has 2 that `main` does not (140747, 225810). All three
  of those are rescued by the retry.
- **Words:** the committed text differs on **12** captures. The word diff
  finds 67 words only `main` wrote and 47 only the revert wrote; `main` writes
  20 more words in all (7028 against 7008). The lost chunk itself costs 3
  words of hypothesis (276 against 279) on its capture.
- **Edits:** `main` is better on 9 of the 12, the revert on 2, 1 ties; 741
  against 765 edits. The capture that loses the chunk is one of the 9: 13
  edits against 19.

## Conclusion

The pre-registered criterion is met in the letter: the revert loses fewer
chunks (0 against 1) and loses none of its own. But the same run shows that
the rule is also the whole of `main`'s WER gain, on 9 captures against 2, and
that the capture where it loses the chunk still scores better with it. A plain
revert would trade 24 edits for one 0.6 s chunk.

**Keep the rule; a guard is worth designing only if it keeps the gain.** The
guard the clamp experiment named — settle a chunk only once it holds some
minimum of speech — is now testable against a known answer: on this corpus it
must bring `e2` to 0 on `capture-2026-09-18-231741` while leaving the other 11
changed captures as `main` has them. The dev subset holds 231741, 140747 and
163026, so a candidate guard can be screened there in two minutes before one
full run confirms it. No guard was written here.

What this does not show: whether the rule helps because it closes chunks
earlier (shorter windows) or because it moves where later windows start; the
word diff counts changed words but does not attribute them. One lost chunk in
333 is also too rare to rank against the gain with any interval.
