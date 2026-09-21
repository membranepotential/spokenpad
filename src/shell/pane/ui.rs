//! The Neovim the pane embeds, and the UI channel to it.
//!
//! `nvim --embed` turns the process's stdin and stdout into a msgpack-RPC
//! channel. spokenpad attaches to it as a UI with `nvim_ui_attach`, which
//! makes Neovim describe its whole display as `redraw` notifications — the
//! events [`core::grid`](crate::core::grid) types — and lets the pane send
//! keys and mouse clicks back.
//!
//! That channel is only the *drawing*. Committed text still reaches this same
//! editor the way it reaches every other one: over the Unix socket its
//! `--listen` opened, through the daemon's own append path. The pane never
//! writes to the buffer.
//!
//! A thread reads the channel, because the redraw stream arrives whenever
//! Neovim has something to say rather than when the pane asks. It decodes and
//! forwards, so the pane's loop can block on one receiver and use no CPU while
//! nothing happens.
use crate::{core::grid::RedrawEvent, shell::nvim::rpc, shell::pane::Event};
use anyhow::{Context, Result, bail};
use rmpv::Value;
use std::{
    io::{BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::Sender,
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// What the editor said.
#[derive(Debug)]
pub enum FromEditor {
    /// One frame's worth of redraw events, already typed.
    Redraw(Vec<RedrawEvent>),
    /// The answer to a request the pane sent.
    Response {
        id: u64,
        /// `Nil` when the call succeeded.
        error: Value,
        result: Value,
    },
    /// The channel ended: Neovim exited, or its stdout closed.
    Gone,
}

/// How long to wait for a Neovim that was asked to quit before killing it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The embedded Neovim process and the writing half of its UI channel.
pub struct Editor {
    process: Child,
    stdin: Option<ChildStdin>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Editor {
    /// Start `command` with `--embed` already in its argv, and begin reading
    /// its side of the channel into `events`.
    pub fn spawn(mut command: Command, events: Sender<Event>) -> Result<Self> {
        let mut process = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start the embedded nvim")?;
        let stdin = process.stdin.take().context("nvim gave no stdin")?;
        let stdout = process.stdout.take().context("nvim gave no stdout")?;
        let reader = std::thread::Builder::new()
            .name("spokenpad-pane-ui".to_owned())
            .spawn(move || read_channel(stdout, &events))
            .context("start the pane's UI reader thread")?;
        Ok(Self {
            process,
            stdin: Some(stdin),
            reader: Some(reader),
            next_id: 1,
        })
    }

    /// Ask Neovim to describe a grid of this size, and return the request id
    /// its answer will carry.
    ///
    /// `ext_linegrid` is the only extension asked for: with it Neovim reports
    /// one grid of cells, and it draws the command line and its messages into
    /// that grid rather than expecting the UI to render them.
    pub fn attach(&mut self, columns: u16, rows: u16) -> Result<u64> {
        self.request(
            "nvim_ui_attach",
            vec![
                Value::from(columns),
                Value::from(rows),
                Value::Map(vec![
                    (Value::from("ext_linegrid"), Value::from(true)),
                    (Value::from("rgb"), Value::from(true)),
                ]),
            ],
        )
    }

    /// Keys, in the notation [`core::keys`](crate::core::keys) produces.
    pub fn input(&mut self, keys: &str) -> Result<()> {
        self.notify("nvim_input", vec![Value::from(keys)])
    }

    /// A mouse button or a wheel step at a grid position.
    ///
    /// `button` is `"left"`, `"right"`, `"middle"` or `"wheel"`; `action` is
    /// `"press"`, `"drag"`, `"release"`, or a direction for the wheel.
    pub fn input_mouse(
        &mut self,
        button: &str,
        action: &str,
        modifiers: &str,
        row: u16,
        column: u16,
    ) -> Result<()> {
        self.notify(
            "nvim_input_mouse",
            vec![
                Value::from(button),
                Value::from(action),
                Value::from(modifiers),
                // Grid 0 means "the grid the UI is showing", which with
                // `ext_linegrid` and no multigrid is the only one there is.
                Value::from(0),
                Value::from(row),
                Value::from(column),
            ],
        )
    }

    pub fn try_resize(&mut self, columns: u16, rows: u16) -> Result<()> {
        self.notify(
            "nvim_ui_try_resize",
            vec![Value::from(columns), Value::from(rows)],
        )
    }

    /// Tell Neovim whether the window has the keyboard focus, so it can fire
    /// `FocusGained`/`FocusLost` and stop blinking the cursor.
    pub fn set_focus(&mut self, focused: bool) -> Result<()> {
        self.notify("nvim_ui_set_focus", vec![Value::from(focused)])
    }

    pub fn request(&mut self, method: &str, arguments: Vec<Value>) -> Result<u64> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send(rpc::Message::Request {
            id,
            method: method.to_owned(),
            arguments,
        })?;
        Ok(id)
    }

    pub fn notify(&mut self, method: &str, arguments: Vec<Value>) -> Result<()> {
        self.send(rpc::Message::Notification {
            method: method.to_owned(),
            arguments,
        })
    }

    fn send(&mut self, message: rpc::Message) -> Result<()> {
        let bytes = message.encode()?;
        let stdin = self
            .stdin
            .as_mut()
            .context("the pane's channel to nvim is closed")?;
        stdin.write_all(&bytes).context("write to nvim")?;
        stdin.flush().context("flush the channel to nvim")
    }

    /// Ask Neovim to quit, wait for it, and kill it if it will not.
    ///
    /// Closing stdin is the polite half: `--embed` treats the channel closing
    /// as the UI detaching, and Neovim exits once its last UI is gone.
    pub fn shutdown(&mut self) {
        let _ = self.notify("nvim_command", vec![Value::from("qall!")]);
        self.stdin = None;
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            match self.process.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Editor {
    fn drop(&mut self) {
        if self.stdin.is_some() || matches!(self.process.try_wait(), Ok(None)) {
            self.shutdown();
        }
    }
}

/// Decode Neovim's side of the channel until it ends.
///
/// One message at a time, each bounded by the same depth and byte limits the
/// socket transport uses. Anything that is not a `redraw` notification or a
/// response is logged and dropped: this UI asks for no extension that would
/// make Neovim send it a request.
fn read_channel(stdout: std::process::ChildStdout, events: &Sender<Event>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let message = match read_message(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                log::warn!("pane UI channel: {error:#}");
                break;
            }
        };
        let event = match message {
            rpc::Message::Notification { method, arguments } if method == "redraw" => {
                FromEditor::Redraw(RedrawEvent::parse_batch(&arguments))
            }
            rpc::Message::Response { id, error, result } => {
                FromEditor::Response { id, error, result }
            }
            rpc::Message::Notification { method, .. } => {
                log::debug!("pane UI: ignoring notification {method}");
                continue;
            }
            rpc::Message::Request { id, method, .. } => {
                log::debug!("pane UI: ignoring request {id} for {method}");
                continue;
            }
        };
        if events.send(Event::Editor(event)).is_err() {
            return;
        }
    }
    let _ = events.send(Event::Editor(FromEditor::Gone));
}

/// The next message, or `None` at a clean end of the channel.
fn read_message(reader: &mut BufReader<std::process::ChildStdout>) -> Result<Option<rpc::Message>> {
    let mut budget = Budget {
        reader,
        remaining: rpc::RPC_MAX_BYTES,
    };
    let value = match rmpv::decode::read_value_with_max_depth(&mut budget, rpc::RPC_MAX_DEPTH) {
        Ok(value) => value,
        Err(rmpv::decode::Error::InvalidMarkerRead(error))
            if error.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            return Ok(None);
        }
        Err(error) => bail!("decode a message from nvim: {error}"),
    };
    match rpc::Message::parse(&value) {
        Some(message) => Ok(Some(message)),
        None => bail!("nvim sent something that is not an RPC message"),
    }
}

/// A reader that refuses to hand the decoder more than one message's budget,
/// so a broken peer announcing a huge array cannot exhaust memory.
struct Budget<'a> {
    reader: &'a mut BufReader<std::process::ChildStdout>,
    remaining: usize,
}

impl std::io::Read for Budget<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "one nvim message exceeded the byte budget",
            ));
        }
        let capacity = buffer.len().min(self.remaining);
        let count = std::io::Read::read(self.reader, &mut buffer[..capacity])?;
        self.remaining -= count;
        Ok(count)
    }
}
