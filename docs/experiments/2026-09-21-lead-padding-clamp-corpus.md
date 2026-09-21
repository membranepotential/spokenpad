# The lead-padding clamp, and the rest of the constant-RAM change, over the corpus

_2026-09-21, `examples/corpus.rs` at 24f42df. Parakeet TDT 0.6B v3 int8,
greedy, 1 s trailing silence, live path. Timings taken while other agents ran
on the machine (load average 15–30) and compare only runs of this batch._

## Question

Two changes since the recorded corpus baseline alter which audio a committed
window holds, and both can move a transcript:

- **[16f789e] the lead-padding clamp.** A chunk's window began `pad_seconds`
  before its first speech with nothing stopping that reach at the previous
  chunk's speech end, so a chunk after a short pause was handed words the
  previous chunk had already committed. The clamp stops the lead at the
  previous chunk's `speech_end`. It takes audio away from the recognizer, so
  it can also lose a word.
- **[9f632f9] and [1e2631f], the constant-RAM work.** A chunk followed by a
  long enough pause now closes at the end of the slice, the committed offset
  advances over settled silence, one tick is handed at most
  `preview.max_seconds` of audio, and the release decodes only what is still
  held.

sherpa-onnx also went 1.13.6 → 1.13.8, reported WER-neutral on the five clips.

Before deploying this to the live service: does it transcribe better or worse,
and should the clamp be kept or reverted?

## Method

Three binaries over the same 181 captures, same config, same references:

| run | code |
|---|---|
| `old-code` | the corpus baseline: `ce311f6`, sherpa 1.13.6, before all three commits |
| `new-clamp` | today's `main` |
| `no-clamp` | today's `main` with 16f789e's one expression reverted in a scratch copy, **not committed**: `c.start.saturating_sub(lead).max(committed)` back to `c.start.saturating_sub(lead)` |

`new-clamp` against `no-clamp` isolates the clamp exactly — same sherpa, same
daemon loop, one expression apart. `old-code` against `new-clamp` is the
deploy question, and bundles all four differences.

```sh
corpus --config greedy.toml --corpus eval-samples/local --path live \
       --jsonl clamp.jsonl --label new-clamp --jobs 2 --with-text
```

The scratch tree came from `git archive`, and the revert was verified three
ways: the expression is absent from its `src/core/segments.rs`, the two
binaries differ, and `old-code` reproduced its recorded run to the digit.

`--with-text` records each committed chunk, so two runs can be compared seam by
seam. Nothing of the speech is printed here or in the scratch tooling: counts,
word positions and word lengths only.

## Data

The local corpus of 2026-09-21: 181 captures, 75.1 minutes, 162 with a
reference (124 English with 5383 words, 38 German with 1449). References are
ASR, not ground truth
([the corpus experiment](2026-09-21-gladia-reference-transcripts.md)).

## Results

### The clamp changes nothing on any of the 181 captures

| | WER | en | de | edits | hyp words | e1 | e2 | "Yeah." | words lost at ends | seams that wrote a word twice |
|---|---|---|---|---|---|---|---|---|---|---|
| `new-clamp` | 10.85% | 9.31% | 16.56% | 741 | 7028 | 5 | 1 | 0 | 5 | 4 (6 words) |
| `no-clamp` | 10.85% | 9.31% | 16.56% | 741 | 7028 | 5 | 1 | 0 | 5 | 4 (6 words) |

Not "close": **identical, capture by capture.** The committed text matches on
all 181, the paired bootstrap difference is 0.00 points with a [0.00, 0.00]
interval, and every count above agrees. The clamp never fired.

That is a statement about this corpus, not about the defect. The clamp needs
two chunks in one slice with a pause between them shorter than the lead
padding; 16f789e measured it on 400 *generated* captures walked at three tick
cadences and found 8 such windows in 7 of them. Real dictation at a 1.1 s tick
rarely puts two chunks in one slice at all.

So the pair was run again at `--tick-ms 9000`, a cadence that puts several
chunks in one slice — the slowest of the three 16f789e used:

