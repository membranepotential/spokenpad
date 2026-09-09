//! A dedicated Neovim sink, spoken to directly over msgpack-RPC.
use crate::{
    config::{self, Nvim},
    geometry::{Rect, pick_output, placement},
    x11,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rmpv::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, OpenOptionsExt},
            net::UnixStream,
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_POLL: Duration = Duration::from_millis(25);
const MAP_TIMEOUT: Duration = Duration::from_secs(3);
const RPC_MAX_DEPTH: usize = 32;
const RPC_MAX_BYTES: usize = 8 * 1024 * 1024;
const INDICATOR: &str = include_str!("lua/nvim_indicator.lua");
const RUST_EXTENSION: &str = include_str!("lua/nvim_rust.lua");
const BUNDLED_INIT: &str = include_str!("lua/dictation_init.lua");

const OWNERSHIP_QUERY: &str = r#"
local marker = vim.g.spokenpad_owner
local buf = _G.Spokenpad and _G.Spokenpad.buf or nil
if buf and vim.api.nvim_buf_is_valid(buf) and vim.api.nvim_buf_is_loaded(buf) then
  return { marker or vim.NIL, buf, vim.api.nvim_buf_get_name(buf) }
end
return { marker or vim.NIL, vim.NIL, vim.NIL }
"#;

const OPEN_BUFFER: &str = r#"
vim.cmd.edit(vim.fn.fnameescape(...))
return vim.api.nvim_get_current_buf()
"#;

const SPAWN_READINESS_QUERY: &str = r#"
local marker = vim.g.spokenpad_owner
local ready = vim.g.spokenpad_startup_ready
return { marker or vim.NIL, ready or vim.NIL }
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorPhase {
    Idle,
    Recording,
    Transcribing,
}

impl IndicatorPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Recording => "recording",
            Self::Transcribing => "transcribing",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IndicatorUpdate<'a> {
    pub phase: Option<IndicatorPhase>,
    pub level: Option<f64>,
    pub preview: Option<&'a str>,
    pub latched: Option<bool>,
    pub previewing: Option<bool>,
}

pub struct NvimSession {
    config: Nvim,
    client: Option<RpcClient>,
    buffer: Option<i64>,
    path: Option<PathBuf>,
    process: Option<Child>,
    marker: Option<String>,
    session_nonce: Option<String>,
    append_sequence: u64,
}

impl NvimSession {
    pub fn new(config: Nvim) -> Self {
        Self {
            config,
            client: None,
            buffer: None,
            path: None,
            process: None,
            marker: None,
            session_nonce: None,
            append_sequence: 0,
        }
    }

