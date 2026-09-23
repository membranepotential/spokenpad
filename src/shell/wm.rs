//! The window manager, spoken to over its own IPC socket.
//!
//! i3 and sway share one protocol (see [`core::wm`](crate::core::wm)); this
//! module sends one request per connection under an absolute deadline. It
//! has one caller: a pane about to open under sway, which reads none of the
//! properties that keep the pane unfocused elsewhere, adds its own
//! `no_focus` rule here first.
use crate::{
    core::wm::{self, Message, WmKind},
    shell::unix_socket::{self, ConnectError},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
/// A tree of a few hundred windows is well under a megabyte; this only stops
/// a peer announcing a length that would exhaust memory.
const MAX_REPLY_BYTES: usize = 64 * 1024 * 1024;

/// The window manager answering on one IPC socket, and what it said it is.
pub struct Wm {
    socket: PathBuf,
    kind: WmKind,
}

impl Wm {
    /// The window manager answering on `socket`.
    pub fn at(socket: PathBuf) -> Result<Self> {
        let reply = request(&socket, Message::GetVersion, "")?;
        let kind = wm::parse_version(&reply)?;
        Ok(Self { socket, kind })
    }

    /// The window manager answering on `socket`, if and only if the process
    /// listening there is `pid`: the kernel's peer credentials of the
    /// connection name the listener, so a socket of another instance — a
    /// second sway, or one left over from an earlier session and reused —
    /// is refused rather than trusted.
    pub fn of_process(socket: PathBuf, pid: u32) -> Result<Self> {
        let listener = peer_pid(&socket)?;
        ensure!(
            listener == pid,
            "{} belongs to process {listener}, not to {pid}",
            socket.display()
        );
        Self::at(socket)
    }

    pub fn kind(&self) -> WmKind {
        self.kind
    }

    /// Makes sway refuse focus to the pane, opened now, or says why it
    /// cannot.
    ///
    /// sway focuses every window it maps on the focused workspace unless a
    /// `no_focus` rule matches it (`should_focus` in `sway/tree/view.c`), and
    /// reads neither `_NET_WM_USER_TIME` nor the window type. So this adds
    /// that rule over IPC — never to the user's configuration file — and
    /// requires sway's reply to say it took it. `GET_CONFIG` cannot show a
    /// rule added this way; `tests/pane_focus_wms.rs` proves that sway honours
    /// it. The first window on an empty workspace is focused whatever the
    /// rules say, so that is refused too.
    ///
    /// i3 accepts `no_focus` only in its configuration, so this is sway's
    /// alone.
    pub fn refuse_pane_focus(&self) -> Result<()> {
        ensure!(
            self.kind == WmKind::Sway,
            "{} cannot add a `no_focus` rule while it runs",
            self.kind
        );
        let command = wm::pane_no_focus_command();
        wm::check_command_reply(&request(&self.socket, Message::RunCommand, &command)?)
            .with_context(|| format!("sway refused `{command}`"))?;
        self.ensure_workspace_holds_a_window()
    }

    /// sway gives the first window on a workspace the focus whatever
    /// `no_focus` says, and a new window lands on the focused one.
    fn ensure_workspace_holds_a_window(&self) -> Result<()> {
        let empty = wm::focused_workspace_is_empty(&request(&self.socket, Message::GetTree, "")?)
            .context("cannot tell which workspace the window would open on")?;
        ensure!(
            !empty,
            "the focused workspace is empty, and {} gives the first window on a workspace focus despite `no_focus`",
            self.kind
        );
        Ok(())
    }
}

/// One request on a fresh connection, under one absolute deadline: a window
/// manager that stops answering must not hold the editor thread, which is
/// opening a pane while the user is already speaking.
fn request(socket: &Path, message: Message, payload: &str) -> Result<String> {
    let deadline = Instant::now() + TIMEOUT;
    let remaining = remaining_until(deadline);
    let mut stream = connect(socket, deadline)?;
    stream.set_write_timeout(Some(remaining()?))?;
    stream
        .write_all(&wm::encode(message, payload)?)
        .context("send IPC request")?;
    let mut header = [0_u8; wm::HEADER_LEN];
    read_exact_by(&mut stream, &mut header, &remaining)?;
    let length = wm::reply_length(&header, message)?;
    ensure!(
        length <= MAX_REPLY_BYTES,
        "IPC reply of {length} bytes exceeds {MAX_REPLY_BYTES}"
    );
    let mut body = vec![0_u8; length];
    read_exact_by(&mut stream, &mut body, &remaining)?;
    String::from_utf8(body).context("IPC reply is not UTF-8")
}

/// The process listening on `socket`, from the connection's peer
/// credentials (`SO_PEERCRED`, which the kernel fills in at `listen`).
fn peer_pid(socket: &Path) -> Result<u32> {
    let stream = connect(socket, Instant::now() + TIMEOUT)?;
    let credentials =
        unix_socket::peer_credentials(&stream).context("read the socket's peer credentials")?;
    u32::try_from(credentials.pid).context("the socket's peer has no process id")
}

/// A connection to the window manager's socket, made before `deadline`: one
/// that has stopped accepting leaves its accept queue full, and a blocking
/// connect would wait for it forever.
fn connect(socket: &Path, deadline: Instant) -> Result<UnixStream> {
    unix_socket::connect(socket, deadline)
        .map_err(|failure| match failure {
            ConnectError::TimedOut => anyhow!("window manager did not answer in time"),
            ConnectError::Refused(error) => error.into(),
            ConnectError::Other(error) => error,
        })
        .with_context(|| format!("connect to window manager at {}", socket.display()))
}

/// How much of the time until `deadline` is left, for each step of one
/// exchange; an error once none is.
fn remaining_until(deadline: Instant) -> impl Fn() -> Result<Duration> {
    move || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .context("window manager did not answer in time")
    }
}

