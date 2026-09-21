# Hardware

← [docs index](README.md) | The keycode chain below is why
`shell/hotkey.rs` (see [architecture.md](architecture.md)) reads evdev
directly instead of using a keysym-based hotkey library — reinforced by
[constraints.md](constraints.md#read-evdev-read-only). The dictation
window's per-output placement is `pick_output`/`placement` in
[`core/geometry.rs`](../src/core/geometry.rs).

spokenpad needs Linux with evdev, a microphone PortAudio can open, and an
x86-64 or aarch64 CPU. Any desktop works, X11 or Wayland; only managed mode
needs i3 or sway. Figures below come from one example machine and are
labelled as such.

## Hotkey: an evdev key code

`hotkey.key_code` is the Linux evdev code of the push-to-talk key, not the
X11 keycode (which is the evdev code + 8, the standard XKB offset) and not a
keysym. The default, `186`, is `KEY_F16`: no application binds it, so holding
it collides with nothing. Most keyboards lack the key, but keyboard firmware
(QMK, VIA) or a remapper such as keyd can send F13–F24 from any key. The
chain for the default:

```
evdev 186 (KEY_F16)  →  X11 keycode 194  →  keysym XF86Launch7
```

To use another key, find its code with `evtest` (pick the keyboard, press
the key, read `code NNN`) or `libinput debug-events --show-keycodes`, and set
`hotkey.key_code`. On X11, `xev` prints the X11 keycode; subtract 8. The
daemon exits with code 3 when no readable input device advertises that code;
reading `/dev/input` needs membership in the `input` group.

**Why evdev and not a keysym.** A keysym-based hotkey library resolves the
binding through the X keymap, and X11 keycodes and their layout mapping are
per-device and fragile. `shell/hotkey.rs` binds the raw evdev code upstream
of any X11 layout translation, and opens the device read-only
([constraints.md](constraints.md#read-evdev-read-only)).

`hotkey.cancel_key_code` defaults to evdev `1` (`KEY_ESC`); an omitted value
keeps that default. The current TOML surface cannot disable cancelling
([`config.rs`](../src/config.rs)).

### Per-device keyboard layouts

A keyboard can carry its own XKB layout, set with `setxkbmap -device`, for
example an external keyboard with a different layout from the laptop's. A
uinput clone of the keyboard would lose that layout, which is one
reason spokenpad never grabs or clones a device
([constraints.md](constraints.md#read-evdev-read-only)).

## Displays

Only managed mode places a window. On i3 it opens on whichever output holds
the mouse pointer; sway gives a client no way to read the pointer, so there
it opens on the focused output ([nvim-window.md](nvim-window.md#placement)).
[`core/geometry.rs`](../src/core/geometry.rs) picks the output as a pure
function over the output rectangles the window manager reports over IPC, so
any number and arrangement of outputs works.

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