| tick | | WER | edits | hyp words | e1 | e2 | words lost at ends | captures whose text differs |
|---|---|---|---|---|---|---|---|---|
| 1100 ms | clamp / no clamp | 10.85% / 10.85% | 741 / 741 | 7028 / 7028 | 5 / 5 | 1 / 1 | 5 / 5 | **0** |
| 9000 ms | clamp / no clamp | 10.88% / 10.88% | 743 / 743 | 7000 / 7000 | 7 / 7 | 0 / 0 | 5 / 5 | **1** |

At 9 s the clamp finally fires: one capture of 46 reference words commits
different text, two words apart between the runs. It scores **the same** either
way — 13 edits, 28.3% — so on the one occasion the clamp changed anything on
real audio, it changed which words were wrong and not how many.

### The rest of the change is a small improvement

`old-code` → `new-clamp`, paired bootstrap over captures, 5000 resamples:

| | WER difference (old minus new) | 95% interval | captures old better / worse / equal |
|---|---|---|---|
| all | +0.35 points | [+0.08, +0.63] | 2 / 9 / 151 |
| English | +0.26 points | [+0.05, +0.51] | 1 / 7 / 116 |
| German | +0.69 points | [−0.46, +1.40] | 1 / 2 / 35 |

| | WER | en | de | hyp words | e1 | e2 | "Yeah." | words lost at ends | empty ref, words anyway |
|---|---|---|---|---|---|---|---|---|---|
| `old-code` | 11.20% | 9.57% | 17.25% | 7008 | 5 | **0** | 0 | 5 | 2 |
| `new-clamp` | 10.85% | 9.31% | 16.56% | 7028 | 5 | **1** | 0 | 5 | 2 |

The committed text differs on **12 of 181** captures, 38 edit sites in all, 3
of them at a capture's very end. The two longest captures in the corpus hold
most of it: the 323 s one (623 → 644 words, 44 edits between the runs) and the
232 s one (279 → 276, 8 edits). Those are exactly the captures where the new
`preview.max_seconds` bound on one tick's work changes where the ticks land,
so different chunk boundaries there are expected rather than surprising.

**One regression, small but real:** one speech chunk is now empty after the
bare retry that was not before (`e2` 0 → 1), out of the same 5 that decode
empty at the first try. One chunk of speech is lost where none was. It is
traced to its cause in the next section.

## Which change loses the chunk (2026-09-22)

The lost chunk is in `capture-2026-09-18-231741.wav` (232 s, 270 reference
words, 12 committed chunks). Five builds of today's `main`, each with one
difference reverted in a scratch copy and none of them committed, replayed that
one capture with every window and decode traced:

| build | e1 | e2 | WER on this capture | committed chunks |
|---|---|---|---|---|
| `main` | 2 | **1** | 4.8% | 12 |
| sherpa-onnx 1.13.6 instead of 1.13.8 | 2 | **1** | 4.8% | 12 |
| `preview.max_seconds` tick bound reverted | 2 | **1** | 4.8% | 12 |
| end-of-slice chunk close reverted | 1 | **0** | 7.0% | 12 |
| settled-silence advance reverted | 1 | **0** | 7.0% | 12 |
| the recorded baseline, old code | 1 | **0** | 7.0% | — |

So sherpa and the tick bound are innocent, and the two that matter are the
**end-of-slice chunk close** and the **settled-silence advance** — reverting
either one alone restores the old behaviour exactly.

The window boundaries say why, in capture-absolute samples:

| build | the window over this speech | length | speech in it |
|---|---|---|---|
| `main` | `[1223680, 1497024)` then `[1490400, 1516000)` | 17.08 s + **1.60 s** | 17 s + **0.60 s** |
| either revert | `[1223744, 1531200)` | 19.22 s | all of it |

The old code decoded that 0.60 s of speech inside a 19 s window and got words
for it. `main` splits it off into a window of 1.60 s holding 0.60 s of speech,
and Parakeet returns `""` for that window **both padded and bare**, so the
retry cannot rescue it.

