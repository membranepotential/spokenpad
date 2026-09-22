//! A dedicated Neovim sink, spoken to directly over msgpack-RPC.
//!
//! The session owns exactly one thing: a connection to an editor holding a
//! pinned dictation buffer. That pairing is the invariant — a client without a
//! buffer, or a buffer without the file it was opened on, is not a state this
//! module can be in, which is why they live together in `Connected`.
//!
//! Everything the editor does on its side is in `lua/spokenpad.lua`; the
//! transport is in `rpc`. What is left here is the impure middle: spawning
//! or adopting an editor, proving it belongs to spokenpad, and appending
//! committed text so that a failure to land it is visible rather than silent.
//! With no editor to append to, `passage` writes the text to the file itself.
//!
//! Two modes differ only in who starts that editor: the user, with
//! `spokenpad editor` (`attach`), or the daemon, in a window it draws itself
//! (`pane`, in [`shell::pane`](crate::shell::pane)). Everything after it
//! answers on its socket is the same code.
mod passage;
pub(crate) mod rpc;

pub use passage::DetachedWrite;

use crate::{
    config::{self, Mode, Nvim, PaneLayout},
    core::{session::NoticeText, state::IndicatorPhase, wm::Criterion},
    shell::{
        pane::{host::PaneHost, place, x11},
        wm::{self, Wm},
    },
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rmpv::Value;
use rpc::{Patience, RpcClient, RpcFailure, Waiting};
use std::{
    convert::Infallible,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Liveness and ownership probes: one cheap round trip on an editor that is
/// otherwise idle. This is on the dictation path — the window is checked on
/// every key-down — so a wedged editor has to be given up on quickly and
/// reattached to, rather than delaying the recording that is already running.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Committed text. Deliberately as short as the probe: an append that has not
/// come back in two seconds is ambiguous rather than lost, and the operation
/// id makes the reconnect-and-repeat that follows idempotent. A longer
/// deadline would only make the editor thread sit on a queue of undelivered
/// utterances for longer before doing anything about it.
const APPEND_TIMEOUT: Duration = Duration::from_secs(2);
/// The same deadline once the daemon is stopping. Everything the editor
/// thread still does has to fit in `daemon::SHUTDOWN_GRACE`, the pane's
/// teardown included, and a healthy editor appends in some tens of
/// milliseconds; one that is slower is given up on, and the text goes to the
/// pending passage.
const QUITTING_TIMEOUT: Duration = Duration::from_millis(250);
/// Loading the Lua module, opening the dictation file and pinning its buffer.
/// The `:edit` in there can run the user's own autocommands on a cold
/// filetype, so it is given more room than a probe; it happens once per
/// connection, off the latency path.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// A cosmetic indicator push, sent as a notification about ten times a second
/// — so only the socket write is waited for, and the only way to reach this
/// deadline is an editor that has stopped draining its socket. Dropping the
/// push, and the connection with it, is then exactly right: committed text
/// must not queue behind a level meter. The one confirmed push is the idle
/// state at `close`, which a detaching daemon waits this long for.
const INDICATOR_TIMEOUT: Duration = Duration::from_millis(250);
const CONNECT_POLL: Duration = Duration::from_millis(25);
/// Ownership probes retried when the peer closes the connection unanswered.
const PROBE_ATTEMPTS: usize = 3;
/// How long a call on an attached editor is waited for while Neovim holds it
/// behind a half-typed command, before it is given up on as if the editor
/// had stopped answering.
///
/// Someone who typed `g` or `"` and stopped to think finishes or cancels it
/// within seconds, so two minutes gives that pause twenty times over and the
/// text still lands, once, in the window. What outlasts it is almost never a
/// person mid-command: a hit-enter prompt or a plugin's `input()` in an
/// editor nobody is looking at, which would otherwise hold this one call for
/// hours while every utterance after it queues behind it. Those then take
/// the path any silent editor takes: the held call keeps its operation id,
/// and if its repeat is not answered either, the text goes to the pending
/// passage with a notification that it may also be in the window.
const HELD_AT_MOST: Duration = Duration::from_secs(120);
/// How often the log repeats that a call is still held.
const HELD_LOG_EVERY: Duration = Duration::from_secs(10);

const SPOKENPAD_LUA: &str = include_str!("../../lua/spokenpad.lua");
const BUNDLED_INIT: &str = include_str!("../../lua/dictation_init.lua");

/// Everything an attaching session needs to know about an editor answering on
/// the dictation socket: who owns it, whether its startup has finished, which
/// buffer — if any — a previous session pinned in it, and the buffer of the
/// file it was started on, which is what an editor opened with
/// `spokenpad editor` holds before any daemon has pinned anything.
const OWNERSHIP_QUERY: &str = r#"
local marker = vim.g.spokenpad_owner
local ready = vim.g.spokenpad_startup_ready
local buf = _G.Spokenpad and _G.Spokenpad.buf or nil
if not (buf and vim.api.nvim_buf_is_valid(buf) and vim.api.nvim_buf_is_loaded(buf)) then
  buf = nil
end
local startup, startup_name
if vim.fn.argc() > 0 then
  local wanted = vim.fn.fnamemodify(vim.fn.argv(0), ":p")
  for _, candidate in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_loaded(candidate) and vim.api.nvim_buf_get_name(candidate) == wanted then
      startup, startup_name = candidate, wanted
      break
    end
  end
end
return {
  marker or vim.NIL,
  ready or vim.NIL,
  buf or vim.NIL,
  buf and vim.api.nvim_buf_get_name(buf) or vim.NIL,
  startup or vim.NIL,
  startup_name or vim.NIL,
}
"#;

const OPEN_BUFFER: &str = r#"
vim.cmd.edit(vim.fn.fnameescape(...))
return vim.api.nvim_get_current_buf()
"#;

/// The whole indicator, as the daemon knows it. Pushing all of it at once
/// keeps the editor's copy a function of daemon state rather than of the
/// history of updates that reached it.
#[derive(Debug, Clone, PartialEq)]
pub struct IndicatorState {
    pub phase: IndicatorPhase,
    pub level: f64,
    pub preview: String,
    /// What happened to this capture, split into the headline the winbar always
    /// draws and the detail it appends when the window is wide enough. Kept
    /// apart from `preview`: the editor shows the notice in the winbar in
    /// every phase, while the preview is virtual text that exists only while
    /// there is a live tail to show.
    pub notice: Option<NoticeText>,
    pub latched: bool,
    pub previewing: bool,
}

impl Default for IndicatorState {
    /// Nothing is being recorded and previews are not paused: the state a
    /// window is left in when the daemon detaches from it.
    fn default() -> Self {
        Self {
            phase: IndicatorPhase::Idle,
            level: 0.0,
            preview: String::new(),
            notice: None,
            latched: false,
            previewing: true,
        }
    }
}

/// A live connection and the buffer it pins. These three are meaningless
/// apart: an editor this session has not pinned a buffer in cannot be
/// appended to, and a path with no buffer behind it cannot be returned as the
/// place the transcript is going.
struct Connected {
    client: RpcClient,
    buffer: i64,
    path: PathBuf,
}

pub struct NvimSession {
    config: Nvim,
    connection: Option<Connected>,
    session_nonce: Option<String>,
    append_sequence: u64,
    /// The thread that owns the window in `mode = "pane"`, started the first
    /// time one is needed and stopped when this session closes. In the other
    /// modes it stays `None` and nothing here touches X at all.
    pane: Option<PaneHost>,
    /// Why the last window this session tried to open did not open, until
    /// one does: the desktop notification for text that went to the file
    /// says this rather than only that no editor is open.
    refused: Option<String>,
    /// Whether the user has been told, this session, that a tiled pane
    /// opens floating under their window manager: once, not every window.
    told_tiled_floats: bool,
    /// Set while the user has closed the last pane themselves and not
    /// pressed the key since. Text that arrives meanwhile goes to the pending
    /// passage rather than into a new pane: only a key press opens one after
    /// the user closed one.
    closed_by_user: Option<ClosedByUser>,
    /// Whether the pane that is open, or last was, became the dictation
    /// window: its editor answered and this session attached to it. Only
    /// such a pane's close is the user's close of the dictation window; a
    /// pane whose editor quit while it started is a failed open.
    pane_attached: bool,
    /// Set when the daemon is stopping, so a call Neovim is holding behind a
    /// half-typed command is given up on at once rather than waited for.
    quitting: Arc<AtomicBool>,
}

impl NvimSession {
    pub fn new(config: Nvim) -> Self {
        Self {
            config,
            connection: None,
            session_nonce: None,
            append_sequence: 0,
            pane: None,
            refused: None,
            told_tiled_floats: false,
            closed_by_user: None,
            pane_attached: false,
            quitting: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag that says the daemon is stopping. Setting it ends the wait
    /// for a call Neovim is holding within half a second.
    pub fn quitting(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.quitting)
    }

    pub fn connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Whether an editor is attached and still pinned to its dictation file.
    /// A connection to one that is not, or that has gone, is dropped here, so
    /// `false` means the next [`ensure`](Self::ensure) opens or attaches a
    /// window afresh.
    pub fn attached(&mut self) -> bool {
        let Self {
            connection,
            pane,
            quitting,
            ..
        } = self;
        let Some(open) = connection.as_mut() else {
            return false;
        };
        let pinned = still_pinned(open, &mut Held::new("the liveness check", pane, quitting));
        if !pinned {
            self.drop_connection();
        }
        pinned
    }

    pub fn config(&self) -> &Nvim {
        &self.config
    }

    /// Replaces the settings the next window opens with, as re-read from the
    /// config file. The window that is open, if any, keeps what it opened
    /// with; so do the pane thread, whose windows each take their own.
    pub fn reconfigure(&mut self, config: Nvim) {
        self.config = config;
    }

    pub fn path(&self) -> Option<&Path> {
        self.connection.as_ref().map(|open| open.path.as_path())
    }

    /// Returns the pinned dictation path, reattaching as needed and, in pane
    /// mode, opening a pane when `want` allows one. `None` means no editor is
    /// open: in attach mode the user has not run `spokenpad editor`, in pane
    /// mode none could or may open, and text goes to the pending passage
    /// through [`append_detached`](Self::append_detached) instead.
    pub fn ensure(&mut self, want: Want) -> Result<Option<PathBuf>> {
        if want == Want::Press {
            self.closed_by_user = None;
        }
        if self.attached()
            && let Some(open) = &self.connection
        {
            return Ok(Some(open.path.clone()));
        }

        let attached = self.attach_existing()?
            || match self.config.mode {
                Mode::Pane if want == Want::Text && !self.may_open_for_text() => Ok(false),
                Mode::Pane => self.open_pane_and_attach(),
                Mode::Attach => Ok(false),
            }
            .inspect(|_| self.refused = None)
            .inspect_err(|error| self.refused = Some(format!("{error:#}")))?;
        if !attached {
            return Ok(None);
        }
        let path = self
            .connection
            .as_ref()
            .context("nvim connected without a dictation path")?
            .path
            .clone();
        passage::settle(&self.config, &path);
        Ok(Some(path))
    }

    /// Whether text, arriving with no editor attached, may open a pane: not
    /// after the user closed the last one, whether or not that close has
    /// been reported to the daemon yet, and not while the last one is still
    /// closing — its editor gone, its window not yet — since that may be the
    /// user's close, not yet recorded. A pane that failed on its own is
    /// replaced, so that the capture it showed has a window again.
    ///
    /// `alive` is read first: the pane's thread records a close before it
    /// clears `alive`, so a pane found gone has its close already recorded.
    fn may_open_for_text(&self) -> bool {
        self.closed_by_user.is_none()
            && !self.pane.as_ref().is_some_and(|pane| {
                pane.alive().load(Ordering::Acquire) || pane.closed_by_user_pending()
            })
    }

    /// When the user closed the pane — its window, or `:q` in it — if they
    /// did since the last time this was asked. From then until the next key
    /// press, text no longer opens a pane ([`Want::Text`]). Always `None` in
    /// attach mode: the daemon cannot tell `:q` in an editor it did not start
    /// from that editor dying.
    pub fn take_user_close(&mut self) -> Option<Instant> {
        let at = self.pane.as_ref()?.take_closed_by_user()?;
        if !std::mem::take(&mut self.pane_attached) {
            return None;
        }
        self.closed_by_user = Some(ClosedByUser {
            file: self.connection.take().map(|open| open.path),
        });
        Some(at)
    }

    /// Tells the user, through the desktop's notification service, that
    /// closing the dictation window stopped the recording it was showing:
    /// the window that would have said so is gone. Once per close.
    pub fn notify_closed_mid_capture(&self) {
        if !self.config.notify {
            return;
        }
        let file = match self
            .closed_by_user
            .as_ref()
            .and_then(|closed| closed.file.as_ref())
        {
            Some(path) => format!(" What was already transcribed is in {}.", path.display()),
            None => String::new(),
        };
        let body = format!(
            "You closed the dictation window, so the recording in progress was cancelled \
             and the rest of it is not transcribed.{file} The recording itself is kept."
        );
        if let Err(error) = wm::run(&[
            "notify-send",
            "--app-name=spokenpad",
            "spokenpad: recording cancelled",
            &body,
        ]) {
            log::debug!("desktop notification unavailable: {error:#}");
        }
    }

    /// Appends committed text straight to the pending passage, for when no
    /// editor can take it. The next editor opens on that file.
    pub fn append_detached(&self, text: &str, continued: bool) -> Result<DetachedWrite> {
        passage::append(&self.config, text, continued)
    }

    /// Tells the user, through the desktop's notification service, that
    /// dictation is going to a file no editor shows. Best effort: a desktop
    /// without `notify-send` or a notification daemon only gets the log line.
    pub fn notify_detached(&self, path: &Path, why: Detached) {
        if !self.config.notify {
            return;
        }
        let (title, body) = match (why, &self.refused) {
            (Detached::Unconfirmed, _) => (
                "spokenpad: text saved outside the window",
                format!(
                    "The dictation window did not confirm it received the last text, \
                     so it is saved to {} as well. If the window shows it too, it is in both.",
                    path.display()
                ),
            ),
            (Detached::NoEditor, _) if self.closed_by_user.is_some() => (
                "spokenpad: text saved after the window closed",
                format!(
                    "You closed the dictation window before this text arrived, so it is saved \
                     to {}. The next dictation opens a window on it.",
                    path.display()
                ),
            ),
            (Detached::NoEditor, Some(reason)) => (
                "spokenpad: the dictation window could not open",
                format!("{reason}.\nSaved to {}.", path.display()),
            ),
            (Detached::NoEditor, None) => (
                "spokenpad: no dictation editor is open",
                format!(
                    "Saved to {}. Run `spokenpad editor` to see it.",
                    path.display()
                ),
            ),
        };
        if let Err(error) = wm::run(&["notify-send", "--app-name=spokenpad", title, &body]) {
            log::debug!("desktop notification unavailable: {error:#}");
        }
    }

    /// Says, in the log every time and in one desktop notification a
    /// session, that `pane_layout = "tiled"` opens floating here: a tiled
    /// pane gives up the window type that refuses focus on window managers
    /// that ignore the user time, and is proven unfocused only on
    /// [`x11::TILED_PROVEN`].
    fn tell_tiled_floats(&mut self, manager: &x11::Manager) {
        let name = manager
            .name
            .as_deref()
            .unwrap_or("an unnamed window manager");
        let reason = format!(
            "nvim.pane_layout = \"tiled\" is proven never to take the focus only on i3, sway, \
             Openbox and KWin; this display runs {name}, so the pane opens floating"
        );
        log::warn!("{reason}");
        if self.told_tiled_floats || !self.config.notify {
            return;
        }
        self.told_tiled_floats = true;
        if let Err(error) = wm::run(&[
            "notify-send",
            "--app-name=spokenpad",
            "spokenpad: the pane opens floating here",
            &reason,
        ]) {
            log::debug!("desktop notification unavailable: {error:#}");
        }
    }

    /// Appends literal text and does not return until Neovim confirms its save.
    ///
    /// A failure says whether the text can have landed: see
    /// [`AppendFailure`].
    pub fn append(&mut self, text: &str, continued: bool) -> Result<usize, AppendFailure> {
        let arguments = self
            .append_arguments(text, continued)
            .map_err(AppendFailure::NotSent)?;
        let open = self
            .connection
            .as_mut()
            .ok_or_else(|| AppendFailure::NotSent(anyhow!("not connected to nvim")))?;
        let sent = open.client.send_request(
            "nvim_exec_lua",
            append_call(arguments.clone()),
            patient_deadline(&self.quitting),
        );
        let id = match sent {
            Ok(id) => id,
            Err(error) => {
                // The editor never received the whole request — typically it
                // has exited and the write hit a closed socket — so it cannot
                // have appended anything.
                self.drop_connection();
                return Err(AppendFailure::NotSent(error.into()));
            }
        };
        self.confirm_append(id, arguments)
            .map_err(AppendFailure::Unconfirmed)
    }

    /// The operation id and arguments of the next append.
    fn append_arguments(&mut self, text: &str, continued: bool) -> Result<Vec<Value>> {
        self.append_sequence = self
            .append_sequence
            .checked_add(1)
            .context("nvim append operation counter exhausted")?;
        if self.session_nonce.is_none() {
            self.session_nonce = Some(new_marker()?);
        }
        let operation = format!(
            "{}:{}",
            self.session_nonce.as_deref().expect("initialized above"),
            self.append_sequence
        );
        Ok(vec![
            Value::from(operation),
            Value::from(text),
            Value::from(continued),
        ])
    }

    /// Waits for the reply to append request `id`, which was sent whole, and
    /// handles a lost reply by repeating the same operation once.
    fn confirm_append(&mut self, id: u64, arguments: Vec<Value>) -> Result<usize> {
        let Self {
            connection,
            pane,
            quitting,
            ..
        } = self;
        let open = connection.as_mut().context("not connected to nvim")?;
        let mut held = Held::new("an append", pane, quitting);
        let replied = open.client.reply(
            id,
            "nvim_exec_lua",
            patient_deadline(quitting),
            Patience::WhileTyping(&mut |time| held.watch(time)),
        );
        drop(held);
        let value = match replied {
            Ok(value) => value,
            // Stopping: there is no time to reconnect and repeat, which can
            // take seconds on an editor that stopped answering. Unconfirmed
            // now, the text reaches the pending passage within the grace.
            Err(RpcFailure::Timeout(reason)) if self.quitting.load(Ordering::Acquire) => {
                self.drop_connection();
                bail!("append outcome is unknown, and the daemon is stopping: {reason}");
            }
            Err(RpcFailure::Timeout(reason)) => {
                // The request may have completed after its reply was lost.
                // Reconnect and repeat the same operation id; the Lua side
                // returns its cached result instead of appending twice. Any
                // failure of the retry leaves this session disconnected, so
                // the next utterance reattaches rather than writing into a
                // client whose reply stream is out of step.
                let pinned = self
                    .path()
                    .context("append timed out on a session with no pinned file")?
                    .to_owned();
                self.drop_connection();
                ensure!(
                    self.attach_existing()?,
                    "append outcome is unknown after timeout: {reason}"
                );
                // The cache that makes the repeat idempotent is keyed on the
                // pinned buffer, and a buffer that is gone takes it with it —
                // reattaching then pins a *fresh* file, and repeating there
                // would write this utterance into a second one. Same path,
                // same buffer: a file that is still pinned is the buffer the
                // editor was already holding. So the text stays in the log
                // and the recovery WAV rather than landing twice.
                if self.path() != Some(pinned.as_path()) {
                    self.drop_connection();
                    bail!("append outcome is unknown: the dictation file changed during reconnect");
                }
                match self.request_append(arguments) {
                    Ok(value) => value,
                    Err(error) => {
                        self.drop_connection();
                        return Err(error.into());
                    }
                }
            }
            Err(error) => {
                self.drop_connection();
                return Err(error.into());
            }
        };
        let count = value
            .as_u64()
            .context("nvim append returned a non-integer line count")?;
        usize::try_from(count).context("nvim line count does not fit usize")
    }

    /// Pushes the whole indicator as a notification; transport failures surface.
    pub fn set_indicator(&mut self, state: &IndicatorState) -> Result<()> {
        self.push(indicator_fields(state))
    }

    /// Asks the editor to copy its whole dictation buffer to `+`.
    ///
    /// A request, not a notification: a missing clipboard provider or a
    /// buffer that went away is something the caller must see, not a fact
    /// left to a log nobody is watching. `Spokenpad.copy_buffer` never
    /// raises -- it wraps its own work in `pcall` -- so an `Err` here is a
    /// transport failure, exactly like any other request, and the connection
    /// is dropped the same way `request_append` drops it.
    pub fn copy_buffer(&mut self) -> Result<CopyOutcome> {
        let Self {
            connection,
            pane,
            quitting,
            ..
        } = self;
        let open = connection.as_mut().context("not connected to nvim")?;
        let mut held = Held::new("the clipboard copy", pane, quitting);
        let result = open.client.request(
            "nvim_exec_lua",
            vec![
                Value::from("return Spokenpad.copy_buffer()"),
                Value::Array(Vec::new()),
            ],
            patient_deadline(quitting),
            Patience::WhileTyping(&mut |time| held.watch(time)),
        );
        drop(held);
        match result {
            Ok(value) => parse_copy_outcome(&value),
            Err(error) => {
                self.drop_connection();
                Err(error.into())
            }
        }
    }

    /// Detaches without killing the editor or closing the user's passage.
    pub fn close(&mut self) {
        if let Some(open) = self.connection.as_mut() {
            // A window left showing "REC", a half-lit meter and a preview
            // that will never land says the daemon is still listening when it
            // has exited. Sent as a request, unlike every other indicator
            // push: nvim discards input it has not parsed yet when a channel
            // reaches EOF, so a notification written immediately before the
            // socket closes is regularly lost. Best effort all the same — a
            // detaching daemon waits a quarter of a second for cosmetics.
            let _ = open.client.request(
                "nvim_exec_lua",
                indicator_call(indicator_fields(&IndicatorState::default())),
                deadline(INDICATOR_TIMEOUT),
                Patience::Deadline,
            );
        }
        self.drop_connection();
        // An editor the user opened outlives the daemon on purpose: they may
        // still be reading what they dictated. A pane cannot — its window and
        // its editor are threads and children of this process — so it is
        // closed here, which writes every modified buffer on the way out.
        self.pane = None;
    }

    fn push(&mut self, fields: Vec<(Value, Value)>) -> Result<()> {
        let Some(open) = self.connection.as_mut() else {
            return Ok(());
        };
        let result = open.client.notify(
            "nvim_exec_lua",
            indicator_call(fields),
            deadline(INDICATOR_TIMEOUT),
        );
        if result.is_err() {
            self.drop_connection();
        }
        result.map_err(Into::into)
    }

    fn request_append(&mut self, arguments: Vec<Value>) -> Result<Value, RpcFailure> {
        let Self {
            connection,
            pane,
            quitting,
            ..
        } = self;
        let open = connection
            .as_mut()
            .ok_or_else(|| RpcFailure::Other(anyhow!("not connected to nvim")))?;
        let mut held = Held::new("an append", pane, quitting);
        open.client.request(
            "nvim_exec_lua",
            append_call(arguments),
            patient_deadline(quitting),
            Patience::WhileTyping(&mut |time| held.watch(time)),
        )
    }

    fn drop_connection(&mut self) {
        self.connection = None;
    }

    /// Connects to the socket and asks who owns it. A peer that accepts the
    /// connection and then closes it before answering is retried briefly: an
    /// editor exiting, or a listener whose descriptor a forking process still
    /// held for a moment, looks exactly like that, and a few milliseconds later
    /// the same path is either refused (stale) or answered (live).
    fn probe_existing(&self) -> Result<Probe> {
        let mut last_gone = None;
        for _ in 0..PROBE_ATTEMPTS {
            let mut client =
                match RpcClient::connect(&self.config.socket_path, deadline(PROBE_TIMEOUT)) {
                    Ok(client) => client,
                    Err(error) if error.is_stale_socket() => return Ok(Probe::Stale),
                    Err(error) => return Err(error.into()),
                };
            match client.request(
                "nvim_exec_lua",
                vec![Value::from(OWNERSHIP_QUERY), Value::Array(Vec::new())],
                deadline(PROBE_TIMEOUT),
                Patience::Deadline,
            ) {
                Ok(value) => return Ok(Probe::Live(client, parse_ownership(&value)?)),
                Err(error @ RpcFailure::PeerGone(_)) => {
                    last_gone = Some(error);
                    std::thread::sleep(CONNECT_POLL);
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(
            anyhow::Error::from(last_gone.expect("at least one attempt"))
                .context("the nvim socket keeps accepting and closing connections"),
        )
    }

    fn attach_existing(&mut self) -> Result<bool> {
        let metadata = match fs::symlink_metadata(&self.config.socket_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("inspect nvim socket path"),
        };
        ensure!(
            metadata.file_type().is_socket(),
            "refusing non-socket path at {}",
            self.config.socket_path.display()
        );
        let (mut client, ownership) = match self.probe_existing()? {
            Probe::Stale => {
                remove_stale_socket(&self.config.socket_path)?;
                return Ok(false);
            }
            Probe::Live(client, ownership) => (client, ownership),
        };
        let expected_marker = read_marker(&marker_path(&self.config.socket_path))?;
        // The marker file proves spokenpad launched this editor. A pinned
        // buffer under the dictation directory proves the same thing from the
        // other side, for a marker file that was lost or replaced.
        let owned = expected_marker.is_some() && expected_marker == ownership.marker;
        let adoptable = self.adoptable_buffer(ownership.pinned.as_ref())?;
        ensure!(
            owned || adoptable.is_some(),
            "refusing unrelated nvim socket {}",
            self.config.socket_path.display()
        );
        if self.config.mode == Mode::Pane && !has_ui(&mut client)? {
            self.stop_invisible(client)?;
            return Ok(false);
        }
        // An editor spokenpad started that nothing has pinned yet — one the
        // user opened with `spokenpad editor` — is adopted on the file it was
        // started on, when that is a dictation file.
        let adoptable = match adoptable {
            Some(adopted) => Some(adopted),
            None if owned => self.adoptable_buffer(ownership.startup.as_ref())?,
            None => None,
        };

        load_spokenpad_lua(&mut client, deadline(SETUP_TIMEOUT))?;
        let mut fresh = None;
        let (buffer, path) = match adoptable {
            Some(adopted) => adopted,
            None => {
                let file = fresh.insert(NewFileGuard::claim(&self.config)?);
                let buffer = open_buffer(&mut client, file.path(), deadline(SETUP_TIMEOUT))?;
                (buffer, file.path().to_owned())
            }
        };
        // The editor is only dedicated if spokenpad opened it. The Lua side
        // also remembers this across reloads, so reattaching to the window
        // this daemon opened before a restart keeps its chrome.
        setup_buffer(&mut client, buffer, owned, deadline(SETUP_TIMEOUT))?;
        if let Some(file) = fresh.as_mut() {
            file.keep();
        }
        self.connection = Some(Connected {
            client,
            buffer,
            path,
        });
        Ok(true)
    }

    /// Ends an editor of spokenpad's that no window shows, and clears its
    /// socket, so that the next pane opens rather than text going where
    /// nobody sees it. In pane mode every editor spokenpad started is drawn
    /// by a pane, so one with no UI is invisible: what `:restart` leaves
    /// behind (Neovim 0.12 starts a new server with the same arguments,
    /// which waits for a UI the pane never gives it), or the editor of a pane
    /// a crashed daemon took with it. It has had no window to be typed into,
    /// so it holds nothing to lose.
    fn stop_invisible(&self, mut client: RpcClient) -> Result<()> {
        log::info!(
            "stopping the editor on {}: no window shows it (left behind by `:restart`?)",
            self.config.socket_path.display()
        );
        // Scheduled, so the call is answered before the editor goes.
        client.request(
            "nvim_exec_lua",
            vec![
                Value::from("vim.schedule(function() vim.cmd('qall!') end)"),
                Value::Array(Vec::new()),
            ],
            deadline(PROBE_TIMEOUT),
            Patience::Deadline,
        )?;
        drop(client);
        let gone_by = deadline(PROBE_TIMEOUT);
        while Instant::now() < gone_by {
            match RpcClient::connect(&self.config.socket_path, gone_by) {
                Err(error) if error.is_stale_socket() => {
                    if self.config.socket_path.exists() {
                        remove_stale_socket(&self.config.socket_path)?;
                    }
                    return Ok(());
                }
                _ => std::thread::sleep(CONNECT_POLL),
            }
        }
        bail!(
            "an editor no window shows is still listening on {} after it was told to quit",
            self.config.socket_path.display()
        )
    }

    fn adoptable_buffer(&self, pinned: Option<&Pinned>) -> Result<Option<(i64, PathBuf)>> {
        let Some(pinned) = pinned else {
            return Ok(None);
        };
        Ok(inside_dictation_dir(&self.config, Path::new(&pinned.name))
            .context("canonicalize dictation directory")?
            .map(|_| (pinned.buffer, PathBuf::from(&pinned.name))))
    }

    /// Wait for the editor inside a pane this session just opened to answer
    /// on its socket, and adopt it. `gone` says whether it has already
    /// exited, which is the one failure worth giving up on early.
    fn await_editor(
        &mut self,
        fresh: &mut NewFileGuard,
        marker: &str,
        deadline: Instant,
        mut gone: impl FnMut() -> bool,
    ) -> Result<bool> {
        let mut last_error = None;
        // A healthy client is handed back to the next attempt: an editor that
        // is merely still initializing answers the next probe on the same
        // connection, so only a connection that actually broke is remade.
        let mut client = None;
        while Instant::now() < deadline {
            if gone() {
                bail!("dictation editor exited during startup");
            }
            if self.config.socket_path.exists() {
                // Everything in here is transient until the outer deadline:
                // an editor still loading a plugin manager answers late, half
                // ready, or not at all, and treating that as a hard failure
                // used to SIGKILL a perfectly healthy editor mid-startup.
                match self.connect_spawned(client.take(), fresh.path(), marker, deadline) {
                    Ok(Readiness::Ready(open)) => {
                        self.connection = Some(open);
                        fresh.keep();
                        return Ok(true);
                    }
                    Ok(Readiness::Waiting { reason, keep }) => {
                        last_error = Some(reason.to_owned());
                        client = Some(keep);
                    }
                    Err(error) => last_error = Some(format!("{error:#}")),
                }
            }
            std::thread::sleep(CONNECT_POLL);
        }
        bail!(
            "nvim did not answer on {} within {:.1}s{}",
            self.config.socket_path.display(),
            self.config.startup_timeout_s,
            last_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )
    }

    /// Open a window spokenpad draws itself, with Neovim embedded in it.
    ///
    /// Unlike a terminal this needs no rule in the user's configuration: the
    /// window carries the properties that make a window manager refuse it
    /// focus, which `tests/pane_window.rs` and `tests/pane_focus_wms.rs`
    /// check against their own ablations. sway alone reads none of them, and
    /// is given a rule over IPC instead ([`refuse_pane_focus_on_sway`]).
    /// What it does need is an X display, and saying so plainly
    /// is the whole error path — with none, the text goes to the pending
    /// passage.
    fn open_pane_and_attach(&mut self) -> Result<bool> {
        let opened = self.try_open_pane();
        if !matches!(opened, Ok(true)) {
            // Whatever went wrong, no window may be left behind. One that
            // outlived its attach would be a live editor on the dictation
            // socket that this session does not own — and every later
            // key-down would refuse that socket rather than replace it, so
            // the pane would look alive while every utterance went to a file.
            self.pane_close();
        }
        opened
    }

    fn try_open_pane(&mut self) -> Result<bool> {
        use crate::shell::pane::{Options, host::Opening};

        // Whatever pane was there is replaced; this one counts once it is
        // attached.
        self.pane_attached = false;
        // One deadline for the whole thing: the window, and the editor
        // answering inside it. Two would let a slow window spend the
        // editor's budget as well as its own.
        let deadline = Instant::now() + Duration::from_secs_f64(self.config.startup_timeout_s);
        let (target, display, manager) = pane_target(&self.config)?;
        refuse_pane_focus_on_sway(&self.config, &manager)?;
        let layout = x11::layout_under(self.config.pane_layout, &manager);
        if layout != self.config.pane_layout {
            self.tell_tiled_floats(&manager);
        }
        let mut fresh = NewFileGuard::claim(&self.config)?;
        let (command, marker) = pane_launch(&self.config, fresh.path())?;
        let value = marker.value().to_owned();

        let host = match self.pane.as_mut() {
            Some(host) => host,
            None => self.pane.insert(PaneHost::start()?),
        };
        let (columns, rows) = host.open(
            Opening {
                command,
                options: Options {
                    display,
                    family: self.config.font_family.clone(),
                    size: self.config.font_size,
                    dimensions: self.config.pane_dimensions,
                    layout,
                    attach_timeout: deadline.saturating_duration_since(Instant::now()),
                    target: Some(target),
                    title: "spokenpad dictation".to_owned(),
                },
            },
            deadline,
        )?;
        // What was asked and what was applied, every time: a pane that
        // floats where `pane_layout = "tiled"` was set is then settled by the
        // log rather than by a guess. The size is the grid at the map; a
        // window manager that tiles the pane resizes it afterwards.
        log::info!(
            "{}",
            pane_opened(
                self.config.pane_layout,
                layout,
                manager.name.as_deref(),
                (columns, rows)
            )
        );
        // The editor is waited for on its own socket, which cannot tell a
        // slow start from one that died sourcing a broken configuration. The
        // pane can: it drops a pane whose editor is gone, and this flag goes
        // with it.
        let alive = host.alive();
        let attached = self.await_editor(&mut fresh, &value, deadline, || {
            !alive.load(std::sync::atomic::Ordering::Acquire)
        })?;
        if attached {
            marker.keep();
            self.pane_attached = true;
        }
        Ok(attached)
    }

    /// Close the pane, if this session owns one.
    fn pane_close(&mut self) {
        if let Some(host) = self.pane.as_ref() {
            host.close();
        }
    }

    /// One attempt at adopting the editor this session just spawned, over
    /// `client` if a previous attempt left one healthy. Each RPC is capped
    /// well inside the startup deadline so that one slow call cannot consume
    /// the whole window and leave no attempt to retry with.
    ///
    /// The client is taken by value and returned only with [`Readiness`]: an
    /// attempt that failed drops it, which is exactly when reconnecting is
    /// the right answer.
    fn connect_spawned(
        &self,
        client: Option<RpcClient>,
        target: &Path,
        marker: &str,
        startup_deadline: Instant,
    ) -> Result<Readiness> {
        let mut client = match client {
            Some(client) => client,
            None => RpcClient::connect(
                &self.config.socket_path,
                capped(startup_deadline, PROBE_TIMEOUT),
            )?,
        };
        let ownership = query_ownership(
            &mut client,
            capped(startup_deadline, PROBE_TIMEOUT),
            Patience::Deadline,
        )?;
        match ownership.marker.as_deref() {
            None => {
                return Ok(Readiness::Waiting {
                    reason: "spawned nvim has not initialized its ownership marker",
                    keep: client,
                });
            }
            Some(actual) if actual != marker => {
                return Ok(Readiness::Waiting {
                    reason: "another nvim is answering on the dictation socket",
                    keep: client,
                });
            }
            Some(_) => {}
        }
        // The marker alone only proves our `--cmd` ran. The readiness flag is
        // set from a one-shot VimEnter registered after it, so it also proves
        // the user's own startup handlers have finished.
        if ownership.ready.as_deref() != Some(marker) {
            return Ok(Readiness::Waiting {
                reason: "spawned nvim user configuration is still initializing",
                keep: client,
            });
        }
        load_spokenpad_lua(&mut client, capped(startup_deadline, SETUP_TIMEOUT))?;
        let buffer = open_buffer(&mut client, target, capped(startup_deadline, SETUP_TIMEOUT))?;
        setup_buffer(
            &mut client,
            buffer,
            true,
            capped(startup_deadline, SETUP_TIMEOUT),
        )?;
        Ok(Readiness::Ready(Connected {
            client,
            buffer,
            path: target.to_owned(),
        }))
    }
}

/// The log line for a pane that just opened: the layout asked for and the one
/// applied under `manager`, and its grid in cells.
fn pane_opened(
    asked: PaneLayout,
    applied: PaneLayout,
    manager: Option<&str>,
    (columns, rows): (u16, u16),
) -> String {
    let under = manager.unwrap_or("an unnamed window manager");
    let why = if asked == applied {
        String::new()
    } else {
        format!(" (asked {asked}: {under} is not proven to keep a {asked} pane unfocused)")
    };
    format!("opened the pane {applied}{why} under {under}, {columns}x{rows} cells")
}

/// The outcome of one attempt to adopt a freshly spawned editor.
enum Readiness {
    Ready(Connected),
    /// Not ready yet, for `reason`; `keep` is the still-healthy connection the
    /// next attempt should reuse.
    Waiting {
        reason: &'static str,
        keep: RpcClient,
    },
}

/// The user closed the last pane themselves.
struct ClosedByUser {
    /// The file it showed, if the session was attached to it.
    file: Option<PathBuf>,
}

/// Why the daemon wants an editor, which decides whether a pane may open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// A key press: open a pane if none is attached.
    Press,
    /// Text to deliver: reattach, but open a pane only where the last one
    /// did not close by the user's hand. After the user closed it, text goes
    /// to the pending passage until the next press.
    Text,
}

/// What an editor answering on the dictation socket says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ownership {
    /// `g:spokenpad_owner`, set by the `--cmd` spokenpad spawned it with.
    marker: Option<String>,
    /// `g:spokenpad_startup_ready`, set from a one-shot `VimEnter` registered
    /// after the user's own startup handlers.
    ready: Option<String>,
    /// The buffer a previous session pinned, if it is still loaded.
    pinned: Option<Pinned>,
    /// The buffer of the file the editor was started on, if still loaded.
    startup: Option<Pinned>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pinned {
    buffer: i64,
    name: String,
}

/// Why text went to the pending passage, for the notification that says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detached {
    /// No editor could take it: none is open, or it could not be reached.
    NoEditor,
    /// The editor was sent it and never confirmed it, so the passage may be
    /// a second copy.
    Unconfirmed,
}

/// Why [`NvimSession::append`] did not confirm the text, split by the one
/// question the caller must answer next: can the text be in the editor?
#[derive(Debug)]
pub enum AppendFailure {
    /// The editor never received the whole request, so the text is certainly
    /// not in its buffer and may be written elsewhere without doubling it.
    NotSent(anyhow::Error),
    /// The request was sent and its outcome is unknown: the editor may have
    /// appended the text. The daemon writes it to the pending passage anyway,
    /// and says it may be in both: a second copy the user can delete is
    /// better than text that is only in the log.
    Unconfirmed(anyhow::Error),
}

impl std::fmt::Display for AppendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSent(error) => write!(f, "not sent: {error:#}"),
            Self::Unconfirmed(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for AppendFailure {}

/// What `Spokenpad.copy_buffer` did, as it reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The buffer's text, past its trailing blank lines, is now in `+`.
    Copied,
    /// Nothing but trailing blank lines was there; the clipboard is untouched.
    Empty,
    /// The Lua side caught a failure (typically a missing clipboard
    /// provider) and reported it rather than raising.
    Failed(String),
}

fn parse_ownership(value: &Value) -> Result<Ownership> {
    let fields = value
        .as_array()
        .filter(|fields| fields.len() == 6)
        .context("nvim ownership query returned malformed data")?;
    let text = |field: &Value| -> Result<Option<String>> {
        if field.is_nil() {
            return Ok(None);
        }
        Ok(Some(
            field
                .as_str()
                .context("nvim ownership query returned a non-string field")?
                .to_owned(),
        ))
    };
    let buffer = |handle: &Value, name: &Value| -> Result<Option<Pinned>> {
        let name = text(name)?;
        Ok(match handle {
            Value::Nil => None,
            handle => Some(Pinned {
                buffer: handle
                    .as_i64()
                    .context("nvim returned a non-integer buffer handle")?,
                name: name.context("nvim reported a buffer with no file name")?,
            }),
        })
    };
    Ok(Ownership {
        marker: text(&fields[0])?,
        ready: text(&fields[1])?,
        pinned: buffer(&fields[2], &fields[3])?,
        startup: buffer(&fields[4], &fields[5])?,
    })
}

/// `Spokenpad.copy_buffer` returns `{status, detail}`; `detail` is only
/// meaningful for `"error"`, and is `vim.NIL` otherwise.
fn parse_copy_outcome(value: &Value) -> Result<CopyOutcome> {
    let fields = value
        .as_array()
        .filter(|fields| fields.len() == 2)
        .context("nvim copy_buffer returned malformed data")?;
    let status = fields[0]
        .as_str()
        .context("nvim copy_buffer status is not a string")?;
    match status {
        "copied" => Ok(CopyOutcome::Copied),
        "empty" => Ok(CopyOutcome::Empty),
        "error" => Ok(CopyOutcome::Failed(
            fields[1]
                .as_str()
                .context("nvim copy_buffer error carries no message")?
                .to_owned(),
        )),
        other => bail!("nvim copy_buffer returned an unknown status {other:?}"),
    }
}

/// The editor's own argv: the configured command, the init and colourscheme,
/// the ownership marker, the socket, the readiness flag and the file.
pub(crate) fn editor_argv(
    config: &Nvim,
    target: &Path,
    marker: &str,
    init: Option<&Path>,
) -> Result<Vec<String>> {
    let mut argv = config.editor.clone();
    if let Some(init) = init {
        argv.extend(["-u".to_owned(), utf8_path(init)?.to_owned()]);
    }
    if let Some(colorscheme) = &config.colorscheme {
        let name = lua_colorscheme_literal(colorscheme)?;
        let opaque = !config.transparent;
        argv.extend([
            "-c".to_owned(),
            format!("lua if _G.SpokenpadColorscheme then SpokenpadColorscheme({name}, {opaque}) else pcall(vim.cmd.colorscheme, {name}) end"),
        ]);
    }
    argv.extend([
        "--cmd".to_owned(),
        format!("let g:spokenpad_owner = '{marker}'"),
        "--listen".to_owned(),
        utf8_path(&config.socket_path)?.to_owned(),
        "-c".to_owned(),
        format!(
            "lua vim.g.spokenpad_startup_ready = nil; vim.api.nvim_create_autocmd('VimEnter', {{ once = true, callback = function() vim.g.spokenpad_startup_ready = '{marker}' end }})"
        ),
        utf8_path(target)?.to_owned(),
    ]);
    Ok(argv)
}

/// A colourscheme name as a Lua string literal.
///
/// Defence in depth: `config.rs` validates the same pattern when it loads, and
/// this refuses anything else again at the one point where the value becomes
/// code. Quoting a name that cannot contain a quote, a backslash or a newline
/// is then just interpolation, with no escaping layer to get wrong.
fn lua_colorscheme_literal(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
        "nvim.colorscheme must match [A-Za-z0-9_.-]+, got {name:?}"
    );
    Ok(format!("'{name}'"))
}

fn indicator_fields(state: &IndicatorState) -> Vec<(Value, Value)> {
    vec![
        (Value::from("phase"), Value::from(state.phase.as_str())),
        (Value::from("level"), Value::F64(clamp_level(state.level))),
        (Value::from("preview"), Value::from(state.preview.as_str())),
        // Absence travels as the empty string rather than as nil: nvim turns a
        // msgpack nil inside a map into `vim.NIL`, which Lua cannot tell from
        // a field the daemon meant to set. The two halves travel as two fields
        // so the editor never has to split a sentence it did not compose.
        (
            Value::from("notice"),
            Value::from(state.notice.as_ref().map_or("", |notice| notice.headline)),
        ),
        (
            Value::from("notice_detail"),
            Value::from(
                state
                    .notice
                    .as_ref()
                    .map_or("", |notice| notice.detail.as_ref()),
            ),
        ),
        (Value::from("latched"), Value::from(state.latched)),
        (Value::from("previewing"), Value::from(state.previewing)),
    ]
}

fn append_call(arguments: Vec<Value>) -> Vec<Value> {
    vec![
        Value::from("return Spokenpad.append_once(...)"),
        Value::Array(arguments),
    ]
}

/// `Spokenpad.push` wraps the update in a pcall, so an error never becomes a
/// message in a window that cannot take focus to answer the prompt it raises.
fn indicator_call(fields: Vec<(Value, Value)>) -> Vec<Value> {
    vec![
        Value::from("Spokenpad.push(...)"),
        Value::Array(vec![Value::Map(fields)]),
    ]
}

fn clamp_level(level: f64) -> f64 {
    if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The first deadline of a patient call: [`APPEND_TIMEOUT`], or
/// [`QUITTING_TIMEOUT`] once the daemon is stopping, so that a held call
/// asks `nvim_get_mode` at once and is given up on, and a frozen editor is
/// found out, within the shutdown grace.
fn patient_deadline(quitting: &AtomicBool) -> Instant {
    deadline(match quitting.load(Ordering::Acquire) {
        true => QUITTING_TIMEOUT,
        false => APPEND_TIMEOUT,
    })
}

fn deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

fn capped(deadline: Instant, timeout: Duration) -> Instant {
    (Instant::now() + timeout).min(deadline)
}

/// Removes a freshly created dictation file unless a session pins it.
///
/// The file is created before the editor is started, so that its name is
/// settled and its permissions are ours from the first byte. A spawn that then
/// failed used to leave a zero-byte file behind on every attempt. Anything
/// with content in it is left alone: that is the user's transcript. A pending
/// passage that `spokenpad editor` took is made pending again instead.
struct NewFileGuard {
    path: PathBuf,
    keep: bool,
    /// The socket whose pending pointer this passage was taken from, so it
    /// can be restored if the editor never starts.
    taken_from: Option<PathBuf>,
}

impl NewFileGuard {
    /// The pending passage, if dictation went to a file while no editor was
    /// open, so the new editor shows it; otherwise a new, empty file. For an
    /// editor the daemon opens itself, on the thread that writes the passage;
    /// the pointer is settled once the editor holds the file.
    fn claim(config: &Nvim) -> Result<Self> {
        let path = match passage::pending(config) {
            Some(pending) => pending,
            None => new_file(config)?,
        };
        Ok(Self {
            path,
            keep: false,
            taken_from: None,
        })
    }

    /// As [`claim`](Self::claim), for `spokenpad editor`: the pending passage
    /// is taken, so the daemon starts a new one rather than write into the
    /// file this editor shows.
    fn take(config: &Nvim) -> Result<Self> {
        Ok(match passage::take(config)? {
            Some(pending) => Self {
                path: pending,
                keep: false,
                taken_from: Some(config.socket_path.clone()),
            },
            None => Self {
                path: new_file(config)?,
                keep: false,
                taken_from: None,
            },
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for NewFileGuard {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        if let Some(socket) = &self.taken_from {
            if let Err(error) = passage::restore(socket, &self.path) {
                log::warn!(
                    "{} is no longer the pending dictation file: {error:#}",
                    self.path.display()
                );
            }
        } else if fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() == 0) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn still_pinned(open: &mut Connected, held: &mut Held<'_>) -> bool {
    // A bare `nvim_eval "1"` proves only that the editor answers. After the
    // user `:bdelete`s the dictation buffer it answers exactly as before, and
    // every append of that utterance failed against a buffer that was gone.
    //
    // Patient: an editor whose user has half a command typed is alive and
    // still pinned, and dropping it would send the next utterance elsewhere.
    query_ownership(
        &mut open.client,
        deadline(PROBE_TIMEOUT),
        Patience::WhileTyping(&mut |time| held.watch(time)),
    )
    .is_ok_and(|ownership| {
        ownership
            .pinned
            .is_some_and(|pin| pin.buffer == open.buffer)
    })
}

/// One call Neovim is holding behind a command half typed in its window,
/// watched from the session: how long it may be held, what the log says
/// meanwhile, and the notice in the pane while it lasts.
struct Held<'a> {
    what: &'static str,
    pane: Option<&'a PaneHost>,
    quitting: &'a AtomicBool,
    /// How long it had been held when the log last said so.
    logged: Option<Duration>,
    shown: bool,
}

/// What to do about a call held for so long, with the daemon stopping or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldVerdict {
    Wait,
    /// The daemon is stopping: give up now, so the text reaches a file within
    /// the shutdown grace.
    Abandon,
    /// Held past [`HELD_AT_MOST`]: treat it like an editor that stopped
    /// answering.
    GiveUp,
}

fn held_verdict(held: Duration, quitting: bool) -> HeldVerdict {
    match (quitting, held >= HELD_AT_MOST) {
        (true, _) => HeldVerdict::Abandon,
        (false, true) => HeldVerdict::GiveUp,
        (false, false) => HeldVerdict::Wait,
    }
}

impl<'a> Held<'a> {
    fn new(what: &'static str, pane: &'a Option<PaneHost>, quitting: &'a AtomicBool) -> Self {
        Self {
            what,
            pane: pane.as_ref(),
            quitting,
            logged: None,
            shown: false,
        }
    }

    /// Asked every half second while the call waits.
    fn watch(&mut self, waiting: Waiting) -> Result<(), RpcFailure> {
        let held = match waiting {
            // Not held yet, only unanswered: nothing to say unless the
            // daemon is stopping, which cannot wait for the deadline.
            Waiting::Unanswered if self.quitting.load(Ordering::Acquire) => {
                return Err(RpcFailure::Abandoned(format!(
                    "the daemon is stopping while {} is unanswered",
                    self.what
                )));
            }
            Waiting::Unanswered => return Ok(()),
            Waiting::Held(held) => held,
        };
        let seconds = held.as_secs();
        match held_verdict(held, self.quitting.load(Ordering::Acquire)) {
            HeldVerdict::Abandon => {
                return Err(RpcFailure::Abandoned(format!(
                    "the daemon is stopping while nvim holds {} behind a half-typed command",
                    self.what
                )));
            }
            HeldVerdict::GiveUp => {
                return Err(RpcFailure::Timeout(format!(
                    "nvim held {} behind a half-typed command or a prompt for {seconds}s; \
                     giving up on it",
                    self.what
                )));
            }
            HeldVerdict::Wait => {}
        }
        if self.logged.is_none_or(|last| held >= last + HELD_LOG_EVERY) {
            log::warn!(
                "nvim has held {} for {seconds}s: a command or a prompt in its window is \
                 waiting for keys; finish it or press <Esc> (given up on after {}s)",
                self.what,
                HELD_AT_MOST.as_secs()
            );
            self.logged = Some(held);
        }
        if !self.shown
            && let Some(pane) = self.pane
        {
            pane.show_held(true);
            self.shown = true;
        }
        Ok(())
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.shown
            && let Some(pane) = self.pane
        {
            pane.show_held(false);
        }
    }
}

enum Probe {
    /// Nothing listens on the socket path.
    Stale,
    Live(RpcClient, Ownership),
}

fn query_ownership(
    client: &mut RpcClient,
    deadline: Instant,
    patience: Patience,
) -> Result<Ownership> {
    let value = client.request(
        "nvim_exec_lua",
        vec![Value::from(OWNERSHIP_QUERY), Value::Array(Vec::new())],
        deadline,
        patience,
    )?;
    parse_ownership(&value)
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reinspect stale socket {}", path.display()))?;
    ensure!(
        metadata.file_type().is_socket(),
        "refusing to remove non-socket path at {}",
        path.display()
    );
    fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))
}

/// Whether any UI is attached to the editor: a pane, or a terminal.
fn has_ui(client: &mut RpcClient) -> Result<bool> {
    let uis = client.request(
        "nvim_list_uis",
        Vec::new(),
        deadline(PROBE_TIMEOUT),
        Patience::Deadline,
    )?;
    Ok(!uis
        .as_array()
        .context("nvim_list_uis returned something other than a list")?
        .is_empty())
}

/// Loads `spokenpad.lua` into the editor, which defines the `Spokenpad`
/// table every later call uses. Loading it again replaces the functions and
/// keeps the state.
fn load_spokenpad_lua(client: &mut RpcClient, deadline: Instant) -> Result<()> {
    client.request(
        "nvim_exec_lua",
        vec![Value::from(SPOKENPAD_LUA), Value::Array(Vec::new())],
        deadline,
        Patience::Deadline,
    )?;
    Ok(())
}

fn open_buffer(client: &mut RpcClient, path: &Path, deadline: Instant) -> Result<i64> {
    client
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_BUFFER),
                Value::Array(vec![Value::from(utf8_path(path)?)]),
            ],
            deadline,
            Patience::Deadline,
        )?
        .as_i64()
        .context("nvim returned a non-integer buffer handle")
}

fn setup_buffer(
    client: &mut RpcClient,
    buffer: i64,
    dedicated: bool,
    deadline: Instant,
) -> Result<()> {
    client.request(
        "nvim_exec_lua",
        vec![
            Value::from("Spokenpad.setup(...)"),
            Value::Array(vec![Value::from(buffer), Value::from(dedicated)]),
        ],
        deadline,
        Patience::Deadline,
    )?;
    Ok(())
}

/// `candidate` resolved, symlinks and all, when that lands inside the
/// dictation directory; `None` when it lands elsewhere, or when either
/// cannot be resolved because it does not exist (a candidate that cannot be
/// resolved for any reason is not proven inside). The one check behind every
/// path spokenpad trusts with a transcript it did not create itself: a
/// pinned buffer, or the pending passage's pointer. Errors only when the
/// dictation directory exists and cannot be resolved.
fn inside_dictation_dir(config: &Nvim, candidate: &Path) -> std::io::Result<Option<PathBuf>> {
    let root = match config.dictation_dir.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(candidate
        .canonicalize()
        .ok()
        .filter(|resolved| resolved.starts_with(&root)))
}

fn new_file(config: &Nvim) -> Result<PathBuf> {
    fs::create_dir_all(&config.dictation_dir).with_context(|| {
        format!(
            "create dictation directory {}",
            config.dictation_dir.display()
        )
    })?;
    let base = chrono::Local::now()
        .format(&config.file_template)
        .to_string();
    let original = Path::new(&base);
    let stem = original
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(&base);
    let extension = original.extension().and_then(|value| value.to_str());
    for collision in 0_u32..10_000 {
        let name = if collision == 0 {
            base.clone()
        } else if let Some(extension) = extension {
            format!("{stem}-{collision}.{extension}")
        } else {
            format!("{stem}-{collision}")
        };
        let path = config.dictation_dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", path.display()));
            }
        }
    }
    bail!("could not allocate a collision-free dictation filename")
}

fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path must be UTF-8: {}", path.display()))
}

pub(crate) fn marker_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".owner");
    PathBuf::from(name)
}

pub(crate) fn new_marker() -> Result<String> {
    let mut bytes = [0_u8; 32];
    File::open("/dev/urandom")
        .context("open system random source")?
        .read_exact(&mut bytes)
        .context("read ownership marker")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn read_marker(path: &Path) -> Result<Option<String>> {
    let marker = match fs::read_to_string(path) {
        Ok(marker) => marker,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    ensure!(
        marker.len() == 64 && marker.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid nvim ownership marker at {}",
        path.display()
    );
    Ok(Some(marker))
}

/// An ownership marker on disk, removed again unless the editor it names
/// actually started.
///
/// The marker is how a later `ensure` recognises an editor as spokenpad's, so
/// one left behind by a spawn that failed names an editor that never existed.
pub struct OwnershipMarker {
    value: String,
    path: PathBuf,
    keep: bool,
}

impl OwnershipMarker {
    fn write(socket: &Path) -> Result<Self> {
        if let Some(parent) = socket.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory {}", parent.display()))?;
        }
        let value = new_marker()?;
        let path = marker_path(socket);
        write_marker(&path, &value)?;
        Ok(Self {
            value,
            path,
            keep: false,
        })
    }

    fn value(&self) -> &str {
        &self.value
    }

    /// The editor this marker names is answering: leave the file in place.
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for OwnershipMarker {
    fn drop(&mut self) {
        if !self.keep
            && let Err(error) = fs::remove_file(&self.path)
        {
            log::debug!("could not remove {}: {error}", self.path.display());
        }
    }
}

pub(crate) fn write_marker(path: &Path, marker: &str) -> Result<()> {
    let parent = path.parent().context("ownership marker has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    temporary.write_all(marker.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("persist ownership marker {}", path.display()))?;
    Ok(())
}

/// The command that starts the Neovim behind a pane, and the ownership marker
/// that will say it is spokenpad's.
///
/// The marker is written here and removed again when it is dropped, unless
/// [`OwnershipMarker::keep`] says the editor came up. One left behind names an
/// editor that never existed; nothing reads it while no socket is there, but a
/// file that claims something untrue should not outlive the attempt that
/// wrote it. The two are returned separately so that taking the command
/// cannot quietly drop the marker with it.
///
/// `--embed` is what turns the editor's stdin and stdout into the UI channel
/// the pane draws from; everything else is the argv every other spokenpad
/// editor gets, so an `NvimSession` recognises it as spokenpad's.
pub fn pane_launch(config: &Nvim, target: &Path) -> Result<(Command, OwnershipMarker)> {
    let marker = OwnershipMarker::write(&config.socket_path)?;
    let init = resolve_init(config)?;
    let mut argv = editor_argv(config, target, marker.value(), init.as_deref())?;
    // After the configured command, so an `nvim.editor` with arguments of its
    // own keeps them.
    argv.insert(config.editor.len(), "--embed".to_owned());
    let (program, arguments) = argv.split_first().context("empty nvim command")?;
    let mut command = Command::new(program);
    command.args(arguments);
    if let Some(display) = &config.display {
        // The editor is told which display it is on, rather than left to
        // inherit one. It shares the window's, which is what its own
        // clipboard provider needs to reach the right selection — and the
        // daemon's inherited environment need not name the same one.
        command.env("DISPLAY", display);
    }
    Ok((command, marker))
}

/// Where a pane should open: the monitor under the pointer, and the pointer.
/// The pane sizes itself from `nvim.pane_dimensions` once it knows its font.
///
/// The X connection this asks over is opened and closed here rather than
/// handed to the pane: the pane opens its own, and one short-lived connection
/// per window is cheaper than threading one through. Returns the display name
/// too, so the pane opens on the one that was measured, and the window manager
/// that runs it.
fn pane_target(config: &Nvim) -> Result<(place::Target, String, x11::Manager)> {
    use crate::shell::pane::xkb;

    let display = config.display.clone().context(
        "the dictation pane needs an X display, and $DISPLAY was not set when \
         spokenpad started. On Wayland without Xwayland, set nvim.mode = \"attach\" \
         and run `spokenpad editor` in a terminal; with Xwayland, import DISPLAY into \
         the user manager (`systemctl --user import-environment DISPLAY`)",
    )?;
    xkb::load()?;
    let name =
        std::ffi::CString::new(display.clone()).context("the display name contains a NUL")?;
    let (connection, screen) = x11rb::xcb_ffi::XCBConnection::connect(Some(&name))
        .with_context(|| format!("connect to the X display {display}"))?;
    Ok((
        place::target(&connection, screen)?,
        display,
        x11::manager(&connection, screen),
    ))
}

/// sway focuses every window it maps unless a `no_focus` rule matches it,
/// whatever the window says about itself. So when sway runs the display the
/// pane opens on, spokenpad adds that rule for the pane's own `WM_CLASS`
/// before the map, and refuses to open where sway would focus it anyway: as
/// the first window on a workspace.
///
/// sway is recognised by the display, not by the environment: its Xwayland
/// window manager calls itself [`x11::WLROOTS_WM`], the X server names the
/// process that runs it, and that process must be `sway`. A `$SWAYSOCK` that was
/// never imported, or is left over from another session, must not be what
/// decides whether a window takes the focus. sway's socket is looked for
/// where the sway running the display puts it — `sway-ipc.<uid>.<pid>.sock`
/// in the runtime directory, `<pid>` being the process the display names as
/// its window manager — and then at `$SWAYSOCK`; either counts only if the
/// process listening on it is that sway. The pane opens only when one of
/// them answers and takes the rule. Another wlroots compositor (labwc,
/// Wayfire, river) looks the same from the display and is refused, saying it
/// is not sway: its focus behaviour is not verified. A display that does not
/// name its window manager's process is refused too.
fn refuse_pane_focus_on_sway(config: &Nvim, manager: &x11::Manager) -> Result<()> {
    use crate::core::wm::{Property, WmKind};
    use std::os::unix::fs::MetadataExt as _;

    if manager.name.as_deref() != Some(x11::WLROOTS_WM) {
        return Ok(());
    }
    // Everything below fails closed: a window that might take the focus is
    // not opened.
    let pid = manager.pid.context(
        "the pane cannot open: the X display is run by a wlroots compositor, and the X \
         server does not say which process, so spokenpad cannot tell whether it is sway \
         and cannot keep the pane unfocused. Set nvim.mode = \"attach\"",
    )?;
    let program = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_owned())
        .with_context(|| format!("read which program process {pid}, running the display, is"))?;
    ensure!(
        program == "sway",
        "the pane cannot open: the X display is run by {program}, a wlroots compositor that \
         is not sway. It focuses new windows in ways spokenpad has not verified, and has no \
         rule spokenpad can add, so spokenpad cannot keep the pane unfocused there. Set \
         nvim.mode = \"attach\""
    );
    // sway names its socket by its own pid and uid, which owns the runtime
    // directory; reading the owner needs no `getuid`. `$SWAYSOCK` counts only
    // when the process listening on it is this sway.
    let discovered = config.runtime_dir.as_ref().and_then(|dir| {
        let uid = std::fs::metadata(dir).ok()?.uid();
        Some(dir.join(format!("sway-ipc.{uid}.{pid}.sock")))
    });
    let sway = discovered
        .iter()
        .chain(config.sway_socket.iter())
        .find_map(|socket| {
            Wm::of_process(socket.clone(), pid)
                .inspect_err(|error| log::debug!("not this display's sway: {error:#}"))
                .ok()
                .filter(|wm| wm.kind() == WmKind::Sway)
        });
    let Some(sway) = sway else {
        let socket = match &config.sway_socket {
            None => "$SWAYSOCK is not set".to_owned(),
            Some(socket) => format!(
                "$SWAYSOCK names {}, which is not the socket of this sway (process {pid}): \
                 left from an earlier session, or another sway's",
                socket.display()
            ),
        };
        bail!(
            "the pane cannot open: sway runs the X display and focuses every new window unless \
             spokenpad tells it not to over sway's IPC socket, and this sway's socket was not \
             found ({socket}). Import SWAYSOCK into the user manager (`exec systemctl --user \
             import-environment SWAYSOCK` in the sway config), or set nvim.mode = \"attach\""
        );
    };
    let criteria = [
        Criterion::new(Property::Instance, x11::INSTANCE)?,
        Criterion::new(Property::Class, x11::CLASS)?,
    ];
    sway.refuse_focus(&criteria).context(
        "the pane cannot open without taking the focus under sway; \
         nvim.mode = \"attach\" works everywhere",
    )
}

/// `nvim.init` as the path nvim is given, writing out the bundled one.
pub(crate) fn resolve_init(config: &Nvim) -> Result<Option<PathBuf>> {
    Ok(match config.init.as_deref() {
        Some(path) if path == Path::new("bundled") => Some(materialize_bundled_init()?),
        Some(path) => Some(path.to_owned()),
        None => None,
    })
}

/// `spokenpad editor`: replaces this process with the dictation editor, in
/// whatever terminal it was run from, listening on the dictation socket.
///
/// It opens the pending passage when dictation went to a file while no
/// editor was open, and a new dictation file otherwise. A pending passage is
/// taken: until the daemon attaches, what it writes goes to a new passage,
/// never into the file this editor already shows. It writes the
/// ownership marker the way a spawn does, so the daemon adopts it on the next
/// key-down. Returns only on failure: a live editor already on the socket, or
/// an editor that could not be executed.
pub fn open_editor(config: &Nvim) -> Result<Infallible> {
    // If the editor never starts, the guard removes a new file again or
    // makes a taken passage pending again.
    let (_passage, mut command) = editor_command(config)?;
    let error = command.exec();
    Err(error).context("start the dictation editor")
}

/// Everything `spokenpad editor` does before it becomes the editor: the
/// socket is free, the file is chosen, the marker is written.
fn editor_command(config: &Nvim) -> Result<(NewFileGuard, Command)> {
    match fs::symlink_metadata(&config.socket_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect nvim socket path"),
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket(),
                "refusing non-socket path at {}",
                config.socket_path.display()
            );
            match RpcClient::connect(&config.socket_path, deadline(PROBE_TIMEOUT)) {
                Err(error) if error.is_stale_socket() => remove_stale_socket(&config.socket_path)?,
                _ => bail!(
                    "a dictation editor is already open on {}",
                    config.socket_path.display()
                ),
            }
        }
    }
    let passage = NewFileGuard::take(config)?;
    if let Some(parent) = config.socket_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    let marker = new_marker()?;
    write_marker(&marker_path(&config.socket_path), &marker)?;
    let init = resolve_init(config)?;
    let argv = editor_argv(config, passage.path(), &marker, init.as_deref())?;
    let (program, arguments) = argv.split_first().context("empty nvim command")?;
    let mut command = Command::new(program);
    command.args(arguments);
    Ok((passage, command))
}

fn materialize_bundled_init() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let directory = config::state_dir().join("private");
    if !directory.exists() {
        fs::DirBuilder::new().mode(0o700).create(&directory)?;
    }
    let path = directory.join("dictation_init.lua");
    if fs::read_to_string(&path).ok().as_deref() == Some(BUNDLED_INIT) {
        return Ok(path);
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    temporary.write_all(BUNDLED_INIT.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .context("persist bundled nvim init")?;
    Ok(path)
}

#[cfg(test)]
mod tests;
