# Progressive commit

← [docs index](README.md) | Implemented by `shell/inference.rs`,
`core/segments.rs`, `core/decode.rs`, and `core/session.rs`; mirrored offline
by `spokenpad.vad` and `spokenpad.decode`.

Text is committed while the user is still speaking, once later audio cannot
change the chunk it came from. Releasing the key decodes only the open tail,
so release latency is bounded by one chunk rather than the whole passage.

## Vocabulary

| Term | Meaning |
|---|---|
| capture | Audio accumulated since key-down, in memory and independently on disk |
| VAD span | One raw run of speech reported by Silero |
| chunk | One or more spans merged for recognizer context and padded with real surrounding audio |
| settled chunk | A chunk whose boundary cannot move as more audio arrives |
| open tail | Audio after the committed offset; may still change |
| committed offset | Exclusive input frame through which text has landed |

## Chunk construction

Raw speech spans normally merge until they contain `vad.chunk_seconds` of
speech. This preserves context across ordinary breathing and thinking pauses;
decoding every short span separately measured about four WER points worse.

A long internal silence closes the pending chunk early. The boundary is:

```text
max(1 second, 2 × max(edge_pad_seconds, pad_seconds))
```

With the defaults this is four seconds. Both sides of such a split become
context edges and receive `edge_pad_seconds` when the adjacent speech is wide
enough. The padding-aware threshold keeps those windows from overlapping while
allowing short pauses to retain useful context. It also prevents a long quiet
middle from making speech too sparse in one recognizer window, a case in which
Parakeet can return an empty or truncated transcript.

Each chunk carries real audio around its detected speech: `pad_seconds` at
internal boundaries and the larger `edge_pad_seconds` at the outer edges when
there is enough speech for extra silence not to dominate. `end_frame` remains
the unpadded speech boundary; advancing by padded length would skip or repeat
audio at the next split.

## Recording tick

On each preview tick the inference worker receives audio from a lagging hint,
then slices from its authoritative committed offset.

1. Split the remainder into chunks. No speech in it means no chunks, and the
   tick ends there with an empty preview.
2. Decode and commit every settled chunk in order.
3. Advance the offset to each committed chunk's `end_frame`.
4. Decode the first unsettled chunk as preview only.
5. Render that preview as Neovim extmark virtual text.

No tick is issued at all when the pipeline has no segmenter: with nothing ever
settling, the preview would be a re-decode of the whole growing capture. Ticks
also pause while the uncommitted tail exceeds `preview.max_seconds` (30 s) and
resume by themselves once it falls back under; the winbar says which.

The worker owns the offset because preview and release inference are serialized
there. The main loop's hint exists only to avoid copying an entire long capture
on every tick and is never trusted for correctness. Both are `Frames` —
capture-absolute sample offsets with their own type, distinct from indices into
a snapshot, which may begin after the capture did.

## Release and cancellation

Release invalidates queued previews and decodes `capture[committed_offset:]`
through the same segment/decode pipeline used by recovery. A preview already in
inference may finish its current chunk; any settled commit it produced stands,
and release begins after it.

Cancellation means “stop adding.” Already committed text and the independently
recorded WAV remain, and an append queued before the cancel is still written.
It applies only while recording: once the key is released the audio is captured
and the final decode is under way, so a cancel is ignored rather than allowed to
destroy it. The `Utterance` lifecycle (`Live` → `Released` | `Cancelled`,
monotone) prevents work queued for an older capture from advancing or resetting
a newer one's offset.

## Invariants

- No committed region is decoded again on release.
- Preview re-decode is confined to the bounded open tail.
- Preview text never enters the Neovim buffer or transcript file.
- Empty chunk text still advances a settled offset.
- A capture with no VAD speech in it yields no chunks and is not decoded: no
  recognizer call, no commit, an empty preview, and the committed offset stays
  where it was.
- If all segmented release decodes are empty and there was more than one
  chunk, one whole-buffer retry is allowed as an explicit recovery exception.
  It does not apply to a capture that had no chunks: that audio was never
  claimed to be speech.
- A missing/corrupt VAD model falls back to one whole-buffer chunk.

## Verification

Unit tests cover offset ownership, stale work, cancellation, long-pause
closure, padding, settlement, preview isolation, and release tails;
`tests/e2e.rs` checks progressive commits landing before release through the
real event loop and a real nvim. `examples/verify_native.rs` prints the
segment bounds, settlement flags, commit offsets, raw text and final
remainder of a simulated live passage as JSON, for inspection against the
real models.
