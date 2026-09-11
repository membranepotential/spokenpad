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
mod rpc;

use crate::{
    config::{self, Nvim},
    core::{
        geometry::{Rect, pick_output, placement},
        session::NoticeText,
        state::IndicatorPhase,
    },
    shell::x11,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rmpv::Value;
use rpc::{RpcClient, RpcFailure};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{FileTypeExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
const MAP_TIMEOUT: Duration = Duration::from_secs(3);

const SPOKENPAD_LUA: &str = include_str!("../../lua/spokenpad.lua");
const BUNDLED_INIT: &str = include_str!("../../lua/dictation_init.lua");

/// Everything an attaching session needs to know about an editor answering on
/// the dictation socket: who owns it, whether its startup has finished, and
/// which buffer — if any — a previous session pinned in it.
const OWNERSHIP_QUERY: &str = r#"
local marker = vim.g.spokenpad_owner
local ready = vim.g.spokenpad_startup_ready
local buf = _G.Spokenpad and _G.Spokenpad.buf or nil
if not (buf and vim.api.nvim_buf_is_valid(buf) and vim.api.nvim_buf_is_loaded(buf)) then
  buf = nil
end
return {
  marker or vim.NIL,
  ready or vim.NIL,
  buf or vim.NIL,
  buf and vim.api.nvim_buf_get_name(buf) or vim.NIL,
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
    process: Option<Child>,
    session_nonce: Option<String>,
    append_sequence: u64,
}

impl NvimSession {
    pub fn new(config: Nvim) -> Self {
        Self {
            config,
            connection: None,
            process: None,
            session_nonce: None,
            append_sequence: 0,
        }
    }

    pub fn connected(&self) -> bool {
        self.connection.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.connection.as_ref().map(|open| open.path.as_path())
    }

    /// Returns the pinned dictation path, reattaching or spawning as needed.
    pub fn ensure(&mut self) -> Result<PathBuf> {
        if let Some(open) = self.connection.as_mut() {
            if still_pinned(open) {
                return Ok(open.path.clone());
            }
            self.drop_connection();
        }

        if self.attach_existing()? || self.spawn_and_attach()? {
            let open = self
                .connection
                .as_ref()
                .context("nvim connected without a dictation path")?;
            return Ok(open.path.clone());
        }
        bail!("could not connect to the dictation nvim")
    }

    /// Appends literal text and does not return until Neovim confirms its save.
    pub fn append(&mut self, text: &str, continued: bool) -> Result<usize> {
        ensure!(self.connection.is_some(), "not connected to nvim");
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
        let arguments = vec![
            Value::from(operation),
            Value::from(text),
            Value::from(continued),
        ];

        let value = match self.request_append(arguments.clone()) {
            Ok(value) => value,
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
            );
        }
        self.drop_connection();
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
        let open = self
            .connection
            .as_mut()
            .ok_or_else(|| RpcFailure::Other(anyhow!("not connected to nvim")))?;
        open.client.request(
            "nvim_exec_lua",
            vec![
                Value::from("return Spokenpad.append_once(...)"),
                Value::Array(arguments),
            ],
            deadline(APPEND_TIMEOUT),
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

        client.request(
            "nvim_exec_lua",
            vec![Value::from(SPOKENPAD_LUA), Value::Array(Vec::new())],
            deadline(SETUP_TIMEOUT),
        )?;
        let mut fresh = None;
        let (buffer, path) = match adoptable {
            Some(adopted) => adopted,
            None => {
                let file = fresh.insert(NewFileGuard::create(&self.config)?);
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

    fn adoptable_buffer(&self, pinned: Option<&Pinned>) -> Result<Option<(i64, PathBuf)>> {
        let Some(pinned) = pinned else {
            return Ok(None);
        };
        let root = match self.config.dictation_dir.canonicalize() {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("canonicalize dictation directory"),
        };
        let candidate = match Path::new(&pinned.name).canonicalize() {
            Ok(candidate) => candidate,
            Err(_) => return Ok(None),
        };
        Ok(candidate
            .starts_with(&root)
            .then(|| (pinned.buffer, PathBuf::from(&pinned.name))))
    }

    fn spawn_and_attach(&mut self) -> Result<bool> {
        if self.config.terminal.is_empty() {
            ensure!(
                self.config
                    .editor
                    .iter()
                    .any(|argument| argument == "--headless"),
                "terminal=[] is safe only with an editor command that explicitly contains --headless"
            );
        } else {
            ensure!(
                alacritty_declares_instance(&self.config.terminal),
                "graphical nvim requires alacritty --class GENERAL,{{instance}} so its X11 instance is provable before mapping"
            );
            ensure!(
                x11::has_no_focus_rule(&self.config.window_instance),
                "active i3 configuration does not prove a no_focus rule for instance {:?}",
                self.config.window_instance
            );
        }
        // Only a window manager can be asked where the window should go, and
        // only a graphical editor has one. On the headless path these are two
        // subprocess round trips (xrandr, xdotool) that answer nothing.
        let window_rect = (!self.config.terminal.is_empty())
            .then(|| self.window_placement())
            .flatten();

        let mut fresh = NewFileGuard::create(&self.config)?;
        if let Some(parent) = self.config.socket_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory {}", parent.display()))?;
        }
        let marker = new_marker()?;
        write_marker(&marker_path(&self.config.socket_path), &marker)?;
        let init = match self.config.init.as_deref() {
            Some(path) if path == Path::new("bundled") => Some(materialize_bundled_init()?),
            Some(path) => Some(path.to_owned()),
            None => None,
        };
        let argv = spawn_argv(
            &self.config,
            fresh.path(),
            &marker,
            window_rect,
            init.as_deref(),
        )?;
        let (program, arguments) = argv.split_first().context("empty nvim command")?;
        let mut command = Command::new(program);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(test)]
        if self.config.terminal.is_empty() {
            let log = self
                .config
                .socket_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("nvim.log");
            command.env("NVIM_LOG_FILE", log);
        }
        // SAFETY: this closure calls only the async-signal-safe `setsid` between
        // fork and exec, and does not capture or allocate.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("start {program}"))?;
        let mut editor = SpawnedEditorGuard::new(child);
        self.reap_replaced_process();

        let deadline = Instant::now() + Duration::from_secs_f64(self.config.startup_timeout_s);
        if let Some(rect) = window_rect {
            self.place_when_mapped(&mut editor, rect, deadline);
        }
        let mut last_error = None;
        // A healthy client is handed back to the next attempt: an editor that
        // is merely still initializing answers the next probe on the same
        // connection, so only a connection that actually broke is remade.
        let mut client = None;
        while Instant::now() < deadline {
            if editor.exited() {
                bail!("dictation editor exited during startup");
            }
            if self.config.socket_path.exists() {
                // Everything in here is transient until the outer deadline:
                // an editor still loading a plugin manager answers late, half
                // ready, or not at all, and treating that as a hard failure
                // used to SIGKILL a perfectly healthy editor mid-startup.
                match self.connect_spawned(client.take(), fresh.path(), &marker, deadline) {
                    Ok(Readiness::Ready(open)) => {
                        self.connection = Some(open);
                        self.process = editor.release();
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
        let ownership = query_ownership(&mut client, capped(startup_deadline, PROBE_TIMEOUT))?;
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
        client.request(
            "nvim_exec_lua",
            vec![Value::from(SPOKENPAD_LUA), Value::Array(Vec::new())],
            capped(startup_deadline, SETUP_TIMEOUT),
        )?;
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

    fn place_when_mapped(
        &self,
        editor: &mut SpawnedEditorGuard,
        rect: Rect,
        startup_deadline: Instant,
    ) {
        let deadline = (Instant::now() + MAP_TIMEOUT).min(startup_deadline);
        while Instant::now() < deadline {
            if editor.exited() {
                return;
            }
            if x11::i3_window_exists(&self.config.window_instance) {
                let _ = x11::place_window(&self.config.window_instance, rect);
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn window_placement(&self) -> Option<Rect> {
        let outputs = x11::outputs();
        let pointer = x11::pointer_position();
        let anchor = pointer.map(|(x, y)| Rect {
            x,
            y,
            width: 1,
            height: 1,
        });
        let output = pick_output(
            &outputs,
            anchor.unwrap_or(Rect {
                x: i32::MAX,
                y: i32::MAX,
                width: 1,
                height: 1,
            }),
        )?;
        Some(placement(output, pointer, self.config.window_fraction))
    }

    fn reap_replaced_process(&mut self) {
        if let Some(mut process) = self.process.take()
            && process.try_wait().ok().flatten().is_none()
        {
            std::thread::spawn(move || {
                let _ = process.wait();
            });
        }
    }
}

impl Drop for NvimSession {
    fn drop(&mut self) {
        self.reap_replaced_process();
    }
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pinned {
    buffer: i64,
    name: String,
}

fn parse_ownership(value: &Value) -> Result<Ownership> {
    let fields = value
        .as_array()
        .filter(|fields| fields.len() == 4)
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
    let name = text(&fields[3])?;
    let pinned = match fields[2] {
        Value::Nil => None,
        ref handle => Some(Pinned {
            buffer: handle
                .as_i64()
                .context("nvim returned a non-integer buffer handle")?,
            name: name.context("nvim reported a pinned buffer with no file name")?,
        }),
    };
    Ok(Ownership {
        marker: text(&fields[0])?,
        ready: text(&fields[1])?,
        pinned,
    })
}

/// The argv for a fresh editor. Pure: the caller materializes `init` and
/// resolves the window rect, so what is left is a total function of the
/// configuration and can be read — and tested — as one.
fn spawn_argv(
    config: &Nvim,
    target: &Path,
    marker: &str,
    rect: Option<Rect>,
    init: Option<&Path>,
) -> Result<Vec<String>> {
    let (x, y) = rect.map_or((0, 0), |rect| (rect.x, rect.y));
    let substitute = |argument: &str| {
        argument
            .replace("{instance}", &config.window_instance)
            .replace("{x}", &x.to_string())
            .replace("{y}", &y.to_string())
    };
    let mut argv: Vec<String> = config.terminal.iter().map(|arg| substitute(arg)).collect();
    argv.extend(config.editor.iter().cloned());
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

fn alacritty_declares_instance(arguments: &[String]) -> bool {
    let Some(program) = arguments
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        .and_then(|argument| argument.to_str())
    else {
        return false;
    };
    let Some((command, options)) = arguments.split_last() else {
        return false;
    };
    // The editor is appended after this template. A class flag inside the
    // child command, a second class override, an alternative config file that
    // could carry one, or embedding in another window all make the identity of
    // the window we are about to open unprovable.
    if program != "alacritty"
        || !matches!(command.as_str(), "-e" | "--command")
        || options.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "-e" | "--command" | "--embed" | "--config-file"
            ) || argument.starts_with("--class=")
                || argument.starts_with("--embed=")
                || argument.starts_with("--config-file=")
                || argument.contains("window.class")
        })
    {
        return false;
    }
    let mut classes = options.windows(2).filter(|pair| pair[0] == "--class");
    classes.next().is_some_and(|pair| {
        pair[1]
            .split_once(',')
            .is_some_and(|(general, instance)| !general.is_empty() && instance == "{instance}")
    }) && classes.next().is_none()
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

fn deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

fn capped(deadline: Instant, timeout: Duration) -> Instant {
    (Instant::now() + timeout).min(deadline)
}

/// Kills a spawned editor, and its process group, unless startup succeeded.
///
/// The guard owns the `Child`: a bare `waitpid` behind `Child`'s back would
/// race the wait `Child` performs itself and could reap a recycled pid.
struct SpawnedEditorGuard(Option<Child>);

impl SpawnedEditorGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn exited(&mut self) -> bool {
        self.0
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_some())
    }

    /// Hands the editor over to a caller that intends to keep it running.
    fn release(&mut self) -> Option<Child> {
        self.0.take()
    }
}

impl Drop for SpawnedEditorGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let group = i32::try_from(child.id()).unwrap_or(i32::MAX);
        // SAFETY: spawned editors call setsid, so the child's pid is also its
        // process-group id, and a negative pid signals that group alone. The
        // child has not been reaped yet, so its pid cannot have been recycled.
        let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Removes a freshly created dictation file unless a session pins it.
///
/// The file is created before the editor is started, so that its name is
/// settled and its permissions are ours from the first byte. A spawn that then
/// failed used to leave a zero-byte file behind on every attempt. Anything
/// with content in it is left alone: that is the user's transcript.
struct NewFileGuard {
    path: PathBuf,
    keep: bool,
}

impl NewFileGuard {
    fn create(config: &Nvim) -> Result<Self> {
        Ok(Self {
            path: new_file(config)?,
            keep: false,
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
        if fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() == 0) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn still_pinned(open: &mut Connected) -> bool {
    // A bare `nvim_eval "1"` proves only that the editor answers. After the
    // user `:bdelete`s the dictation buffer it answers exactly as before, and
    // every append of that utterance failed against a buffer that was gone.
    query_ownership(&mut open.client, deadline(PROBE_TIMEOUT)).is_ok_and(|ownership| {
        ownership
            .pinned
            .is_some_and(|pin| pin.buffer == open.buffer)
    })
}

enum Probe {
    /// Nothing listens on the socket path.
    Stale,
    Live(RpcClient, Ownership),
}

fn query_ownership(client: &mut RpcClient, deadline: Instant) -> Result<Ownership> {
    let value = client.request(
        "nvim_exec_lua",
        vec![Value::from(OWNERSHIP_QUERY), Value::Array(Vec::new())],
        deadline,
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

fn open_buffer(client: &mut RpcClient, path: &Path, deadline: Instant) -> Result<i64> {
    client
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(OPEN_BUFFER),
                Value::Array(vec![Value::from(utf8_path(path)?)]),
            ],
            deadline,
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
    )?;
    Ok(())
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

fn marker_path(socket: &Path) -> PathBuf {
    let mut name = socket.as_os_str().to_owned();
    name.push(".owner");
    PathBuf::from(name)
}

fn new_marker() -> Result<String> {
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

fn write_marker(path: &Path, marker: &str) -> Result<()> {
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
