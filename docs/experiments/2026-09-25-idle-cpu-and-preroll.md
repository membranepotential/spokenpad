# Where does an idle daemon spend its CPU, and what is the pre-roll worth?

_2026-09-25, `main` at a9279dc plus the change that adds `--skip-ms` and the
lost-head count to `examples/corpus.rs` (committed with this file), and, for
the second idle measurement, the idle loop change of the same day. The
author's laptop, 12 cores, built-in microphone on PipeWire (quantum 1024 at
48 kHz). The two corpus runs ran while the test suite was built alongside;
their timings are not compared._

## Question

The daemon used about 2 % of a core while nobody dictated. Where does that
come from, and how much of it can go? The largest part turned out to be the
pre-roll's input stream, which is open all the time. That raised a second
question: what does the pre-roll buy? Could the microphone be opened only at
the press?

## Method

1. **Idle wakeups.** The voluntary context switches and CPU ticks of each
   thread of the running daemon (`/proc/PID/task/*/status`, `stat`), sampled
   over 5 to 20 s with no capture running; `pw-top` for the PipeWire graph.
   The same for a build with the idle loop change
   ([decisions](../decisions.md), "An idle daemon waits for requests instead
   of polling"), run by hand with the author's config, once as it is
   (`preroll_seconds = 0.25`) and once with `preroll_seconds = 0`, each
   measured over 20 s after 20 s of warm-up. The service was stopped
   meanwhile.
2. **Opening the microphone.** A scratch script, not kept: Python
   `sounddevice` on the system PortAudio (the library the daemon links)
   opens the default input the way `shell/audio.rs` does (mono float32,
   16 kHz, `default_low_input_latency`) and records 1.5 s. It reports the
   time from the open call to the first callback and the leading samples
   that are exactly zero. Five rounds with the service stopped: one open
   after WirePlumber had suspended the microphone (`pactl list sources
   short` says `SUSPENDED`, after about 5 s), one at once after it.
3. **The pre-roll on the corpus.** `examples/corpus.rs` gained `--skip-ms`,
   which drops audio from the start of each capture, and counts reference
   words lost at the start of a capture as it counts those lost at the end.
   Every corpus capture was recorded with `audio.preroll_seconds = 0.25`
   (the default since v1) and its WAV begins with the pre-roll, so
   `--skip-ms 250` replays each capture as it would have been recorded
   without one. Live path, `config.example.toml`'s defaults, Parakeet TDT
   0.6B v3 int8, greedy, sherpa-onnx 1.13.8, 6 threads, 3 jobs, previews
   skipped:

   ```sh
   corpus --config config.example.toml --path live --jobs 3 --skip-ms 0|250 \
          --label preroll-skip-0|250 --jsonl runs/2026-09-25-preroll-skip.jsonl
   ```

   A scratch script compared the two runs capture by capture, and measured
   the sound level in 50 ms frames of each capture's first 250 ms against
   its own noise floor (10th percentile of its frames) and speech level
   (90th percentile); a frame above a quarter of the way from floor to
   speech counts as sound.

## Data

The whole local corpus: 181 captures, 75.1 minutes, 162 with a reference,
6832 reference words (`eval-samples/local/`). The level check used the 167
captures of at least 0.5 s.

## Results

**Idle wakeups, before the change** (packaged 1.0.1, `preroll_seconds =
0.25`):

| thread | wakeups/s | CPU |
|---|---|---|
| PortAudio callback (unnamed) | 47 | 1.1–1.7 % |
| PipeWire `data-loop.0` of the stream | 47 | 0.2–0.3 % |
| event loop (`spokenpad`) | 50 | 0.2 % |
| editor thread (`spokenpad-nvim`) | 50 | 0.1 % |

`pw-top`: spokenpad was the only client of the microphone
(`alsa_input…analog-stereo`, quantum 1024 at 48 kHz, i.e. every 21 ms),
so the graph ran, and the device stayed out of suspend, for spokenpad
alone; `pipewire` itself used about 0.3 %.

**Idle wakeups, with the idle loop change:**

| thread | `preroll_seconds = 0.25` | `preroll_seconds = 0` |
|---|---|---|
| PortAudio callback | 47/s, 1.85 % | — |
| two PipeWire `data-loop.0` | 47/s each, 0.55 % | — |
| editor thread | 15/s, 0.00 % | 15/s, 0.00 % |
| control socket thread | 10/s, 0.10 % | 10/s, 0.05 % |
| event loop | 4/s, 0.00 % | 4/s, 0.00 % |
| **whole daemon** | **2.5 %** | **0.05 %** |

The callback's share varies between samples (1.1 % and 1.85 % in two).

**Opening the microphone** (5 rounds each):

| | `open()` | first callback | audio begins after the open call | leading zeros |
|---|---|---|---|---|
| suspended microphone | 3–4 ms | 27–28 ms | 6–8 ms | 1 ms |
| not yet suspended | 3–5 ms | 6–21 ms | ≤ 1 ms | 1 ms |

"Audio begins" is the first callback minus the 21 ms block it carried.

**The pre-roll on the corpus:**

| | WER | edits | captures losing words at the start (words) | words lost at the ends | empty chunks (after retry) | seam repeats |
|---|---|---|---|---|---|---|
| with pre-roll | 10.73 % | 733 | 1 (24) | 6 | 5 (1) | 3 |
| first 250 ms dropped | 10.95 % | 748 | 2 (6) | 11 | 6 (2) | 5 |

Capture by capture: 119 unchanged, 31 worse (+63 edits), 31 better (−54
edits). 8 captures were cut into a different number of chunks. The two
captures that lose words at the start without the pre-roll are 0.5 s and
1.1 s long, with 2 and 4 reference words, and every word of both was already
wrong with the pre-roll: their edits do not change. The one capture losing
24 words at its start with the pre-roll loses them to something else (its
first 500 ms are at the noise floor) and does not without it.

Sound above the noise floor in the pre-roll: 11 of 167 captures (7 %), in
every one of them within the last 100 ms before the press. Whether that is
the start of speech or the sound of the key itself, the level does not tell.

## Conclusion

- The idle cost is the pre-roll's input stream: with it closed, the daemon
  idles at 0.05 %. Beyond spokenpad's own threads, the open stream keeps
  PipeWire's graph running and the microphone out of suspend.
- The event loop's and editor thread's 50 wakeups a second were avoidable
  and are gone (4 and 15 a second; the decision entry above).
- On this machine opening the microphone at the press costs about 7 ms of
  audio even from suspend, so the pre-roll is not needed to hide a slow
  open. What it keeps is sound from before the press reached the daemon.
- On this corpus that sound is worth 0.2 WER points (15 edits), less than
  the churn that shifting every chunk boundary causes capture by capture
  (31 captures gain 63 edits, 31 lose 54). No capture loses a word it had
  right. The corpus is the
  author's habit with a pre-roll in place; someone who starts speaking
  before the key is down would lose more. Other devices (a USB or Bluetooth
  microphone) may open more slowly; not measured.
- Whether the default pre-roll stays is left to the user; see
  [../decisions.md](../decisions.md).