    pub fn connected(&self) -> bool {
        self.client.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns the pinned dictation path, reattaching or spawning as needed.
    pub fn ensure(&mut self) -> Result<PathBuf> {
        if let Some(client) = self.client.as_mut() {
            if client
                .request("nvim_eval", vec![Value::from("1")], RPC_TIMEOUT)
                .is_ok()
            {
                return self
                    .path
                    .clone()
                    .context("connected nvim has no pinned dictation path");
            }
            self.drop_connection();
        }

        if self.attach_existing()? || self.spawn_and_attach()? {
            return self
                .path
                .clone()
                .context("nvim connected without a dictation path");
        }
        bail!("could not connect to the dictation nvim")
    }

    /// Appends literal text and does not return until Neovim confirms its save.
    pub fn append(&mut self, text: &str, continued: bool) -> Result<usize> {
        ensure!(
            self.client.is_some() && self.buffer.is_some(),
            "not connected to nvim"
        );
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
        let args = vec![
            Value::from(operation.clone()),
            Value::from(text),
            Value::from(continued),
        ];

        let first = self.client.as_mut().expect("checked above").request(
            "nvim_exec_lua",
            vec![
                Value::from("return Spokenpad.append_once(...)"),
                Value::Array(args.clone()),
            ],
            RPC_TIMEOUT,
        );
        let value = match first {
            Ok(value) => value,
            Err(RpcFailure::Timeout(reason)) => {
                // The request may have completed after its reply was lost. Reconnect
                // and repeat the same operation ID; the Lua side returns its cached
                // result instead of appending twice.
                self.drop_connection();
                ensure!(
                    self.attach_existing()?,
                    "append outcome is unknown after timeout: {reason}"
                );
                self.client.as_mut().expect("attached above").request(
                    "nvim_exec_lua",
                    vec![
                        Value::from("return Spokenpad.append_once(...)"),
                        Value::Array(args),
                    ],
                    RPC_TIMEOUT,
                )?
            }
            Err(error) => {
                self.drop_connection();
                return Err(error.into());
            }
        };
        let count = value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
            .context("nvim append returned a non-integer line count")?;
        usize::try_from(count).context("nvim line count does not fit usize")
    }

    /// Sends a cosmetic update as a notification; transport failures surface.
    pub fn set_state(&mut self, update: IndicatorUpdate<'_>) -> Result<()> {
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        let mut fields = Vec::new();
        if let Some(phase) = update.phase {
            fields.push((Value::from("phase"), Value::from(phase.as_str())));
        }
        if let Some(level) = update.level {
            let level = if level.is_finite() {
                level.clamp(0.0, 1.0)
            } else {
                0.0
            };
            fields.push((Value::from("level"), Value::F64(level)));
        }
        if let Some(preview) = update.preview {
            fields.push((Value::from("preview"), Value::from(preview)));
        }
        if let Some(latched) = update.latched {
            fields.push((Value::from("latched"), Value::from(latched)));
        }
        if let Some(previewing) = update.previewing {
            fields.push((Value::from("previewing"), Value::from(previewing)));
        }
        if fields.is_empty() {
            return Ok(());
        }
        let result = client.notify(
            "nvim_exec_lua",
            vec![
                Value::from("Spokenpad.set_state(...)"),
                Value::Array(vec![Value::Map(fields)]),
            ],
            RPC_TIMEOUT,
        );
        if result.is_err() {
            self.drop_connection();
        }
        result.map_err(Into::into)
    }

    /// Detaches without killing the editor or closing the user's passage.
    pub fn close(&mut self) {
        self.drop_connection();
    }

    pub fn detach(&mut self) {
        self.close();
    }

    fn drop_connection(&mut self) {
        self.client = None;
        self.buffer = None;
        self.path = None;
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
        let mut client = match RpcClient::connect(&self.config.socket_path, RPC_TIMEOUT) {
            Ok(client) => client,
            Err(error) if error.is_stale_socket() => {
                remove_stale_socket(&self.config.socket_path)?;
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let ownership = client.request(
            "nvim_exec_lua",
            vec![Value::from(OWNERSHIP_QUERY), Value::Array(Vec::new())],
            RPC_TIMEOUT,
        )?;
        let details = ownership
            .as_array()
            .filter(|values| values.len() == 3)
            .context("nvim ownership query returned malformed data")?;
        let expected_marker = read_marker(&marker_path(&self.config.socket_path))?;
        let marker_matches = expected_marker
            .as_deref()
            .is_some_and(|expected| details[0].as_str().is_some_and(|actual| actual == expected));
        let adoptable = self.adoptable_buffer(&details[1], &details[2])?;
        ensure!(
            marker_matches || adoptable.is_some(),
            "refusing unrelated nvim socket {}",
            self.config.socket_path.display()
        );

        let source = indicator_source()?;
        client.request(
            "nvim_exec_lua",
            vec![Value::from(source), Value::Array(Vec::new())],
            RPC_TIMEOUT,
        )?;
        let (buffer, path) = if let Some(adopted) = adoptable {
            adopted
        } else {
            let target = self.new_file()?;
            let buffer = open_buffer(&mut client, &target)?;
            (buffer, target)
        };
        setup_buffer(&mut client, buffer)?;
        self.marker = expected_marker.or_else(|| details[0].as_str().map(str::to_owned));
        self.client = Some(client);
        self.buffer = Some(buffer);
        self.path = Some(path);
        Ok(true)
    }

    fn adoptable_buffer(&self, buffer: &Value, path: &Value) -> Result<Option<(i64, PathBuf)>> {
        let (Some(buffer), Some(raw_path)) = (buffer.as_i64(), path.as_str()) else {
            return Ok(None);
        };
        let root = match self.config.dictation_dir.canonicalize() {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("canonicalize dictation directory"),
        };
        let candidate = match Path::new(raw_path).canonicalize() {
            Ok(candidate) => candidate,
            Err(_) => return Ok(None),
        };
        Ok(candidate
            .starts_with(&root)
            .then_some((buffer, PathBuf::from(raw_path))))
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

        let target = self.new_file()?;
        if let Some(parent) = self.config.socket_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create socket directory {}", parent.display()))?;
        }
        let marker = new_marker()?;
        write_marker(&marker_path(&self.config.socket_path), &marker)?;
        let window_rect = self.window_placement();
        let argv = self.spawn_argv(&target, &marker, window_rect)?;
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
        let mut startup_guard = ProcessGroupKillGuard::new(child.id());
        self.reap_replaced_process();
        self.process = Some(child);

        let deadline = Instant::now() + Duration::from_secs_f64(self.config.startup_timeout_s);
        if let Some(rect) = window_rect.filter(|_| !self.config.terminal.is_empty()) {
            self.place_when_mapped(rect, deadline);
        }
        let mut last_error = None;
        while Instant::now() < deadline {
            if self
                .process
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_some()
            {
                bail!("dictation editor exited during startup");
            }
            if self.config.socket_path.exists() {
                match RpcClient::connect_until(&self.config.socket_path, deadline) {
                    Ok(mut client) => {
                        match client.request_until("nvim_eval", vec![Value::from("1")], deadline) {
                            Ok(_) => {
                                let readiness = client.request_until(
                                    "nvim_exec_lua",
                                    vec![
                                        Value::from(SPAWN_READINESS_QUERY),
                                        Value::Array(Vec::new()),
                                    ],
                                    deadline,
                                )?;
                                let details = readiness
                                    .as_array()
                                    .filter(|values| values.len() == 2)
                                    .context(
                                        "spawned nvim readiness query returned malformed data",
                                    )?;
                                if details[0].is_nil() {
                                    last_error = Some(
                                        "spawned nvim has not initialized its ownership marker"
                                            .to_owned(),
                                    );
                                    std::thread::sleep(CONNECT_POLL);
                                    continue;
                                }
                                let actual_marker = details[0].as_str().context(
                                    "spawned nvim returned a malformed ownership marker",
                                )?;
                                ensure!(
                                    actual_marker == marker,
                                    "spawned nvim reported a different ownership marker"
                                );
                                if details[1].as_str() != Some(marker.as_str()) {
                                    last_error = Some(
                                        "spawned nvim user configuration is still initializing"
                                            .to_owned(),
                                    );
                                    std::thread::sleep(CONNECT_POLL);
                                    continue;
                                }
                                client.request_until(
                                    "nvim_exec_lua",
                                    vec![
                                        Value::from(indicator_source()?),
                                        Value::Array(Vec::new()),
                                    ],
                                    deadline,
                                )?;
                                let buffer = open_buffer_until(&mut client, &target, deadline)?;
                                setup_buffer_until(&mut client, buffer, deadline)?;
                                self.client = Some(client);
                                self.buffer = Some(buffer);
                                self.path = Some(target);
                                self.marker = Some(marker);
                                startup_guard.disarm();
                                return Ok(true);
                            }
                            Err(error) => last_error = Some(error.to_string()),
                        }
                    }
                    Err(error) => last_error = Some(error.to_string()),
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

    fn place_when_mapped(&mut self, rect: Rect, startup_deadline: Instant) {
        let deadline = (Instant::now() + MAP_TIMEOUT).min(startup_deadline);
        while Instant::now() < deadline {
            if self
                .process
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_some()
            {
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

    fn spawn_argv(&self, target: &Path, marker: &str, rect: Option<Rect>) -> Result<Vec<String>> {
        let (x, y) = rect.map_or((0, 0), |rect| (rect.x, rect.y));
        let substitute = |argument: &str| {
            argument
                .replace("{instance}", &self.config.window_instance)
                .replace("{x}", &x.to_string())
                .replace("{y}", &y.to_string())
        };
        let mut argv: Vec<String> = self
            .config
            .terminal
            .iter()
            .map(|arg| substitute(arg))
            .collect();
        argv.extend(self.config.editor.iter().cloned());
        if let Some(init) = &self.config.init {
            let init = if init == Path::new("bundled") {
                materialize_bundled_init()?
            } else {
                init.clone()
            };
            argv.extend(["-u".to_owned(), utf8_path(&init)?.to_owned()]);
        }
        if let Some(colorscheme) = &self.config.colorscheme {
            let name = serde_json::to_string(colorscheme)?;
            let opaque = !self.config.transparent;
            argv.extend([
                "-c".to_owned(),
                format!("lua if _G.SpokenpadColorscheme then SpokenpadColorscheme({name}, {opaque}) else pcall(vim.cmd.colorscheme, {name}) end"),
            ]);
        }
        argv.extend([
            "--cmd".to_owned(),
            format!("let g:spokenpad_owner = '{marker}'"),
            "--listen".to_owned(),
            utf8_path(&self.config.socket_path)?.to_owned(),
            "-c".to_owned(),
            format!(
                "lua vim.g.spokenpad_startup_ready = nil; vim.api.nvim_create_autocmd('VimEnter', {{ once = true, callback = function() vim.g.spokenpad_startup_ready = '{marker}' end }})"
            ),
            utf8_path(target)?.to_owned(),
        ]);
        Ok(argv)
    }

    fn new_file(&self) -> Result<PathBuf> {
        fs::create_dir_all(&self.config.dictation_dir).with_context(|| {
            format!(
                "create dictation directory {}",
                self.config.dictation_dir.display()
            )
        })?;
        let base = chrono::Local::now()
            .format(&self.config.file_template)
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
            let path = self.config.dictation_dir.join(name);
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
    // child command, a second class override, or embedding in another window
    // cannot prove the identity of the window we are about to open.
    if program != "alacritty"
        || !matches!(command.as_str(), "-e" | "--command")
        || options.iter().any(|argument| {
            matches!(argument.as_str(), "-e" | "--command" | "--embed")
                || argument.starts_with("--class=")
                || argument.starts_with("--embed=")
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

impl Drop for NvimSession {
    fn drop(&mut self) {
        self.reap_replaced_process();
    }
}

struct ProcessGroupKillGuard {
    process_group: libc::pid_t,
    armed: bool,
}

impl ProcessGroupKillGuard {
    fn new(pid: u32) -> Self {
        Self {
            process_group: pid as libc::pid_t,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: spawned editors call setsid, making their pid their process-group id.
            let _ = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
            let mut status = 0;
            // SAFETY: waits only for the exact child pid and writes to this local status.
            let _ = unsafe { libc::waitpid(self.process_group, &mut status, 0) };
        }
    }
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

fn open_buffer(client: &mut RpcClient, path: &Path) -> Result<i64> {
    open_buffer_until(client, path, Instant::now() + RPC_TIMEOUT)
}

fn open_buffer_until(client: &mut RpcClient, path: &Path, deadline: Instant) -> Result<i64> {
    client
        .request_until(
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

fn setup_buffer(client: &mut RpcClient, buffer: i64) -> Result<()> {
    setup_buffer_until(client, buffer, Instant::now() + RPC_TIMEOUT)
}

fn setup_buffer_until(client: &mut RpcClient, buffer: i64, deadline: Instant) -> Result<()> {
    client.request_until(
        "nvim_exec_lua",
        vec![
            Value::from("Spokenpad.setup(...)"),
            Value::Array(vec![Value::from(buffer)]),
        ],
        deadline,
    )?;
    Ok(())
}

fn indicator_source() -> Result<String> {
    let write_hook = "vim.cmd(\"silent! noautocmd write\")";
    ensure!(
        INDICATOR.matches(write_hook).count() == 1,
        "indicator write hook changed unexpectedly"
    );
    let write_checked = INDICATOR.replacen(write_hook, "vim.cmd(\"silent noautocmd write\")", 1);
    let footer = "_G.Spokenpad = M\nreturn true";
    ensure!(
        write_checked.matches(footer).count() == 1,
        "indicator module footer changed unexpectedly"
    );
    let body = write_checked.replacen(footer, RUST_EXTENSION, 1);
    Ok(format!("local previous = _G.Spokenpad\n{body}"))
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

struct RpcClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

#[derive(Debug)]
enum RpcFailure {
    Timeout(String),
    StaleSocket(std::io::Error),
    Other(anyhow::Error),
}

impl RpcFailure {
    fn is_stale_socket(&self) -> bool {
        matches!(
            self,
            Self::StaleSocket(error)
                if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
        )
    }
}

impl std::fmt::Display for RpcFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout(message) => formatter.write_str(message),
            Self::StaleSocket(error) => error.fmt(formatter),
            Self::Other(error) => error.fmt(formatter),
        }
    }
}

impl From<RpcFailure> for anyhow::Error {
    fn from(value: RpcFailure) -> Self {
        match value {
            RpcFailure::Timeout(message) => anyhow!(message),
            RpcFailure::StaleSocket(error) => error.into(),
            RpcFailure::Other(error) => error,
        }
    }
}

impl RpcClient {
    fn connect(path: &Path, timeout: Duration) -> std::result::Result<Self, RpcFailure> {
        Self::connect_until(path, Instant::now() + timeout)
    }

    fn connect_until(path: &Path, deadline: Instant) -> std::result::Result<Self, RpcFailure> {
        let writer = connect_unix_until(path, deadline)?;
        let reader = BufReader::new(
            writer
                .try_clone()
                .map_err(|error| RpcFailure::Other(error.into()))?,
        );
        Ok(Self {
            reader,
            writer,
            next_id: 1,
        })
    }

    fn request(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        timeout: Duration,
    ) -> std::result::Result<Value, RpcFailure> {
        self.request_until(method, arguments, Instant::now() + timeout)
    }

    fn request_until(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        deadline: Instant,
    ) -> std::result::Result<Value, RpcFailure> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.send_until(
            Value::Array(vec![
                Value::from(0),
                Value::from(id),
                Value::from(method),
                Value::Array(arguments),
            ]),
            deadline,
        )?;
        loop {
            let mut reader = DeadlineRead::new(&mut self.reader, deadline, RPC_MAX_BYTES);
            let message = rmpv::decode::read_value_with_max_depth(&mut reader, RPC_MAX_DEPTH)
                .map_err(|error| {
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        RpcFailure::Timeout(format!("nvim RPC {method} timed out"))
                    } else {
                        RpcFailure::Other(error.into())
                    }
                })?;
            let Some(parts) = message.as_array() else {
                continue;
            };
            if parts.len() != 4 || parts[0].as_i64() != Some(1) || parts[1].as_u64() != Some(id) {
                continue;
            }
            if !parts[2].is_nil() {
                return Err(RpcFailure::Other(anyhow!(
                    "nvim RPC {method} failed: {}",
                    parts[2]
                )));
            }
            return Ok(parts[3].clone());
        }
    }

    fn notify(
        &mut self,
        method: &str,
        arguments: Vec<Value>,
        timeout: Duration,
    ) -> std::result::Result<(), RpcFailure> {
        self.send_until(
            Value::Array(vec![
                Value::from(2),
                Value::from(method),
                Value::Array(arguments),
            ]),
            Instant::now() + timeout,
        )
    }

    fn send_until(
        &mut self,
        message: Value,
        deadline: Instant,
    ) -> std::result::Result<(), RpcFailure> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &message)
            .map_err(|error| RpcFailure::Other(error.into()))?;
        if bytes.len() > RPC_MAX_BYTES {
            return Err(RpcFailure::Other(anyhow!(
                "nvim RPC request exceeded {RPC_MAX_BYTES} bytes"
            )));
        }
        let mut writer = DeadlineWrite::new(&mut self.writer, deadline);
        writer.write_all(&bytes).map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                RpcFailure::Timeout("nvim RPC write timed out".to_owned())
            } else {
                RpcFailure::Other(error.into())
            }
        })?;
        writer.flush().map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) {
                RpcFailure::Timeout("nvim RPC flush timed out".to_owned())
            } else {
                RpcFailure::Other(error.into())
            }
        })
    }
}

struct DeadlineRead<'a> {
    reader: &'a mut BufReader<UnixStream>,
    deadline: Instant,
    remaining: usize,
}

impl<'a> DeadlineRead<'a> {
    fn new(reader: &'a mut BufReader<UnixStream>, deadline: Instant, budget: usize) -> Self {
        Self {
            reader,
            deadline,
            remaining: budget,
        }
    }
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let remaining_time = self.deadline.saturating_duration_since(Instant::now());
        if remaining_time.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nvim RPC read deadline expired",
            ));
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "nvim RPC response exceeded byte budget",
            ));
        }
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining_time))?;
        let capacity = buffer.len().min(self.remaining);
        let count = self.reader.read(&mut buffer[..capacity])?;
        self.remaining -= count;
        Ok(count)
    }
}

