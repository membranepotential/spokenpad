"""Hotkey watcher: push-to-talk key detection straight off evdev.

The Keychron Q10 Pro's M4 key emits evdev ``KEY_F16`` (186), which X11 maps
to the keysym ``XF86Launch7`` -- unreachable from keysym-based hotkey
libraries (they hook X, not the kernel). Reading ``/dev/input/event*``
directly is the only way to see it.

This is also the single most safety-critical module in the project.
:class:`evdev.InputDevice` is constructed with ``readonly=True`` everywhere
below, which is not the default: ``InputDevice.__init__`` tries
``O_RDWR`` before falling back to ``O_RDONLY``, and a device node owned by
the ``input`` group is normally ``rw-rw----``, so the read-write attempt
would silently succeed. We never call :meth:`evdev.InputDevice.grab`
(``EVIOCGRAB``) and never construct a ``uinput`` device. The tool this
project replaces grabbed every keyboard and re-injected through uinput
clones; those clones inherit the *default* XKB layout and silently
overwrite the user's per-device ``setxkbmap -device N -layout us -variant
de_se_fi``. Grabbing or writing to any evdev node is off the table -- full
stop -- regardless of how convenient it would be for, say, suppressing the
key from reaching the focused application.
"""

from __future__ import annotations

import contextlib
import logging
import os
import selectors
import threading
import time
from collections.abc import Callable
from typing import Self

import evdev
import pyudev
from evdev import ecodes

from voice_kb.config import HotkeyConfig

logger = logging.getLogger(__name__)

type KeyEventCallback = Callable[[float], None]
"""Invoked on the watcher's background thread with ``time.monotonic()`` at
the moment the event was read, ready to feed straight into
:func:`voice_kb.state.step` as the ``at`` field of a ``KeyDown``/``KeyUp``/
``Cancelled`` event."""

_INPUT_EVENT_PREFIX = "/dev/input/event"

_STOP = object()
_UDEV = object()


class HotkeyPermissionError(PermissionError):
    """No ``/dev/input/event*`` device could be opened for reading.

    Almost always means the current user is not in the ``input`` group.
    """