/// `read_exact`, with the socket timeout shrunk to what is left of the
/// deadline before every read, so a peer dribbling bytes cannot extend it.
fn read_exact_by(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    remaining: &impl Fn() -> Result<Duration>,
) -> Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        stream.set_read_timeout(Some(remaining()?))?;
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => bail!("window manager closed the IPC connection mid-reply"),
            Ok(count) => filled += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                bail!("window manager did not answer in time")
            }
            Err(error) => return Err(error).context("read IPC reply"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::unix_socket::FullQueue;
    use serde_json::json;
    use std::{
        os::unix::net::UnixListener,
        sync::{Arc, Mutex},
        thread::{self, JoinHandle},
    };

    /// A window manager that answers each request type with a canned reply
    /// and records every command it was asked to run. It serves until the
    /// test drops it, one request per connection, as spokenpad sends them.
    struct FakeWm {
        _directory: tempfile::TempDir,
        socket: PathBuf,
        commands: Arc<Mutex<Vec<String>>>,
        _server: JoinHandle<()>,
    }

    impl FakeWm {
        fn start(replies: Vec<(Message, String)>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("ipc.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&commands);
            let server = thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut header = [0_u8; wm::HEADER_LEN];
                    if stream.read_exact(&mut header).is_err() {
                        continue;
                    }
                    let length = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
                    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
                    let mut payload = vec![0_u8; length];
                    stream.read_exact(&mut payload).unwrap();
                    let Some((message, reply)) =
                        replies.iter().find(|(message, _)| *message as u32 == kind)
                    else {
                        continue;
                    };
                    if *message == Message::RunCommand {
                        recorded
                            .lock()
                            .unwrap()
                            .push(String::from_utf8(payload).unwrap());
                    }
                    let _ = stream.write_all(&wm::encode(*message, reply).unwrap());
                }
            });
            Self {
                _directory: directory,
                socket,
                commands,
                _server: server,
            }
        }
    }

    /// The pane's rule goes to sway over IPC, anchored, and counts only when
    /// sway says it took it and the window would not be the first on its
    /// workspace. i3 cannot take a rule at runtime, so it is refused there.
    #[test]
    fn sway_is_given_a_runtime_no_focus_rule_and_i3_is_not_asked() {
        let occupied = json!({"id": 1, "type": "root", "nodes": [
            {"id": 2, "type": "workspace", "focused": false, "nodes": [
                {"id": 4, "type": "con", "focused": true, "pid": 4242, "app_id": "foot"}
            ]}
        ]});
        let empty = json!({"id": 1, "type": "root", "nodes": [
            {"id": 2, "type": "workspace", "focused": true, "nodes": [], "floating_nodes": []}
        ]});
        let unfocused = json!({"id": 1, "type": "root", "nodes": []});
        let sway = json!({"variant": "sway", "major": 1}).to_string();
        let rule = r#"no_focus [instance="^spokenpad-pane$" class="^spokenpad-pane$"]"#;
        for (tree, accepted, expected) in [
            (&occupied, true, None),
            (&empty, true, Some("the focused workspace is empty")),
            (&unfocused, true, Some("cannot tell which workspace")),
            (&occupied, false, Some("sway refused `no_focus")),
        ] {
            let reply = json!([if accepted {
                json!({"success": true})
            } else {
                json!({"success": false, "error": "Token 'x' is not recognized"})
            }]);
            let fake = FakeWm::start(vec![
                (Message::GetVersion, sway.clone()),
                (Message::GetTree, tree.to_string()),
                (Message::RunCommand, reply.to_string()),
            ]);
            let result = Wm::at(fake.socket.clone()).unwrap().refuse_pane_focus();
            match expected {
                None => result.unwrap(),
                Some(expected) => {
                    let error = format!("{:#}", result.unwrap_err());
                    assert!(error.contains(expected), "{error}");
                }
            }
            assert_eq!(*fake.commands.lock().unwrap(), [rule]);
        }

        let fake = FakeWm::start(vec![
            (Message::GetVersion, json!({"major": 4}).to_string()),
            (Message::RunCommand, json!([{"success": true}]).to_string()),
        ]);
        let error = Wm::at(fake.socket.clone())
            .unwrap()
            .refuse_pane_focus()
            .unwrap_err()
            .to_string();
        assert!(error.contains("i3 cannot add"), "{error}");
        assert!(fake.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn a_silent_window_manager_times_out_instead_of_hanging() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ipc.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let _held = thread::spawn(move || {
            let accepted = listener.accept();
            thread::sleep(Duration::from_secs(5));
            drop(accepted);
        });
        let started = Instant::now();
        let error = Wm::at(socket).err().expect("no reply").to_string();
        assert!(error.contains("did not answer in time"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    /// A socket counts only for the process that listens on it: the fake
    /// window manager listens in this test's process.
    #[test]
    fn a_socket_counts_only_for_the_process_listening_on_it() {
        let fake = FakeWm::start(vec![(
            Message::GetVersion,
            json!({"variant": "sway", "major": 1}).to_string(),
        )]);
        let wm = Wm::of_process(fake.socket.clone(), std::process::id()).unwrap();
        assert_eq!(wm.kind(), WmKind::Sway);
        let error = format!(
            "{:#}",
            Wm::of_process(fake.socket.clone(), std::process::id() + 1)
                .err()
                .expect("another process's socket")
        );
        assert!(error.contains("belongs to process"), "{error}");
    }

    /// A window manager that has stopped accepting fills its accept queue;
    /// a blocking connect would then wait forever. This one gives up at the
    /// deadline, and says the window manager did not answer.
    #[test]
    fn a_full_accept_queue_times_out_instead_of_hanging() {
        let full = FullQueue::new();
        let started = Instant::now();
        let error = connect(&full.socket, Instant::now() + Duration::from_millis(200))
            .expect_err("the queue is full");
        assert!(started.elapsed() < Duration::from_secs(1));
        let error = format!("{error:#}");
        assert!(error.contains("did not answer in time"), "{error}");
    }
}
