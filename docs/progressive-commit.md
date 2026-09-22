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
| retained window | The part of the capture still in memory: from the committed offset to the last frame |
| VAD span | One raw run of speech reported by Silero |
| chunk | One or more spans merged for recognizer context and padded with real surrounding audio |
| settled chunk | A chunk whose boundary cannot move as more audio arrives |
| open tail | Audio after the committed offset; may still change |
| committed offset | Exclusive input frame through which text has landed |
| settled silence | Trailing silence old enough that no later window can reach into it |

## Chunk construction

Raw speech spans normally merge until they contain `vad.chunk_seconds` of
speech. This preserves context across ordinary breathing and thinking pauses;
decoding every short span separately measured about four WER points worse.

A long internal silence closes the pending chunk early, and so does the same
length of silence at the end of the slice: the pause is a decode boundary
whether or not the speaker has started again. The boundary is:

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

Lead padding stops at the previous chunk's `end_frame`. It may use the silence
after it — that audio is not committed, the offset stops at the speech — but
never the speech before it, which is already in the file. Without the clamp a
chunk after a pause shorter than its padding began up to 0.3 s inside the
previous chunk's speech and handed the recognizer words it had already
appended.

Unbroken speech is not cut here at all. Silero ends a span at
`vad.max_speech_seconds` (20 s), that span is already past `chunk_seconds`, so
it closes a chunk on its own; `chunk_seconds` only decides how many *shorter*
runs are merged into one window.

## Recording tick

On each preview tick the inference worker receives audio from a lagging hint,
then slices from its authoritative committed offset.

1. Split the remainder into chunks. No speech in it means no chunks, and the
   tick ends there with an empty preview and the settled-silence commit below.
2. Decode and commit every settled chunk in order.
3. Advance the offset to each committed chunk's `end_frame`.
4. Decode the first unsettled chunk as preview only.
5. Render that preview as Neovim extmark virtual text.

When the split holds no chunk at all, or only settled silence after the last
one, the offset still moves: it advances to one split threshold before the end
of the slice, on the detector's window grid, and commits empty text. Nothing is
decoded there — the VAD heard no speech — but the audio behind it is finished
with, which is what keeps a silent latch from growing.

While the uncommitted tail exceeds `preview.max_seconds` (30 s) the tick still runs
and still commits every settled chunk; only the cosmetic decode of the open
tail is skipped, so the tail settles and previews resume by themselves. The
winbar says which. Such a tick reads the first `preview.max_seconds` of the
tail only (`TickKind::Window`; the worker refuses one that holds less). When no chunk settles inside that window —
slow dictation whose pauses are too short to settle a chunk and whose speech
is too little to fill one — the window is committed through its last pause:
the end of the last speech the detector heard with silence after it, either
before the next speech or, for the window's last speech, for at least
`vad.pad_seconds` before the window ends. Every segment before the pause is
decoded, padded by at most `vad.pad_seconds` of the silence after it, and
committed at its own speech end, and the offset moves to the pause. What
follows the pause stays for the next tick: a word the window's end would cut
in two, and one begun too briefly before it for the detector to call it
speech yet, which a cut at the window's end left under an empty commit and
never decoded. Only a window with no pause at all — one run of speech the
detector never breaks — is committed through its end, where the cut may fall
inside a word; that is the price of the bound, and it is paid only there.
Without either, every later tick read the same window, nothing committed
until the release, and the tail grew with the capture. A release during such
a commit keeps the segment whose decode it waited for, as it keeps a settled
chunk, stops before the next one, and the release decodes the rest.

The worker owns the offset because preview and release inference are serialized
there. The main loop's hint exists only to avoid copying an entire long capture
on every tick and is never trusted for correctness. Both are `Frames` —
capture-absolute sample offsets with their own type, distinct from indices into
a snapshot, which may begin after the capture did.

## What is kept in memory

The capture buffer holds `committed offset .. last frame` and nothing else.
`AudioCapture::discard_before` drops whole device buffers from the front as the
committed offset moves, once per pass of the event loop. This is safe by
construction rather than by measurement: a tick slices from the committed
offset, a release slices from the committed offset, and a window's lead padding
is clamped to the start of the slice it was cut from, so no decode can read a
sample before that offset. The recovery WAV is written from the callback
before any of this and still holds the complete capture.

The retained window is therefore the open tail: one unsettled chunk, the
keep-back, and the audio that arrived while the tick was running.

The worst case is wider than it sounds, because a chunk holds
`vad.chunk_seconds` of *speech* and the pauses between its spans count for
nothing. Each of those pauses is under the split threshold, and each span is
at least `vad.min_speech_seconds`, so with the defaults a chunk can stretch
over 10 s of speech in about 67 spans of 0.15 s with just under 4 s of silence
between them:

```text
chunk_seconds + ceil(chunk_seconds / min_speech_seconds) x settling_silence
  = 10 s + 67 x 4 s = 278 s = 17 MB
```

plus one split threshold of silence (4 s with the default padding), kept back
so that a later window's lead padding has real audio to use and the detector
sees the same windows it would have seen, plus one `preview.interval_seconds` and
the decode of audio arriving meanwhile. Unbroken speech is the easy case: a
Silero span ends at `vad.max_speech_seconds` (20 s), which closes a chunk on
its own.

