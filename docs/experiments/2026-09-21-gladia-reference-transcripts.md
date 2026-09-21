# Reference transcripts for the local corpus, from Gladia

_2026-09-21, `scripts/gladia-references.sh` at ce311f6. Gladia v2 pre-recorded
API, default model. No timing claim._

## Question

Five eval clips cannot separate two decoders. Can an external ASR service give
usable reference transcripts for the author's own recovery captures — an hour
of real dictation — and what is such a reference worth?

## Method

`scripts/gladia-references.sh` uploads each wav, starts a pre-recorded
transcription, polls until `done`, keeps the raw response, and deletes the job
and its uploaded audio. Endpoints, from the current API reference:

| step | request |
|---|---|
| upload | `POST https://api.gladia.io/v2/upload`, multipart field `audio` |
| start | `POST /v2/pre-recorded`, `{audio_url, language_config}` |
| poll | `GET /v2/pre-recorded/{id}` until `status` is `done` |
| delete | `DELETE /v2/pre-recorded/{id}` |

The transcript is `result.transcription.full_transcript`, the languages heard
are `result.transcription.languages`. Auth is `x-gladia-key`, read from `.env`
into a mode-0600 curl config file so it never appears in a command line.

The language setting had to be chosen, not guessed. Three probes:

1. **Three captures, `{"languages":["en","de"],"code_switching":true}` against
   `{"languages":["en"]}`.** The two runs disagreed on 8.6% of the words of the
   one long capture. Reading the differences, code switching put German words
   into plainly English sentences (an English phrase came back as two unrelated
   German words; "bishop" as "Bischof") and dropped a chess term the
   English-only run got right. The docs warn against code switching with an
   open language list; it is no better with a list of two.
2. **Thirty captures, every sixth one, with detection left open
   (`{"languages":[]}`).** 21 English, 6 German, 3 with no speech. Comparing
   those six against the forced-English run of the same files showed the
   problem: they are genuinely German, and forcing `"en"` had **translated**
   them into English. A German reference in English would score a correct
   German transcript as entirely wrong.
3. **Nine captures (the six German, three English),
   `{"languages":["en","de"],"code_switching":false}`.** Reproduced the
   detection of probe 2 exactly, matched the English-only run word for word on
   the English captures, and returned German for the German ones.

Probe 3's setting is the script's default.

## Data

Every wav in `~/.local/state/spokenpad/audio` (176 recovery captures) and the
five committed eval clips: 181 files, 73.4 minutes, 16 kHz mono. The responses
and the index live in the git-ignored `eval-samples/local/`. Transcripts of the
author's speech are private and appear nowhere in the repository.

Result of the full run:

| | captures | reference words |
|---|---|---|
| English | 124 | 5360 |
| German | 38 | 1427 |
| no speech at all | 19 | 0 |
| **total** | **181** | **6787** |

Every one of the 181 deletions returned HTTP 202, so no audio and no
transcript is left on Gladia.

Captures with an empty reference are an empty press, a cancelled take or room
noise. The harness scores no WER for them — there is nothing to divide by —
and counts them apart, together with how many decoded to words anyway, which
is the hallucination-on-silence check.

## Results

**Gladia against the five hand-checked references** (same normalisation as
`examples/eval.rs`):

| clip | human words | Gladia words | edits | WER |
|---|---|---|---|---|
| cd-home | 2 | 2 | 0 | 0.0% |
| keyboard-layout-reset | 19 | 17 | 3 | 15.8% |
| new-model-review | 59 | 59 | 0 | 0.0% |
| replacements-critique | 73 | 59 | 20 | 27.4% |
| shell-commands | 29 | 29 | 8 | 27.6% |
| **aggregate** | **182** | | **31** | **17.0%** |

Every one of those 31 edits is technical vocabulary or spoken punctuation:
`udev` heard as `udef`, `mkdir` as `there`, `rm -rf` collapsed to one word, a
spelled-out `s e t` as `zset`, and `test_file` written out as three words where
the human reference symbolises it. These five clips exist to exercise exactly
that, so they are the worst case for this reference and not representative of
the other 176.

**Parakeet on German.** Greedy decoding, whole path, on the nine probe-3
captures: 0.0%, 11.1%, 14.3% and 28.0% WER on four of the German ones, 100% on
a five-second one of three reference words. Parakeet TDT v3 is multilingual and
transcribes the author's German, so the German captures are usable corpus rows
rather than noise — but only with German references.

## Conclusion

The corpus is usable, with the reference's bias stated every time it is used:

- **A reference this noisy cannot resolve a point or two.** Differences that
  small are reported, not acted on.
- **It is systematically friendly to a system that writes what Gladia writes.**
  A change that improves technical vocabulary will look worse here.
- **The counts beside WER are what the corpus is really for.** Chunks that
  decoded to nothing, reference words lost at the end of a capture, commits
  that are only "Yeah." — none of those need the reference to be right about
  the words.
- **The aggregate must be read per language.** 21% of the reference words are
  German; one number over both hides which of them moved.

Open: the 19 empty-reference captures are Gladia's judgement that nobody spoke,
not a human's. A capture that holds one quiet word would be scored as
hallucination if the recognizer found it.
