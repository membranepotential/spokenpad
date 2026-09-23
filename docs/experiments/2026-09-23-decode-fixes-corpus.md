# Do the 2026-09-22 decode fixes change what the corpus commits?

_2026-09-23. `main` at 06043fc against this branch at 4321c76 (the
`max_seconds = 20` runs at 8db528a, which changes only the editor), plus
the window-tick count in `examples/corpus.rs` (committed with this file).
Parakeet TDT 0.6B v3 int8, greedy, sherpa-onnx 1.13.8, 6 threads, 1 s
trailing silence, live path, 1.1 s tick of audio, previews skipped, 2 jobs.
Timings taken while the user worked on the machine (load average 10–11)._

## Question

Since `main`, the branch changes the decode path in three ways: a window
tick (a tick over the first `preview.max_seconds` of a longer tail) in which
nothing settles is committed through its last pause, or whole when it has
none (P1-001); every segment of such a commit and of a release commits at its
own speech end (P1-002); and `merge_spans` returns that pause. Do these
change any committed word on the local corpus, and do the cuts inside a
window lose or repeat words at its edges?

The review of the branch also found that the harness itself ticked every
tail as a window without `--previews`, so its numbers were not the daemon's.
That is fixed first (4321c76), and every branch number below uses the fixed
harness.

**What would change a decision**, written before the runs: the fixes ship
if they lose no chunk after the retry, lose no words at the ends and write
no word twice more often than `main` on the default configuration. If only
a few captures reach the new path, their counts are reported on their own.

## Method

Each build's own `examples/corpus.rs`, with the same configuration file
(`[asr]` as in `eval-samples/local/runs/configs/greedy.toml`, without
`family`, which the branch no longer accepts):

```sh
corpus --config greedy.toml --corpus eval-samples/local --path live \
       --subset dev --jobs 2 --all --with-text --jsonl dev.jsonl --label dev-{main,new}
corpus --config greedy.toml --corpus eval-samples/local --path live \
       --jobs 2 --all --with-text --jsonl full.jsonl --label full-{main,new}
```

`main` was built in a scratch worktree with its own `target/`, since
removed. The two runs were compared capture by capture on every count and on
the committed chunks, in memory. The run files, which hold hypothesis text,
were deleted after the comparison.

The branch's harness counts the ticks that read a window of a longer tail
(`ticks over a tail longer than preview.max_seconds`, `window_ticks` in the
JSON lines). Only those can commit a window in which nothing settled.

The default configuration reached that path on no capture (below), so the
same comparison was repeated with `[preview] max_seconds = 20` and `= 10`,
both values the configuration accepts. On `main` such a window commits only
what settled; the rest waits for the release, which decodes it whole. The
replay has no silence timeout, so it cannot show what that cost on `main`
(P1-001: the latch stopped itself mid-sentence); it shows only what the cuts
cost in words. Six of the captures that lost speech at 10 s were traced
again with a scratch print of each window tick, commit and decode (offsets,
durations and word counts only; not kept).

## Data

The local corpus, dataset `2026-09-21.1`: 181 captures, 75.1 minutes, 6832
reference words (single-language captures: 6272), and its dev subset of 28
captures, 17.8 minutes, 1424 reference words.

## Results

### The default configuration: nothing changes

| run | WER | single-language | e1 | e2 | lost at ends | dup (words) | chunks | window ticks | wall |
|---|---|---|---|---|---|---|---|---|---|
| dev, `main` | 14.19% | – | 5 | 1 | 5 | 4 (6) | 77 | – | 76 s |
| dev, branch | 14.19% | – | 5 | 1 | 5 | 4 (6) | 77 | 0 | 78 s |
| full, `main` | 10.85% | 9.90% | 5 | 1 | 5 | 4 (6) | 333 | – | 334 s |
| full, branch | 10.85% | 9.90% | 5 | 1 | 5 | 4 (6) | 333 | 0 | 337 s |

Per `spoken` group, both builds on the full corpus: `en` 8.68%, `de`
14.13%, `mixed` 21.43%. The committed chunks are identical on all 181
captures, and so is every count. `main`'s dev run matches the recorded dev
run of 2026-09-22 on every count.

