# What does a daemon-owned dictation window cost?

_2026-09-21, on the P2 commits of the own-window plan. Headless: Xvfb 21.1.24,
i3 4.25.1, Neovim 0.12.5, on an Intel i7-9850H. Other work was running on the
machine, so the timings are upper bounds._

## Question

Phase P0 proved the window can refuse focus and P1 that spokenpad can draw
Neovim in it. P2 wires it into the daemon as `nvim.mode = "pane"`, and the
question is what that costs a running daemon: how long a dictation waits for a
window, how much memory the window adds, what the process burns while the
window sits open, and which system libraries a binary with this in it needs.

The last one is not idle curiosity. Dictation's default mode opens no window
at all, so anything the pane drags into the binary is paid for by people who
never use it.

## Method

[`tests/pane_daemon.rs`](../../tests/pane_daemon.rs) runs the real
`shell::daemon::serve` with a synthetic microphone and a recogniser that
always returns the same sentence, on an Xvfb and an i3 it starts itself. One
push-to-talk is one second of audio; the pane opens on the key-down, as
managed mode's terminal does, so the window is opening while the recording
runs.

- **Latency** is measured from the key-up — when the daemon has everything it
  needs — to the transcript being readable in the dictation file.
- **Memory** is this process's resident set from `/proc/self/statm`, before
  the first dictation and after the pane is up.
- **Idle cost** is user plus system time from `/proc/self/stat`, in
  [`tests/pane_render.rs`](../../tests/pane_render.rs), over two seconds with
  the window open. Because moving the pointer costs the *test* something
  whether or not the pane hears about it, the same sweep beside the window is
  the control.
- **Libraries** are read with `ldd` from the release binary.

## Data

No recordings. One second of synthetic audio per dictation, three dictations,
and a window of `window_fraction` 0.33 on a 1280x800 screen at font size 16.

## Results

| measurement | value |
|---|---|
| key-up to the transcript in the file, opening the window on the way | **300 ms** |
| resident memory the pane adds | **5.4 MB** (33.1 MB → 38.5 MB) |
| processor time over 2 s, window open, nothing happening | **0.0 ms** |
| processor time over 2 s, pointer sweeping across the window | 0.0 ms; beside it, 0.0 ms |
| an append over the editor's socket, to drawn pixels | 95–151 ms (five runs, P1's measurement) |
| `ldd target/release/spokenpad` | no `libxcb`, no `libxcb-xkb`, no `libxkbcommon`, no `libxkbcommon-x11` |
| whole `tests/pane_daemon.rs` | 6–7 s; `pane_render` 9–13 s; `pane_window` 11–13 s |

What the test asserts beside the numbers, all passing:

| | |
|---|---|
| no `DISPLAY` | the daemon says so and the transcript reaches the pending passage |
| a dictation | opens a window i3 reports as `focused: false` and floating, and the window that had the focus keeps it, by the i3 tree and by `GetInputFocus` |
| `nvim.copy_to_clipboard` | the release puts the buffer in that X server's clipboard, through Neovim's own provider |
| closing the window | ends the passage: the next dictation opens a new window on a new file |
| stopping the daemon | leaves nothing listening on the socket and no window in the tree |

## Conclusion

**The window is cheap enough that the cost is not an argument against it.**

- **300 ms from key-up**, with the window opening inside it, against managed
  mode's 244 ms from key-*down* to a placed terminal answering RPC. The two
  numbers measure different things and neither is on the latency path: the
  window opens while the user is still speaking, and a decode is seconds.
- **5.4 MB** is the framebuffer, the rasterised glyphs and the editor's UI
  client, against a terminal emulator's tens of megabytes — which pane mode
  does not start at all. On a daemon that already holds 1.2 GB of model this
  does not register.
- **Nothing while idle**, by construction rather than by tuning: the pane's
  loop blocks on one channel that two threads feed, with no timer and no
  polling. The pointer sweep is the interesting case, because a pane sits
  under someone's mouse path all day, and after P1's review narrowed the event
  mask to "motion only while a button is down" it costs the same as no pointer
  at all — at the resolution the process clock offers, which is one 10 ms tick.
- **The binary is unchanged for everyone else.** libxcb and libxkbcommon are
  opened with `dlopen` when a pane opens, so `ldd` on the shipped binary lists
  none of them and an attach-mode user on a machine without them still starts.
  `spokenpad check` reports the three things a pane needs — libraries, font,
  display — and exits non-zero when one is missing.

**What this does not show.** Only i3, only headless, only this machine's
fonts and this one monitor. Multi-monitor placement is written against RandR's
monitor list and exercised only against a single-monitor Xvfb; the fallback to
the root window's geometry is exercised not at all. The Wayland path — the
pointer being stale, the window going to a corner — is decided by
`WAYLAND_DISPLAY` and has never been run under a compositor. Those are P3 and
P4.

The decision this supports is in
[decisions.md](../decisions.md#spokenpad-draws-its-own-dictation-window-nvimmode--pane-2026-09-21).
