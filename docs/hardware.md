# Hardware

← [docs index](README.md) | The keycode chain below is why `hotkey.py`
(see [architecture.md](architecture.md)) must read evdev directly instead of
using a keysym-based hotkey library — reinforced by
[constraints.md](constraints.md#read-evdev-read-only). The dictation
window's per-output placement is `dictation_rect` in
[`geometry.py`](../src/voice_kb/geometry.py).

This is not a general compatibility target. voice-kb is built and tuned
against one specific machine and input device; other hardware may need
different keycodes and output geometry.

## Keyboard: Keychron Q10 Pro, M4 key

The M4 programmable key produces:

```
evdev 186 (KEY_F16)  →  X11 keycode 194  →  keysym XF86Launch7
```

`186` is `KEY_F16` in the Linux input-event-codes namespace, which is why
`HotkeyConfig.key_code` in [`config.py`](../src/voice_kb/config.py) defaults
to `186` and is documented there as "the evdev code, not the X11 keycode
(which is this + 8)". The evdev-to-X11 offset of `+8` is the standard
XKB convention (X11 keycodes start at 8), not something specific to this
key.

**This is why hotkey.py reads evdev directly rather than binding a keysym.**
A keysym-based hotkey library (anything built on X11 key-grab APIs) resolves
bindings through `XF86Launch7`, and would work for this key in principle —
but doing so requires going through the X keymap, which is exactly the layer
[constraints.md](constraints.md#read-evdev-read-only) rules out touching for
the M4 key: X11 keycodes and their layout mapping are per-device and
fragile in the way that motivated reading evdev in the first place.
`hotkey.py` binds on the raw evdev code (`186`) instead, upstream of any X11
layout translation.

`HotkeyConfig.cancel_key_code` defaults to evdev `1` (`KEY_ESC`); `None`
disables cancelling ([`config.py`](../src/voice_kb/config.py)).

### Per-device layout

The Keychron is configured with a per-device XKB layout (`us` /
`de_se_fi`) applied by `~/.local/bin/keychron-add.sh`, external to this
repo. This is the exact state that a uinput keyboard clone would destroy —
see [constraints.md](constraints.md#read-evdev-read-only) for why that rules
out grabbing the device.

## Displays: dual 4K, X11

```
HDMI-1-0            0,0      3840x2160
eDP-1 (primary)  3840,0      3840x2160
```

Two 4K outputs side by side, laptop panel (`eDP-1`) primary and positioned to
the right of the external `HDMI-1-0` output in the X11 coordinate space.
The dictation window opens on whichever output holds the mouse pointer
([nvim-window.md](nvim-window.md)), which on this layout means computing
which of the two `3840x2160` regions the pointer's coordinates fall into —
[`geometry.py`](../src/voice_kb/geometry.py) owns that calculation as a pure
function over output rectangles (see
[architecture.md](architecture.md)).

## CPU / GPU

- CPU: Intel i7-9850H, 6 cores — `AsrConfig.num_threads = 6` in
  [`config.py`](../src/voice_kb/config.py) matches this exactly, one thread
  per physical core.
- GPU: GTX 1650, 4 GB VRAM, deliberately unused — see
  [constraints.md](constraints.md#cpu-only).

## Audio

PipeWire. `AudioConfig.device = None`
([`config.py`](../src/voice_kb/config.py)) means the PipeWire/PulseAudio
default source; no device is hardcoded.

## Window manager

i3 on X11 (not Wayland). The dictation window is placed and refused focus
through i3 (`for_window` rules, see [nvim-window.md](nvim-window.md)), and
the hotkey is read from evdev because keysym-based libraries cannot see M4.
