# Do the 2026-09-22 decode fixes change what the corpus commits?

_2026-09-23. `main` at 06043fc against this branch at 4321c76 (the
`max_seconds = 20` runs at 8db528a, which changes only the editor), plus
the window-tick count in `examples/corpus.rs` (committed with this file).
A third build, the whole-tail tick that replaced the window commit, was
measured later the same day at the commit that adds it (after cbca2c0), on
an idle machine; its section is [below](#the-whole-tail-tick-the-truncation-was-the-cause)._
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

### The whole-tail tick: the truncation was the cause

The trace above suggested a second reading: the window commit loses words
because it cuts where no chunk boundary is, and it has to cut only because
a tick read no more than `preview.max_seconds` of the tail. The third build
takes the truncation out instead: every tick hands the worker the whole
open tail, the detector reads all of it, and only chunks that settle by the
usual rules are decoded ([decisions.md](../decisions.md#a-tick-reads-the-whole-open-tail-the-window-commit-is-removed-2026-09-23)).
`preview.max_seconds` then bounds only the preview, which the replay skips,
so the replay's committed text cannot depend on it. The three runs, full
corpus, live path, same configuration files, `--jobs 2`, nothing else
running. `main` at 10 s was run again beside them, from its own build of
06043fc, and matched its row below on every count; the run files were
compared capture by capture in memory and deleted:

| `max_seconds` | build | WER | single-language | e1 | e2 | lost at ends (captures > 5) | dup (words) | chunks | ticks over a longer tail (captures) |
|---|---|---|---|---|---|---|---|---|---|
| 30 | `main` | 10.85% | 9.90% | 5 | 1 | 5 (0) | 4 (6) | 333 | – |
| 30 | window commit | 10.85% | 9.90% | 5 | 1 | 5 (0) | 4 (6) | 333 | 0 |
| 30 | **whole tail** | 10.85% | 9.90% | 5 | 1 | 5 (0) | 4 (6) | 333 | 0 |
| 20 | `main` | 10.93% | 10.04% | 7 | 0 | 5 (0) | 5 (7) | 337 | – |
| 20 | window commit | 10.98% | 10.04% | 4 | 1 | 5 (0) | 5 (7) | 348 | 50 (34) |
| 20 | **whole tail** | 10.85% | 9.90% | 5 | 1 | 5 (0) | 4 (6) | 333 | 142 (34) |
| 10 | `main` | 10.33% | 9.39% | 4 | 0 | 4 (0) | 3 (5) | 333 | – |
| 10 | window commit | 12.57% | 11.86% | 19 | 5 | 57 (3) | 6 (9) | 585 | 422 (117) |
| 10 | **whole tail** | 10.85% | 9.90% | 5 | 1 | 5 (0) | 4 (6) | 333 | 1565 (117) |

(The window commit counted only ticks that read a window; the whole-tail
build counts every tick over a tail longer than `preview.max_seconds`.)

- **The committed chunks are identical on all 181 captures in all three
  runs**, compared capture by capture, and every count matches `main` and
  the window commit at the default. At 10 s, 1565 ticks in 117 captures ran
  over a tail longer than `max_seconds`; none of them cut anything.
- **Against `main` at 10 s the whole-tail build is 0.52 points worse**: 35
  edits over 52 captures that commit different chunks, every one longer
  than 10 s, `main` better on 21 of them and worse on 9, one more chunk
  empty on the first decode and one more after the retry, one more word
  lost at an end, one more repeated seam. Those 52 captures commit the same
  number of chunks on both builds in 48 cases, so the windows differ, not
  the number of cuts. Not isolated here; the likely cause is the padding: a
  chunk that is the last one in a 10 s slice gets `vad.edge_pad_seconds`
  (2 s) of trailing audio, where the same chunk followed by more speech in
  the whole tail gets `vad.pad_seconds` (0.5 s). `main` at 10 s is not a
  configuration anyone runs for accuracy: in slow dictation it commits
  nothing and ends the latch (P1-001), which this replay, having no silence
  timeout, cannot show.

The cost of reading the whole tail is the detector pass. The harness now
times every pass (`detector passes` in its report): 4181 passes per run,
the longest tail one pass read 29.7 s in every run, the slowest pass
190–207 ms with two jobs sharing the machine. The corpus never holds a tail
longer than that, whatever `max_seconds` is. Over longer tails,
`silero_split_time_over_a_long_tail` in `tests/e2e.rs` (release build, idle,
worst of three):

| tail | 30 s | 60 s | 120 s | 280 s |
|---|---|---|---|---|
| one detector pass | 118 ms | 232 ms | 462 ms | 1072 ms |

About 3.9 ms per second of tail. 280 s is about the longest tail the chunk
rules leave open (10 s of speech in the shortest spans, each followed by
just under the 4 s that settles a chunk). The next tick waits at least as
long as the last took, so the worker stays idle at least half the time at
any tail length; a release queued behind a tick waits for at most one pass
and one chunk's decode.

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

**Update, the same day: the truncation was the cause, and the whole-tail
tick removes it.** With every tick reading the whole tail, the committed
text is the same at 30, 20 and 10 s, and the same as `main` at the
default: `e2` 1, 5 words lost at the ends, 4 repeated seams, 10.85% WER.
Against the window commit at 10 s that is 1.7 WER points, 4 lost chunks
and 52 lost words better. The pre-registered bar for this build, no worse
than `main` at 20 and 10 s on any count, fails at both. At 20 s it fails
on `e2` alone (1 against 0; better on WER, `e1` and seams, equal on lost
words); the one chunk is the one `main` also loses at the default. At 10 s
it fails on every count by the margins above, because `main` at 10 s cuts
different windows on 52 captures, and they happen to decode better. The window commit is removed; see
[decisions.md](../decisions.md#a-tick-reads-the-whole-open-tail-the-window-commit-is-removed-2026-09-23).
Still open: whether trailing padding explains `main`'s gain at 10 s, and a
replay with the daemon's silence timeout.
