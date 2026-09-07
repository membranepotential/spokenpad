"""The sink: a floating neovim the transcript is appended to.

Nothing is pasted anywhere. The transcript reaches nvim over its msgpack-RPC
socket -- no clipboard, no synthetic keystrokes, and no transcript text ever
crossing a shell or an argv boundary, which is a stronger version of the rule
in ``docs/constraints.md`` rather than a departure from it.

The window
----------
voice-kb spawns its own terminal running ``nvim --listen <socket> <file>``
with a distinct X11 instance name (:attr:`NvimConfig.window_instance`), so the
window manager can be told once to float it and never focus it -- see
``docs/nvim-window.md``. This module does not manage focus itself beyond
asking i3 to bring an existing window to the current workspace; that is the
WM's job, and reaching for ``xdotool windowfocus`` here would recreate the
focus-steal this project already forbids for the overlay.

The file
--------
The buffer is a real timestamped file under
:attr:`NvimConfig.dictation_dir`, saved after every committed utterance.
A dictation that vanishes because a scratch buffer was closed is the same
class of failure as one dropped by a streaming decoder, and this project
exists to eliminate it.

One file **per window**, not per day. Closing the window ends the passage, and
the next dictation opens a new window on a new file rather than appending
under everything said an hour ago. While the window stays open every utterance
goes into it, including across a daemon restart -- reattaching adopts the
buffer that is on screen instead of opening a second file underneath it.

Reconnection
------------
A session is reattached rather than respawned whenever the socket still
answers, so closing and reopening the daemon does not litter the desktop with
terminals, and quitting nvim simply means the next dictation opens a fresh
one. Every entry point reports failure as a value or a log line; none of them
raise into the Qt thread.
"""

from __future__ import annotations

import contextlib
import logging
import socket
import subprocess
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from importlib import resources
from pathlib import Path
from typing import Any, Final

import msgpack
import pynvim

from voice_kb import x11
from voice_kb.config import NvimConfig
from voice_kb.geometry import Output, Rect, dictation_rect, pick_output
from voice_kb.state import Phase

log = logging.getLogger("voice-kb.nvim")

_CONNECT_POLL_S: Final = 0.1

#: How long to wait for i3 to report the new window, and how often to ask.
#: Measured: alacritty appears in i3's tree ~0.18s after the spawn.
_MAP_TIMEOUT_S: Final = 3.0
_MAP_POLL_S: Final = 0.02

#: How long a cached monitor layout is trusted. Long enough that a burst of
#: dictations costs one xrandr, short enough that plugging in a screen is
#: noticed without restarting the daemon.
_OUTPUTS_TTL_S: Final = 5.0
_I3_TIMEOUT_S: Final = 2.0
_SOCKET_PROBE_S: Final = 0.25

#: How long a window that is *already* open gets to answer before it is
#: written off as wedged. Reattachment is on the key-down path, so this is
#: latency the user feels; a healthy editor answers in about a millisecond.
_REATTACH_TIMEOUT_S: Final = 2.0

#: msgpack-RPC message types and the id used for the readiness probe. Only
#: these two appear here; the real session speaks through pynvim.
_RPC_REQUEST: Final = 0
_RPC_RESPONSE: Final = 1
_PROBE_MSGID: Final = 1


def _is_probe_reply(message: object) -> bool:
    """Whether ``message`` is the response to the readiness probe.

    Notifications arrive on the same stream (nvim announces things whether or
    not anyone asked), so the type and the message id both have to match --
    treating any traffic as the answer would call an editor ready the moment
    it said anything at all.
    """
    return (
        isinstance(message, (list, tuple))
        and len(message) == 4
        and message[0] == _RPC_RESPONSE
        and message[1] == _PROBE_MSGID
    )

_OPEN_BUFFER_LUA: Final = """
vim.cmd.edit(vim.fn.fnameescape(...))
return vim.api.nvim_get_current_buf()
"""