Ordinary speech is nowhere near that. Over half an hour of four-seconds-on,
six-seconds-off, the measurement is 1.7 MiB above the baseline, against
116.2 MiB when the committed audio is kept; the policy replay of mixed bursts
and pauses peaks at 26.0 s of retained audio
([experiment](experiments/2026-09-21-constant-ram-recording.md)). Both are
measurements of those schedules, not the bound.

`MAX_UTTERANCE_SECONDS` (3600) is a ceiling on that retained window, not on
the length of a capture. While the tick commits nothing comes near it; it is
reached only where ticks are too rare to keep up.
Reaching it is final for that capture: accepting audio again after a hole would
splice two moments that were never spoken together. So reaching it now *ends*
the capture — `AudioCapture` reports it, `Session::cap` turns it into
`Event::Exhausted`, and the state machine answers with the same
`Command::Decode` a key release produces, which stops the recorder too: the
recovery WAV is finished and closed there, not carried on past the ceiling.
Until 2026-09-21 it only marked the utterance released, which stopped the
decoding and left the recorder writing to disk with nothing reading it.

## When a capture ends by itself

Dropping committed audio left a capture with no length of its own. Three rules
in `core/state.rs` give it one, and all three end it the way a release does:
the tail is decoded, everything spoken is kept, and one notice says which rule
it was.

- **Silence.** A latched capture with no key down that has heard no speech
  for `capture.silence_timeout_seconds` (300 s) ends. "Heard speech" is text the
  recognizer produced — a settled commit or a live preview — or speech the
  detector found in a tick's audio that ends later than any it found before
  in this capture (`Preview::heard`), whichever came last. The second counts
  speech the recognizer has not made words of yet, as in a tick that only
  commits, and speech it never makes words of. `Session` raises
  `Event::Speech` for either, stamped when the result came back rather than
  at the position of the audio it describes, so a slow worker can only delay
  the stop and never end a capture whose words are merely undecoded. An
  empty commit (settled silence), an empty preview, and the same speech
  heard again by the next tick prove nothing, which is exactly what a quiet
  latch produces every tick.

  "No key down" is `last_press` older than `KEY_SETTLED` (1 s). A held
  push-to-talk key re-fires its binding every few tens of milliseconds, and
  so does the Shift+key that latched a capture — its repeats fire the toggle
  binding and refresh `last_press` — so ending either would only let the next
  repeat start the capture after it.

  The rule is off for a capture made before the speech model was ready: no
  tick runs then. Only a tick reports speech, so `Config::validate` also requires the timeout to be at
  least two `preview.interval_seconds`: the earliest a capture can report speech
  is one tick after the press, and a shorter timeout would end a capture that
  was never given the chance.
- **Length.** Any capture reaching `MAX_CAPTURE` (4 h) ends, whatever is
  being said into it. This is what keeps the recovery WAV readable: a WAV's
  RIFF sizes overflow at about 37 hours, and `read_capture` derives its own
  limit from `MAX_CAPTURE` plus the widest pre-roll and post-roll, so the two
  cannot drift apart.
- **Memory.** The in-memory ceiling above.

Only a key press can be read as a tap: a capture one of these rules ends is
never discarded for being shorter than `MINIMUM_HOLD`.

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

A stop of the daemon is not a cancel of what it owes. From its first moment
every capture with a recovery WAV is on the daemon's list of recordings whose
text is not all written (`Transcription`, in `shell/daemon/transcriptions.rs`, with a `Stage`:
capturing, finishing its tail, kept until the model is ready, sent to the
engine, or left to retry). A user's cancel takes it off; its last `Finished`
does too, unless that decode failed: a release decode that fails leaves the
capture to retry, kept from pruning and listed from where its text reaches,
with the "recording partly transcribed" notice, since this daemon's attempt
was the first. What is still on the list at the stop — a capture held then, which
the stop cancels, or one whose release decode did not end within the
shutdown's wait for the engine — goes to `waiting.tsv` with how far its text
reaches, and the next start transcribes the rest, as it does a recording made
before the model was ready. A running daemon writes only the recordings made
before the model was ready to that list: after a crash, a capture decoded as
it was captured would otherwise be written a second time from where the list
last saw it.

## Invariants

- No committed region is decoded again on release.
- Audio before the committed offset is dropped, and no window ever reads it.
- Settled silence advances the offset without any decode, by a whole number of
  detector windows, so the audio that stays is covered by the same windows.
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
closure, padding, settlement, preview isolation, and release tails. The
`long_capture` tests in `core/decode.rs` replay half an hour of synthetic
capture through the real merge policy and a counting recognizer, assert the
retained window stays under 45 s, and assert that dropping the committed audio
decodes exactly the same windows, over the same capture-absolute samples, as
keeping all of it. `tests/e2e.rs` checks progressive commits landing before
release through the real event loop and a real nvim, and a latched capture that
runs through a pause long enough to settle, and a latch nobody ends: it stops
on the silence rule, commits what was said exactly once, shows the notice, and
closes its recovery WAV. Two ignored tests measure the
memory and check the real Silero against the silence that was dropped.
