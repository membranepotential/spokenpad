# A 28-capture dev subset of the local corpus, and which captures mix languages

_2026-09-22, `examples/corpus.rs` at the commit that adds `--subset`, `main`
at fd36708. Parakeet TDT 0.6B v3 int8, sherpa-onnx 1.13.8, 6 threads. Timings
taken while another agent built on the machine (load average about 14)._

## Question

A whole-corpus run (181 captures, 75 minutes) takes 8–20 minutes, and it was
run for questions a much smaller set could answer. Two questions:

1. Can about 30 captures, chosen from the failures the corpus has already
   shown, stand in for the corpus during iteration, so that the full corpus is
   only the release gate?
2. How many captures mix English and German? The references were made with
   `{"languages": ["en","de"], "code_switching": false}`, so a capture that
   switches language has a reference in one language only, and its WER is
   partly the reference's error.

**What would change a decision**, written before the runs:

- If the subset under greedy does not reproduce, capture by capture, the
  failures the recorded full-corpus run of today's `main` shows on those
  captures (5 chunks empty at the first decode, 1 after the retry, 4 seams
  that wrote a word twice, 5 words lost at the ends, 1 capture without speech
  decoded to words), the replay is not deterministic and a subset cannot
  replace anything.
- If the subset under beam search does not show speech lost after the retry,
  it cannot catch the regression that made greedy the default, and it is not
  fit to gate iteration.

## Method

**Language flags.** hunspell (`de_DE`, `en_US`, local dictionaries) tagged
every word of each reference and of each greedy live hypothesis from the
recorded run of `main` as German-only, English-only or either; a short list of
function words both languages share was forced to "either". Every capture with
a word in the other language than Gladia's was then read locally and judged: a
single German term or name inside English, or English technical words inside
German, leaves the capture in its main language; a phrase or sentence in the
other language makes it `mixed`. The flag went into `references.json` as a new
field `spoken`, dataset version `2026-09-21.1` (audio and references
unchanged). The harness reports WER per `spoken` group and filters on it with
`--spoken`.

**Choosing the subset.** Every failure count in the recorded runs was mapped
to its captures: the live runs of today's `main` (greedy, `clamp.jsonl`) and of
beam search (`results-2026-09-21.jsonl`), the whole-path runs of both, the old
code, the 9 s cadence pair, and the padding variants. The subset takes every
capture that carries a failure of the default configuration, the captures that
carry beam search's losses, then coverage (short, long, German, mixed, no
speech). Candidates were read locally and rated for privacy: none, low
(generic talk, no names) or high (names, personal or identifying
details). The list is `eval-samples/local/subsets/dev.txt`;
ratings and reasons, as categories, are in `dev-notes.tsv` beside it, which is
git-ignored with the rest of the dataset.

**Validation.** The subset replayed on the live path under both decoders,
harness built from `main` plus this change:

```sh
corpus --config greedy.toml --corpus eval-samples/local --path live \
       --subset dev --jobs 2 --all --with-text --jsonl runs/dev.jsonl --label dev-greedy
corpus --config beam.toml   ... --label dev-beam
```

Both configs are the ones in `eval-samples/local/runs/configs/`: Parakeet, 6
threads, `greedy_search` or `modified_beam_search` with no hotwords.

## Data

