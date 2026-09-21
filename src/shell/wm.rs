//! The window manager, spoken to over its own IPC socket.
//!
//! i3 and sway share one protocol (see [`core::wm`](crate::core::wm)); this
//! module finds the socket, sends one request per connection under an
//! absolute deadline, and reads what spokenpad needs to open a dictation
//! window: which window manager it is, the outputs, the configuration that
//! must prove the `no_focus` rule, and the tree to find the window in. The
//! only subprocesses left are `xdotool` for the pointer on X11 and
//! `i3 --get-socketpath` when no socket is named in the environment.
use crate::core::{
    geometry::{Output, Rect},
    wm::{self, Criterion, Message, WmKind},
};
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
    /// The running i3 or sway. `$SWAYSOCK` is read before `$I3SOCK` (sway sets
    /// both), and `i3 --get-socketpath` is the last resort, since a daemon
    /// started by systemd may not inherit either variable.
    pub fn connect() -> Result<Self> {
        let named = ["SWAYSOCK", "I3SOCK"]
            .into_iter()
            .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
            .map(PathBuf::from);
        let socket = match named {
            Some(socket) => socket,
            None => PathBuf::from(
                run(&["i3", "--get-socketpath"])
                    .context("no $SWAYSOCK or $I3SOCK, and `i3 --get-socketpath` failed")?
                    .trim(),
            ),
        };
        Self::at(socket)
    }

    /// The window manager answering on `socket`.
    pub fn at(socket: PathBuf) -> Result<Self> {
        let reply = request(&socket, Message::GetVersion, "")?;
        let kind = wm::parse_version(&reply)?;
        Ok(Self { socket, kind })
    }

    pub fn kind(&self) -> WmKind {
        self.kind
    }

    /// The monitor layout, queried afresh. Once per window spawn, so a monitor
    /// plugged in a moment ago is on the list.
    pub fn outputs(&self) -> Result<Vec<Output>> {
        wm::parse_outputs(&request(&self.socket, Message::GetOutputs, "")?)
    }

    /// The pointer, where it can be read. On i3 that is X11 and `xdotool`
    /// knows; under sway no protocol lets a client ask (an Xwayland `xdotool`
    /// sees the pointer only while it is over an Xwayland window), so the
    /// placement falls back to the focused output instead.
    pub fn pointer(&self) -> Option<(i32, i32)> {
        match self.kind() {
            WmKind::I3 => pointer_from_xdotool(),
            WmKind::Sway => None,
        }
    }

    /// The first of `criteria` the loaded configuration does not prove a
    /// `no_focus` rule for, or `None` when every one is proven. Only the text
    /// `GET_CONFIG` returns counts — nothing is read from disk — so on sway,
    /// which returns its main file alone, a rule must be in that file.
    pub fn unproven_no_focus<'a>(
        &self,
        criteria: &'a [Criterion],
    ) -> Result<Option<&'a Criterion>> {
        let sources = wm::parse_config(&request(&self.socket, Message::GetConfig, "")?)?;
        Ok(criteria.iter().find(|criterion| {
            !wm::has_no_focus_rule(sources.iter().map(String::as_str), criterion)
        }))
    }

    /// The first of `criteria` that selects a window now on screen.
    pub fn find<'a>(&self, criteria: &'a [Criterion]) -> Result<Option<&'a Criterion>> {
        wm::find_window(&request(&self.socket, Message::GetTree, "")?, criteria)
    }

    /// The id of the focused node.
    pub fn focused_node(&self) -> Result<Option<i64>> {
        wm::focused_node(&request(&self.socket, Message::GetTree, "")?)
    }

    /// Floats, sizes and places the window `criterion` selects. A criteria
    /// command acts on the matched window and never moves focus.
    pub fn place(&self, criterion: &Criterion, rect: Rect) -> Result<()> {
        let command = wm::placement_command(self.kind(), criterion, rect);
        wm::check_command_reply(&request(&self.socket, Message::RunCommand, &command)?)
    }
}

/// One request on a fresh connection, under one absolute deadline: a window
/// manager that stops answering must not hold the editor thread, which is
/// opening a window while the user is already speaking.
fn request(socket: &Path, message: Message, payload: &str) -> Result<String> {
    let deadline = Instant::now() + TIMEOUT;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .context("window manager did not answer in time")
    };
    let mut stream = UnixStream::connect(socket)
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