struct DeadlineWrite<'a> {
    writer: &'a mut UnixStream,
    deadline: Instant,
}

impl<'a> DeadlineWrite<'a> {
    fn new(writer: &'a mut UnixStream, deadline: Instant) -> Self {
        Self { writer, deadline }
    }
}

impl Write for DeadlineWrite<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nvim RPC write deadline expired",
            ));
        }
        self.writer.set_write_timeout(Some(remaining))?;
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nvim RPC flush deadline expired",
            ));
        }
        self.writer.set_write_timeout(Some(remaining))?;
        self.writer.flush()
    }
}

fn connect_unix_until(
    path: &Path,
    deadline: Instant,
) -> std::result::Result<UnixStream, RpcFailure> {
    let bytes = path.as_os_str().as_bytes();
    ensure_socket_path(bytes).map_err(RpcFailure::Other)?;
    // SAFETY: socket has no pointer arguments and returns a new owned descriptor.
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw_fd == -1 {
        return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
    }
    // SAFETY: `raw_fd` was just returned by socket and has no other owner.
    let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    // SAFETY: zero is a valid initial representation for sockaddr_un.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *target = source as libc::c_char;
    }
    let address_length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // SAFETY: address points to an initialized sockaddr_un of `address_length` bytes.
    let result = unsafe {
        libc::connect(
            owned.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_length,
        )
    };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(RpcFailure::StaleSocket(error));
        }
        wait_for_connect(owned.as_raw_fd(), deadline)?;
    }
    set_blocking(owned.as_raw_fd()).map_err(|error| RpcFailure::Other(error.into()))?;
    Ok(UnixStream::from(owned))
}

