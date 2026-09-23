# Does a longer `vad.pad_seconds` lower the live path's word error rate?

_2026-09-23, `main` at d94581f. Parakeet TDT 0.6B v3 int8, greedy, sherpa-onnx
1.13.8, 6 threads, live path, 1.1 s preview tick (previews skipped), 2 jobs.
Other agents built on the machine at the same time (12 cores); timings are
labelled accordingly._

## Question

The 2026-09-23 decode-fixes experiment
([2026-09-23-decode-fixes-corpus.md](2026-09-23-decode-fixes-corpus.md))
found that at a forced 10 s `preview.max_seconds` the whole-tail build loses
0.52 WER points against `main`, and suspected the padding without isolating
it: a chunk that ends a slice gets `vad.edge_pad_seconds` (2 s) of trailing
context, one followed by more speech in the open tail gets only
`vad.pad_seconds` (0.5 s). Does raising `vad.pad_seconds` toward
`edge_pad_seconds` lower the live path's WER at the default
`preview.max_seconds` of 30 s?

## Method

`examples/corpus.rs`, three configs differing only in `[vad] pad_seconds`
(0.5, the default; 1.0; 2.0), otherwise `config.example.toml`'s defaults
(`asr.num_threads = 6`; `asr.model_dir` and `vad.model` left out, so the
already-downloaded default models are used). Configs written to the
scratchpad, not committed; no source file touched.

```sh
corpus --config pad-X.toml --path live --subset dev --jobs 2 \
       --all --jsonl dev-pad-X.jsonl --label dev-pad-X
```

Per-capture comparison used the JSON lines' numeric fields (edits, e1/e2,
seam repeats, lost-tail words, committed chunk count) only; `--with-text`
was never passed, so no reference or hypothesis text was written anywhere.
The decode path is deterministic: the 2026-09-22 dev-subset and 2026-09-23
decode-fixes experiments each reproduced every count exactly across separate
builds and runs, so one run per configuration is compared directly, with no
repeat-run baseline measured here.

## Data

The local corpus's dev subset, dataset `2026-09-21.1`: 28 captures, 17.8
minutes, 1424 reference words. No full-corpus run followed: the dev subset
already shows a clear, monotonic result, and the default wins outright, so
there is no other setting to confirm at the release gate.

## Results

| `pad_seconds` | WER | edits | e1 | e2 | lost-tail words (captures > 5) | seam repeats (words) | chunks | wall |
|---|---|---|---|---|---|---|---|---|
| 0.5 (default) | 14.19% | 202 | 5 | 1 | 5 (0) | 4 (6) | 77 | 121 s |
| 1.0 | 14.96% | 213 | 5 | 1 | 11 (1) | 16 (18) | 77 | 118 s |
| 2.0 | 15.73% | 224 | 5 | 1 | 11 (1) | 16 (24) | 77 | 126 s |

Per capture, against the default (every capture not listed commits
identical text at all three settings):

- Nine captures lose 1-7 edits at both 1.0 and 2.0, all to more repeated
  words at a chunk seam: seam-repeat events rise from 4 to 16, seam-repeat
  words from 6 to 18 (1.0) or 24 (2.0). This is the largest single driver of
  the WER increase.
- One capture gains 3 edits back at both settings
  (`capture-2026-09-17-193035.wav`).
- One capture, `capture-2026-09-14-140747.wav` (91 s, German), loses a
  chunk at both 1.0 and 2.0: 7 committed chunks at the default become 6, one
  segment newly decodes to nothing on the first pass and stays empty after
  the bare retry (e1 0->1, e2 0->1), and its lost-tail count rises from 0 to
  6 words.
- One capture, `capture-2026-09-18-231741.wav` (232 s), moves the other way
  on empty decodes: at the default it has 2 chunks empty on the first pass,
  1 still empty after the retry (e1 2, e2 1); at 1.0 and 2.0 both recover
  (e1 0, e2 0) and gain a 13th committed chunk. But its seam repeats rise
  from 1 to 3 (1.0) or 3 (2.0), so it still costs 3 (1.0) or 7 (2.0) edits
  net. The aggregate e1 (5) and e2 (1) counts are unchanged only because
  this capture's gain offsets `capture-2026-09-14-140747.wav`'s loss.
- No capture reaches a different chunk *boundary*: `merge_spans`'s settling
  silence is `2 x max(edge_pad_seconds, pad_seconds)`
  (`src/core/segments.rs`), which stays at `2 x 2.0 s` for every
  `pad_seconds` tested here, all `<= edge_pad_seconds`. Only the length of
  context each chunk's decode window carries changes, not where the VAD
  cuts.

## Conclusion

**No: raising `vad.pad_seconds` raises WER on the dev subset, monotonically,
mostly by writing more words twice at chunk seams, though it also moves one
capture off and one capture onto the empty-decode knife edge**
([2026-09-22-empty-chunk-flips.md](2026-09-22-empty-chunk-flips.md)). The
default (0.5 s) already wins outright over both alternatives tested, so no
full-corpus run was needed.

The mechanism: a chunk followed by more speech gets `pad_seconds` of that
future speech appended to its decode window as trailing context
(`merge_spans` in `src/core/segments.rs`). A wider window makes it more
likely the recognizer transcribes some of that future speech as this
chunk's own words, and the next chunk, which starts at the same speech
boundary regardless of padding, decodes and commits the same words again.

This does not overturn the decode-fixes finding that padding is the likely
cause of `main`'s edge at a forced 10 s preview window: there, the
*shorter* window's *last* chunk in an open tail gets the *wider*
`edge_pad_seconds`, and the gain came from more trailing context on a chunk
with no more speech after it yet. The two findings agree on the mechanism
in opposite directions: more trailing context helps a chunk that is the
last one in an open, unsettled tail, and hurts a chunk that has more
speech right after it. Widening `pad_seconds` gives every ordinary chunk
the edge case's context length, including the chunks in the middle of a
capture where that length causes the seam repeats measured here.

**Decision: keep `vad.pad_seconds` at its default, 0.5 s.** No
`decisions.md` entry follows: the default is unchanged. Still open, from
the decode-fixes experiment: what actually explains `main`'s gain at a
forced 10 s preview window, and a corpus replay with the daemon's silence
timeout.
