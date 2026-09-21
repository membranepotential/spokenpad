# Hardware

← [docs index](README.md) | The dictation window's per-output placement is
`pick_output`/`placement` in [`core/geometry.rs`](../src/core/geometry.rs).

spokenpad needs Linux, a microphone PortAudio can open, and an x86-64 or
aarch64 CPU. Any desktop works, X11 or Wayland; only managed mode needs i3 or
sway. Figures below come from one example machine and are labelled as such.

## Keys

spokenpad reads no input device, so it has no key of its own: the window
manager or desktop runs `spokenpad start|stop|toggle|cancel` from the user's
bindings ([README](../README.md#bind-your-keys),
[constraints.md](constraints.md#read-no-input-device)). The README's
examples use F16, which keyboard firmware (QMK, VIA) or a remapper such as
keyd can send from any key. Its chain on a standard XKB layout:

```
evdev 186 (KEY_F16)  →  X11 keycode 194  →  keysym XF86Launch7
```

So a binding names it as keycode `194` or keysym `XF86Launch7`, not `F16`.
`xev` (X11) or `wev` (Wayland) prints both for any key.

### Per-device keyboard layouts

A keyboard can carry its own XKB layout, set with `setxkbmap -device`, for
example an external keyboard with a different layout from the laptop's. A
uinput clone of the keyboard would lose that layout, which is one reason
spokenpad never touches an input device
([constraints.md](constraints.md#read-no-input-device)).

## Displays

Attach mode places no window. The two that do — managed and pane — both open
on whichever output holds the mouse pointer, and both fall back to a corner
where the pointer cannot be read: sway gives a client no way to ask, and
Xwayland answers with where the pointer last was over an X window
([nvim-window.md](nvim-window.md#placement)).
[`core/geometry.rs`](../src/core/geometry.rs) picks the output as a pure
function over output rectangles, so any number and arrangement works. Where
those rectangles come from differs: managed mode reads them from the window
manager over IPC, pane mode from RandR, falling back to the root window's
size on a server with no RandR.

Pane mode also needs a display it can rasterise into: a 24- or 32-bit visual
with the usual channel masks, which is every X server anyone runs. It refuses
anything else by name rather than drawing wrong colours.

## CPU

Decoding runs on the CPU only; a GPU is never used
([constraints.md](constraints.md#cpu-only)). `asr.num_threads` defaults to 6;
set it to your number of physical cores. On the example machine (Intel
i7-9850H, 6 cores) the default Parakeet model decodes at 9–17× real time,
see [asr.md](asr.md#speed-on-a-cpu). Measured peak memory of one
`spokenpad transcribe`: 1.2 GB with Parakeet, 0.36 GB with Whisper tiny.en.

## Audio

PortAudio, through its ALSA or JACK host API. PipeWire serves both;
PulseAudio serves ALSA through its plugin. `audio.device` unset
([`config.rs`](../src/config.rs)) means PortAudio's default input
device; no device is hardcoded. A configured value is a *query*, not a name:
PortAudio's device list is enumerated and the query's whitespace-separated
words are matched case-insensitively, **in order**, against `"<device
name>, <host API>"`, with a unique exact match winning an otherwise
ambiguous query (see `[audio]` in [`config.example.toml`](../config.example.toml)).

The input stream stays open while spokenpad runs, because the pre-roll ring has
to be warm at the key press. A stream that stops delivering audio is reopened by
the watchdog — mid-capture, where the gap is reported to the user, and while
idle, where it is only logged.

## Window manager

Attach mode (the default) needs nothing from the window manager: the user
opens the editor in any terminal. Managed mode needs i3 or sway, where the
dictation window must float and must never take focus; the window manager
enforces both through the rules in `packaging/i3/` or `packaging/sway/`
([nvim-window.md](nvim-window.md)).

Pane mode needs no rule and no named window manager: the window it draws
carries the properties that make a window manager float it and refuse it
focus, and every window manager reads those without being told. It needs an X
display, which on Wayland means Xwayland. **Verified on i3 only so far** —
the rest is read from source, and sway and awesome are known to want more
([constraints.md](constraints.md#no-window-spokenpad-opens-may-take-focus)).
