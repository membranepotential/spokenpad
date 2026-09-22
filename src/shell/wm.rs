//! The window manager, spoken to over its own IPC socket.
//!
//! i3 and sway share one protocol (see [`core::wm`](crate::core::wm)); this
//! module sends one request per connection under an absolute deadline. It
//! has one caller: a pane about to open under sway, which reads none of the
//! properties that keep the pane unfocused elsewhere, adds its own
//! `no_focus` rule here first. It also runs short-lived helpers, such as
//! `notify-send`, bounded in time and output.
use crate::core::wm::{self, Criterion, Message, WmKind};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{ErrorKind, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
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

    /// Makes sway refuse focus to a window matching all of `criteria`, opened
    /// now, or says why it cannot.
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
    pub fn refuse_focus(&self, criteria: &[Criterion]) -> Result<()> {
        ensure!(
            self.kind == WmKind::Sway,
            "{} cannot add a `no_focus` rule while it runs",
            self.kind
        );
        let command = wm::no_focus_command(criteria)?;
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
    let remaining = remaining_until(Instant::now() + TIMEOUT);
    let mut stream = connect_by(socket, &remaining)
        .with_context(|| format!("connect to window manager at {}", socket.display()))?;
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
    let remaining = remaining_until(Instant::now() + TIMEOUT);
    let stream = connect_by(socket, &remaining)
        .with_context(|| format!("connect to window manager at {}", socket.display()))?;
    let credentials = peer_credentials(&stream).context("read the socket's peer credentials")?;
    u32::try_from(credentials.pid).context("the socket's peer has no process id")
}

/// The process, user and group on the other end of a connected Unix socket
/// (`SO_PEERCRED`: for the side that listens, as it was at `listen`).
pub(crate) fn peer_credentials(stream: &UnixStream) -> std::io::Result<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` is a live `ucred` of `length` bytes, which is
    // what SO_PEERCRED writes, and `stream` is open for the whole call.
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast::<libc::c_void>(),
            &raw mut length,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(credentials)
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

/// `UnixStream::connect` under the deadline.
///
/// A blocking connect to a Unix socket waits for as long as the listener's
/// accept queue is full, which a window manager that has stopped accepting
/// leaves it forever. So the socket is non-blocking while it connects: a
/// full queue answers `EAGAIN`, and the connect is retried until the
/// deadline; the stream is blocking again (with per-call timeouts) after.
fn connect_by(path: &Path, remaining: &impl Fn() -> Result<Duration>) -> Result<UnixStream> {
    use std::os::{
        fd::{FromRawFd as _, OwnedFd},
        unix::ffi::OsStrExt as _,
    };

    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero `sockaddr_un` is a valid value of the type; the
    // family and path are filled in below.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(
        bytes.len() < address.sun_path.len(),
        "socket path is longer than a Unix socket address can hold"
    );
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: `socket` takes no pointers.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create a Unix socket");
    }
    // SAFETY: `fd` was just returned by `socket` and nothing else owns it.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    loop {
        // SAFETY: `address` is an initialised `sockaddr_un` at least `length`
        // bytes long, and `socket` is open for the duration of the call.
        let connected = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                length,
            )
        };
        if connected == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            // The listener's queue is full: it has stopped accepting, or is
            // slow to. Try again until the deadline says otherwise.
            Some(libc::EAGAIN) => {
                std::thread::sleep(remaining()?.min(Duration::from_millis(5)));
            }
            _ => return Err(error.into()),
        }
    }
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
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

/// A short-lived helper process, bounded in time and output.
pub(crate) fn run(args: &[&str]) -> Result<String> {
    run_bounded(args, TIMEOUT, MAX_OUTPUT_BYTES)
}

fn run_bounded(args: &[&str], timeout: Duration, max_output: usize) -> Result<String> {
    let (program, tail) = args.split_first().context("empty subprocess command")?;
    let mut child = Command::new(program)
        .args(tail)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {program}"))?;
    let mut stdout = child.stdout.take().context("capture subprocess stdout")?;
    set_nonblocking(stdout.as_raw_fd()).context("make subprocess stdout nonblocking")?;
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut status: Option<ExitStatus> = None;
    let mut eof = false;
    while status.is_none() || !eof {
        drain_output(&mut stdout, &mut bytes, max_output, &mut eof).inspect_err(|_| {
            terminate_group(&mut child);
        })?;
        if status.is_none() {
            status = child.try_wait().context("poll subprocess")?;
        }
        if status.is_some() && eof {
            break;
        }
        if Instant::now() >= deadline {
            terminate_group(&mut child);
            bail!("{program} timed out after {:.3}s", timeout.as_secs_f64());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.context("subprocess ended without an exit status")?;
    if !status.success() {
        bail!("{program} exited with {status}");
    }
    String::from_utf8(bytes).context("subprocess emitted non-UTF-8 output")
}

fn set_nonblocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fd` is a live pipe descriptor and F_GETFL/F_SETFL do not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; the flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn drain_output(
    stdout: &mut impl Read,
    bytes: &mut Vec<u8>,
    max_output: usize,
    eof: &mut bool,
) -> Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                *eof = true;
                return Ok(());
            }
            Ok(count) => {
                if bytes.len().saturating_add(count) > max_output {
                    bail!("subprocess output exceeded {max_output} bytes");
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("read subprocess stdout"),
        }
    }
}

fn terminate_group(child: &mut Child) {
    let process_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
    // SAFETY: a negative PID targets only the child-created process group.
    let _ = unsafe { libc::kill(-process_group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::wm::Property;
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

    fn criterion(property: Property, value: &str) -> Criterion {
        Criterion::new(property, value).unwrap()
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
        let criteria = [
            criterion(Property::Instance, "spokenpad-pane"),
            criterion(Property::Class, "spokenpad-pane"),
        ];
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
            let result = Wm::at(fake.socket.clone()).unwrap().refuse_focus(&criteria);
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
            .refuse_focus(&criteria)
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
    /// deadline.
    #[test]
    fn a_full_accept_queue_times_out_instead_of_hanging() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("ipc.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let mut queued = Vec::new();
        let error = loop {
            let remaining = remaining_until(Instant::now() + Duration::from_millis(200));
            let started = Instant::now();
            match connect_by(&socket, &remaining) {
                Ok(stream) => queued.push(stream),
                Err(error) => {
                    assert!(started.elapsed() < Duration::from_secs(1));
                    break error.to_string();
                }
            }
            assert!(queued.len() < 10_000, "the accept queue never filled");
        };
        assert!(error.contains("did not answer in time"), "{error}");
    }

    #[test]
    fn bounded_runner_rejects_excess_output() {
        let error = run_bounded(&["sh", "-c", "printf 123456789"], Duration::from_secs(1), 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeded 8 bytes"), "{error}");
    }

    #[test]
    fn bounded_runner_does_not_wait_on_a_descendants_pipe() {
        let started = Instant::now();
        let error = run_bounded(
            &["sh", "-c", "(sleep 5) &"],
            Duration::from_millis(100),
            1024,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