class HotkeyWatcher:
    """Watches evdev keyboards for :class:`.HotkeyConfig`'s key codes.

    Devices are opened read-only (see the module docstring) and polled
    concurrently with :mod:`selectors` on a single background thread, so one
    slow/removed device cannot stall the others. A :mod:`pyudev` monitor on
    the ``input`` subsystem re-arms watching as devices are added or
    removed, so a keyboard reconnect (e.g. after the user's udev/systemd
    unit runs) is picked up without restarting the daemon.
    """

    def __init__(
        self,
        config: HotkeyConfig,
        *,
        on_key_down: KeyEventCallback,
        on_key_up: KeyEventCallback,
        on_cancel: KeyEventCallback | None = None,
    ) -> None:
        self._config = config
        self._on_key_down = on_key_down
        self._on_key_up = on_key_up
        self._on_cancel = on_cancel

        self._devices: dict[str, evdev.InputDevice[str]] = {}
        self._selector: selectors.BaseSelector | None = None
        self._udev_monitor: pyudev.Monitor | None = None
        self._stop_r: int | None = None
        self._stop_w: int | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        """Open matching devices and start watching them in the background.

        Raises :class:`HotkeyPermissionError` if no ``/dev/input/event*``
        device could be opened at all. Devices this process cannot read
        (e.g. another user's session) are skipped silently, since that is
        the common case on a multi-seat machine.
        """
        if self._thread is not None:
            return

        self._selector = selectors.DefaultSelector()
        self._udev_monitor = pyudev.Monitor.from_netlink(pyudev.Context())
        self._udev_monitor.filter_by("input")
        self._udev_monitor.start()
        self._selector.register(self._udev_monitor, selectors.EVENT_READ, _UDEV)

        self._stop_r, self._stop_w = os.pipe()
        os.set_blocking(self._stop_r, False)
        self._selector.register(self._stop_r, selectors.EVENT_READ, _STOP)

        try:
            self._scan_devices()
        except Exception:
            self._teardown()
            raise

        self._thread = threading.Thread(target=self._run, name="voice-kb-hotkey", daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop watching and release every open device."""
        if self._thread is None:
            return
        assert self._stop_w is not None
        os.write(self._stop_w, b"x")
        self._thread.join(timeout=2.0)
        self._thread = None
        self._teardown()

    def _teardown(self) -> None:
        for fd in (self._stop_r, self._stop_w):
            if fd is not None:
                os.close(fd)
        self._stop_r = self._stop_w = None

        for device in self._devices.values():
            device.close()
        self._devices.clear()

        if self._selector is not None:
            self._selector.close()
            self._selector = None
        self._udev_monitor = None

    def __enter__(self) -> Self:
        self.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.stop()

    # ---------------------------------------------------------------- setup

    def _scan_devices(self) -> None:
        paths = evdev.list_devices()
        if not paths:
            raise HotkeyPermissionError("no /dev/input/event* devices exist on this system")

        # Not `any(...)`: every path must be tried regardless of earlier
        # results, so a plain list comprehension rather than a short-
        # circuiting generator expression.
        saw_permission_error = any([self._try_add_device(path) for path in paths])

        if self._devices:
            return
        if saw_permission_error:
            raise HotkeyPermissionError(
                "no /dev/input/event* device could be opened for reading; "
                "add yourself to the 'input' group (sudo usermod -aG input "
                "$USER) and log out and back in"
            )
        raise HotkeyPermissionError(
            "no keyboard advertising the configured hotkey "
            f"(evdev code {self._config.key_code}) was found under /dev/input"
        )

    def _try_add_device(self, path: str) -> bool:
        """Open and register ``path`` if it matches. Returns whether the
        attempt failed on a permission error, so :meth:`_scan_devices` can
        distinguish "wrong device" from "right device, no access" in its
        error message when nothing ends up watched."""
        if path in self._devices:
            return False
        try:
            device = evdev.InputDevice(path, readonly=True)
        except PermissionError:
            logger.warning("no permission to read %s (not in the 'input' group?)", path)
            return True
        except OSError as e:
            logger.debug("could not open %s: %s", path, e)
            return False

        if not self._wants(device):
            device.close()
            return False

        self._devices[path] = device
        if self._selector is not None:
            self._selector.register(device, selectors.EVENT_READ, path)
        logger.info("watching %s (%s) for the hotkey", path, device.name)
        return False

    def _wants(self, device: evdev.InputDevice[str]) -> bool:
        key_codes = set(device.capabilities().get(ecodes.EV_KEY, []))
        if self._config.key_code in key_codes:
            return True
        return self._config.cancel_key_code is not None and (
            self._config.cancel_key_code in key_codes
        )

    def _drop_device(self, path: str) -> None:
        device = self._devices.pop(path, None)
        if device is None:
            return
        if self._selector is not None:
            with contextlib.suppress(KeyError):
                self._selector.unregister(device)
        device.close()
        logger.info("stopped watching %s (device removed)", path)

    # ------------------------------------------------------------------ run

    def _run(self) -> None:
        assert self._selector is not None
        while True:
            for key, _mask in self._selector.select(timeout=1.0):
                tag = key.data
                if tag is _STOP:
                    return
                if tag is _UDEV:
                    self._drain_udev()
                else:
                    self._read_device(str(tag))

    def _drain_udev(self) -> None:
        assert self._udev_monitor is not None
        while (device := self._udev_monitor.poll(timeout=0)) is not None:
            node = device.device_node
            if node is None or not node.startswith(_INPUT_EVENT_PREFIX):
                continue
            if device.action == "remove":
                self._drop_device(node)
            elif device.action == "add":
                self._try_add_device(node)

    def _read_device(self, path: str) -> None:
        device = self._devices.get(path)
        if device is None:
            return
        try:
            events = list(device.read())
        except BlockingIOError:
            return
        except OSError as e:
            logger.info("lost %s: %s", path, e)
            self._drop_device(path)
            return

        for event in events:
            if event.type != ecodes.EV_KEY:
                continue
            self._handle_key(event.code, event.value)

    def _handle_key(self, code: int, value: int) -> None:
        # value: 0 = up, 1 = down, 2 = auto-repeat. Auto-repeat is passed
        # through as another down event rather than dropped: `state.step`
        # already treats a down event while Recording as a no-op, so this
        # costs nothing and means a missed initial down (e.g. a device that
        # only just got re-armed after a hot-plug) is self-healing as soon
        # as the kernel starts repeating.
        now = time.monotonic()
        if code == self._config.key_code:
            if value in (1, 2):
                self._on_key_down(now)
            elif value == 0:
                self._on_key_up(now)
        elif (
            self._config.cancel_key_code is not None
            and code == self._config.cancel_key_code
            and value == 1
            and self._on_cancel is not None
        ):
            self._on_cancel(now)