_ADOPT_BUFFER_LUA: Final = """
-- Find a dictation buffer this nvim already holds, so reattaching to a window
-- the user still has open keeps writing where they can see it, instead of
-- opening a second file underneath them. Matched on the dictation directory
-- rather than an exact name: the running window may have been opened by an
-- earlier daemon, with an earlier timestamp in its file name.
local dir = ...
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then
    local name = vim.api.nvim_buf_get_name(buf)
    if name:sub(1, #dir) == dir then
      vim.cmd.edit(vim.fn.fnameescape(name))
      return { vim.api.nvim_get_current_buf(), name }
    end
  end
end
return nil
"""

_PHASE_NAMES: Final[dict[Phase, str]] = {
    Phase.IDLE: "idle",
    Phase.RECORDING: "recording",
    Phase.TRANSCRIBING: "transcribing",
}


@dataclass(frozen=True, slots=True)
class Appended:
    """The utterance is in the buffer and the file has been written."""

    line: int
    """The buffer's line count afterwards -- evidence the text landed."""

    elapsed_ms: float


@dataclass(frozen=True, slots=True)
class AppendFailed:
    """The utterance did not land. ``reason`` is for logs, not for parsing."""

    reason: str


type AppendResult = Appended | AppendFailed


def _indicator_source() -> str:
    """The Lua half of this module, shipped alongside it in the package."""
    return resources.files("voice_kb").joinpath("nvim_indicator.lua").read_text(encoding="utf-8")