**No capture reaches the new path.** On the replay's clock, no tail ever
grows past 30 s without a chunk settling in it, so no window tick runs, and
neither the pause cut nor the per-segment `through` of a window commit is
exercised. The per-segment `through` of the release changes no text, only
how far each commit reaches. That the detector's speech now resets the
silence timeout is not measurable here: the replay has none.

### A shorter window: the new path, forced

| `max_seconds` | build | WER | single-language | e1 | e2 | lost at ends (captures > 5) | dup (words) | chunks | window ticks (captures) |
|---|---|---|---|---|---|---|---|---|---|
| 20 | `main` | 10.93% | 10.04% | 7 | 0 | 5 (0) | 5 (7) | 337 | – |
| 20 | branch | 10.98% | 10.04% | 4 | **1** | 5 (0) | 5 (7) | 348 | 50 (34) |
| 10 | `main` | 10.33% | 9.39% | 4 | 0 | 4 (0) | 3 (5) | 333 | – |
| 10 | branch | **12.57%** | **11.86%** | 19 | **5** | **57 (3)** | 6 (9) | 585 | 422 (117) |

- **At 20 s** the branch commits different chunks on 31 captures, better on
  6 and worse on 11, 3 edits more in all. One chunk is lost after the retry
  (`capture-2026-09-18-231741`, 13 → 14 edits).
- **At 10 s** 117 captures reach the new path, every one longer than 11 s.
  On them WER goes from 9.84% to 12.22%; the branch is better on 19 captures
  and worse on 51. Five chunks are lost after the retry against none, and 53
  more words are lost at the ends, 49 of them on three captures of 13–15 s.
  Seam repeats go from 3 to 6.

The trace of the six captures that lost a chunk or their ending at 10 s:

| capture | dur | what happened |
|---|---|---|
| `capture-2026-09-14-120818` | 15 s | the window had no pause and was committed whole through 10.0 s; the release, 10.0–15.2 s, decoded to nothing, padded and bare: 28 words lost |
| `capture-2026-09-14-125720` | 13 s | cut at the pause at 6.6 s; the release, 6.6–12.9 s, decoded to nothing: 14 words lost |
| `capture-2026-09-21-122623` | 15 s | cut at 11.1 s; the release, 11.1–14.5 s, decoded to nothing: 7 words lost |
| `capture-2026-09-16-123824` | 28 s | cuts at 9.3 s and 18.1 s; every decode returned words; 4 words lost at the end |
| `capture-2026-09-19-190030` | 50 s | a 1.8 s segment of a window commit decoded to nothing, padded and bare |
| `capture-2026-09-20-162514` | 16 s | a 1.1 s segment at the start of the first window decoded to nothing; no edit more |

In the first three, the audio after the cut decodes to nothing as a whole,
padded and bare, where `main` decodes the same audio in one window with the
speech before it and every word arrives. The likely cause, not isolated
here: the audio after a cut holds at most the rest of a short pause before
its speech, and none after a whole-window cut.

## Conclusion

**The decode changes are safe to ship at the default `preview.max_seconds`
of 30 s, on the evidence there is: they change no committed word of 181
captures.** The pre-registered criterion holds with every count tied. But
the corpus never reaches the path the changes add, so it does not measure
that path at 30 s at all; its safety there rests on the unit tests and on
how rarely a real tail passes 30 s without a chunk settling.

Where the path does run, it costs words. At 20 s the cost is within noise
apart from one lost chunk; at 10 s it is 2.2 WER points, five lost chunks
and 53 words lost at the ends. Most of the losses sit in the audio right
after a cut, which Parakeet decodes to nothing: the same knife edge the bare retry
exists for ([empty-chunk flips](2026-09-22-empty-chunk-flips.md)), here
without a retry that helps. A user who lowers `preview.max_seconds`
to save CPU pays that price on every capture with a long unsettled tail.

Still open, and the next thing to measure: giving the decode after a cut
lead-in silence (as the trailing second of zeros does for the end of a
window, [trailing silence](2026-09-21-trailing-silence-padding.md)), or
cutting only at a pause long enough to leave some. The 10 s configuration
above is a ready test for either: it must bring `e2` back to 0 and the
words lost at the ends back to 4. The corpus replay cannot weigh this cost
against what `main` loses in the same regime, a capture its silence timeout
ends mid-sentence; both need a replay with the daemon's timeout.
[decisions.md](../decisions.md#a-window-that-settles-nothing-is-cut-at-its-last-pause-2026-09-23)
records the measured cost.
