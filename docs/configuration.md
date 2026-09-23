# Configuration

← [docs index](README.md) | The file itself is
[`config.example.toml`](../config.example.toml): every key at its default,
with one or two lines on what it does. This page says why each default is
what it is, and what was measured on the way. The code that reads and checks
it is [`config.rs`](../src/config.rs).

## How the file is read

- `~/.config/spokenpad/config.toml` (`$XDG_CONFIG_HOME`), or the file given
  with `-c`. Every key is optional; an absent key keeps its default. An
  unknown key is an error, so a typo cannot pass silently.
- A key spokenpad no longer reads is refused with what became of it: `[hotkey]`
  (spokenpad reads no keyboard), `nvim.mode = "managed"` and its keys,
  `asr.family` and `asr.language` (Parakeet only), `vad.enabled` and
  `preview.enabled` (always on), `nvim.startup_timeout_seconds` (a fixed
  30 s, below), `nvim.notify` (no desktop notifications; the log says it),
  and the durations renamed to seconds
  (`audio.preroll_ms` is now `audio.preroll_seconds`, and so on; the message
  gives the value converted). See
  [decisions.md](decisions.md#every-duration-is-in-seconds-and-its-key-says-so-2026-09-22).
- Every duration is in seconds, and its key ends in `_seconds`.
- A running daemon reads the file again whenever it opens a dictation window
  or attaches to one: `[nvim]` applies to that window. Every other section
  takes effect after `systemctl --user restart spokenpad`. A file that does
  not load leaves the settings in use, and the window says why in a few
  words; `spokenpad check` prints the whole report.
- Paths expand `~` and `$VAR`; an unset `$VAR` is an error rather than a
  directory named after it. A relative model path resolves against the
  config file's own directory.

## [audio]

- `sample_rate` must be 16000. Silero's window is 512 samples at 16 kHz and
  Parakeet reads the same rate; nothing resamples in between.
- `preroll_seconds` (0.25) keeps audio from before the key goes down, so the
  first word is not clipped. It costs an always-open input stream. At 0 the
  stream is opened at each press instead, and a quick start can lose its
  first syllable.
- `postroll_seconds` (0.25, at most 1) keeps capturing after the key comes
  up, because speech was still sounding at key-up in 22 of 128 recorded
  captures. It runs from the `stop`; a `start` meanwhile ends it at once, so a
  quick re-press never loses its first word. A discard (a tap held too
  briefly, `spokenpad cancel`) takes no post-roll.
- `device` is a PortAudio device-name query: case-insensitive words matched
  in order against the device name and host API. `spokenpad check` lists the
  input devices and marks the one the daemon opens, or says why it opens
  none (a query that matches none, or several).

## [capture]

- `silence_timeout_seconds` (300) ends a latched capture that has heard no
  speech for that long; 0 turns it off. "Speech" is text the recognizer
  produced or speech the detector heard, so thinking pauses do not count.
  It must be at least twice `preview.interval_seconds`: the earliest a
  capture can report speech is one tick after the press. It skips a latch
  re-pressed within the last second, since a held Shift+key re-fires the
  toggle binding, and is off for a capture made before the speech model is
  ready. See
  [progressive-commit.md](progressive-commit.md#when-a-capture-ends-by-itself).
- Two limits have no setting because they are not preferences: a capture
  ends after 4 hours (`MAX_CAPTURE`, which keeps the recovery WAV readable),
  and at the in-memory ceiling of 60 minutes of held audio.

## [recording]

- `enabled` (on) writes every capture to a WAV while you speak. It is the
  safety net, on by default because an opt-in one is off exactly when it was
  needed: on 2026-09-08 a 13m41s hold hit the in-memory ceiling, and 3m41s of
  dictation was gone. With a recording, `spokenpad transcribe <wav>` recovers
  a decode that failed, was cancelled or stopped short. Until the speech
  model is ready the recording is the only copy of a capture, which the
  daemon transcribes once it is; with recording off such a capture is lost.
  About 32 KB per second of speech.
- `dir` should be a directory of its own: spokenpad prunes it, deleting only
  the `capture-*.wav` files it wrote. It is created 0700; one that exists
  keeps its permissions, and the daemon warns when others can list it.
- `max_total_bytes` (5 GiB, about 46 hours of speech) is enforced oldest
  first, at the start and at each capture. Age is not a criterion: a
  recording is pruned because something newer needs the space. The capture
  being written, and every recording still to be transcribed, is never
  pruned.

Since 2026-09-21 a capture holds only its open tail in memory: 1.7 MiB over
half an hour, against 116 MiB before
([experiment](experiments/2026-09-21-constant-ram-recording.md)).

## [asr]

- `model_dir` holds a NeMo transducer: `encoder`, `decoder` and `joiner`
  (`.int8.onnx` preferred over `.onnx`) and `tokens.txt`. The default is
  where `spokenpad fetch-models` puts Parakeet TDT 0.6B v3, and where the
  daemon, `check` and `transcribe` download it themselves when it is
  missing. See [asr.md](asr.md).
- `num_threads` (6): CPU threads per decode.
- `decoding`: `greedy_search` unless `vocabulary` or `hotwords_score` is
  set. Beam search on Parakeet TDT returns nothing, or an invented "Yeah.",
  for clear speech about one time in five (sherpa-onnx issue #3267);
  replaying 170 real captures it left 19 speech chunks empty against 4 for
  greedy. The defect is specific to TDT: on a transducer that is not TDT,
  such as parakeet-unified-en-0.6b, beam search and hotwords work
  ([decisions.md](decisions.md#greedy-decoding-by-default-beam-search-drops-speech-2026-09-21)).
- `hotwords_score` (1.5) is the per-token bias for each phrase in
  `vocabulary`. Measured on Parakeet: 1.5 fixes "mkir" to "mkdir"; above
  about 3.0 it fires on unrelated audio; 6.0 rewrites ordinary words
  ([asr.md](asr.md#measured-tuning-hotwords_score)).
- `vocabulary`: phrases to bias the decoder toward. It switches to beam
  search, with the failure rate above.

## [vad]

The detector is a correctness setting before anything else, and it is always
on. Parakeet returns an empty string when speech is a small fraction of its
window, and invents words ("Thank you.") for a window with no speech at all;
the detector decides what is decoded and cuts it into chunks. It does not
make decoding faster (12.3–12.6× real time whole-buffer against 11.0–11.7×
segmented); it makes text arrive while you speak. On the five reference
clips it scores 18.7% WER against 14.3% whole-buffer, but over 181 real
captures it wins on all eight model and decoder settings tried, by 0.2 to 9.3
points ([experiment](experiments/2026-09-21-live-path-against-whole-file.md)).
See [constraints.md](constraints.md#every-committed-sample-is-decoded-exactly-once-never-streamed).

- `model`: the Silero weights, fetched like the speech model. A detector that
  does not load leaves the speech model unavailable, and the window says so.
- `threshold` (0.5): speech probability above which a frame counts. Lower it
  if a quiet voice or a distant microphone is dropped; raise it if room noise
  is transcribed.
- `min_silence_seconds` (0.35): silence that ends a run. Too low chops a
  sentence at every breath, and the recognizer capitalises each piece as a
  fresh start; too high makes delivery less incremental.
- `min_speech_seconds` (0.15): the shortest run that counts, so a cough is
  not a segment.
- `max_speech_seconds` (20): the longest run; the audio continues in the
  next one.
- `chunk_seconds` (10): speech merged into one chunk before it is decoded.
  Decoding every run separately scored 37.7% WER against 33.7% for the
  whole buffer; merged to 10 s, 33.4%. Lower it for text sooner, at some
  cost in accuracy.
- `pad_seconds` (0.5): real audio kept either side of a chunk, because
  Silero's boundaries clip onsets and endings; 0.5 s beat 0.2 s by 2–4 WER
  points at every chunk size tried.
- `edge_pad_seconds` (2): a wider margin before the first chunk and after
  the last, for chunks of at least 3 s of speech. A word clipped off an edge
  is the first or last word of the sentence.

## [text]

- `strip_fillers` and `fillers`: removed as whole words, case-insensitively
  ("um" leaves "umbrella" alone).
- `replacements`: exact, whole-word, case-sensitive substitutions, applied in
  the order written. A key may begin or end with punctuation ("e.g."). Never
  fuzzy: the tool this project replaced turned "set" into "sed" and "reset"
  into "rust" by edit distance. Prefer `asr.vocabulary` for misheard terms
  ([constraints.md](constraints.md#bias-vocabulary-at-decode-time-never-fuzzy-replacement)).
- `trailing_space`: a space after each transcript.

## [nvim]

The transcript goes to a Neovim buffer over its socket; nothing is pasted
and no keystrokes are synthesised ([nvim-window.md](nvim-window.md)).

- `mode`: `pane` (the default) has the daemon open a window it draws itself,
  one no window manager focuses, verified on i3, sway, Openbox and KWin; it
  needs an X display, also on Wayland through Xwayland. `attach` has you run
  `spokenpad editor` in any terminal, on any desktop.
- `editor`: the Neovim command; spokenpad appends the init, the
  colourscheme, `--listen` and the file, and pane mode `--embed`.
- `init`: your own config when unset; `"bundled"` opens the window about
  3.5× faster on a full LazyVim setup (0.21 s against 0.73 s), and
  `colorscheme` then still brings your theme
  ([nvim-window.md](nvim-window.md)). Committed text is written with
  `noautocmd`, so a format-on-save cannot reflow it.
- `colorscheme` and `transparent`: with `init = "bundled"` a theme is loaded
  from its own plugin directory, at its defaults; `transparent` lets the
  background show through as a configured theme usually does.
- `pane_dimensions`, `pane_layout`, `font_family`, `font_size` (12): the
  pane's size in cells, its layout, and its font, sized in points exactly as
  Alacritty's `font.size` and scaled by the X resource `Xft.dpi` (96 dpi
  when unset). A tiled pane is allowed only where it is proven never to take
  the focus: i3, sway, Openbox and KWin. The pane opens beside the pointer,
  never under it ([constraints.md](constraints.md#no-window-spokenpad-opens-may-take-focus)).
- An editor spokenpad opens has 30 s to start and answer. That is not a
  setting: it only guards against an editor that never answers, such as a
  configuration stuck at a prompt. It is generous because a first open took
  13.5 s while a plugin manager did one-time work, and killing a healthy
  editor is the worse failure.
- `socket_path`: where Neovim listens; spokenpad reattaches to a live socket
  rather than opening a second window.
- `dictation_dir` and `file_template`: one file per editor, saved after every
  change. A real file rather than a scratch buffer, because a transcript
  lost with a closed buffer is the failure this project exists to prevent.
  Put a time in the template, or two sessions in one day share a page.
  The directory is created 0700 when it does not exist; one that exists is
  left as you set it.
- `copy_to_clipboard` (off): after each release, copy the whole buffer to
  `+` through Neovim's own clipboard provider.

## [preview]

- `interval_seconds` (1.0, at least 0.2): the tick. Each tick commits every
  chunk that has settled and decodes the open tail for the preview, which is
  virtual text and never file content. The real gap is
  max(interval − last decode, last decode), so the worker stays idle at least
  half the time. See [progressive-commit.md](progressive-commit.md).
- `max_seconds` (30): the longest open tail that is previewed. Past it the
  preview pauses and the tick still commits every chunk that settles. It
  bounds the cosmetic decode only: a tick always reads the whole tail, so
  the value changes no committed word.