fn pointer_from_xdotool() -> Option<(i32, i32)> {
    let text = run(&["xdotool", "getmouselocation", "--shell"]).ok()?;
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|raw| raw.parse().ok())
    };
    Some((value("X=")?, value("Y=")?))
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

    #[test]
    fn i3_is_asked_for_outputs_config_and_tree_over_its_socket() {
        let fake = FakeWm::start(vec![
            (
                Message::GetVersion,
                json!({"major": 4, "minor": 25, "patch": 1}).to_string(),
            ),
            (
                Message::GetOutputs,
                json!([
                    {"name": "DP-1", "active": true, "primary": true, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}},
                    {"name": "xroot-0", "active": false, "rect": {"x": 0, "y": 0, "width": 3840, "height": 1080}}
                ])
                .to_string(),
            ),
            (
                Message::GetConfig,
                json!({"config": "include i3.d/*.conf\n", "included_configs": [
                    {"path": "/x/i3.d/spokenpad.conf", "raw_contents": "no_focus [instance=\"spokenpad\"]\n"}
                ]})
                .to_string(),
            ),
            (
                Message::GetTree,
                json!({"id": 1, "nodes": [], "floating_nodes": [
                    {"id": 9, "focused": false, "window_properties": {"instance": "spokenpad"}}
                ]})
                .to_string(),
            ),
            (Message::RunCommand, json!([{"success": true}]).to_string()),
        ]);
        let wm = Wm::at(fake.socket.clone()).unwrap();
        assert_eq!(wm.kind(), WmKind::I3);
        let outputs = wm.outputs().unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].primary);

        let instance = [criterion(Property::Instance, "spokenpad")];
        assert_eq!(wm.unproven_no_focus(&instance).unwrap(), None);
        let other = [criterion(Property::Instance, "other")];
        assert_eq!(wm.unproven_no_focus(&other).unwrap(), Some(&other[0]));

        let found = wm
            .find(&instance)
            .unwrap()
            .expect("the window is on screen");
        let rect = Rect {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        wm.place(found, rect).unwrap();
        assert_eq!(
            *fake.commands.lock().unwrap(),
            [r#"[instance="spokenpad"] floating enable, resize set 300 200, move position 10 20"#]
        );
        assert_eq!(wm.focused_node().unwrap(), None);
    }

    /// sway's `GET_CONFIG` is the main file as loaded, without its includes.
    /// A rule that is only in an included file on disk may never have been
    /// loaded (linked in since the last reload), so it proves nothing.
    #[test]
    fn sway_rules_count_only_in_the_config_sway_returned() {
        let configs = tempfile::tempdir().unwrap();
        let main = configs.path().join("config");
        std::fs::create_dir(configs.path().join("config.d")).unwrap();
        std::fs::write(
            configs.path().join("config.d/50-spokenpad.conf"),
            "no_focus [app_id=\"spokenpad\"]\nno_focus [instance=\"spokenpad\"]\n",
        )
        .unwrap();
        let main_text = "include config.d/*\nno_focus [instance=\"spokenpad\"]\n";
        std::fs::write(&main, main_text).unwrap();
        let fake = FakeWm::start(vec![
            (
                Message::GetVersion,
                json!({"variant": "sway", "major": 1, "loaded_config_file_name": main}).to_string(),
            ),
            (Message::GetConfig, json!({"config": main_text}).to_string()),
            (
                Message::GetTree,
                json!({"id": 1, "nodes": [{"id": 4, "focused": true, "app_id": "spokenpad"}]})
                    .to_string(),
            ),
            (
                Message::RunCommand,
                json!([{"success": false, "error": "No matching node"}]).to_string(),
            ),
        ]);
        let wm = Wm::at(fake.socket.clone()).unwrap();
        assert_eq!(wm.kind(), WmKind::Sway);
        let both = [
            criterion(Property::AppId, "spokenpad"),
            criterion(Property::Instance, "spokenpad"),
        ];
        assert_eq!(wm.unproven_no_focus(&both).unwrap(), Some(&both[0]));
        assert_eq!(wm.unproven_no_focus(&both[1..]).unwrap(), None);

        assert_eq!(wm.find(&both).unwrap(), Some(&both[0]));
        assert_eq!(wm.focused_node().unwrap(), Some(4));
        let error = wm
            .place(
                &both[0],
                Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("No matching node"), "{error}");
        assert_eq!(
            *fake.commands.lock().unwrap(),
            [r#"[app_id="spokenpad"] floating enable, resize set 1 1, move absolute position 0 0"#]
        );
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
