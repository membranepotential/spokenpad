# A public reproduction of the TDT beam-search loss, and the patch

_2026-09-21, spokenpad at `67d1a5b`, sherpa-onnx **1.13.8**, Parakeet TDT 0.6B
v3 int8 and v2 int8. Machine: Intel i7-9850H, 12 threads, busy with parallel
agent builds (load 30–50), so wall times are upper bounds._

Follow-up to
[2026-09-21-beam-search-upstream-fix.md](2026-09-21-beam-search-upstream-fix.md),
which found the cause on the user's own recordings and measured it on
sherpa-onnx 1.13.6. That file left one thing open: every demonstration used
audio that cannot leave this machine.

## Question

1. Does the loss reproduce on audio anyone can download, with the released
   sherpa-onnx binary rather than through spokenpad?
2. Does it reproduce on Parakeet TDT **v2**, which the upstream issue names,
   and not only on the v3 we ship?
3. Is there a fix that keeps the TDT duration skip for blanks — and with it
   stock's decoding speed — instead of forcing a blank to one frame?

## Method

### The audio

The Parakeet model repository ships its own test clip. spokenpad already
downloads it as part of the pinned model set
(`core/models.rs`), so nothing new is fetched:

- `https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/test_wavs/en.wav`
- sha256 `148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6`,
  3.85 s, 24 kHz mono. It says "Ask not what your country can do for you, ask
  what you can do for your country."

Clips are built from it with `sox` (installed; `sox --version` reports
SoX_ng v14.8.0.1):

```sh
sox en.wav -r 16000 -c 1 -b 16 en16.wav                # 16 kHz, what spokenpad reads
sox en16.wav repro.wav trim 0 1.2 pad 3 3              # 3 s + 1.2 s speech + 3 s
```

Two sweeps, both under `~/.cache/spokenpad-dev/beam-fix/repro/`:

- `grid/`: speech `{0.6, 1.2, 3.85} s` × lead `{0, 3, 6} s` × trail
  `{0, 3, 6, 9} s`, 36 clips (`make-grid.sh`).
- `grid2/`: speech 1.2 s × lead `{0, 1, 2, 3} s` × trail `{0, 1, 2, 3} s`,
  16 clips.

The v2 model (`csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`,
`main`) was downloaded to the scratch directory, never to the user's model
directory: `encoder.int8.onnx` sha256
`a32b12d17bbbc309d0686fbbcc2987b5e9b8333a7da83fa6b089f0a2acd651ab`.

### The decoders

Two independent paths, so the result does not depend on spokenpad:

- The released `sherpa-onnx-offline` from
  `sherpa-onnx-v1.13.8-linux-x64-shared.tar.bz2`, run directly
  (`repro/upstream-cli.sh`).