The local corpus, 181 captures, 75.1 minutes, 6832 reference words. The dev
subset: **28 captures, 17.8 minutes, 1424 reference words**; by `spoken`, 19
English, 4 German, 3 mixed, 2 without speech. Its ids are listed in
[eval-samples/README.md](../../eval-samples/README.md#the-dev-subset).

## Results

### Captures that mix languages

| `spoken` | captures | greedy WER (`main`, full corpus) |
|---|---|---|
| `en` | 113 | |
| `de` | 34 | |
| single-language, together | 147 | **9.9%** (621 / 6272) |
| `mixed` | 15 | **21.4%** (120 / 560) |
| `none` | 19 | – |

Of the 15 mixed captures, Gladia called 12 English and 3 German; in one of the
three, reference and hypothesis disagree on which language was spoken, and it
counts as mixed for that reason. One capture Gladia called German is English
apart from one German term, and its reference is English: `spoken = en`. So
Gladia's language is wrong or incomplete for 16 of the 162 captures with
speech.

Leaving the mixed captures out moves `main`'s aggregate from 10.85% to 9.90%:
about a point of the corpus WER is reference error on the minority language.
The mixed captures carry no chunk failure (e1, e2 and seam repeats are zero on
them) and 1 of the 5 words lost at the ends.

### Privacy

36 captures were considered as candidates: 13 rated none, 18 low, 4 high, and
1 left out unread for its length (323 s). The subset holds **13 none, 14 low
and 1 high**. The high one is kept because it is the only capture in the
corpus where the lead-padding clamp changes the committed text (at a 9 s
tick), and it is marked as such in the notes. The other three high candidates
were left out; each failure they carry is also carried by a capture already in
the subset. The other 145 captures were not rated. The flagging pass, which
read every capture with an other-language stretch, suggests that many
captures hold personal details, so no claim is made about how many of them
are clean.

### The subset reproduces `main` exactly

| | captures | WER | edits | hyp words | e1 | e2 | lost at ends | dup | "Yeah." | no speech, words anyway | wall |
|---|---|---|---|---|---|---|---|---|---|---|---|
| recorded full run, restricted to the 28 | 28 | 14.19% | 202 | 1447 | 5 | 1 | 5 | 4 | 0 | 1 | – |
| `dev-greedy` | 28 | **14.19%** | 202 | 1447 | 5 | 1 | 5 | 4 | 0 | 1 | **111 s** |
| recorded full run, all 181 | 181 | 10.85% | 741 | 7028 | 5 | 1 | 5 | 4 | 0 | 2 | 8–20 min |

The committed text is identical on all 28 captures, 77 committed chunks both
times. The subset holds **every** e1, e2, seam repeat and lost-tail word the
full corpus shows under the default configuration. 111 s wall for 17.8 minutes
of audio (9.6x real time).

### The subset catches beam search's loss

| run | WER | e1 | e2 | lost at ends (captures > 5 words) | "Yeah." | wall |
|---|---|---|---|---|---|---|
| `dev-greedy` | 14.19% | 5 | 1 | 5 (0) | 0 | 111 s |
| `dev-beam` | 13.62% | **18** | **2** | **53 (2)** | **3** | 132 s |
| full corpus, beam, 2026-09-21 (sherpa 1.13.6, old code) | 11.52% | 19 | 2 | 57 (2) | 3 | 1357 s |

Every count that made greedy the default shows up on 28 captures: speech lost
after the retry on the same 2 captures, the same 2 captures losing their
endings, and the invented "Yeah." commits. The subset does its job as a
regression detector.

Two findings on the way:

- **39 of beam's 53 "lost" words are the reference's, not beam's.**
  `capture-2026-09-21-171005` has a 39-word reference of which greedy writes
  almost none (6 hypothesis words, 37 edits), and its shape (one sentence
  three times over) looks like the reference model hallucinating. The chunk
  beam leaves empty there is real; the 39 words are probably not. The beam
  loss on solid ground is `capture-2026-09-21-142322`: one chunk empty after
  the retry, 9 words lost at the end, edits doubled (6 → 12). The suspect
  reference also adds 37 edits to greedy's 741 on the full corpus.
- **WER does not track the corpus between decoders.** On the subset beam
  scores 0.57 points *better* than greedy. The last full-corpus comparison
  (sherpa 1.13.6) had greedy 0.32 points better with an interval of
  [−1.22, +0.47], and restricted to these 28 captures that older pair also had
  greedy better (14.89% against 15.17%). There is no full-corpus beam run on
  1.13.8 to compare with, and none was made here. Between two builds of the
  greedy path the subset tracks in direction: old code → `main` is −0.70
  points on the subset and −0.35 on the corpus, larger on the subset because
  the captures that changed are over-represented in it.

## Conclusion

**Use `--subset dev` to iterate and the full corpus to decide.** The subset
reproduces the default configuration's failures exactly, in about two
minutes, and it shows the regression that made greedy the default. Its WER is
not an estimate of the corpus WER: 14.2% against 10.9%, by construction,
since the subset was picked for its failures. A WER difference between two
configurations on it is not evidence the corpus would confirm. Read the counts
on the subset; read WER on the corpus.

- About 1 point of the corpus WER is reference error on the 15 mixed
  captures. Quote the single-language number (`--spoken en --spoken de`)
  beside the aggregate when a WER is compared.
- Two references are wrong in a way that matters: one capture with speech has
  an empty reference, one has a reference that looks invented. They are noted
  in the dataset README and not corrected; correcting them needs a new dataset
  version, which the user has not asked for.
- Open: whether the subset keeps its value as the code changes. It carries
  today's failures; a change that creates a failure on a capture outside it
  shows only on the full corpus, which is why that stays the gate.
