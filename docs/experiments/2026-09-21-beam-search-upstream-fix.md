# Can beam search be made reliable on Parakeet TDT?

_2026-09-21, spokenpad at `51b2c72`, sherpa-onnx 1.13.6, Parakeet TDT 0.6B v3
int8 + Silero VAD, `asr.num_threads = 6`. Machine: Intel i7-9850H, 12 threads,
busy with parallel agent builds throughout (load average ~22), so every wall
time below is an upper bound and only comparable within this file._

## Question

spokenpad decodes Parakeet greedily since
[decisions.md, "Greedy decoding by default"](../decisions.md#greedy-decoding-by-default-beam-search-drops-speech-2026-09-21),
because `modified_beam_search` drops about one speech window in five. Beam
search is still the only search sherpa-onnx applies hotwords in, so we want it
back. Three questions:

1. Is the upstream bug ([k2-fsa/sherpa-onnx#3267](https://github.com/k2-fsa/sherpa-onnx/issues/3267))
   fixed on master or in a release newer than 1.13.6?
2. Does the open fix attempt ([PR #3657](https://github.com/k2-fsa/sherpa-onnx/pull/3657))
   work on our audio?
3. If not, what does, and how could we ship it?

## Upstream state (checked 2026-09-21)

| | |
|---|---|
| Issue #3267 | **open**, no maintainer comment on the thread |
| PR #3657 | **open, not merged**, last commit 2026-06-03 |
| Maintainer's only comment on #3657 | 2026-06-06, asks for a WAV that reproduces the bug; **unanswered** since |
| Latest sherpa-onnx release | v1.13.8, 2026-09-10 (v1.13.7 2026-09-01) |
| Crates `sherpa-onnx` / `sherpa-onnx-sys` | 1.13.8, published 2026-09-11, versions track the C++ tag 1:1 |
| Commits to `offline-transducer-modified-beam-search-nemo-decoder.cc` since v1.13.6 | **none** |

The decoder source file is byte-identical in v1.13.6 and v1.13.8 (verified by
downloading both), as is `offline-transducer-greedy-search-nemo-decoder.cc`.
So upgrading to 1.13.8 neither fixes nor changes anything measured here, and
every patch in this file applies to 1.13.8 unchanged. The last change to the
decoder was [#3589](https://github.com/k2-fsa/sherpa-onnx/pull/3589)
(2026-05-08, in v1.13.6 already); #3267 was filed after it.

`sherpa-onnx-sys` 1.13.8 keeps the `SHERPA_ONNX_LIB_DIR` escape hatch and the
same static library list, so the method below carries over.

## What PR #3657 changes

Three changes, against a base identical to v1.13.6:

1. `max_symbols_per_frame` 10 → `is_tdt_ ? 5 : 10`.
2. Blank and unk candidates no longer get the duration log-probability added
   to their score; non-blank candidates still do.
3. Blank advances by `is_tdt_ ? std::max(1, predicted_skip) : 1` instead of
   `std::max(1, predicted_skip)`.

Reading them against the greedy TDT decoder (`DecodeOneTDT` in the same tree):

- **Change 1 is a no-op for this model.** The cap only fires when a
  hypothesis emits `max_symbols_per_frame` tokens at one frame, which needs a
  run of duration-0 predictions. Measured: a variant with only this change
  produced output identical to stock on all 18 windows of the sweep below.
- **Change 3 is a no-op everywhere.** For TDT the two expressions are the
  same text; for non-TDT `predicted_skip` never leaves its initial `1`, so
  `std::max(1, predicted_skip)` and the new `is_tdt_ ? … : 1` both give 1.
- **Change 2 is the only change with an effect, and it points the wrong
  way.** It makes a blank step cheaper than a token step, and cheap blank
  steps are exactly what the decoder already over-prefers. Measured below:
  across the two sweeps it left 15 of 18 windows without text where stock
  left 10.

The PR's stated root cause — "missing `blank && skip==0` force-advance guard,
causing an infinite same-frame loop" — does not exist in v1.13.6: the guard is
the `std::max(1, predicted_skip)` the PR rewrites.

Two further defects the PR does not touch:

- **Hotword scores are counted twice.** `hotwords_score_` is added to
  `token_logits[token_id]` before the top-k selection, and then
  `ContextGraph::ForwardOneStep` adds its own score to `new_hyp.log_prob`.
  The pre-top-k boost is deliberate (it lets a hotword token survive
  selection), but it is also left in the score that is compared.
- **The score is a plain sum over decoding steps with no length term**, and
  that is the actual bug; see the next section.

One thing that is *not* a defect, despite reading like one: the hypothesis
stores the decoder states from *before* its last token and re-feeds that token
each round, so every token still enters the prediction network exactly once.
A variant that caches the prediction-network output instead and runs the
decoder only on emission produced output identical to stock on all 18 sweep
windows, confirming the two are the same computation. It runs the prediction
network fewer times, so it is worth having, but it fixes nothing.

## Why beam search loses the speech

Instrumenting the decoder to print the final beam of the lost tail
(a 10.6 s window, 145 encoder frames, whose last 4.1 s are clear speech):

| beam entry | tokens | steps | end frame | total log-prob | per step |
|---|---|---|---|---|---|
| **winner: empty** | 0 | 47 | 145 | **−20.81** | −0.443 |
| the correct transcript | 12 | 52 | 145 | −23.20 | −0.446 |
| a near-identical rival | 12 | 52 | 145 | −23.25 | −0.447 |
| a near-identical rival | 12 | 52 | 145 | −23.94 | −0.460 |

Both paths end at the same frame and cost almost the same *per step*. The
empty path wins because it takes five steps fewer, and the score is
`sum over steps of (log P(token) + log P(duration))` with nothing that
accounts for how much audio a step consumed. A blank may consume up to the
model's largest duration in one step, so a blank-only path crosses the
utterance in the fewest steps and collects the fewest negative terms. Whether
it wins is a matter of a couple of nats — hence "about one window in five".

Two corollaries, both measured:

- A wider beam makes it **worse** (see the sweep table): more search finds
  more of the degenerate short path.
- Length-normalising the score would not help: per step the empty path is
  already slightly ahead (−0.443 vs −0.446).

## Method

sherpa-onnx v1.13.6 was checked out, configured and built once from source to
obtain the headers and the exact compile flags. A full source build could not
be used for measurements: linked into the Rust binary it aborts with
`free(): invalid pointer` inside `onnxruntime::DeviceDiscovery`, because the
locally compiled `libsherpa-onnx-core.a` (GCC 16.2.1) exports `std::regex`
template instantiations that override the ones the distributed
`libonnxruntime.a` was built against.

So each variant instead replaces **one object file** inside the stock
prebuilt static archive:

```sh
c++ $DEFINES $INCLUDES -O3 -DNDEBUG -std=c++17 -fPIC -fvisibility=hidden \
  -c variants/<variant>.cc \
  -o obj/<variant>/offline-transducer-modified-beam-search-nemo-decoder.cc.o
cp -al <stock-lib-dir> lib-<variant>          # hardlink the untouched archives
rm lib-<variant>/libsherpa-onnx-core.a
cp <stock-lib-dir>/libsherpa-onnx-core.a lib-<variant>/
ar r lib-<variant>/libsherpa-onnx-core.a obj/<variant>/...o
ranlib lib-<variant>/libsherpa-onnx-core.a
```

`$DEFINES` and `$INCLUDES` are copied verbatim from the source build's
`flags.make`, **plus `-D_GLIBCXX_USE_CXX11_ABI=0`**. That flag is not
optional: the released static archive is built with the pre-C++11
`std::string` ABI (`nm -C libsherpa-onnx-core.a` finds `std::string`, never
`std::__cxx11::basic_string`). Compiled with the default ABI the spliced
object still links, and the ordinary decoding path still agrees with stock
exactly, but `std::string` is 32 bytes instead of 8, so `ContextState::phrase`
shifts every field after it. The first decode with a hotword list then
segfaults iterating `hyp.context_state->next`. Stock does not crash, and
*every* variant did until the flag was added — including PR #3657's — which is
how the flag was found.

The control for this trick is variant `v0stock`: the unmodified v1.13.6
source, recompiled and spliced in the same way, produced text identical to the
untouched prebuilt archive on all 18 sweep windows. It does so under both
ABIs, and `v11blank1` produces identical text under both ABIs too, so the
decoding measurements taken before the flag was added still stand.

`sherpa-onnx-sys`'s build script links against any directory named by
`SHERPA_ONNX_LIB_DIR` instead of downloading the release archive, so a variant
is measured with:

```sh
SHERPA_ONNX_LIB_DIR=/home/felix/.cache/spokenpad-dev/beam-fix/lib-<variant> \
  cargo build --locked --release --example decode_probe
```

Nothing in the repository changes: no `Cargo.toml` edit, no `[patch]`, no
vendored file. `main` keeps linking the stock release archive.

The measurements run through `examples/decode_probe`, added by this change. It
has two modes: `--range START:END` decodes chosen second ranges of one WAV
twice, with and without the second of trailing zeros
`shell/inference.rs` appends, and prints both texts; without `--range` it
splits every given WAV with the VAD and counts what the
[`Pipeline`](../../src/core/decode.rs) retry policy would have produced,
printing counts only, never the text of a private recording.

### Variants

| name | change against v1.13.6 |
|---|---|
| `v0stock` | none (splice control) |
| `v1pr3657` | PR #3657, all three changes |
| `v2nodur` | duration log-prob dropped from every candidate's score |
| `v3cap5` | PR change 1 alone (`max_symbols_per_frame = is_tdt_ ? 5 : 10`) |
| `v5beam8` | `v2nodur` with the beam widened from 4 to 8 |
| `v6cache` | prediction-network output cached in the hypothesis |
| `v8group` | top-k taken inside each frame offset instead of across all |
| `v9groupnodur` | `v8group` + `v2nodur` |
| **`v11blank1`** | **a blank advances exactly one frame, never `predicted_skip`** |
| `v12blank1nodur` | `v11blank1` + `v2nodur` |

`v11blank1` is the whole fix: one line in the blank branch,

```c++
-            new_hyp.frame_offset =
-                hyp.frame_offset + std::max(1, predicted_skip);
+            new_hyp.frame_offset = hyp.frame_offset + 1;
```

A blank that may jump the model's largest duration can cross the utterance
without ever being scored on the frames it jumps. Forcing it to one frame
makes a blank-only path pay `log P(blank)` at every speech frame, which is
what makes the correct path win. The duration head still drives every
non-blank advance, so token timestamps and durations are unchanged.

## Data

- One recovery capture, `capture-2026-09-21-142322.wav`, 38.6 s, the one whose
  tail was lost. The tail is its last 10.6 s (from 27.97 s); its speech is at
  about 32.6–34.2 s and 35.1–36.7 s.
  - Sweep A: nine windows, start `27.97 + k × 0.25 s` for `k = 0…8`, end
    38.57 s.
  - Sweep B: nine windows, start 27.97 s, end `38.57 − k × 0.20 s`.
  - Every window contains all of the speech.
- All 176 recovery captures in `~/.local/state/spokenpad/audio/`, 73.4 min of
  audio, replayed whole through the VAD.
- `eval-samples/shell-commands.wav` (20.4 s, committed reference) for the
  hotword check.

The private recovery captures are never quoted: the sweep reports how many
windows produced text, not what they said, and the corpus replay reports only
counts. The one word quoted in this file comes from the committed
`eval-samples` reference.

## Results

### Sweep over the lost tail, 9 + 9 windows

"padded" is the normal decode (1 s of trailing zeros), "bare" the retry
without them, "either" what the pipeline would end up with.

| variant | A padded | A bare | A either | B padded | B bare | B either |
|---|---|---|---|---|---|---|
| greedy (shipping default) | **9/9** | 9/9 | **9/9** | **9/9** | 9/9 | **9/9** |
| beam, stock v1.13.6 | 3/9 | 5/9 | 5/9 | 1/9 | 2/9 | 3/9 |
| beam, `v1pr3657` | 0/9 | 1/9 | 1/9 | 1/9 | 1/9 | 2/9 |
| beam, `v3cap5` | 3/9 | 5/9 | 5/9 | 1/9 | 2/9 | 3/9 |
| beam, `v6cache` | 3/9 | 5/9 | 5/9 | 1/9 | 2/9 | 3/9 |
| beam, `v8group` | 3/9 | 5/9 | 5/9 | 1/9 | 2/9 | 3/9 |
| beam, `v2nodur` | 7/9 | 2/9 | 7/9 | 3/9 | 6/9 | 7/9 |
| beam, `v9groupnodur` | 7/9 | 3/9 | 7/9 | 3/9 | 6/9 | 7/9 |
| beam, `v5beam8` | 3/9 | 1/9 | 4/9 | 1/9 | 3/9 | 3/9 |
| beam, **`v11blank1`** | **9/9** | 9/9 | **9/9** | **9/9** | 9/9 | **9/9** |
| beam, `v12blank1nodur` | 7/9 | 8/9 | 8/9 | 2/9 | 6/9 | 6/9 |

`v0stock` is omitted: identical to the stock row, which is the point of the
control.

### Whole-corpus VAD replay

All 176 recovery captures, split by the VAD into 325 speech windows, each
decoded the way the pipeline does: one padded decode, then a bare retry only
if that came back empty.

| decoder | empty on first decode | still empty after the retry | window decoded as just "Yeah." | words |
|---|---|---|---|---|
| greedy (shipping default) | 5 | **0** | 0 | 7023 |
| beam, stock v1.13.6 | 20 | 3 | 3 | 6881 |
| beam, **`v11blank1`** | **1** | 1 | 2 | 7004 |

The fix takes beam search from 20 empty first decodes to 1, and from three
lost windows to one. Stock beam search produced 142 words fewer than greedy;
the fix recovers 123 of them, ending 19 words (0.3 %) short of greedy.

Two things it does not fix: the one window that stays empty through the
retry, and the invented "Yeah." — 2 windows against stock's 3 and greedy's 0.
So the fix removes most of #3267 but not all of it, and greedy is still the
safer decoder on this corpus.

### Hotwords still bias the beam

`eval-samples/shell-commands.wav` decoded as one 20.4 s window, the same
comparison [asr.md](../asr.md#measured-tuning-hotwords_score) uses. Reported
is only how each decode spelled the reference's `mkdir`.

| decoder | `vocabulary` | padded | bare |
|---|---|---|---|
| greedy | — (rejected by the config) | `MK D I R` | `MK D I R` |
| beam, stock | none | `Mk Dir` | `mkdir` |
| beam, stock | `mkdir`, score 1.5 | `mkdir` | `mkdir` |
| beam, stock | `mkdir`, score 3.0 | `mkdir` | `mkdir` |
| beam, `v11blank1` | none | `MKDIR` | `MKDIR` |
| beam, `v11blank1` | `mkdir`, score 1.5 | **`mkdir`** | **`mkdir`** |
| beam, `v11blank1` | `mkdir`, score 3.0 | `mkdir` | `mkdir` |

So biasing still works with the fix: the hotword's own spelling replaces the
model's. Stock at score 3.0 also over-fires, turning an unrelated phrase into
`mkdir` and shortening the decode from 32 to 30 words, the side effect
asr.md already records; `v11blank1` at 3.0 did not over-fire on this clip
(35 words against 34 at 1.5). One clip is not enough to call that an
improvement.

Word error rate on this clip is not reported: decoded as a single whole-clip
window rather than through the VAD it sits between 41 % and 59 % for every
row above, far above the 17.6 % asr.md measures over the VAD path, and one
clip cannot separate the variants. That comparison belongs in the WER harness.

### Cost

Nine 10.6 s windows decoded padded and bare (18 decodes) plus one model load,
three runs each, interleaved; machine load 8–21 throughout, so read the ratios,
not the seconds.

| decoder | run 1 | run 2 | run 3 | mean | against greedy |
|---|---|---|---|---|---|
| greedy | 15.02 s | 16.69 s | 16.51 s | 16.07 s | — |
| beam, stock | 16.93 s | 17.39 s | 18.50 s | 17.61 s | +10 % |
| beam, `v11blank1` | 18.53 s | 19.28 s | 20.90 s | 19.57 s | +22 % |

Making a blank advance one frame costs about 11 % over stock beam search,
because a blank-only stretch now takes one step per frame instead of one step
per predicted duration. On the whole-corpus replay the two beam passes ran
concurrently and finished in the same minute, so the difference does not
matter at spokenpad's scale: the decode is fired once per utterance and runs
at roughly 15× real time.

## How a fix could ship

The bug is in the C++ static library, not in the Rust bindings, so every
option is about which `libsherpa-onnx-core.a` the linker sees.

| option | effort | build cost | maintenance | single static binary | plain `cargo build` |
|---|---|---|---|---|---|
| Wait for upstream | none | none | none | kept | kept |
| Upstream our one-line fix, keep greedy meanwhile | small (a PR plus a shareable repro) | none | none | kept | kept |
| Build sherpa-onnx from source in `scripts/install.sh` | large | ~30 min per machine, plus a 110 MB onnxruntime download and ~4 GB of scratch | high | kept | **broken** (needs cmake, git, a C++ toolchain) |
| Vendor a patched static archive, point `SHERPA_ONNX_LIB_DIR` at it | medium | none after the one-off build | rebuild per sherpa release | kept | **broken** without the env var |
| Splice one recompiled object into the downloaded archive at build time | medium | seconds, plus a 50 MB source checkout | high | kept | **broken** (needs a C++ compiler and the sherpa sources) |
| `[patch]` `sherpa-onnx-sys` | medium | — | — | kept | kept, but it only changes *where the archive comes from*: the patched archive still has to be built and hosted somewhere, so this is the vendoring row with extra steps |
| Reimplement TDT beam search in Rust | very large | none | high | kept | kept |

Notes behind the table:

- **Building from source is worse than it looks.** On this machine a source
  build of v1.13.6 links but crashes at start-up (`free(): invalid pointer`,
  `std::regex` instantiations from GCC 16 overriding the ones the distributed
  static `libonnxruntime.a` expects). Any from-source option has to solve
  that first, on every user's toolchain.
- **Reimplementing is not possible over the crate.** `sherpa-onnx` 1.13.6/1.13.8
  exposes recogniser-level types only; `OfflineTransducerModelConfig` carries
  `encoder`/`decoder`/`joiner` as *file paths*, and neither the crate nor
  `sherpa-onnx-sys` surfaces an ONNX session, a logit or a decoder state. It
  would mean loading the three `.onnx` files with `ort` and reimplementing
  feature extraction as well.
- **Splicing one object is sharper than it looks.** It only works because
  the spliced translation unit is compiled with exactly the archive's ABI, and
  that ABI is not declared anywhere: `-D_GLIBCXX_USE_CXX11_ABI=0` had to be
  discovered from a segfault. The same trap is waiting for any header whose
  layout the upstream build changes, and a mismatch links cleanly and fails at
  run time on one code path.
- **Vendoring costs the property that makes this project easy to build.**
  Today `cargo build --locked --release` needs nothing but the Rust
  toolchain; the sys crate downloads a verified release archive. Every
  local-library option replaces that with a step the user has to get right.

### Recommendation

Keep greedy as the shipping default, and upstream the one-line fix.

The fix is small, it is in the file the maintainer already has an open PR on,
and the maintainer's blocker is a reproducer, which the measurements here
supply except for the audio. Finding a clip we may share that reproduces the
loss is the one open piece of work; the user's recordings cannot leave this
machine. Until it lands in a release we lose nothing we have today: greedy is
the default, and hotwords are the only thing beam search buys.

Do not vendor a patched library for this. It costs the one-command build, it
has to be redone for every sherpa-onnx release, and it would be carried for a
feature (`asr.vocabulary`) that is off by default.

## Conclusion

- **#3267 is not fixed anywhere.** Not on master, not in v1.13.7 or v1.13.8;
  the decoder file has not been touched since before v1.13.6. The 1.13.8
  upgrade is safe to do for other reasons and changes nothing here.
- **PR #3657 does not fix it and should not be adopted.** Two of its three
  changes are no-ops for Parakeet TDT, and the third makes the loss worse: on
  the lost tail it produced text for 1 of 9 windows where stock managed 3.
- **The cause is the score, not a missing guard.** The beam's score is a sum
  of per-step log-probabilities with nothing that accounts for how much audio
  a step consumed, and a blank may consume up to the model's largest duration
  in one step. The instrumented beam shows the empty hypothesis beating the
  correct one by 2.4 nats while both end at the same frame, purely on step
  count.
- **One line fixes most of it.** Making a blank advance exactly one frame
  forces a blank-only path to be scored at every frame it crosses. On the
  9 + 9 window sweep it matches greedy, 18 of 18. On 325 corpus windows it
  cuts empty first decodes from 20 to 1 and recovers 123 of the 142 words
  stock beam search lost against greedy, at about 11 % more decode time.
  Hotword biasing still works.
- **It is not a complete fix.** One window still decodes empty through the
  retry, and two still decode as an invented "Yeah.", against greedy's none.

Nothing here changes what spokenpad ships. Greedy stays the default; the
recommendation above is to send the one-line fix upstream rather than vendor
a patched library, and to revisit `asr.decoding` only once it is in a
release. The WER comparison that would decide whether beam search is
*better* than greedy when it does not drop anything is not in this file; it
belongs to the reference-transcript harness.

### Open questions

- A reproducer the maintainer can use. The failure is easy to show on the
  user's own recordings, which cannot leave this machine; a clip we may share
  still has to be found.
- Whether the remaining "Yeah." windows have the same cause or a second one.
  The same instrumentation would answer it.
- Whether a fix that keeps the duration skip for blank but charges the path
  for every frame it skips would be both correct and as fast as stock. It was
  not tried.
- [asr.md](../asr.md) still calls `modified_beam_search` the default and
  reports its timings; that text predates the greedy switch and is stale.