fn ensure_socket_path(path: &[u8]) -> Result<()> {
    ensure!(!path.contains(&0), "nvim socket path contains NUL");
    ensure!(
        path.len()
            < std::mem::size_of::<libc::sockaddr_un>()
                - std::mem::offset_of!(libc::sockaddr_un, sun_path),
        "nvim socket path is too long"
    );
    Ok(())
}

fn wait_for_connect(fd: libc::c_int, deadline: Instant) -> std::result::Result<(), RpcFailure> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as libc::c_int;
        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: descriptor is valid for one pollfd element for this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready == 0 {
            return Err(RpcFailure::Timeout(
                "nvim socket connect timed out".to_owned(),
            ));
        }
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(RpcFailure::Other(error.into()));
        }
        let mut socket_error: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        // SAFETY: pointers reference writable objects of the advertised sizes.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut socket_error).cast(),
                &mut length,
            )
        } == -1
        {
            return Err(RpcFailure::Other(std::io::Error::last_os_error().into()));
        }
        if socket_error != 0 {
            return Err(RpcFailure::StaleSocket(std::io::Error::from_raw_os_error(
                socket_error,
            )));
        }
        return Ok(());
    }
}

fn set_blocking(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: fd is owned and live; fcntl does not retain pointers.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above; flags came from F_GETFL for this descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        process::Command,
        thread,
    };

    struct ProcessGroupGuard(libc::pid_t);

    impl ProcessGroupGuard {
        fn for_session(session: &NvimSession) -> Self {
            Self(session.process.as_ref().expect("session spawned nvim").id() as libc::pid_t)
        }
    }

    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            // SAFETY: spawned editors create a process group whose id is their pid.
            let _ = unsafe { libc::kill(-self.0, libc::SIGKILL) };
        }
    }

    fn headless(directory: &Path) -> Nvim {
        Nvim {
            terminal: Vec::new(),
            editor: vec![
                "nvim".to_owned(),
                "--headless".to_owned(),
                "-u".to_owned(),
                "NONE".to_owned(),
                "-i".to_owned(),
                "NONE".to_owned(),
            ],
            socket_path: directory.join("nvim.sock"),
            dictation_dir: directory.join("dictation"),
            startup_timeout_s: 10.0,
            ..Nvim::default()
        }
    }

    #[test]
    fn headless_append_literal_text_and_adopt() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = headless(directory.path());
        let mut first = NvimSession::new(config.clone());
        let path = first.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&first);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(first.append("$(touch nope)\nGrüße 東京", false).unwrap(), 2);
        first.detach();

        let mut restarted = NvimSession::new(config);
        assert_eq!(restarted.ensure().unwrap(), path);
        restarted.append("continued", true).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "$(touch nope)\nGrüße 東京 continued\n"
        );
    }

    #[test]
    fn headless_append_preserves_trailing_lf_and_empty_continuation() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut session = NvimSession::new(headless(directory.path()));
        let path = session.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&session);

        assert_eq!(session.append("first\nsecond\n", false).unwrap(), 3);
        assert_eq!(session.append("", true).unwrap(), 2);
        assert_eq!(fs::read_to_string(path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn spawned_editor_waits_for_owner_and_user_init() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let init = directory.path().join("delayed-init.lua");
        fs::write(
            &init,
            r#"
vim.wait(250)
vim.g.spokenpad_init_finished = true
vim.api.nvim_create_autocmd("VimEnter", {
  once = true,
  callback = function()
    vim.wait(250)
    vim.g.spokenpad_vimenter_finished = true
  end,
})
"#,
        )
        .unwrap();
        let mut config = headless(directory.path());
        config.editor = vec![
            "nvim".to_owned(),
            "--headless".to_owned(),
            "-i".to_owned(),
            "NONE".to_owned(),
            "--cmd".to_owned(),
            "lua vim.wait(250)".to_owned(),
        ];
        config.init = Some(init);
        let mut session = NvimSession::new(config);
        session.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&session);

        let ready = session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        "return vim.g.spokenpad_init_finished == true and vim.g.spokenpad_vimenter_finished == true",
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        assert_eq!(ready.as_bool(), Some(true));
    }

    #[test]
    fn headless_preview_is_inline_virtual_and_buffer_local() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = headless(directory.path());
        let mut session = NvimSession::new(config.clone());
        let path = session.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&session);
        session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_ui_attach",
                vec![
                    Value::from(40),
                    Value::from(10),
                    Value::Map(vec![
                        (Value::from("rgb"), Value::from(true)),
                        (Value::from("ext_linegrid"), Value::from(true)),
                    ]),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();

        session
            .set_state(IndicatorUpdate {
                phase: Some(IndicatorPhase::Recording),
                preview: Some("draft words"),
                ..IndicatorUpdate::default()
            })
            .unwrap();
        session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
local buf = Spokenpad.buf
local marks = vim.api.nvim_buf_get_extmarks(buf, Spokenpad.ns, 0, -1, { details = true })
assert(#marks == 1, "preview must use exactly one extmark")
local details = marks[1][4]
assert(details.virt_lines == nil, "short empty-buffer preview added a virtual spacer")
assert(details.virt_text[1][1] == "draft words", "preview must overlay the empty buffer row")
assert(details.virt_text_pos == "overlay", "empty-buffer preview is not inline")
assert(vim.deep_equal(vim.api.nvim_buf_get_lines(buf, 0, -1, false), { "" }))

vim.cmd("redraw!")
local function screen_contains(needle)
  for row = 1, 10 do
    local line = ""
    for col = 1, 40 do
      line = line .. vim.fn.screenstring(row, col)
    end
    if line:find(needle, 1, true) then
      return true
    end
  end
  return false
end
assert(screen_contains("draft words"), "first preview exists but is clipped off-screen")

vim.fn.setreg("z", "sentinel")
vim.api.nvim_buf_call(buf, function()
  vim.cmd([[normal! gg0"zy$]])
end)
assert(not vim.fn.getreg("z"):find("draft words", 1, true), "preview leaked into yank")

assert(not vim.diagnostic.is_enabled({ bufnr = buf }), "owned buffer diagnostics enabled")
local ordinary = vim.api.nvim_create_buf(true, false)
assert(vim.diagnostic.is_enabled({ bufnr = ordinary }), "ordinary buffer diagnostics disabled")
vim.api.nvim_buf_delete(ordinary, { force = true })

for _, foreground in ipairs({ 0x123456, 0xabcdef }) do
  vim.api.nvim_set_hl(0, "Normal", { fg = foreground })
  vim.api.nvim_set_hl(0, "Comment", { fg = foreground + 1 })
  vim.api.nvim_exec_autocmds("ColorScheme", { modeline = false })
  local comment = vim.api.nvim_get_hl(0, { name = "Comment", link = false })
  local preview = vim.api.nvim_get_hl(0, { name = "SpokenpadPreview", link = false })
  assert(preview.fg == comment.fg, "preview foreground drifted from Comment")
  assert(preview.italic == true, "preview lost its provisional italic")
end
return true
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        session.detach();
        let mut reloaded = NvimSession::new(config);
        assert_eq!(reloaded.ensure().unwrap(), path);
        reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_ui_attach",
                vec![
                    Value::from(40),
                    Value::from(10),
                    Value::Map(vec![
                        (Value::from("rgb"), Value::from(true)),
                        (Value::from("ext_linegrid"), Value::from(true)),
                    ]),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
assert(Spokenpad.state.phase == "recording", "phase lost on reload")
assert(Spokenpad.state.preview == "draft words", "preview lost on reload")
local marks = vim.api.nvim_buf_get_extmarks(
  Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }
)
assert(#marks == 1 and marks[1][4].virt_text[1][1] == "draft words")
vim.cmd("messages clear")
return true
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();

        reloaded.append("committed", false).unwrap();
        let messages = reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from("return vim.api.nvim_exec2('messages', { output = true }).output"),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        assert_eq!(messages.as_str(), Some(""), "successful write was noisy");
        reloaded
            .set_state(IndicatorUpdate {
                preview: Some("next draft"),
                ..IndicatorUpdate::default()
            })
            .unwrap();
        reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
local marks = vim.api.nvim_buf_get_extmarks(
  Spokenpad.buf, Spokenpad.ns, 0, -1, { details = true }
)
assert(#marks == 1, "preview after committed text must use exactly one extmark")
local details = marks[1][4]
assert(#details.virt_lines == 1, "preview after committed text has a virtual spacer")
assert(details.virt_lines[1][1][1] == "next draft", "preview text must be first")
assert(details.virt_lines_above == false, "non-empty preview must follow committed text")
assert(vim.deep_equal(
  vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false), { "committed" }
))
return true
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "committed\n");

        let long_paragraph = (0..48)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        reloaded.append(&long_paragraph, true).unwrap();
        reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
vim.cmd("redraw!")
local visible = false
for row = 1, 10 do
  local line = ""
  for col = 1, 40 do
    line = line .. vim.fn.screenstring(row, col)
  end
  visible = visible or line:find("word47", 1, true) ~= nil
end
assert(visible, "committed tail fell below the viewport")
return true
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        reloaded
            .set_state(IndicatorUpdate {
                preview: Some("growing preview keeps its newest tailmarker visible"),
                ..IndicatorUpdate::default()
            })
            .unwrap();
        reloaded
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
vim.cmd("redraw!")
local function screen_contains(needle)
  local rows = {}
  for row = 1, 10 do
    local line = ""
    for col = 1, 40 do
      line = line .. vim.fn.screenstring(row, col)
    end
    rows[#rows + 1] = line
    if line:find(needle, 1, true) then
      return true, table.concat(rows, "|")
    end
  end
  return false, table.concat(rows, "|")
end
local tail_visible, rendered = screen_contains("tailmarker")
assert(tail_visible, "preview tail fell below a long wrapped paragraph: " .. rendered
  .. " view=" .. vim.inspect(vim.fn.winsaveview())
  .. " height=" .. vim.inspect(vim.api.nvim_win_text_height(0, { start_row = 0, end_row = 0 }))
  .. " winheight=" .. vim.api.nvim_win_get_height(0))
Spokenpad.set_state({
  preview = "a longer replacement preview still follows through to growthmarker",
})
vim.cmd("redraw!")
assert(screen_contains("growthmarker"), "growing preview stopped following before commit")

vim.api.nvim_win_call(0, function()
  vim.cmd("normal! gg0zt")
end)
local before = vim.fn.winsaveview()
Spokenpad.set_state({ preview = "do not snap back to moved reader" })
vim.cmd("redraw!")
local after = vim.fn.winsaveview()
assert(vim.deep_equal(before, after), "preview update snapped a moved reader to the end")
assert(not screen_contains("do not snap"), "moved reader unexpectedly followed preview")
Spokenpad.set_state({ preview = "still do not snap on a later update" })
vim.cmd("redraw!")
assert(vim.deep_equal(before, vim.fn.winsaveview()), "later preview update resumed following")
assert(not screen_contains("still do not snap"), "later preview update snapped back")
return true
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
    }

    #[test]
    fn failed_save_rolls_back_before_the_next_append() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut session = NvimSession::new(headless(directory.path()));
        let path = session.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&session);
        session.append("first", false).unwrap();
        session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from("vim.bo[Spokenpad.buf].readonly = true"),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();

        assert!(session.append("must roll back", false).is_err());
        session.ensure().unwrap();
        session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from("vim.bo[Spokenpad.buf].readonly = false"),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();
        session.append("second", false).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "first\n\nsecond\n");
    }

    #[test]
    fn append_retries_an_ambiguous_timeout_exactly_once() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut session = NvimSession::new(headless(directory.path()));
        let path = session.ensure().unwrap();
        let _guard = ProcessGroupGuard::for_session(&session);
        session
            .client
            .as_mut()
            .unwrap()
            .request(
                "nvim_exec_lua",
                vec![
                    Value::from(
                        r#"
local append_once = Spokenpad.append_once
local first = true
Spokenpad.append_once = function(...)
  local result = append_once(...)
  if first then
    first = false
    vim.wait(2200)
  end
  return result
end
"#,
                    ),
                    Value::Array(Vec::new()),
                ],
                RPC_TIMEOUT,
            )
            .unwrap();

        assert_eq!(session.append("only once", false).unwrap(), 1);
        assert_eq!(fs::read_to_string(path).unwrap(), "only once\n");
    }

    #[test]
    fn unrelated_socket_is_rejected_without_mutation() {
        if Command::new("nvim").arg("--version").output().is_err() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let config = headless(directory.path());
        let mut child = Command::new("nvim")
            .args(["--headless", "-u", "NONE", "--listen"])
            .arg(&config.socket_path)
            .args(["-i", "NONE"])
            .env("NVIM_LOG_FILE", directory.path().join("nvim.log"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let _guard = ProcessGroupGuard(child.id() as libc::pid_t);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !config.socket_path.exists() && Instant::now() < deadline {
            std::thread::sleep(CONNECT_POLL);
        }
        let mut session = NvimSession::new(config);
        let error = session.ensure().unwrap_err().to_string();
        assert!(error.contains("unrelated nvim socket"), "{error}");
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn ordinary_file_at_socket_path_is_refused_and_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let config = headless(directory.path());
        fs::write(&config.socket_path, b"not a socket").unwrap();
        let mut session = NvimSession::new(config.clone());

        let error = session.ensure().unwrap_err().to_string();
        assert!(error.contains("refusing non-socket path"), "{error}");
        assert_eq!(fs::read(&config.socket_path).unwrap(), b"not a socket");
        assert!(!config.dictation_dir.exists());
    }

    #[test]
    fn graphical_editor_without_a_terminal_is_refused_before_file_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = headless(directory.path());
        config.editor = vec!["neovide".to_owned()];
        let mut session = NvimSession::new(config.clone());

        let error = session.ensure().unwrap_err().to_string();
        assert!(error.contains("explicitly contains --headless"), "{error}");
        assert!(!config.dictation_dir.exists());
    }

    #[test]
    fn graphical_terminal_must_set_the_x11_instance_not_just_a_title() {
        assert!(alacritty_declares_instance(&[
            "alacritty".to_owned(),
            "--class".to_owned(),
            "Floating,{instance}".to_owned(),
            "-e".to_owned(),
        ]));
        assert!(!alacritty_declares_instance(&[
            "alacritty".to_owned(),
            "--title".to_owned(),
            "{instance}".to_owned(),
            "-e".to_owned(),
        ]));
        assert!(!alacritty_declares_instance(&[
            "other-terminal".to_owned(),
            "--class".to_owned(),
            "Floating,{instance}".to_owned(),
        ]));
        for arguments in [
            vec![
                "alacritty",
                "-e",
                "echo",
                "--class",
                "Floating,{instance}",
                "-e",
            ],
            vec![
                "alacritty",
                "--class",
                "Floating,{instance}",
                "--class",
                "Other,other",
                "-e",
            ],
            vec![
                "alacritty",
                "--class",
                "Floating,{instance}",
                "-o",
                "window.class.instance='other'",
                "-e",
            ],
        ] {
            let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(!alacritty_declares_instance(&arguments));
        }
    }

    #[test]
    fn rpc_request_has_one_deadline_while_bytes_dribble_in() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("dribble.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            let mut response = Vec::new();
            rmpv::encode::write_value(
                &mut response,
                &Value::Array(vec![
                    Value::from(1),
                    Value::from(1),
                    Value::Nil,
                    Value::from(1),
                ]),
            )
            .unwrap();
            for byte in response {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(30));
            }
        });
        let mut client = RpcClient::connect(&socket, Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Duration::from_millis(100),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(300));
        drop(client);
        peer.join().unwrap();
    }

    #[test]
    fn rpc_request_times_out_when_peer_never_replies() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("stalled.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 256];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = RpcClient::connect(&socket, Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let error = client
            .request(
                "nvim_eval",
                vec![Value::from("1")],
                Duration::from_millis(50),
            )
            .unwrap_err();
        assert!(matches!(error, RpcFailure::Timeout(_)), "{error}");
        assert!(started.elapsed() < Duration::from_millis(200));
        drop(client);
        peer.join().unwrap();
    }
}
