# What a long recording costs in RAM, before and after dropping committed audio

_2026-09-21, on top of 51b2c72; extended after the review of ba805a9 with the
padding, detector-pass and overlap numbers below. Intel i7-9850H, 12 threads,
31 GiB. Other agents were building on the machine: that affects the detector
timings, which are therefore upper bounds, and nothing else — the rest is this
process's own RSS and arithmetic._

## Question

A latched capture grew by about 230 MB per hour, which is why it was capped at
3600 seconds. Can the daemon hold a constant amount of audio instead, and does
dropping the audio it has already transcribed change what the recognizer sees?

## Method

Three measurements, all in this repository:

1. `tests/e2e.rs::ram_over_a_long_latched_capture` (ignored). Half an hour of
   synthetic speech — four seconds of tone, six of silence, repeating — is fed
   through the real `AudioCapture` in 20 ms device buffers, with the real
   decode worker ticking every 1.1 s over the real merge policy and a counting
   recognizer. It runs twice: dropping the committed audio, and keeping it as
   the daemon used to. Peak RSS above the baseline is read from
   `/proc/self/status`.

   ```sh
   cargo test --locked --release --test e2e -- --ignored --exact \
       ram_over_a_long_latched_capture --nocapture
   ```

2. `src/core/decode.rs::long_capture` (normal test run). Half an hour of mixed
   bursts, pauses on both sides of the split threshold, one 180-second stretch
   of unbroken speech and one five-minute forgotten-latch silence, replayed
   twice: once with the capture buffer trimmed and settled silence finishing
   the offset, once with neither. Every decode window is recorded in
   capture-absolute samples.

3. `src/core/segments.rs::no_committed_window_reaches_back_over_committed_speech`
   (normal test run). 400 seeded captures — twelve bursts of 0.15–12 s with
   pauses of 0.05–9 s, on both sides of the 4 s split threshold — each walked
   at tick cadences of 1.1 s, 6 s and 9 s, checking every committed window
   against the offset the previous commit left behind. The same walk, with the
   padding clamp removed, is how the overlap numbers below were taken.

4. `tests/e2e.rs::silero_split_time_over_a_long_tail` (ignored). The real
   Silero pass over tails of 30 s, 60 s and 270 s built from the bundled
   sample and half-second gaps; warmed once, then the worst of three.

   ```sh
   cargo test --locked --release --test e2e -- --ignored --exact \
       silero_split_time_over_a_long_tail --nocapture
   ```

5. `tests/e2e.rs::dropping_settled_silence_does_not_move_the_detector_spans`
   (ignored). The real Silero VAD (sherpa-onnx 1.13.6, `silero_vad.onnx`) over
   the bundled `test_en.wav` with 4 s of leading silence and with 60 s of it —
   a difference of 896 000 samples, 1750 whole detector windows.

   ```sh
   cargo test --locked --release --test e2e -- --ignored --exact \
       dropping_settled_silence_does_not_move_the_detector_spans
   ```

## Data

Synthetic audio only, generated in the tests; no recordings. The Silero check
uses `test_en.wav`, the sample that ships with the pinned model download.

## Results

Peak RSS above the baseline, 30 minutes of latched capture at 16 kHz mono:

| Capture buffer | Peak RSS | Per hour |
|---|---|---|
| Keeps committed audio (before) | 116.2 MiB | 233 MB |
| Drops committed audio (after) | 1.7 MiB | flat |

The "before" figure matches the 230 MB/hour the cap was set for: 16 000
samples/s × 4 bytes = 64 KB/s. The "after" figure fell from 3.6 MiB to 1.7 MiB
once a tick was capped at `preview.max_seconds` of audio (below): the snapshot
handed to the worker is the other copy of the tail.

Retained audio in the 30-minute policy replay: 26.0 s at the peak, against a
stated bound of 45 s and against the whole 1800 s the reference run holds. For
unbroken speech the peak is one Silero span plus the second that settles the
chunk it closes.

Decode windows, trimmed run against reference run: identical — same count,
same capture-absolute sample ranges, same order. The recognizer is called
slightly *more* often in the trimmed run, all of it cosmetic previews: a tail
the committed offset has walked out of is a tail worth redrawing.

Silero spans with 4 s and with 60 s of leading silence: identical after
shifting by the 56 s difference — same number of windows, same starts, ends,
speech boundaries and settled flags.

Padding that reaches into a neighbouring chunk's speech, over the 400 captures
at all three cadences:

| Side | Windows affected | Captures | Worst |
|---|---|---|---|
| Lead, before the clamp | 8 | 7 / 400 | 0.370 s |
| Lead, after the clamp | 0 | 0 / 400 | — |
| Trail (unchanged) | 201 | — | 0.435 s |

The lead side is the one that repeats *committed* speech: that audio has been
decoded and written, so decoding it again can write a word twice. The trail
side reaches forward into speech the next chunk will decode, is bounded by
`pad_seconds`, and predates all of this work; it is left alone here.

One detector pass over an open tail, real Silero, sherpa-onnx 1.13.6:

| Tail | `SpeechSegmenter::split` |
|---|---|
| 30 s | 287 ms |
| 60 s | 521 ms |
| 270 s | 2371 ms |

This is uninterruptible work inside a tick, so a release arriving during it
waits for it. It is why a tick is handed at most `preview.max_seconds` of
audio however long the tail has grown.

## Conclusion

Memory during a recording is constant. What is held is the open tail: one
chunk of speech with the pauses inside it, one split threshold (4 s) of silence
kept back for the next window's padding, and the audio that arrives while a
tick runs.

Dropping the audio behind the committed offset cannot change a decode — no
window can reach across that offset — and the third measurement closes the one
part that was not arithmetic: Silero's state after four seconds of silence is
the same as after a minute of it, so finishing settled silence early moves no
span. This was worth measuring because the detector carries an LSTM state
across its windows, which no amount of reading the merge policy can rule on.

**Open: the effect of the padding clamp on WER has not been measured.** It
takes audio away from the recognizer's input — the lead padding of every chunk
that follows a close with a short pause, not only the overlapping ones — so it
can move a transcript in either direction. It wants a corpus replay through the
WER harness before anyone calls it free.

Not shown: behaviour without a VAD model. Nothing settles there, so nothing can
be dropped and the 3600-second ceiling still applies, unchanged. The same is
true of `preview.enabled = false` and of a `preview.interval_ms` long enough
that few ticks fire: the setting is documented to stop progressive commit
altogether, and what is not committed cannot be dropped.

Decision: [decisions.md](../decisions.md), "Constant memory while recording".