The split happens because the previous chunk closes at `vad.chunk_seconds` of
speech, the 0.60 s that follows starts a new pending chunk, and the new
end-of-slice rule closes that chunk on the 4.75 s of trailing silence in the
slice instead of leaving it open to merge with the next speech. The silence
advance is the other half: it moves the committed offset, which moves where the
next slice starts, which moves the detector's window grid and so where the
speech target is crossed.

**This is the failure `docs/progressive-commit.md` already names** — "a long
quiet middle making speech too sparse in one recognizer window, a case in which
Parakeet can return an empty or truncated transcript" — arriving through a new
door. A chunk of 0.60 s of speech in a 1.60 s window is exactly the shape the
project memory records as a knife edge.

Note the direction, which is not what "lost speech" suggests: the build that
loses the chunk scores **4.8%** on this capture and the builds that keep it
score **7.0%**. The 19 s window that swallowed the 0.60 s also decoded the rest
of itself worse.

### Open, not isolated

Work stopped here on the user's instruction, with these questions unanswered:

- **How often it happens.** Over the whole corpus `main` has 5 chunks empty at
  the first decode and 1 lost; the recorded baseline had 5 and 0. A run of the
  end-of-slice-close revert over all 181 captures was started to count how many
  chunks change empty/non-empty status **in either direction** between the two
  builds, and was killed before it finished. Without that number this is one
  capture, and one capture cannot say whether the new rule loses speech more
  often than it saves it.
- **Whether it is worth fixing.** The obvious guard — refuse to settle a chunk
  holding less than some minimum of speech, and let it merge with the next —
  fights the constant-RAM goal the end-of-slice close was written for, since an
  unsettled chunk is audio that cannot be dropped. No fix is proposed here on
  the strength of a single capture.
- Nothing about this is the lead-padding clamp, which the section above shows
  changes nothing at all.

### The trailing side of a window is not clamped, and it shows

Four chunk seams in the corpus wrote a word twice — the next committed chunk
began with the words the previous one ended with — for 6 words over 333
committed chunks. That is with the clamp, and `no-clamp` has exactly the same
four, so none of them is a lead-padding overlap: they are the **trailing** pad
of a chunk reaching forward into the next chunk's speech, which nothing
clamps. It matches the synthetic study's untouched trailing-side overlap (201
windows, worst 0.435 s).

Six words in 75 minutes is not urgent. It is, however, the same class of defect
the lead clamp was written for — committed text that does not match what was
said — and it is now measurable on real captures: `examples/corpus.rs` reports
it as the `dup` column.

## Conclusion

**Keep 16f789e.** The case for reverting would have to be that it costs
accuracy; over 75 minutes of real dictation it costs exactly nothing, byte for
byte, at the cadence the daemon uses — and at a cadence eight times slower,
chosen to provoke it, it changes one capture of 181 without changing that
capture's score. Reverting a correctness fix that is free is strictly worse.

**The new main is safe to deploy**, and slightly better: 0.35 WER points, an
interval that excludes zero, 9 captures improved against 2 worse, and no count
worse except the one chunk now lost after the retry. That one is worth a look
before it is called a coincidence, and it is not the clamp's doing.

What this does not show:

- Which of the three remaining differences (chunk closing, tick bound, sherpa
  1.13.8) causes the extra lost chunk, or the WER gain. Separating them needs
  another build per difference; only the clamp was separated here, because
  only the clamp was up for reverting.
- Whether the trailing pad should be clamped too. Six words say it is real and
  small; a decision needs a measurement of what clamping it *costs*, which is
  the same experiment the lead clamp never got before it landed.
- Why the 9 s cadence has two fewer chunks decode empty at the first try
  (`e1` 7 against 5) yet loses none of them (`e2` 0 against 1) while scoring
  0.03 points worse. The cadence is not a setting anyone should change on that
  evidence, but the extra lost chunk at 1.1 s is the same one flagged above.