class NvimSession:
    """One dictation nvim: the process, the socket, and the buffer in it.

    Not thread-safe, deliberately: it is owned by a single bridge thread in
    :mod:`voice_kb.app`, the same way the recogniser is owned by the worker.
    """

    def __init__(self, config: NvimConfig, *, clock: Callable[[], datetime] | None = None) -> None:
        self._config = config
        self._clock = clock or datetime.now
        self._nvim: Any | None = None
        self._buffer: int | None = None
        self._path: Path | None = None
        self._process: subprocess.Popen[bytes] | None = None
        self._outputs: list[Output] | None = None
        self._outputs_read_at = 0.0

    # ------------------------------------------------------------------ queries

    @property
    def connected(self) -> bool:
        return self._nvim is not None

    @property
    def path(self) -> Path | None:
        """The file this session is dictating into, once there is one.

        ``None`` until a window has been opened. A *file per window*, not per
        day: a closed window means the passage is finished, and the next one
        starts on a clean page rather than under everything said earlier. The
        old file stays on disk -- a fresh start is not the same as discarding
        the previous transcript.
        """
        return self._path

    def _new_path(self) -> Path:
        return self._config.dictation_dir / self._clock().strftime(self._config.file_template)

    # ---------------------------------------------------------------- lifecycle

    def ensure(self) -> bool:
        """Make sure a dictation nvim is up and bound to a dictation buffer.

        Reattaches to a live socket if there is one, spawns a terminal if not.
        Reattaching *adopts* the buffer the window already shows; spawning
        starts a new file. That is the whole rule: an open window continues
        the passage, a closed one ends it.
        Returns whether the session is usable; every failure is logged here
        rather than raised, because a dictation that cannot reach nvim must
        still leave the daemon running (and the text in the log) instead of
        taking the process down.
        """
        if self._nvim is not None and self._socket_alive():
            return True
        self._drop("reconnecting")
        try:
            if self._attach_existing() or self._spawn_and_attach():
                return True
        except Exception as e:  # pynvim raises a wide range; none may escape
            log.error("could not open the dictation window: %s", e)
            self._drop("connection failed")
            return False
        return False

    def close(self) -> None:
        """Detach. The nvim itself is left running -- the user may still be
        editing what they dictated, and killing their editor because a daemon
        exited would lose exactly the text this window exists to keep."""
        self._drop("shutting down")

    # ------------------------------------------------------------------ writing

    def append(self, text: str, *, continued: bool = False) -> AppendResult:
        """Append committed text and save, as a new paragraph or extending one.

        One utterance arrives as several calls, one per speech segment, so
        that text lands while the rest is still decoding. ``continued=False``
        starts the paragraph and every later segment of the same utterance
        passes ``True``, which keeps "one utterance is one paragraph" true
        while letting it grow a piece at a time.

        A *request*, not a notification: this is the one call whose failure
        the user must hear about, so it waits for nvim to confirm the new
        line count.
        """
        if self._nvim is None or self._buffer is None:
            return AppendFailed(reason="not connected to nvim")
        start = time.monotonic()
        try:
            line = int(self._nvim.exec_lua("return VoiceKb.append(...)", text, continued))
        except Exception as e:
            self._drop("append failed")
            return AppendFailed(reason=str(e))
        return Appended(line=line, elapsed_ms=(time.monotonic() - start) * 1000)

    def set_state(
        self,
        *,
        phase: Phase | None = None,
        level: float | None = None,
        preview: str | None = None,
        latched: bool | None = None,
        previewing: bool | None = None,
    ) -> None:
        """Update the indicator. Fire-and-forget, and never raises.

        Sent as a notification: this runs many times a second while the key is
        held, and a round trip per level sample would put nvim's event loop on
        the dictation latency path for something purely cosmetic. Only the
        fields given are changed.
        """
        if self._nvim is None:
            return
        update: dict[str, Any] = {}
        if phase is not None:
            update["phase"] = _PHASE_NAMES[phase]
        if level is not None:
            update["level"] = max(0.0, min(1.0, level))
        if preview is not None:
            update["preview"] = preview
        if latched is not None:
            update["latched"] = latched
        if previewing is not None:
            update["previewing"] = previewing
        if not update:
            return
        try:
            self._nvim.exec_lua("VoiceKb.set_state(...)", update, async_=True)
        except Exception as e:
            log.debug("indicator update dropped: %s", e)
            self._drop("indicator update failed")

    def raise_window(self) -> None:
        """Bring an existing dictation window to the workspace the user is on.

        Only the workspace move, deliberately: it is *not* re-placed on every
        dictation. A window that jumped back under the pointer every time the
        key was pressed would fight anyone who had moved it somewhere they
        wanted it, and the placement in :meth:`place_window` is a starting
        position, not a policy to keep enforcing.
        """
        self._i3("move workspace current")

    def place_window(self, rect: Rect) -> None:
        """Float, size and position a freshly opened window. Once, on spawn.

        ``floating enable`` is repeated here even though the window manager
        rule already says it. i3 evaluates ``for_window`` when it takes the
        window over, so a terminal that sets ``WM_CLASS`` late enough loses
        the match and tiles instead -- pushing the user's work aside, which is
        the one thing this window must never do. Saying it again once the
        window certainly exists is idempotent and costs nothing, and it also
        means placement is correct on a fresh machine before anyone has copied
        the i3 rules in.

        ``no_focus`` has no such fallback: it is genuinely only expressible as
        a window manager rule, and there is no focus command in this project
        to emulate it with.
        """
        self._i3(
            f"floating enable, resize set {rect.width} {rect.height}, "
            f"move position {rect.x} {rect.y}"
        )

    def _i3(self, command: str) -> None:
        """Run one i3 command against the dictation window.

        Movement only -- there is no focus command anywhere in this module. i3
        leaves focus with whatever the user was reading, which is the whole
        point of the window. Silently does nothing when i3 is not the WM.
        """
        try:
            done = subprocess.run(
                ["i3-msg", f'[instance="{self._config.window_instance}"] {command}'],
                capture_output=True,
                timeout=_I3_TIMEOUT_S,
                check=False,
            )
        except (OSError, subprocess.SubprocessError) as e:
            log.debug("could not reach i3 (%s); leaving the window where it is", e)
            return
        if done.returncode != 0:
            log.debug("i3 refused %r: %s", command, done.stderr[:200])

    # ------------------------------------------------------------------ internals

    def _socket_alive(self) -> bool:
        """Whether something is still accepting connections on the socket.

        A plain ``connect``, not an RPC round trip. This runs on every
        key-down, and a pynvim request has no timeout -- an editor busy with
        its own startup would block the bridge thread on what is meant to be a
        liveness check. Connecting cannot block longer than the timeout set
        here, and a nvim that has quit either removed its socket or refuses
        the connection, which is the case this exists to catch.
        """
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.settimeout(_SOCKET_PROBE_S)
                probe.connect(str(self._config.socket_path))
        except OSError:
            return False
        return True

    def _answers_rpc(self, deadline: float, process: subprocess.Popen[bytes] | None) -> bool:
        """Wait until nvim is far enough through startup to serve API requests.

        A raw msgpack-RPC round trip on the socket, because that is the one
        way to bound the wait: pynvim's requests have no timeout, and a plain
        ``connect`` says nothing -- libuv is listening well before the main
        loop can answer anything. Attaching inside that window blocks the
        bridge thread with no way out, which would in turn hang the daemon's
        shutdown.

        One connection and one request, then a poll for the reply. nvim's
        event loop picks the request up when it reaches the loop, so the reply
        arrives on its own the moment the editor is genuinely ready; the poll
        exists only so a terminal that dies during startup is noticed instead
        of waited out. This replaced spawning ``nvim --server ... --remote-expr``
        every 100ms, which cost a whole editor process per probe and put most
        of the ~1s cold open on the user rather than on nvim.
        """
        request = msgpack.packb([_RPC_REQUEST, _PROBE_MSGID, "nvim_eval", ["1"]])
        unpacker = msgpack.Unpacker(raw=False, strict_map_key=False)
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                probe.settimeout(_SOCKET_PROBE_S)
                probe.connect(str(self._config.socket_path))
                probe.sendall(request)
                while time.monotonic() < deadline:
                    if process is not None and process.poll() is not None:
                        return False
                    try:
                        chunk = probe.recv(4096)
                    except TimeoutError:
                        continue
                    if not chunk:
                        return False
                    unpacker.feed(chunk)
                    for message in unpacker:
                        if _is_probe_reply(message):
                            return True
        except OSError as e:
            log.debug("readiness probe on %s failed: %s", self._config.socket_path, e)
            return False
        return False

    def _attach_existing(self) -> bool:
        """Reuse the nvim already listening on the socket, if any."""
        if not self._config.socket_path.exists():
            return False
        if not self._socket_alive():
            log.info("stale nvim socket at %s; starting a new one", self._config.socket_path)
            self._config.socket_path.unlink(missing_ok=True)
            return False
        # A short deadline, not the startup one: this window is already open,
        # so it is either answering now or it is wedged. Waiting out a full
        # startup timeout here would stall the key-down that opened it.
        if not self._answers_rpc(time.monotonic() + _REATTACH_TIMEOUT_S, process=None):
            # Connectable but not serving: an editor still starting up, or one
            # wedged. Either way, attaching would block indefinitely.
            log.warning(
                "nvim on %s is not answering; leaving it alone this time",
                self._config.socket_path,
            )
            return False
        try:
            nvim = pynvim.attach("socket", path=str(self._config.socket_path))
        except OSError as e:
            # It answered the probe a moment ago, so this is a nvim that quit
            # in between. Drop the socket so the spawn path is not tripped by
            # it on the next dictation.
            log.info(
                "nvim on %s went away mid-connect (%s); starting a new one",
                self._config.socket_path,
                e,
            )
            self._config.socket_path.unlink(missing_ok=True)
            return False
        if not self._adopt(nvim):
            # A live nvim with no dictation buffer in it -- the user closed the
            # file but kept the editor. Give them a fresh page rather than
            # resurrecting the one they closed.
            self._bind(nvim, self._new_path())
        log.info("reattached to the dictation nvim on %s", self._config.socket_path)
        return True

    def _spawn_and_attach(self) -> bool:
        # A new window is a new page. The timestamp is taken here, once, so
        # every append for the life of this window goes to the same file.
        target = self._new_path()
        target.parent.mkdir(parents=True, exist_ok=True)
        self._config.socket_path.parent.mkdir(parents=True, exist_ok=True)
        self._config.socket_path.unlink(missing_ok=True)

        # Before the spawn, not after: the terminal is told where to open, so
        # its first frame is already in the right place. Placing it afterwards
        # meant the window appeared wherever the window manager chose -- often
        # the middle of the other monitor -- and then flew across the screen.
        placement = self._placement()
        argv = self._config.spawn_argv(
            socket=self._config.socket_path,
            target=target,
            at=(placement.x, placement.y) if placement is not None else None,
        )
        log.info("opening the dictation window: %s", " ".join(argv))
        try:
            process = subprocess.Popen(
                argv,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                # Its own session, so Ctrl-C in the daemon's terminal does not
                # take the user's editor -- and their unsaved text -- with it.
                start_new_session=True,
            )
        except OSError as e:
            log.error("could not start %r: %s", argv[0], e)
            return False
        self._process = process
        # Nothing ever waits on this process, so a daemon thread reaps it.
        threading.Thread(target=process.wait, daemon=True).start()

        # The size, as soon as i3 knows the window -- which happens while nvim
        # is still starting, so the correction lands before there is anything
        # drawn to see it. Only the size is really outstanding by now; the
        # position is asserted again in the same command because it costs
        # nothing and covers a terminal that ignored the hint.
        if placement is not None and self._config.announces_instance:
            self._place_when_mapped(placement, process)

        nvim = self._wait_for_socket(process)
        if nvim is None:
            return False
        self._bind(nvim, target)
        return True

    def _place_when_mapped(self, rect: Rect, process: subprocess.Popen[bytes]) -> None:
        """Wait for i3 to take the window over, then set its geometry.

        i3 has the window a good while before nvim answers RPC, and this is
        the last visible change to the window, so doing it at the earlier of
        the two moments is the difference between correcting an empty terminal
        and correcting one the user is already reading.
        """
        deadline = time.monotonic() + _MAP_TIMEOUT_S
        while time.monotonic() < deadline:
            if process.poll() is not None:
                return
            if x11.i3_window_exists(self._config.window_instance):
                self.place_window(rect)
                return
            time.sleep(_MAP_POLL_S)
        log.debug(
            "i3 did not report a %r window within %.1fs; leaving placement to it",
            self._config.window_instance,
            _MAP_TIMEOUT_S,
        )

    def warm_up(self) -> None:
        """Pay the one-off cost of talking to X, at startup rather than mid-key.

        The first subprocess this process runs costs ~1.9s where every later
        one costs ~50ms, and ``_placement`` is usually the first. Left alone
        that lands on the user's very first dictation of a session -- the one
        occasion there is nothing else on screen to hide it behind. Called
        once when the bridge thread starts; failure is uninteresting, because
        everything here already degrades to "let the window manager place it".
        """
        started = time.monotonic()
        self._placement()
        log.debug("warmed up X queries in %.0fms", (time.monotonic() - started) * 1000)

    def _placement(self) -> Rect | None:
        """Where a newly opened window should sit: under the pointer, on the
        monitor the pointer is on. ``None`` when X cannot be queried at all,
        in which case the window manager's own placement stands."""
        screens = self._cached_outputs()
        if not screens:
            return None
        pointer = x11.pointer_position()
        if pointer is None:
            target = next((s for s in screens if s.primary), screens[0])
        else:
            # A 1x1 rect at the pointer reuses the same "largest intersection,
            # else primary" rule the overlay uses to pick an output.
            target = pick_output(screens, Rect(x=pointer[0], y=pointer[1], width=1, height=1))
        return dictation_rect(target.rect, pointer, self._config.window_fraction)

    def _cached_outputs(self) -> list[Output]:
        """The monitor layout, re-read at most every few seconds.

        ``xrandr`` is a subprocess and the layout almost never changes, so
        asking again on every key-down is pure latency on the path that opens
        the window. Same cache, same reasoning, as ``Daemon._cached_outputs``
        -- kept separately because this runs on the bridge thread and that one
        runs on the Qt thread, and a shared cache would need a lock to save an
        xrandr call neither of them makes often.
        """
        now = time.monotonic()
        if self._outputs is None or now - self._outputs_read_at > _OUTPUTS_TTL_S:
            self._outputs = x11.outputs()
            self._outputs_read_at = now
        return self._outputs

    def _wait_for_socket(self, process: subprocess.Popen[bytes]) -> Any | None:
        """Wait until nvim is *answering*, it dies, or the timeout expires.

        Answering, not merely listening: the socket file appears, and accepts
        connections, well before nvim has finished loading a configuration and
        can serve API requests. Attaching in that window is what turns a slow
        editor startup into a blocked bridge thread, so the readiness probe --
        which is the one call here with a hard timeout -- gates the attach.

        Only the socket *file* is polled for. Once it exists the probe holds a
        single connection open and waits for its reply, so readiness costs one
        round trip rather than a fresh probe every tick.
        """
        deadline = time.monotonic() + self._config.startup_timeout_s
        while time.monotonic() < deadline:
            if process.poll() is not None:
                log.error(
                    "the dictation terminal exited immediately (status %d); "
                    "check that %r runs on its own",
                    process.returncode,
                    self._config.terminal[0] if self._config.terminal else self._config.editor[0],
                )
                return None
            if self._config.socket_path.exists():
                if not self._answers_rpc(deadline, process):
                    break
                try:
                    return pynvim.attach("socket", path=str(self._config.socket_path))
                except OSError as e:
                    log.error("nvim answered but would not accept a connection: %s", e)
                    return None
            time.sleep(_CONNECT_POLL_S)
        log.error(
            "nvim did not start answering on %s within %.1fs -- raise "
            "nvim.startup_timeout_s, or use a lighter nvim.editor",
            self._config.socket_path,
            self._config.startup_timeout_s,
        )
        return None

    def _bind(self, nvim: Any, path: Path) -> None:
        """Load the Lua half, open ``path``, and point the indicator at it.

        The Lua source is re-sent on every connection, including reattachment,
        so a window opened by an earlier daemon picks up the current indicator
        rather than whatever version it was started with.
        """
        nvim.exec_lua(_indicator_source())
        buffer = int(nvim.exec_lua(_OPEN_BUFFER_LUA, str(path)))
        nvim.exec_lua("VoiceKb.setup(...)", buffer)
        self._nvim = nvim
        self._buffer = buffer
        self._path = path
        log.info("dictating into %s (buffer %d)", path, buffer)

    def _adopt(self, nvim: Any) -> bool:
        """Bind to the dictation buffer a running window already shows.

        Reattachment must not start a new page: the window being open is
        exactly the case where the user is still working on the passage in it.
        Only a *closed* window means the previous transcript is finished.
        """
        nvim.exec_lua(_indicator_source())
        found = nvim.exec_lua(_ADOPT_BUFFER_LUA, str(self._config.dictation_dir))
        if not found:
            return False
        buffer, name = int(found[0]), str(found[1])
        nvim.exec_lua("VoiceKb.setup(...)", buffer)
        self._nvim = nvim
        self._buffer = buffer
        self._path = Path(name)
        log.info("adopted the open dictation buffer %s (buffer %d)", name, buffer)
        return True

    def _drop(self, why: str) -> None:
        """Forget the connection so the next :meth:`ensure` rebuilds it."""
        if self._nvim is not None:
            log.debug("dropping the nvim connection: %s", why)
            # Already gone is the normal case here -- there is nothing to
            # salvage from a connection we are discarding either way.
            with contextlib.suppress(Exception):
                self._nvim.close()
        self._nvim = None
        self._buffer = None
        # The path goes too. It is re-decided on the next connection: adopted
        # from a window that is still open, or freshly timestamped for a new
        # one. Keeping it here would let a closed window's file be reopened.
        self._path = None
