# Field notes from building and running the corpus harness

_2026-09-21 and 2026-09-22. Not an experiment: things learned while running the
others, written down so they are not learned twice. No transcript content._

## The Gladia language option decides whether the references are usable

Three settings were tried before one was trusted, and the failures are quiet
rather than loud:

| setting | what it does to this corpus |
|---|---|
| `{"languages": ["en"]}` | **translates** German captures into English |
| `{"languages": ["en","de"], "code_switching": true}` | puts German words into plainly English sentences |
| `{"languages": ["en","de"], "code_switching": false}` | one language per file, correct on every probe |

The first is the dangerous one. A translated reference reads as perfectly
fluent English, so nothing looks wrong until a German capture scores ~100% WER
against it. 21% of this corpus's reference words are German, and the first full
reference run had to be thrown away and redone. **Check a reference set by
reading a few entries of each detected language before trusting it**, not by
looking at the aggregate.

## Word error rate hides the failure a dictation tool cares about

Every decision the corpus settled came from the counts beside WER, not from WER:

- greedy against beam search on the live path: **−0.32 WER points, interval
  [−1.22, +0.47]** — a tie. The counts: 19 chunks empty against 5, 2 lost
  against 0, 57 reference words lost off the ends of captures against 5.
- the trailing-silence padding: removing it **improves** WER by 0.26 points and
  loses every chunk that decodes empty, because `Padded` and `Bare` become the
  same input and the retry stops being a second opinion.
- the chunk the constant-RAM work loses: the build that loses it scores
  **4.8%** on that capture, the builds that keep it score **7.0%**.

A word lost costs one edit, the same as a word misheard. Over a corpus, a
hundred captures that are nearly right drown two that lost a sentence. If a
harness only reports WER, it will recommend the wrong thing.

## Replay faithfulness

- **Tick on audio time, not wall time.** The daemon ticks on a wall clock, so a
  loaded machine ticks over a longer stretch of audio and lands on different
  chunk boundaries. A replay that uses wall time is not reproducible between
  runs; one that advances a virtual clock by `preview.interval_ms` of audio is,
  and models the idle machine the daemon usually runs on.
- **The cosmetic decode can be skipped, and only through the daemon's own
  door.** `TickKind::Commits` exists for this; a replay that instead hides the
  unsettled chunk from the segmenter diverges as soon as settled silence enters
  the split, because the daemon returns at the first unsettled chunk and never
  reaches the silence commit. Skipping it cuts the replay's decodes by about
  90% (measured 385 s → 65 s over six captures) and cannot change a committed
  word — there is a test asserting exactly that.
- **A rebuilt baseline is worth the build.** The pre-constant-RAM code was
  rebuilt from its own commit and reproduced its recorded run to the digit
  (same WER, same edits, same word count, same counts). Without that control,
  every "the new code is 0.35 points better" claim would rest on trusting two
  different binaries' bookkeeping.

## Cost and shape of a corpus run

Parakeet TDT 0.6B int8, 6 threads, one job, this machine, always under load
from other agents:

| pass | 75 minutes of audio |
|---|---|
| live path, previews skipped | 8–20 min |
| live path with `--previews` | ~10x that |
| whole path | 5–10 min |

Each job loads its own copy of the model (~1.5 GB resident for Parakeet), so
`--jobs` is bounded by memory, not cores. `--jobs 2` was the practical maximum
here with other agents building.

## Two traps in the tooling around the harness

- **A background shell waiter is capped at ten minutes.** A
  `until <condition>; do sleep; done` job launched in the background is killed
  at the tool's timeout and its trailing command still runs, so the completion
  notice arrives *before* the thing being waited for has finished. Several
  "done" notices in this work were wrong for that reason. **Read the log file,
  not the notification**, and re-arm the wait.
- **`--all-targets` does not build an example's test target.** `cargo clippy
  --all-targets` passed while `examples/corpus.rs`'s `#[cfg(test)]` module did
  not compile. `cargo test --locked --example corpus` is the check that catches
  it.

## Where the data is

The dataset, the raw reference responses, the rejected reference probes and
every run file live in `eval-samples/local/`, git-ignored, with a README of
their own. The scratch trees used for the one-line-revert builds are not kept:
each is one `git archive` plus a documented one-line change, so they are
cheaper to recreate than to store.