- `examples/decode_probe --whole`, which decodes each WAV end to end with one
  model load, linked against a stock or patched static archive as
  [the previous experiment](2026-09-21-beam-search-upstream-fix.md#method)
  describes. The splice now targets the v1.13.8 archive; the decoder source
  file is byte-identical in v1.13.6 and v1.13.8, so every variant applies
  unchanged.

### The variants

| name | change against v1.13.8 |
|---|---|
| `v0stock` | none (splice control) |
| `v11blank1` | a blank advances exactly one frame |
| `v13frames` | a blank keeps the duration skip but is charged for every frame it consumes |

`v13frames` is the alternative the previous experiment listed as untried:

```c++
int32_t advance = is_tdt_ ? std::max(1, predicted_skip) : 1;
new_hyp.log_prob = hyp.log_prob +
                   static_cast<float>(advance) * token_logits[token] +
                   duration_log_prob;
new_hyp.frame_offset = hyp.frame_offset + advance;
```

The idea is to keep stock's speed — a blank still crosses several frames in
one step — while making the score comparable per consumed frame.

## Results

### The loss reproduces on the model's own test clip

`3 s + first 1.2 s of en16.wav + 3 s`, through the released
`sherpa-onnx-offline` v1.13.8 with the v3 int8 model, `--num-threads=4`:

| `--decoding-method` | output |
|---|---|
| `greedy_search` | `Ask not what your country` |
| `modified_beam_search` | `` (empty) |

Greedy's own per-token log-probabilities for those nine tokens run from
−0.026 to −0.00003, so the acoustic evidence is not in doubt.

### It is not one unlucky clip

36-clip sweep, v3 int8, stock v1.13.8. Only the 32 clips greedy itself
transcribes can show a loss; a clip is *lost* when the text the pipeline
would keep is empty or has fewer than half of greedy's words. Every loss
below was in fact an empty decode, with and without trailing padding.

| speech | clips greedy transcribes | stock lost | `v11blank1` lost | `v13frames` lost |
|---|---|---|---|---|
| 0.6 s | 8 | 5 | 2 | 3 |
| 1.2 s | 12 | **6** | **0** | **0** |
| 3.85 s | 12 | 0 | 0 | 0 |
| all | 32 | **11** | **2** | **3** |

Both fixes clear every clip with 1.2 s or more of speech. What is left is at
0.6 s — two or three words in a clip of up to 15 s, which is past the point
where greedy is reliable either (greedy transcribes only 8 of those 12
clips).

### How sharp the edge is

The 16-clip sweep at 1.2 s of speech, one cell per (lead, trail) pad. `.` is
text, `X` is an empty decode; `bare` is what the upstream CLI does,
`padded` is with the second of trailing zeros spokenpad's recogniser appends.

```
stock v1.13.8            v11blank1 and v13frames    v2 int8, stock
        trail 0 1 2 3            trail 0 1 2 3            trail 0 1 2 3
bare  lead 0  . X . .    bare  lead 0  . . . .    bare  lead 0  . . . .
      lead 1  . . . .          lead 1  . . . .          lead 1  . . . .
      lead 2  . . . X          lead 2  . . . .          lead 2  . . . .
      lead 3  . . . X          lead 3  . . . .          lead 3  . . . .
padded lead 0 X . . .    padded lead 0 . . . .    padded lead 0 . . . .
      lead 1  . . . .          lead 1  . . . .          lead 1  . . . .
      lead 2  . . X X          lead 2  . . . .          lead 2  . . . .
      lead 3  . . X X          lead 3  . . . .          lead 3  . . . .
```

Adding one second of silence flips a clip: 2 s before + 2 s after decodes,
2 s before + 3 s after returns `""`. Both fixes clear the whole sweep.

### What the beam is doing

The decoder instrumented to print its final beam, on the 7.2 s repro clip
(90 encoder frames), decoded bare:

| stock v1.13.8 | tokens | steps | frames per step | score |
|---|---|---|---|---|
| **winner** | 0 | 27 | 3.3 | **−16.41** |
| | 1 | 27 | 3.3 | −22.96 |
| | 1 | 27 | 3.3 | −23.03 |
| | 1 | 27 | 3.3 | −23.08 |

The correct nine-token path is not in the beam at all. Every surviving path
covers the utterance in 27 steps, because a blank step jumps the predicted
TDT duration: 90 frames for 27 scored steps, and the frames a blank jumps are
never scored. A path that emits the nine tokens needs at least nine more
steps, and each one costs.

With `v11blank1`, same clip, same decode:

| `v11blank1` | tokens | steps | frames per step | score |
|---|---|---|---|---|
| **winner** | 10 | 77 | 1.2 | **−36.82** |
| | 10 | 77 | 1.2 | −37.54 |
| | 10 | 77 | 1.2 | −38.81 |

The empty path is gone: it now pays `log P(blank)` at each of the 90 frames.
Scores are not comparable between the two tables, because the scoring
changed.

### Parakeet TDT v2

The upstream issue reports v2, so the same 36-clip sweep was run against
`sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`, stock v1.13.8:

| speech | clips greedy transcribes | stock lost |
|---|---|---|
| 0.6 s | 11 | 1 |
| 1.2 s | 12 | 0 |
| 3.85 s | 12 | 0 |
| all | 35 | **1** |

**v2 is barely affected**: one lost clip against v3's eleven, and none at all
on the fine sweep. The defect is the same code for both, so this is a
difference in how confidently each checkpoint's duration head skips; v3, the
one spokenpad ships, is the bad case. It also explains why the upstream issue
(filed on v2, where the failure is rare) has been hard to act on.

### Does the frame-cost variant work?

Yes, almost as well as forcing a blank to one frame, and it costs far less.
`v13frames` clears the fine sweep, clears every 36-clip case with 1.2 s or
more of speech, and leaves three of the eight 0.6 s clips (against
`v11blank1`'s two). On the private lost tail it needs the bare retry on one
window per sweep where `v11blank1` needs none.

It is the better trade if decode time matters, and the worse one if
reliability does; see the cost table.

### The sweeps on the user's lost tail, re-measured on 1.13.8

The two 9-window sweeps of the previous experiment, re-run against 1.13.8.
"padded" is the normal decode, "either" what the pipeline keeps after the
bare retry.

| decoder | A padded | A either | B padded | B either |
|---|---|---|---|---|
| greedy | 9/9 | 9/9 | 9/9 | 9/9 |
| beam, stock | 3/9 | 5/9 | 1/9 | 3/9 |
| beam, `v11blank1` | **9/9** | **9/9** | **9/9** | **9/9** |
| beam, `v13frames` | 8/9 | **9/9** | 8/9 | **9/9** |

The stock rows are identical to the 1.13.6 numbers in the previous
experiment, so the onnxruntime bump from 1.27.1 to 1.28.2 changed nothing
here. `v13frames` leaves one window per sweep that only the bare retry
rescues; `v11blank1` leaves none.

### Hotwords

`eval-samples/shell-commands.wav` as one 20.4 s window, reporting only how
each decode spelled the reference's `mkdir`
(the comparison [asr.md](../asr.md#measured-tuning-hotwords_score) uses).

| decoder | `vocabulary` | padded | bare |
|---|---|---|---|
| beam, `v11blank1` | none | `MKDIR` | `MKDIR` |
| beam, `v11blank1` | `mkdir`, 1.5 | **`mkdir`** | **`mkdir`** |
| beam, `v13frames` | none | `mkir` | `mkdir` |
| beam, `v13frames` | `mkdir`, 1.5 | **`mkdir`** | **`mkdir`** |

Both fixes keep hotword biasing. `v13frames` without a vocabulary reproduces
asr.md's original observation exactly, writing `mkir`, and the hotword
corrects it.

### Cost

Nine 10.6 s windows decoded padded and bare (18 decodes) plus one model load.
The machine was shared with other agents the whole evening and individual
rounds varied by a factor of four, so the table reports the **best time each
decoder reached** across all rounds, which is the reading least polluted by
interference. Treat the ratios as approximate.

| decoder | best of the rounds | against greedy | against stock beam |
|---|---|---|---|
| greedy | 14.9 s | — | — |
| beam, stock | 17.5 s | +17 % | — |
| beam, `v13frames` | 18.4 s | +23 % | **+5 %** |
| beam, `v11blank1` | 19.7 s | +32 % | **+12 %** |

`v11blank1` pays for every frame it now steps through one at a time.
`v13frames` keeps the skip, so it costs almost nothing over stock beam
search. The same ordering came out of the 1.13.6 measurement in the previous
experiment on a quieter machine (greedy 16.07 s, stock beam 17.61 s,
`v11blank1` 19.57 s), so the ranking is stable even if the seconds are not.

## The patch

Against `sherpa-onnx/csrc/offline-transducer-modified-beam-search-nemo-decoder.cc`
at v1.13.8 (byte-identical at v1.13.6; the upstream file has CRLF line
endings, so apply with whitespace tolerance).

`v11blank1`, the one that clears every sweep:

```diff
@@ -317,9 +317,9 @@
             for (const auto &state : hyp.decoder_states) {
               new_hyp.decoder_states.push_back(Clone(allocator, &state));
             }
-            // For blank/unk in TDT, always advance by at least 1
-            new_hyp.frame_offset =
-                hyp.frame_offset + std::max(1, predicted_skip);
+            // A blank is scored once per step, so letting it jump the
+            // predicted duration lets a blank-only path cross the utterance
+            // without being scored on the frames it skips. Advance one frame
+            // at a time; the duration head still drives every non-blank
+            // advance, so timestamps and durations are unchanged.
+            new_hyp.frame_offset = hyp.frame_offset + 1;
             new_hyp.num_symbols = 0;
           } else {
             // Non-blank: add token, use new decoder state
```

`v13frames`, the cheaper one that keeps the skip:

```diff
@@ -294,11 +294,6 @@
         // Create candidate hypotheses
         for (int32_t idx : top_k_tokens) {
           int32_t token = idx;
-          // For TDT: joint probability = P(token) * P(duration)
-          // In log space: log P(token, duration) = log P(token) + log
-          // P(duration)
-          float token_log_prob =
-              token_logits[token] + duration_log_prob + hyp.log_prob;
 
           NeMoHypothesis new_hyp;
@@ -307,22 +302,29 @@
           new_hyp.allocator = allocator;
-          new_hyp.log_prob = token_log_prob;
 
           float context_score = 0.0f;
 
           if (token == blank_id || token == unk_id_) {
             // Blank or unk: keep decoder state, advance frame
+            int32_t advance = is_tdt_ ? std::max(1, predicted_skip) : 1;
+            // Charge the blank for every frame it consumes, so a path that
+            // crosses the utterance in a few large blank steps is not
+            // cheaper than one that transcribes it.
+            new_hyp.log_prob = hyp.log_prob +
+                               static_cast<float>(advance) * token_logits[token] +
+                               duration_log_prob;
             new_hyp.decoder_states.reserve(hyp.decoder_states.size());
             for (const auto &state : hyp.decoder_states) {
               new_hyp.decoder_states.push_back(Clone(allocator, &state));
             }
-            // For blank/unk in TDT, always advance by at least 1
-            new_hyp.frame_offset =
-                hyp.frame_offset + std::max(1, predicted_skip);
+            new_hyp.frame_offset = hyp.frame_offset + advance;
             new_hyp.num_symbols = 0;
           } else {
             // Non-blank: add token, use new decoder state
+            new_hyp.log_prob =
+                token_logits[token] + duration_log_prob + hyp.log_prob;
             new_hyp.ys.push_back(token);
```

Both are kept as whole files in
`~/.cache/spokenpad-dev/beam-fix/variants/{v11blank1,v13frames}.cc`, with
`v0stock.cc` as the unmodified original they diff against.

## Conclusion

- **The failure reproduces on public audio with the released binary.** The
  model's own `test_wavs/en.wav`, cut to 1.2 s and padded with 3 s of silence
  on each side, decodes to `Ask not what your country` under
  `greedy_search` and to `""` under `modified_beam_search`, with
  `sherpa-onnx-offline` v1.13.8 and the v3 int8 model. Nothing private is
  needed to show the bug any more.
- **It is common, not exotic.** Of 32 constructed clips that greedy
  transcribes, stock beam search loses 11 outright. One second of extra
  silence flips a clip from correct to empty.
- **It is a v3 problem more than a v2 problem.** The same sweep loses 11
  clips on v3 int8 and 1 on v2 int8. The code path is the same, so the
  difference is in the checkpoints' duration heads. The upstream issue was
  filed against v2, which is probably why it has been hard to reproduce.
- **The instrumented beam confirms the cause.** On the repro clip every
  surviving path covers 90 encoder frames in 27 scored steps, and the winner
  emits nothing. A blank step jumps the predicted TDT duration while paying
  one `log P(blank)`, so the frames it skips are never scored.
- **Two patches work.** `v11blank1` (a blank advances one frame) clears every
  sweep it was given except the shortest clips, at about 12 % more decode
  time than stock beam search. `v13frames` (a blank keeps the skip but is
  charged for every frame it consumes) is nearly as good at about 5 %.
  Both keep hotword biasing.
- **Neither is complete.** Clips with 0.6 s of speech still lose 2 of 8
  (`v11blank1`) and 3 of 8 (`v13frames`).

Nothing that ships changes. Greedy stays the default (`asr.decoding`), and
this file exists so the analysis, the patch and the reproduction survive
whether or not the fix ever goes upstream.

### What is not measured

- **No corpus replay on 1.13.8.** Four 176-capture replays (greedy, stock
  beam, `v11blank1`, `v13frames`) were started and killed after 25 minutes
  because the machine was at load 50 and they write their output only at the
  end, so there are no partial numbers. The corpus figures in
  [the previous experiment](2026-09-21-beam-search-upstream-fix.md#whole-corpus-vad-replay)
  stand for 1.13.6, and the sweeps above show 1.13.8 behaves identically on
  stock. `v13frames` has no corpus numbers at all.
- The two remaining `"Yeah."` windows and the one window `v11blank1` still
  loses were not instrumented; that needed the killed replay to locate them.
- The patches have not been run against upstream's own test suite.
