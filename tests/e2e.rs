//! Headless end-to-end tests of the real daemon loop.
//!
//! [`spokenpad::shell::daemon::serve`] runs with two substitutions and nothing
//! else: a synthetic microphone in place of PortAudio, and (outside the
//! ignored real-model test) a counting recognizer. Control requests reach it
//! either on the request channel the control socket feeds, sent directly so a
//! test can script their order and timing, or through the real control
//! socket from the real `spokenpad start|stop|toggle` commands
//! ([`Settings::socket`]). Session policy, capture arithmetic, the recovery
//! WAV, the decode worker, the editor thread and a genuine `nvim --headless`
//! over msgpack-RPC are the production code paths.
//!
//! Nothing here touches PortAudio, the per-user daemon lock, the service's
//! control socket or the real state directory, so the tests pass while the
//! user's own service is running. Each test owns a temporary directory and kills the editor it
//! started: the editor's socket path is unique to that directory, so the
//! process is found by scanning `/proc/*/cmdline` for it and its process group
//! is signalled. No window is ever opened: the daemon runs in attach mode, and
//! "the user's editor" is started with the real `spokenpad editor` command,
//! configured to run nvim `--headless`. The pane has its own daemon test,
//! `tests/pane_daemon.rs`, on a private X server.

use anyhow::Result;
use spokenpad::{
    config::{Audio, Config, Mode, Recording, Vad},
    core::{
        control::{Received, Request},
        decode::{
            Pipeline, Recognizer, Segment, Segmenter, Split, TickKind, TrailingSilence, Utterance,
            Worker,
        },
        frames::Frames,
        segments::{VAD_WINDOW, merge_spans, settling_silence},
        session::Preparing,
        state::REPEAT_WINDOW,
    },
    shell::{
        audio::{AudioCapture, CallbackCore, InputBackend, InputStream, Teardown},
        control::{ControlServer, Socket},
        daemon::{Devices, Loader, PipelineSource, Reload, SHUTDOWN_GRACE, serve},
        inference::{Transcriber, load_segmenter},
        recorder::read_capture,
    },
};
use std::{
    collections::VecDeque,
    fs,
    ops::Range,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// Amplitude above which the counting recognizer calls a sample speech.
const LOUD: f32 = 0.1;
/// Loud samples the counting recognizer turns into one `"word"`.
const PER_WORD: usize = 8_000;
const RATE: u32 = 16_000;
/// Generous, because each test also starts a real editor.
const DEADLINE: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn nvim_available() -> bool {
    Command::new("nvim").arg("--version").output().is_ok()
}

/// A fake `g:clipboard` that stores what it is given in a plain Lua global
/// instead of shelling out to xclip/xsel, so these tests never touch the
/// real system clipboard. Set with `--cmd`, which runs before nvim would
/// otherwise probe for a real provider (`:h g:clipboard`).
const TEST_CLIPBOARD_CMD: &str = r#"lua vim.g.clipboard = { name = "spokenpad-test", copy = { ["+"] = function(lines) _G.spokenpad_test_clipboard = lines end, ["*"] = function(lines) end }, paste = { ["+"] = function() return { _G.spokenpad_test_clipboard or {}, "v" } end, ["*"] = function() return { {}, "v" } end } }"#;

/// Constant-amplitude "speech": every sample counts as loud, so the counting
/// recognizer's arithmetic is exact.
fn tone(seconds: f64) -> Vec<f32> {
    let frames = (seconds * f64::from(RATE)) as usize;
    (0..frames)
        .map(|i| if i % 2 == 0 { 0.5 } else { -0.5 })
        .collect()
}

fn loud_samples(samples: &[f32]) -> usize {
    samples.iter().filter(|s| s.abs() > LOUD).count()
}

// ---------------------------------------------------------------- microphone

/// A microphone whose "mouth" the test fills. Its stream delivers 20ms of
/// audio at a time, padding with silence when the mouth is empty — a real
/// device never stops delivering.
#[derive(Clone, Default)]
struct FakeMicrophone {
    mouth: Arc<Mutex<VecDeque<f32>>>,
    /// Stop flag of the most recently opened stream, so a test can kill it.
    live: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Set once the device is gone for good: every `open` then fails, as a
    /// device whose USB cable was pulled does.
    unplugged: Arc<AtomicBool>,
}

struct FakeStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeMicrophone {
    fn speak(&self, samples: &[f32]) {
        lock(&self.mouth).extend(samples);
    }
    fn spoken(&self) -> bool {
        lock(&self.mouth).is_empty()
    }
    /// Simulate a device that stops delivering without closing.
    fn die(&self) {
        if let Some(stop) = &*lock(&self.live) {
            stop.store(true, Ordering::Relaxed);
        }
    }
    /// Simulate a device that is gone: the live stream stops and no further
    /// one can be opened, so the next key press cannot start a capture.
    fn unplug(&self) {
        self.unplugged.store(true, Ordering::Relaxed);
        self.die();
    }
}

impl InputStream for FakeStream {
    fn is_active(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }
    fn close(mut self, _teardown: Teardown) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl InputBackend for FakeMicrophone {
    type Stream = FakeStream;
    fn open(&mut self, config: &Audio, core: Arc<CallbackCore>) -> Result<FakeStream> {
        if self.unplugged.load(Ordering::Relaxed) {
            anyhow::bail!("no such input device");
        }
        let stop = Arc::new(AtomicBool::new(false));
        *lock(&self.live) = Some(Arc::clone(&stop));
        let frames = (config.sample_rate as usize / 50).max(1);
        let mouth = Arc::clone(&self.mouth);
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut buffer = vec![0.0_f32; frames];
            while !flag.load(Ordering::Relaxed) {
                buffer.fill(0.0);
                {
                    let mut queue = lock(&mouth);
                    for slot in buffer.iter_mut() {
                        match queue.pop_front() {
                            Some(sample) => *slot = sample,
                            None => break,
                        }
                    }
                }
                core.process(&buffer, 0);
                thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(FakeStream {
            stop,
            thread: Some(thread),
        })
    }
}

// ------------------------------------------------------------------ pipeline

/// Deterministic stand-in for Parakeet: one `"word"` per [`PER_WORD`] loud
/// samples, or the exact loud-sample count when `words` is false — which makes
/// "every committed sample is decoded exactly once" checkable by addition.
#[derive(Clone)]
struct Counting {
    words: bool,
    delay: Duration,
    calls: Arc<Mutex<Vec<usize>>>,
}

impl Recognizer for Counting {
    fn transcribe(&mut self, samples: &[f32], _: TrailingSilence) -> Result<String> {
        let loud = loud_samples(samples);
        lock(&self.calls).push(loud);
        thread::sleep(self.delay);
        Ok(if self.words {
            vec!["word"; loud / PER_WORD].join(" ")
        } else {
            loud.to_string()
        })
    }
}

/// How a test cuts a capture into decode windows.
enum Chunker {
    /// Fixed-length chunks: everything but the last one has settled. Audio
    /// with no loud sample in it is silence, and a real VAD reports no speech
    /// there — which, since 2026-09-11, means no segments and therefore no
    /// decode.
    Fixed(usize),
    /// The real merge, padding and settlement policy over spans read off the
    /// amplitudes on Silero's window grid: everything the daemon does with a
    /// live capture except loading Silero itself. `Fixed` can show none of
    /// it, because it knows nothing about pauses.
    Speech(Vad),
}

impl Segmenter for Chunker {
    fn split(&mut self, samples: &[f32]) -> Result<Split> {
        let size = match self {
            Chunker::Fixed(size) => *size,
            Chunker::Speech(config) => {
                let mut spans: Vec<Range<usize>> = vec![];
                for (i, window) in samples.as_chunks::<VAD_WINDOW>().0.iter().enumerate() {
                    if !window.iter().any(|s| s.abs() > LOUD) {
                        continue;
                    }
                    let (start, end) = (i * VAD_WINDOW, (i + 1) * VAD_WINDOW);
                    match spans.last_mut() {
                        Some(last) if last.end == start => last.end = end,
                        _ => spans.push(start..end),
                    }
                }
                return Ok(merge_spans(&spans, samples.len(), config, RATE));
            }
        };
        if loud_samples(samples) == 0 {
            return Ok(Split::default());
        }
        Ok(Split {
            segments: (0..samples.len())
                .step_by(size)
                .map(|start| {
                    let end = (start + size).min(samples.len());
                    Segment {
                        window: start..end,
                        speech_end: end,
                        settled: start + size < samples.len(),
                    }
                })
                .collect(),
            silent_through: 0,
        })
    }
}

/// A short VAD policy, so a test can hold a pause long enough to settle
/// without running for half a minute.
fn brisk_vad() -> Vad {
    Vad {
        chunk_seconds: 1.0,
        pad_seconds: 0.2,
        edge_pad_seconds: 0.2,
        max_speech_seconds: 5.0,
        ..Vad::default()
    }
}

// ------------------------------------------------------------------- harness

struct Harness {
    directory: TempDir,
    microphone: FakeMicrophone,
    requests: Sender<Received>,
    /// The control socket, listening in the harness's directory, when
    /// [`Settings::socket`] asked for one.
    control: Option<ControlServer>,
    stopping: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<usize>>>,
    socket: PathBuf,
    dictation: PathBuf,
    recordings: PathBuf,
    /// Ends the loader's current attempt, when [`Settings::loading`] asked
    /// for one.
    loader: Option<Sender<Result<(), String>>>,
    served: Option<JoinHandle<Result<()>>>,
    /// The editor `open_editor` started, reaped when the harness is dropped.
    user_editor: Option<Child>,
}

struct Settings {
    /// Open the user's editor with `spokenpad editor` before the first press,
    /// as a user in attach mode does. Without one, text goes to the pending
    /// passage until the test opens one ([`Harness::open_editor`]).
    editor: bool,
    preroll_ms: u32,
    postroll_ms: u32,
    interval_ms: u64,
    chunk: Option<usize>,
    /// Chunk on the real merge policy instead of `chunk`.
    vad: Option<Vad>,
    words: bool,
    delay: Duration,
    /// Also listen on a control socket, for [`Harness::cli`].
    socket: bool,
    /// Mirrors `nvim.copy_to_clipboard`, off by default like the setting
    /// itself; the clipboard tests opt in explicitly.
    copy_to_clipboard: bool,
    /// Mirrors `capture.silence_timeout_s`. Off unless a test asks, so no
    /// other test can end on the clock while it is thinking.
    silence_timeout_s: f64,
    /// Build the pipeline on the inference thread, as the daemon does, each
    /// attempt ending when the test calls [`Harness::load`].
    loading: bool,
    /// Mirrors `preview.max_seconds`: also the window a recording made
    /// before the model was ready is transcribed in.
    max_seconds: f64,
    /// A config file the daemon reads again for each new window. Only its
    /// `nvim.copy_to_clipboard` is taken; everything else stays the
    /// harness's, which no file could express as tersely.
    config_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            editor: true,
            preroll_ms: 250,
            postroll_ms: 250,
            interval_ms: 200,
            chunk: None,
            vad: None,
            words: true,
            delay: Duration::ZERO,
            socket: false,
            copy_to_clipboard: false,
            silence_timeout_s: 0.,
            loading: false,
            max_seconds: 30.,
            config_file: None,
        }
    }
}

impl Harness {
    fn start(settings: Settings) -> Self {
        Self::start_in(tempfile::tempdir().expect("tempdir"), settings)
    }

    /// Stops this daemon and starts another on the same directory: the same
    /// recordings, dictation files and editor socket, as a restart of the
    /// service would.
    fn restart(mut self, settings: Settings) -> Self {
        self.stop();
        let directory =
            std::mem::replace(&mut self.directory, tempfile::tempdir().expect("tempdir"));
        drop(self);
        Self::start_in(directory, settings)
    }

    fn start_in(directory: TempDir, settings: Settings) -> Self {
        let root = directory.path();
        let mut config = Config::default();
        config.audio.preroll_ms = settings.preroll_ms;
        config.audio.postroll_ms = settings.postroll_ms;
        config.recording.dir = root.join("audio");
        config.nvim.mode = Mode::Attach;
        config.nvim.notify = false;
        config.nvim.copy_to_clipboard = settings.copy_to_clipboard;
        config.nvim.editor = [
            "nvim",
            "-u",
            "NONE",
            "-i",
            "NONE",
            "--cmd",
            TEST_CLIPBOARD_CMD,
        ]
        .map(str::to_owned)
        .into();
        config.nvim.socket_path = root.join("nvim.sock");
        config.nvim.dictation_dir = root.join("dictation");
        config.nvim.startup_timeout_s = 10.0;
        config.preview.interval_ms = settings.interval_ms;
        config.capture.silence_timeout_s = settings.silence_timeout_s;
        config.preview.max_seconds = settings.max_seconds;
        config.validate().expect("harness config is valid");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let microphone = FakeMicrophone::default();
        let capture = AudioCapture::with_backend(
            microphone.clone(),
            config.audio.clone(),
            config.recording.clone(),
        );
        let build = {
            let (words, delay, calls) = (settings.words, settings.delay, Arc::clone(&calls));
            let (vad, chunk) = (settings.vad.clone(), settings.chunk);
            move || Pipeline {
                recognizer: Counting {
                    words,
                    delay,
                    calls: Arc::clone(&calls),
                },
                segmenter: vad
                    .clone()
                    .map(Chunker::Speech)
                    .or_else(|| chunk.map(Chunker::Fixed)),
            }
        };
        let (pipeline, loader) = if settings.loading {
            // Each attempt reports a download, then waits for the test to say
            // how it ends.
            let (outcome, outcomes) = mpsc::channel::<Result<(), String>>();
            let load: Loader<Counting, Chunker> = Box::new(move |step| {
                step(Preparing::Downloading { done: 1, total: 4 });
                outcomes
                    .recv()
                    .map_err(|_| anyhow::anyhow!("the test ended"))?
                    .map_err(anyhow::Error::msg)?;
                step(Preparing::Loading);
                Ok(build())
            });
            (PipelineSource::Load(load), Some(outcome))
        } else {
            (PipelineSource::Ready(build()), None)
        };
        let reload: Reload = {
            let harness = config.clone();
            let file = settings.config_file.clone();
            Box::new(move || {
                let mut fresh = harness.clone();
                if let Some(file) = &file {
                    fresh.nvim.copy_to_clipboard = Config::load(Some(file))?.nvim.copy_to_clipboard;
                }
                Ok(fresh)
            })
        };
        let (requests, received) = mpsc::channel();
        // Where `spokenpad start` looks, given the harness's runtime dir.
        let control = settings.socket.then(|| {
            let socket = Socket::bind(&root.join("spokenpad.sock"))
                .expect("listen on the test control socket");
            ControlServer::start(socket, requests.clone()).expect("serve the test control socket")
        });
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let socket = config.nvim.socket_path.clone();
        let dictation = config.nvim.dictation_dir.clone();
        let recordings = config.recording.dir.clone();
        let served = thread::spawn(move || {
            serve(
                &config,
                Devices {
                    capture,
                    requests: received,
                    pipeline,
                    reload,
                },
                stop,
                None,
            )
        });
        let mut harness = Self {
            directory,
            microphone,
            requests,
            control,
            stopping,
            calls,
            socket,
            dictation,
            recordings,
            loader,
            served: Some(served),
            user_editor: None,
        };
        if settings.editor {
            harness.open_editor();
        }
        harness
    }

    /// Lets the loader's attempt end: `Ok` builds the pipeline, `Err` fails
    /// with that reason.
    fn load(&self, outcome: Result<(), &str>) {
        self.loader
            .as_ref()
            .expect("the harness loads its pipeline")
            .send(outcome.map_err(str::to_owned))
            .expect("the loader is waiting");
    }

    fn send(&self, request: Request) {
        self.send_at(request, Instant::now());
    }
    fn send_at(&self, request: Request, at: Instant) {
        self.requests
            .send(Received { request, at })
            .expect("serve is running");
    }
    /// Runs `spokenpad <request>` against this harness's control socket, as
    /// a key binding would, and returns once it exited successfully.
    #[track_caller]
    fn cli(&self, request: &str) {
        assert!(self.control.is_some(), "the harness has no control socket");
        let status = Command::new(env!("CARGO_BIN_EXE_spokenpad"))
            .env("XDG_RUNTIME_DIR", self.directory.path())
            .arg(request)
            .status()
            .expect("run spokenpad");
        assert!(status.success(), "spokenpad {request}: {status}");
    }
    /// Press, and do not return until the capture is actually running: every
    /// capture opens its recovery WAV the moment it starts, so counting those
    /// files is a precise, cheap signal that speech will now be recorded.
    fn press(&self, latch: bool) {
        let before = self.captures();
        self.press_only(latch);
        wait_until("the capture starts", || self.captures() > before);
    }
    /// `toggle` when `latch`, else `start`: a press that may end a latched
    /// recording rather than start one.
    fn press_only(&self, latch: bool) {
        self.send(if latch {
            Request::Toggle
        } else {
            Request::Start
        });
    }
    fn captures(&self) -> usize {
        self.wavs().len()
    }
    /// The recovery WAVs, oldest first; the directory also holds the list of
    /// those waiting to be transcribed.
    fn wavs(&self) -> Vec<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(&self.recordings)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "wav"))
            .collect();
        files.sort();
        files
    }
    fn release(&self) {
        self.send(Request::Stop);
    }
    /// Release, and wait until a new `start` can no longer be taken for the
    /// key's auto-repeat.
    fn release_for_good(&self) {
        self.release();
        thread::sleep(REPEAT_WINDOW + Duration::from_millis(50));
    }
    fn cancel(&self) {
        self.send(Request::Cancel);
    }
    /// Press and release inside the minimum hold, without waiting for it.
    fn tap(&self) {
        let at = Instant::now();
        self.send_at(Request::Start, at);
        self.send_at(Request::Stop, at + Duration::from_millis(50));
    }
    fn say(&self, samples: &[f32]) {
        self.microphone.speak(samples);
        wait_until("the microphone delivered everything", || {
            self.microphone.spoken()
        });
        // The last buffer is in flight; give it one device period.
        thread::sleep(Duration::from_millis(40));
    }

    fn dictation_file(&self) -> Option<PathBuf> {
        let mut files: Vec<_> = fs::read_dir(&self.dictation)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .collect();
        files.sort();
        files.pop()
    }
    fn text(&self) -> String {
        self.dictation_file()
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_default()
    }
    fn recovery_wav(&self) -> (Vec<f32>, u32) {
        read_capture(self.wavs().last().expect("a recovery WAV")).expect("readable WAV")
    }

    /// Evaluate a Vim expression inside the running editor, waiting for it to
    /// finish starting: spawning a real `nvim` takes a second or two.
    #[track_caller]
    fn ask(&self, expression: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let output = Command::new("nvim")
                .args(["--server".as_ref(), self.socket.as_os_str()])
                .args(["--remote-expr", expression])
                .output()
                .expect("run nvim as an RPC client");
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).into_owned();
            }
            assert!(
                Instant::now() < deadline,
                "querying {expression} kept failing: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
    /// Block until the daemon has attached to the dictation editor, which
    /// loads its Lua module into it.
    fn wait_for_editor(&self) {
        wait_until("the daemon attaches to the editor", || {
            self.ask("luaeval('tostring(Spokenpad ~= nil)')").trim() == "true"
        });
    }
    fn indicator(&self, field: &str) -> String {
        self.ask(&format!("luaeval('tostring(Spokenpad.state.{field})')"))
    }

    /// The test clipboard's `+` register, joined the way multiple lines of a
    /// real system clipboard would be. Empty when `Spokenpad.copy_buffer` has
    /// never set it -- an untouched real clipboard is exactly what an empty
    /// dictation buffer must leave behind.
    fn clipboard(&self) -> String {
        self.ask(r#"luaeval('table.concat(_G.spokenpad_test_clipboard or {}, "\n")')"#)
    }

    /// What the dictation window actually renders above the transcript. The
    /// indicator state the daemon pushed is only half the story: a field the
    /// winbar never draws tells the user nothing.
    fn winbar(&self) -> String {
        self.ask("luaeval('vim.wo.winbar')")
    }

    fn calls(&self) -> Vec<usize> {
        lock(&self.calls).clone()
    }

    /// Opens the dictation editor the way a user does in attach mode: by
    /// running `spokenpad editor`, which becomes nvim on the socket. Returns
    /// once it answers.
    fn open_editor(&mut self) {
        let root = self.directory.path();
        let config = root.join("editor.toml");
        let toml = format!(
            "[nvim]\nmode = 'attach'\ninit = 'NONE'\neditor = ['nvim', '--headless', '-i', 'NONE', '--cmd', '{TEST_CLIPBOARD_CMD}']\nsocket_path = '{}'\ndictation_dir = '{}'\n",
            self.socket.display(),
            self.dictation.display()
        );
        fs::write(&config, toml).expect("write the editor's config");
        let child = Command::new(env!("CARGO_BIN_EXE_spokenpad"))
            .env("XDG_STATE_HOME", root)
            .env("XDG_CONFIG_HOME", root)
            .env("XDG_RUNTIME_DIR", root)
            .env("NVIM_LOG_FILE", root.join("nvim.log"))
            .args(["--log-file", "none", "-c"])
            .arg(&config)
            .arg("editor")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own process group, so `kill_editor` can signal all of it.
            .process_group(0)
            .spawn()
            .expect("run spokenpad editor");
        self.user_editor = Some(child);
        // Answering at all is enough: the daemon loads its Lua on attach.
        assert_eq!(self.ask("1").trim(), "1");
    }

    /// How many processes are serving this harness's socket: the editor
    /// `open_editor` started, or none.
    fn editors(&self) -> usize {
        let needle = self.socket.as_os_str().as_encoded_bytes().to_vec();
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| fs::read(entry.path().join("cmdline")).ok())
            .filter(|cmdline| cmdline.split(|byte| *byte == 0).any(|arg| arg == needle))
            .count()
    }

    fn dictation_files(&self) -> usize {
        fs::read_dir(&self.dictation)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }

    /// Stop the daemon and wait for `serve` to return, keeping the harness and
    /// its files alive so a test can inspect what the shutdown delivered.
    #[track_caller]
    fn stop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let Some(served) = self.served.take() else {
            return;
        };
        let deadline = Instant::now() + DEADLINE;
        while !served.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(served.is_finished(), "serve did not stop");
        served
            .join()
            .expect("serve panicked")
            .expect("serve failed");
    }
    #[track_caller]
    fn finish(mut self) {
        self.stop();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(served) = self.served.take() {
            let _ = served.join();
        }
        kill_editor(&self.socket);
        if let Some(mut child) = self.user_editor.take() {
            let _ = child.wait();
        }
    }
}

/// Kill the editor this harness started, and only that one: its socket path is
/// inside the harness's temporary directory, so no other process names it.
fn kill_editor(socket: &Path) {
    let needle = socket.as_os_str().as_encoded_bytes().to_vec();
    let Ok(entries) = fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<libc::pid_t>() else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if !cmdline.split(|byte| *byte == 0).any(|arg| arg == needle) {
            continue;
        }
        // SAFETY: the editor is started in a process group of its own, so its
        // pid is its process group id; the negated pid signals exactly that
        // group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let mut status = 0;
        // SAFETY: waits for one known child pid and writes only to `status`.
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
}

#[track_caller]
fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting until {what}");
}

/// Sum of every integer the counting recognizer wrote into the file.
fn counted(text: &str) -> usize {
    text.split_whitespace()
        .filter_map(|word| word.parse::<usize>().ok())
        .sum()
}

// --------------------------------------------------------------------- tests

#[test]
fn press_speak_release_lands_text_in_the_file() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    // Let the pre-roll ring fill before the press.
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the transcript reaches the file", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
    assert_eq!(
        h.calls().len(),
        1,
        "without a segmenter the capture is decoded once, at the release: {:?}",
        h.calls()
    );
    wait_until("the indicator returns to idle", || {
        h.indicator("phase").trim() == "idle"
    });

    let (wav, rate) = h.recovery_wav();
    assert_eq!(rate, RATE);
    assert_eq!(
        loud_samples(&wav),
        RATE as usize,
        "every spoken sample reached the recovery WAV"
    );
    assert!(
        wav.len() > RATE as usize + RATE as usize / 8,
        "the WAV carries the pre-roll as well as the speech: {} frames",
        wav.len()
    );
    h.finish();
}

/// The clipboard rule is the whole window, not the utterance that just
/// finished: a release must copy everything in the buffer, including a
/// paragraph an earlier utterance already committed. Opt in explicitly:
/// `nvim.copy_to_clipboard` is off by default (see the next test).
#[test]
fn release_copies_the_whole_buffer_to_the_clipboard() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        copy_to_clipboard: true,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the first utterance reaches the file", || {
        !h.text().is_empty()
    });
    wait_until("the first utterance reaches the clipboard", || {
        !h.clipboard().is_empty()
    });
    assert_eq!(h.clipboard(), "word word");

    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the second utterance reaches the file", || {
        h.text().matches("word word").count() == 2
    });
    wait_until("the clipboard follows the second release", || {
        h.clipboard() == "word word\n\nword word"
    });
    assert_eq!(
        h.clipboard(),
        h.text().trim_end(),
        "the clipboard must hold the whole buffer, including the earlier utterance's paragraph"
    );
    h.finish();
}

/// The config file is read again for each new dictation window. An edit
/// between two windows applies to the second without a restart; a file that
/// no longer loads leaves the settings in use and says so in the window.
#[test]
fn each_new_window_reads_the_config_file_again() {
    if !nvim_available() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("config.toml");
    fs::write(&file, "[nvim]\ncopy_to_clipboard = false\n").unwrap();
    let mut h = Harness::start(Settings {
        config_file: Some(file.clone()),
        ..Settings::default()
    });
    let dictate = |h: &Harness, words: usize| {
        h.press(false);
        h.say(&tone(1.0));
        h.release_for_good();
        wait_until("the utterance reaches the file", || {
            h.text().matches("word").count() == words
        });
    };
    dictate(&h, 2);
    assert_eq!(h.clipboard(), "", "the first window was told not to copy");

    fs::write(&file, "[nvim]\ncopy_to_clipboard = true\n").unwrap();
    dictate(&h, 4);
    assert_eq!(
        h.clipboard(),
        "",
        "the edit waits for the next window: this one keeps its settings"
    );

    kill_editor(&h.socket);
    h.open_editor();
    dictate(&h, 2);
    wait_until("the second window copies", || h.clipboard() == "word word");

    kill_editor(&h.socket);
    h.open_editor();
    fs::write(&file, "[nvim]\ncopy_to_clipboard = 3\n").unwrap();
    h.press(false);
    wait_until("the window says the file did not load", || {
        h.indicator("notice").contains("config not reloaded")
    });
    assert!(
        h.indicator("notice_detail").contains("copy_to_clipboard"),
        "{}",
        h.indicator("notice_detail")
    );
    h.say(&tone(1.0));
    h.release();
    wait_until("the settings in use still copy", || {
        h.clipboard() == "word word"
    });
    h.finish();
}

/// `nvim.copy_to_clipboard` is off by default: a release must reach the
/// file exactly as it always does, but never touch the clipboard, and never
/// even ask the editor to.
#[test]
fn clipboard_copy_is_off_by_default() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the utterance reaches the file", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
    // Give a copy that should never be sent time to have arrived anyway.
    thread::sleep(Duration::from_millis(500));
    assert_eq!(
        h.clipboard(),
        "",
        "no copy request must be sent when nvim.copy_to_clipboard is off"
    );
    h.finish();
}

#[test]
fn progressive_commits_append_before_release() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        chunk: Some(RATE as usize),
        words: false,
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(3.0));
    wait_until("a settled chunk is committed while the key is held", || {
        !h.text().is_empty()
    });
    let committed_early = counted(&h.text());
    assert!(committed_early > 0, "nothing committed before the release");

    h.release();
    // Both sides still move after the release: the recovery WAV is finalised
    // while the release is processed, and the tail is committed after that.
    // Wait for the WAV to settle and the commits to catch up with it.
    let mut loud = 0;
    wait_until("every captured sample is committed exactly once", || {
        let previous = loud;
        loud = loud_samples(&h.recovery_wav().0);
        loud > 0 && loud == previous && counted(&h.text()) == loud
    });
    let text = h.text();
    assert!(
        !text.trim().contains("\n\n"),
        "one capture is one paragraph: {text:?}"
    );
    h.finish();
}

#[test]
fn shutdown_delivers_text_the_engine_produced_on_the_way_out() {
    if !nvim_available() {
        return;
    }
    let mut h = Harness::start(Settings {
        delay: Duration::from_millis(600),
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the release decode starts", || !h.calls().is_empty());
    // The recognizer is still inside that decode: its commit can only reach
    // the editor through the shutdown drain.
    h.stop();
    assert_eq!(
        h.text(),
        "word word\n",
        "text decoded while the daemon was stopping never reaches the file"
    );
}

#[test]
fn a_capture_with_no_speech_in_it_is_never_decoded() {
    if !nvim_available() {
        return;
    }
    // A press, a second of saying nothing, a release. Decoding that buffer is
    // how Parakeet hallucinated "Thank you." into the file; with a segmenter
    // loaded, the recognizer must not see it at all.
    let h = Harness::start(Settings {
        chunk: Some(RATE as usize / 2),
        words: false,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    thread::sleep(Duration::from_millis(1000));
    h.release();
    wait_until("the daemon returns to idle", || {
        h.indicator("phase").trim() == "idle"
    });
    // The release decode and any last preview have to be given the chance to
    // append something wrong before "nothing was appended" means anything.
    thread::sleep(Duration::from_millis(300));

    assert_eq!(h.text(), "", "silence must not append anything");
    assert!(
        h.calls().is_empty(),
        "a silent capture must not reach the recognizer: {:?}",
        h.calls()
    );
    assert!(
        h.winbar().contains("nearly silent"),
        "the user is told why nothing appeared: {:?}",
        h.winbar()
    );
    h.finish();
}

#[test]
fn tap_shorter_than_the_minimum_hold_is_discarded_with_a_notice() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.tap();
    h.wait_for_editor();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.text(), "", "a tap must not append anything");
    assert!(h.calls().is_empty(), "a tap must not reach the recognizer");
    // The rendered winbar, not just the pushed state: an idle notice that
    // reaches Lua and is never drawn is exactly the bug this guards.
    wait_until("the user is told why nothing appeared", || {
        h.winbar().contains("held too briefly")
    });
    assert_eq!(h.indicator("phase").trim(), "idle");
    assert_eq!(
        h.indicator("preview").trim(),
        "",
        "a notice is not a preview"
    );
    h.finish();
}

#[test]
fn latched_recording_ends_on_the_second_press() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.press(true);
    h.release();
    h.say(&tone(1.0));
    assert_eq!(h.text(), "", "a latched recording ignores the key release");
    h.press_only(false);
    wait_until("the second press ends the latch", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
    h.finish();
}

/// A held key's auto-repeat, as X11 delivers it to most window managers: a
/// stop/start pair every 40 ms, each pair's processes arriving in either
/// order. It is one capture, one paragraph, and one decode.
#[test]
fn auto_repeat_while_held_is_one_capture() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.press(false);
    h.microphone.speak(&tone(1.0));
    // Alternating, ending stop-first: after a start-first pair the stop has
    // the last word, and the key counts as released once its window closes.
    for i in 0..12 {
        let at = Instant::now();
        let (first, second) = if i % 2 == 1 {
            (Request::Stop, Request::Start)
        } else {
            (Request::Start, Request::Stop)
        };
        h.send_at(first, at);
        h.send_at(second, at + Duration::from_millis(1));
        thread::sleep(Duration::from_millis(40));
    }
    wait_until("the microphone delivered everything", || {
        h.microphone.spoken()
    });
    thread::sleep(Duration::from_millis(40));
    h.release();
    wait_until("the transcript reaches the file", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n", "one capture, one paragraph");
    assert_eq!(h.captures(), 1, "auto-repeat started a second capture");
    assert_eq!(h.calls().len(), 1, "decoded once: {:?}", h.calls());
    h.finish();
}

/// Push-to-talk through the real control socket and the real commands a key
/// binding runs.
#[test]
fn held_capture_through_the_control_socket() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        socket: true,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.cli("start");
    wait_until("the capture starts", || h.captures() == 1);
    h.say(&tone(1.0));
    h.cli("stop");
    wait_until("the transcript reaches the file", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
    // The binding for the key-up fires again with nothing running.
    h.cli("stop");
    h.cli("cancel");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.captures(), 1, "a stray stop or cancel starts nothing");
    assert_eq!(h.text(), "word word\n");
    h.finish();
}

/// Latch with `toggle`; the key-up's `stop` arrives or not, depending on
/// which of Shift and the key came up first, and changes nothing; the next
/// `start` ends it, and its own `stop` begins nothing.
#[test]
fn latched_capture_through_the_control_socket() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        socket: true,
        ..Settings::default()
    });
    h.cli("toggle");
    wait_until("the capture starts", || h.captures() == 1);
    h.cli("stop");
    h.say(&tone(1.0));
    assert_eq!(h.text(), "", "a latched recording ignores the key-up");
    h.cli("start");
    h.cli("stop");
    wait_until("the second press ends the latch", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(h.captures(), 1, "the ending press's stop started nothing");
    h.finish();
}

#[test]
fn cancel_during_recording_keeps_committed_text_and_the_wav() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        chunk: Some(RATE as usize),
        words: false,
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(2.5));
    wait_until("a chunk is committed before the cancel", || {
        counted(&h.text()) > 0
    });
    let committed = h.text();
    h.cancel();
    thread::sleep(Duration::from_millis(400));

    let after = h.text();
    assert!(
        after.starts_with(committed.trim_end()),
        "cancelling unwrote committed text: {committed:?} -> {after:?}"
    );
    let (wav, rate) = h.recovery_wav();
    assert_eq!(rate, RATE);
    assert!(
        loud_samples(&wav) >= 2 * RATE as usize,
        "the recovery WAV survives a cancel: {} loud frames",
        loud_samples(&wav)
    );
    assert!(
        counted(&after) < loud_samples(&wav),
        "cancelling stopped the tail decode"
    );
    h.finish();
}

/// The last syllable is often still sounding when the key comes up; the
/// post-roll keeps it in the capture, the decode and the recovery WAV.
#[test]
fn speech_still_sounding_at_key_up_is_decoded() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        words: false,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    // 150 ms of speech after the key-up, well inside the 250 ms post-roll.
    let tail = tone(0.15);
    h.microphone.speak(&tail);
    wait_until("the transcript reaches the file", || !h.text().is_empty());
    let spoken = RATE as usize + tail.len();
    assert_eq!(
        counted(&h.text()),
        spoken,
        "speech after the key-up is part of the capture: {:?}",
        h.text()
    );
    wait_until("the recovery WAV holds the tail too", || {
        loud_samples(&h.recovery_wav().0) == spoken
    });
    h.finish();
}

/// A latched recording ends on a key press, and that key's release follows
/// within the post-roll. Only a press may cut the post-roll short.
#[test]
fn the_key_up_after_a_latched_stop_does_not_cut_the_postroll() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        words: false,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(true);
    h.release();
    h.say(&tone(1.0));
    h.press_only(false);
    let tail = tone(0.15);
    h.microphone.speak(&tail);
    thread::sleep(Duration::from_millis(50));
    h.release();
    wait_until("the transcript reaches the file", || !h.text().is_empty());
    assert_eq!(
        counted(&h.text()),
        RATE as usize + tail.len(),
        "the key-up cut the post-roll short: {:?}",
        h.text()
    );
    h.finish();
}

/// A press during the previous release's post-roll ends it at once: the new
/// capture starts without waiting it out, so its first word is its own.
#[test]
fn a_quick_re_press_does_not_wait_for_the_postroll() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        postroll_ms: 1_000,
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release_for_good();
    let pressed = Instant::now();
    h.press(false);
    let waited = pressed.elapsed();
    assert!(
        waited < Duration::from_millis(500),
        "the second capture waited {waited:?} for a 1 s post-roll"
    );
    h.say(&tone(1.0));
    h.release();
    wait_until("both paragraphs land", || {
        h.text().matches("word word").count() == 2
    });
    assert_eq!(h.text(), "word word\n\nword word\n");
    h.finish();
}

#[test]
fn a_second_press_while_transcribing_starts_a_new_paragraph() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        delay: Duration::from_millis(300),
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release_for_good();
    // Still transcribing: press again.
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("both paragraphs land", || {
        h.text().matches("word word").count() == 2
    });
    assert_eq!(
        h.text(),
        "word word\n\nword word\n",
        "two captures are two paragraphs, in order"
    );
    h.finish();
}

#[test]
fn the_preview_is_virtual_text_and_never_file_content() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        chunk: Some(RATE as usize * 10),
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    wait_until("a preview is decoded", || {
        h.indicator("preview").contains("word")
    });
    let marks = h.ask(
        "luaeval('vim.json.encode(vim.api.nvim_buf_get_extmarks(Spokenpad.buf, Spokenpad.ns, 0, -1, {details = true}))')",
    );
    assert!(
        marks.contains("word"),
        "the preview must be extmark virtual text: {marks}"
    );
    let lines = h
        .ask("luaeval('vim.json.encode(vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false))')");
    assert!(
        !lines.contains("word"),
        "the preview reached the buffer: {lines}"
    );
    assert!(
        !h.text().contains("word"),
        "the preview reached the file: {:?}",
        h.text()
    );
    h.release();
    wait_until("the release text lands", || h.text().contains("word"));
    h.finish();
}

/// A press that cannot start the microphone has to say so where the user is
/// looking -- and on the first press of a session the daemon has no window
/// yet, so the failure has to reach one: attach to the user's editor here, or
/// open a pane. Setting the notice and returning left a dead key and an empty
/// screen: nothing recorded, nothing said, nowhere.
#[test]
fn a_press_that_cannot_open_the_microphone_reaches_the_window_and_says_so() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.microphone.unplug();
    // Not `press`: no capture starts, so there is no recovery WAV to wait for.
    h.press_only(false);
    wait_until("the window opens and names the failure", || {
        h.winbar().contains("microphone unavailable")
    });
    assert_eq!(h.text(), "", "a failed capture appends nothing");
    h.release();
    h.finish();
}

#[test]
fn a_microphone_restart_marks_the_gap_and_keeps_the_audio() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.press(false);
    h.say(&tone(1.0));
    h.microphone.die();
    wait_until("the watchdog reports the gap", || {
        h.winbar().contains("gap")
    });
    h.say(&tone(1.0));
    h.release();
    wait_until("text still lands after the restart", || {
        !h.text().is_empty()
    });
    assert!(
        h.text().starts_with("word"),
        "audio from both sides of the gap is kept: {:?}",
        h.text()
    );
    let (wav, _) = h.recovery_wav();
    assert!(
        loud_samples(&wav) >= 2 * RATE as usize - RATE as usize / 10,
        "audio from before and after the gap is on disk: {} loud frames",
        loud_samples(&wav)
    );
    h.finish();
}

/// Run with `cargo test --locked --test e2e -- --ignored`.
/// Attach mode: the user opened the editor, the daemon writes into it and
/// opens nothing of its own.
#[test]
fn attach_mode_writes_into_the_users_editor_and_spawns_nothing() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        copy_to_clipboard: true,
        ..Settings::default()
    });
    assert_eq!(h.editors(), 1);
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the transcript reaches the user's editor", || {
        h.text() == "word word\n"
    });
    wait_until("the release copies the buffer", || {
        h.clipboard() == "word word"
    });
    assert_eq!(
        h.ask("luaeval('vim.api.nvim_buf_get_lines(Spokenpad.buf, 0, -1, false)[1]')")
            .trim(),
        "word word"
    );
    assert_eq!(h.editors(), 1, "the daemon opened an editor of its own");
    assert_eq!(h.dictation_files(), 1);
    h.finish();
}

/// Attach mode with nobody's editor open: nothing is lost, nothing is
/// decoded twice, and the editor opened afterwards shows it all and keeps
/// the same file.
#[test]
fn attach_mode_without_an_editor_keeps_the_text_for_the_next_one() {
    if !nvim_available() {
        return;
    }
    let mut h = Harness::start(Settings {
        editor: false,
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the first utterance reaches the file", || {
        h.text() == "word word\n"
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the second utterance is its own paragraph", || {
        h.text() == "word word\n\nword word\n"
    });
    assert_eq!(h.editors(), 0, "attach mode must never open an editor");
    assert_eq!(h.dictation_files(), 1);
    assert_eq!(
        h.calls().len(),
        2,
        "each capture is decoded exactly once: {:?}",
        h.calls()
    );

    h.open_editor();
    assert_eq!(
        h.ask("join(getline(1, '$'), '|')").trim(),
        "word word||word word",
        "the editor opens on what was said while none was open"
    );
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the third utterance lands in the editor", || {
        h.text() == "word word\n\nword word\n\nword word\n"
    });
    assert_eq!(h.dictation_files(), 1, "one passage, one file");
    assert_eq!(h.editors(), 1);
    assert_eq!(h.calls().len(), 3);
    h.finish();
}

/// The user closes the editor while the release is still decoding. The
/// append then fails to reach it, which means the text is certainly not
/// there, so it goes to the pending passage, once, rather than only to the
/// log.
#[test]
fn text_for_an_editor_that_exited_during_the_decode_goes_to_the_pending_passage() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        delay: Duration::from_secs(2),
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    // Once the editor shows the decode, the indicator stays the same until
    // the text arrives, so the append is the next thing sent to the editor.
    wait_until("the editor shows the decode", || {
        h.indicator("phase").trim() == "transcribing"
    });
    kill_editor(&h.socket);
    let pointer = PathBuf::from(format!("{}.pending", h.socket.display()));
    wait_until("the text reaches the pending passage", || {
        fs::read_to_string(&pointer)
            .ok()
            .and_then(|path| fs::read_to_string(path.trim_end()).ok())
            .as_deref()
            == Some("word word\n")
    });
    let mut texts: Vec<String> = fs::read_dir(&h.dictation)
        .unwrap()
        .flatten()
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        ["", "word word\n"],
        "the editor's own file stays empty and the text is written once"
    );
    assert_eq!(h.calls().len(), 1, "decoded once: {:?}", h.calls());
    h.finish();
}

/// The editor has half a command typed when the daemon stops, so the append
/// it holds is given up on. Its text is not left only in the log: it goes to
/// the pending passage, and it is not in the editor as well, because Neovim
/// drops a request whose connection closed before it ran.
#[test]
fn an_append_held_when_the_daemon_stops_goes_to_the_pending_passage() {
    if !nvim_available() {
        return;
    }
    let mut h = Harness::start(Settings {
        delay: Duration::from_secs(1),
        ..Settings::default()
    });
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the editor shows the decode", || {
        h.indicator("phase").trim() == "transcribing"
    });
    // A count waiting for its command: Neovim runs no RPC call until it ends.
    let typed = Command::new("nvim")
        .args(["--server".as_ref(), h.socket.as_os_str()])
        .args(["--remote-send", "2"])
        .status()
        .expect("type into the editor");
    assert!(typed.success());
    // Past the append's own two-second deadline, so it is held.
    thread::sleep(Duration::from_secs(4));
    let stopping = Instant::now();
    h.stop();
    println!(
        "the daemon stopped {} ms after it was told to",
        stopping.elapsed().as_millis()
    );
    let pointer = PathBuf::from(format!("{}.pending", h.socket.display()));
    let passage = fs::read_to_string(&pointer).expect("a pending passage");
    assert_eq!(
        fs::read_to_string(passage.trim_end()).expect("read the passage"),
        "word word\n",
        "the held text should be in the pending passage"
    );
    let cancelled = Command::new("nvim")
        .args(["--server".as_ref(), h.socket.as_os_str()])
        .args(["--remote-send", "<Esc>"])
        .status()
        .expect("cancel the count");
    assert!(cancelled.success());
    let buffer = h.ask("join(getline(1, '$'), '|')");
    assert!(
        !buffer.contains("word"),
        "the editor ran the held append after its connection closed: {buffer:?}"
    );
    assert_eq!(h.calls().len(), 1, "decoded once: {:?}", h.calls());
    h.finish();
}

/// The editor stops answering altogether (here: `SIGSTOP`) while the last
/// utterance decodes, and the daemon is told to stop. The append is given up
/// on without reconnecting and repeating it, and its text reaches the pending
/// passage before the editor thread's shutdown grace runs out.
#[test]
fn a_daemon_stopping_on_a_frozen_editor_still_writes_the_pending_passage() {
    if !nvim_available() {
        return;
    }
    const DECODE: Duration = Duration::from_secs(1);
    let mut h = Harness::start(Settings {
        delay: DECODE,
        ..Settings::default()
    });
    let editor = h.user_editor.as_ref().expect("the user's editor").id() as i32;
    thread::sleep(Duration::from_millis(300));
    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("the editor shows the decode", || {
        h.indicator("phase").trim() == "transcribing"
    });
    // SAFETY: `open_editor` gave the editor a process group of its own, whose
    // id is its pid; `kill` only signals it.
    assert_eq!(unsafe { libc::kill(-editor, libc::SIGSTOP) }, 0);
    let stopping = Instant::now();
    h.stop();
    let took = stopping.elapsed();
    println!(
        "the daemon stopped {} ms after it was told to, {} ms of it the decode",
        took.as_millis(),
        DECODE.as_millis()
    );
    let pointer = PathBuf::from(format!("{}.pending", h.socket.display()));
    let passage = fs::read_to_string(&pointer).expect("a pending passage");
    assert_eq!(
        fs::read_to_string(passage.trim_end()).expect("read the passage"),
        "word word\n",
        "the text should be in the pending passage"
    );
    assert!(
        took < DECODE + SHUTDOWN_GRACE,
        "the stop took {took:?}, past the grace"
    );
    // SAFETY: as above.
    assert_eq!(unsafe { libc::kill(-editor, libc::SIGCONT) }, 0);
    h.finish();
}

/// A latched capture that runs through a pause long enough to settle: the
/// daemon drops the audio it has committed while it records, and the capture
/// still reaches the file whole, with every sample decoded exactly once.
#[test]
fn a_capture_that_runs_through_a_long_pause_is_still_committed_whole() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        vad: Some(brisk_vad()),
        words: false,
        interval_ms: 200,
        ..Settings::default()
    });
    h.press(true);
    h.say(&tone(1.5));
    // Longer than the split threshold, so the chunk closes, the text lands
    // and the daemon has no reason to hold any of it.
    h.say(&vec![0.0; RATE as usize * 2]);
    wait_until("the first chunk is committed while the latch runs", || {
        !h.text().is_empty()
    });
    h.say(&tone(1.5));
    h.say(&vec![0.0; RATE as usize * 2]);
    h.press_only(true);
    h.release_for_good();

    let mut loud = 0;
    wait_until("every captured sample is committed exactly once", || {
        let previous = loud;
        loud = loud_samples(&h.recovery_wav().0);
        loud > 0 && loud == previous && counted(&h.text()) == loud
    });
    let (recorded, rate) = h.recovery_wav();
    assert_eq!(rate, RATE);
    assert!(
        recorded.len() >= 7 * RATE as usize,
        "the recovery WAV holds the whole capture, silence included: {} frames",
        recorded.len()
    );
    h.finish();
}

/// The daemon takes presses before its speech model is ready. While the model
/// downloads and loads, a capture is kept on disk only and the window says
/// so; once the model is ready the recording is transcribed the way a live
/// capture would have been, every sample exactly once, and the next press
/// decodes as it goes again.
#[test]
fn a_capture_made_before_the_model_is_ready_is_transcribed_once_it_is() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        loading: true,
        vad: Some(brisk_vad()),
        words: false,
        max_seconds: 3.0,
        ..Settings::default()
    });
    h.press(true);
    h.say(&tone(1.5));
    h.say(&vec![0.0; RATE as usize * 2]);
    h.say(&tone(1.5));
    h.press_only(true);
    h.release_for_good();
    wait_until("the window says the model is on its way", || {
        h.indicator("notice")
            .contains("downloading the speech model")
            && h.indicator("notice_detail").contains("25%; ")
    });
    wait_until("the window counts the recording", || {
        h.indicator("notice_detail").contains("1 recording waiting")
    });
    assert!(h.text().is_empty(), "nothing is decoded without a model");
    assert!(h.calls().is_empty(), "{:?}", h.calls());

    h.load(Ok(()));
    let loud = loud_samples(&h.recovery_wav().0);
    assert_eq!(
        loud,
        3 * RATE as usize,
        "the recording holds all the speech"
    );
    wait_until("the recording is transcribed, every sample once", || {
        counted(&h.text()) == loud
    });
    wait_until("the notice clears once nothing waits", || {
        h.indicator("notice").trim().is_empty()
    });

    h.press(false);
    h.say(&tone(1.0));
    h.release();
    wait_until("a capture after the load decodes as before", || {
        counted(&h.text()) == loud + RATE as usize
    });
    h.finish();
}

/// Recordings still waiting when the daemon stops are transcribed at the
/// next start, without a press: one it had partly transcribed from where its
/// text reached, so every sample of both is decoded exactly once across the
/// two runs.
#[test]
fn recordings_left_at_a_stop_are_transcribed_at_the_next_start() {
    let settings = |loading, delay| Settings {
        editor: false,
        loading,
        delay,
        vad: Some(brisk_vad()),
        words: false,
        max_seconds: 1.0,
        ..Settings::default()
    };
    // Each decode takes longer than the shutdown waits for the engine, so the
    // stop comes in the middle of the first recording.
    let mut h = Harness::start(settings(true, Duration::from_secs(5)));
    h.press(true);
    h.say(&tone(1.5));
    h.say(&vec![0.0; RATE as usize * 2]);
    h.say(&tone(1.5));
    h.press_only(true);
    h.release_for_good();
    h.press(false);
    h.say(&tone(0.5));
    h.release_for_good();
    let loud: usize = h
        .wavs()
        .iter()
        .map(|path| loud_samples(&read_capture(path).unwrap().0))
        .sum();
    assert_eq!(
        loud,
        7 * RATE as usize / 2,
        "both recordings hold all speech"
    );

    h.load(Ok(()));
    wait_until("the first window's text is written", || dictated(&h) > 0);
    let before = dictated(&h);
    h.stop();
    let list = h.recordings.join("waiting.tsv");
    let saved = fs::read_to_string(&list).expect("the stop saved what waits");
    let through: Vec<usize> = saved
        .lines()
        .map(|line| line.split('\t').next().unwrap().parse().unwrap())
        .collect();
    assert!(
        matches!(through[..], [partly, 0] if partly > 0),
        "the first partly transcribed, the second not at all: {saved}"
    );
    assert_eq!(dictated(&h), before, "{saved}");

    let h = h.restart(settings(true, Duration::ZERO));
    // Until each is transcribed, a crash of this daemon leaves them listed.
    thread::sleep(Duration::from_millis(300));
    assert_eq!(fs::read_to_string(&list).unwrap(), saved, "kept as it was");
    h.load(Ok(()));
    wait_until("the rest of both is transcribed", || dictated(&h) == loud);
    wait_until("the list goes once nothing is left", || !list.exists());
    thread::sleep(Duration::from_millis(300));
    assert_eq!(dictated(&h), loud, "nothing is written twice");
    h.finish();
}

/// What every dictation file of `h` counts, summed.
fn dictated(h: &Harness) -> usize {
    fs::read_dir(&h.dictation)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| counted(&fs::read_to_string(entry.path()).unwrap_or_default()))
                .sum()
        })
        .unwrap_or(0)
}

/// A recording that is gone by the time the model is ready is not counted as
/// transcribed: the window says it was lost.
#[test]
fn a_waiting_recording_that_vanished_is_reported_lost() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        loading: true,
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release_for_good();
    wait_until("the recording waits", || {
        h.indicator("notice_detail").contains("1 recording waiting")
    });
    for entry in fs::read_dir(&h.recordings).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    h.load(Ok(()));
    wait_until("the window says the recording was lost", || {
        h.indicator("notice").contains("recording lost")
    });
    assert!(h.text().is_empty(), "{:?}", h.text());
    h.finish();
}

/// A recording that becomes unreadable after part of it was transcribed is
/// not reported lost: its text so far is in the file, the window says so,
/// and it stays listed for the next start, from where its text reaches. That
/// start finds the file shorter than its text, and says the rest is gone
/// rather than advising a recovery that would fail the same way.
#[test]
fn a_recording_that_fails_partway_is_left_to_the_next_start() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        loading: true,
        vad: Some(brisk_vad()),
        words: false,
        max_seconds: 1.0,
        delay: Duration::from_millis(500),
        ..Settings::default()
    });
    let list = h.recordings.join("waiting.tsv");
    h.press(true);
    h.say(&tone(4.0));
    h.press_only(true);
    h.release_for_good();
    let recording = h.wavs().pop().expect("a recording");
    h.load(Ok(()));
    wait_until("the first window's text is written", || {
        counted(&h.text()) > 0
    });
    // Everything after the WAV header is gone, so the next window the
    // transcription reads fails.
    fs::OpenOptions::new()
        .write(true)
        .open(&recording)
        .unwrap()
        .set_len(44)
        .unwrap();
    wait_until("the window says it was partly transcribed", || {
        h.indicator("notice")
            .contains("recording partly transcribed")
    });
    let detail = h.indicator("notice_detail");
    assert!(
        detail.contains("the next start tries the rest again"),
        "{detail}"
    );
    let listed = fs::read_to_string(&list).expect("listed for the next start");
    let through: usize = listed
        .split('\t')
        .next()
        .and_then(|frames| frames.parse().ok())
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(
        through > 0 && through < 4 * RATE as usize,
        "from where its text reaches: {listed}"
    );

    // A capture running opens the window the notice shows in, and the next
    // press would clear it.
    let h = h.restart(Settings {
        loading: true,
        ..Settings::default()
    });
    h.press(true);
    h.load(Ok(()));
    wait_until("the next start says the rest is gone", || {
        h.indicator("notice").contains("recording shortened")
    });
    let detail = h.indicator("notice_detail");
    assert!(!detail.contains("--from"), "{detail}");
    wait_until("nothing is left to list", || !list.exists());
    h.finish();
}

/// A recording whose transcription fails while the daemon stops is not
/// dropped: it stays listed, from where its text reaches.
#[test]
fn a_recording_that_fails_during_the_stop_stays_listed() {
    let h = Harness::start(Settings {
        editor: false,
        loading: true,
        vad: Some(brisk_vad()),
        words: false,
        max_seconds: 1.0,
        delay: Duration::from_millis(1_500),
        ..Settings::default()
    });
    h.press(true);
    h.say(&tone(4.0));
    h.press_only(true);
    h.release_for_good();
    let recording = h.wavs().pop().expect("a recording");
    h.load(Ok(()));
    wait_until("the first window's text is written", || dictated(&h) > 0);
    // The next window the transcription reads fails, during the stop.
    fs::OpenOptions::new()
        .write(true)
        .open(&recording)
        .unwrap()
        .set_len(44)
        .unwrap();
    let mut h = h;
    h.stop();
    let listed = fs::read_to_string(h.recordings.join("waiting.tsv")).expect("listed");
    let through: usize = listed
        .split('\t')
        .next()
        .and_then(|frames| frames.parse().ok())
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(through > 0, "from where its text reaches: {listed}");
}

/// A model that cannot be had (offline, a failed download, a broken file)
/// does not stop the daemon: the window says why, what is recorded is kept,
/// and the next press tries again. Every recording is transcribed once it
/// works.
#[test]
fn a_model_that_fails_to_load_keeps_the_recordings_and_the_next_press_retries() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        loading: true,
        ..Settings::default()
    });
    h.press(false);
    h.say(&tone(1.0));
    h.release_for_good();
    h.load(Err("the network is unreachable"));
    wait_until("the window names the failure", || {
        h.indicator("notice").contains("no speech model")
    });
    let detail = h.indicator("notice_detail");
    assert!(
        detail.contains("the network is unreachable") && detail.contains("1 recording kept"),
        "{detail}"
    );

    h.press(false);
    h.say(&tone(1.0));
    h.release_for_good();
    h.load(Ok(()));
    wait_until("both recordings are transcribed", || {
        h.text().matches("word").count() == 4
    });
    assert_eq!(h.calls().len(), 2, "each decoded once: {:?}", h.calls());
    h.finish();
}

/// The latch the user forgot. Nobody presses anything: the microphone keeps
/// delivering silence, and the capture ends by itself, decodes its tail, says
/// why in the winbar, and closes its recovery WAV. The next press then starts
/// a fresh capture, as after any release.
#[test]
fn a_forgotten_latch_stops_itself_and_the_next_press_starts_fresh() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings {
        vad: Some(brisk_vad()),
        words: false,
        interval_ms: 200,
        // The shortest the configuration allows, so the suite waits seconds
        // rather than the five minutes a user gets.
        silence_timeout_s: 1.0,
        ..Settings::default()
    });
    h.press(true);
    h.say(&tone(1.5));
    // Nothing else is said and no key is pressed from here on. The fake
    // microphone keeps delivering silence, exactly as a real one does.
    wait_until("the latch ends itself", || {
        h.indicator("phase").trim() == "idle"
    });
    assert!(
        h.winbar().contains("stopped after silence"),
        "the winbar has to say why the recording stopped: {}",
        h.winbar()
    );
    assert_eq!(h.captures(), 1, "no second capture was started");

    // Everything spoken was decoded, and exactly once.
    let spoken = loud_samples(&h.recovery_wav().0);
    assert!(spoken > 0);
    assert_eq!(
        counted(&h.text()),
        spoken,
        "every loud sample is committed exactly once: {:?}",
        h.text()
    );

    // The recorder stopped with the capture: the WAV is complete, readable
    // and no longer growing.
    let (recorded, rate) = h.recovery_wav();
    assert_eq!(rate, RATE);
    assert!(
        recorded.len() >= (1.5 * f64::from(RATE)) as usize,
        "the recovery WAV is short: {} frames",
        recorded.len()
    );
    thread::sleep(Duration::from_millis(400));
    assert_eq!(
        h.recovery_wav().0.len(),
        recorded.len(),
        "the recovery WAV kept growing after the capture ended"
    );

    // A press after an auto-stop is an ordinary new capture.
    let before = h.text();
    h.press(true);
    assert_eq!(h.captures(), 2);
    assert_eq!(
        h.indicator("phase").trim(),
        "recording",
        "the next press records again"
    );
    assert!(
        !h.winbar().contains("stopped after silence"),
        "the press clears the notice"
    );
    h.say(&tone(1.0));
    h.press_only(true);
    h.release_for_good();
    wait_until("the second capture lands", || h.text().len() > before.len());
    h.finish();
}

/// What the process actually holds over a long latched capture, measured
/// twice: dropping the committed audio, and keeping it as the daemon used
/// to. Ignored because it replays half an hour of audio through the capture
/// buffer and takes about a minute.
///
///     cargo test --locked --test e2e -- --ignored --exact \
///         ram_over_a_long_latched_capture --nocapture
#[test]
#[ignore = "measures process RSS over half an hour of simulated capture"]
fn ram_over_a_long_latched_capture() {
    let _alone = one_heavy_test_at_a_time();
    let minutes = 30;
    let keeping = replay_into_capture(minutes, false);
    let dropping = replay_into_capture(minutes, true);
    println!(
        "{minutes} minutes latched: keeping committed audio +{:.1} MiB peak RSS, \
         dropping it +{:.1} MiB peak RSS",
        keeping as f64 / (1 << 20) as f64,
        dropping as f64 / (1 << 20) as f64
    );
    assert!(
        dropping < 32 << 20,
        "held {dropping} bytes above the baseline"
    );
    assert!(
        keeping > 4 * dropping,
        "the run that keeps everything is the one that grows: {keeping} vs {dropping}"
    );
}

/// The ignored tests load real models, and one of them measures the RSS of
/// this whole process: `-- --ignored` runs them on parallel threads, so a
/// model loading next door would land in that measurement. They take turns.
fn one_heavy_test_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Peak RSS above the baseline, in bytes, while `minutes` of speech and
/// silence run through a real [`AudioCapture`] and the real decode worker.
fn replay_into_capture(minutes: usize, dropping: bool) -> usize {
    let backend = DirectMicrophone::default();
    let mut capture = AudioCapture::with_backend(
        backend.clone(),
        Audio {
            sample_rate: RATE,
            preroll_ms: 0,
            postroll_ms: 0,
            device: None,
        },
        Recording {
            enabled: false,
            dir: "/unused".into(),
            max_total_bytes: 1,
        },
    );
    let mut worker = Worker::new(Pipeline {
        recognizer: Counting {
            words: false,
            delay: Duration::ZERO,
            calls: Arc::new(Mutex::new(Vec::new())),
        },
        segmenter: Some(Chunker::Speech(Vad::default())),
    });
    let utterance = Utterance::new(1);
    capture.start_capture().expect("start the capture");

    let buffer = RATE as usize / 50;
    let interval = 1_100 * RATE as usize / 1_000;
    let frames = minutes * 60 * RATE as usize;
    let baseline = resident_bytes();
    let mut peak = 0;
    let mut through = Frames::ZERO;
    let mut next_tick = interval;
    let mut speech = vec![0.0_f32; buffer];
    for start in (0..frames).step_by(buffer) {
        // Four seconds of speech, six of silence: every pause settles, and
        // the silence is what used to keep growing.
        let talking = (start / RATE as usize) % 10 < 4;
        for (i, sample) in speech.iter_mut().enumerate() {
            *sample = match (talking, (start + i) % 2 == 0) {
                (false, _) => 0.0,
                (true, true) => 0.5,
                (true, false) => -0.5,
            };
        }
        backend.feed(&speech);
        if start + buffer < next_tick {
            continue;
        }
        next_tick += interval;
        peak = peak.max(resident_bytes().saturating_sub(baseline));
        let snapshot = capture.snapshot_capture(through);
        worker
            .tick(
                &snapshot.samples,
                snapshot.start,
                &utterance,
                TickKind::Preview,
                |c| through = c.through,
            )
            .expect("tick");
        if dropping {
            capture.discard_before(through);
        }
    }
    peak.max(resident_bytes().saturating_sub(baseline))
}

/// Resident set size of this process, in bytes.
fn resident_bytes() -> usize {
    let status = fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let line = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .expect("VmRSS");
    let kilobytes: usize = line
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .expect("VmRSS value");
    kilobytes * 1024
}

/// A microphone the caller drives itself: no thread, no waiting, one
/// callback per `feed`.
#[derive(Clone, Default)]
struct DirectMicrophone {
    core: Arc<Mutex<Option<Arc<CallbackCore>>>>,
    alive: Arc<AtomicBool>,
}

struct DirectStream(Arc<AtomicBool>);

impl InputStream for DirectStream {
    fn is_active(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn close(self, _teardown: Teardown) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl InputBackend for DirectMicrophone {
    type Stream = DirectStream;
    fn open(&mut self, _config: &Audio, core: Arc<CallbackCore>) -> Result<DirectStream> {
        self.alive.store(true, Ordering::Relaxed);
        *lock(&self.core) = Some(Arc::clone(&core));
        // A real device delivers at once; skip the first-callback stall.
        core.process(&[], 0);
        Ok(DirectStream(Arc::clone(&self.alive)))
    }
}

impl DirectMicrophone {
    fn feed(&self, samples: &[f32]) {
        let core = lock(&self.core).clone().expect("stream opened");
        core.process(samples, 0);
    }
}

/// How long the real Silero pass over an open tail takes. A `Commits` tick
/// runs even past `preview.max_seconds`, and `Pipeline::split` has no abandon
/// check, so whatever this costs is what a release can queue behind.
///
///     cargo test --locked --release --test e2e -- --ignored --exact \
///         silero_split_time_over_a_long_tail --nocapture
#[test]
#[ignore = "loads the real Silero model from the default model directory"]
fn silero_split_time_over_a_long_tail() {
    let _alone = one_heavy_test_at_a_time();
    let config = Config::default();
    let sample = config.asr.model_dir.join("test_en.wav");
    if !sample.is_file() || !config.vad.model.is_file() {
        eprintln!("skipping: `spokenpad fetch-models` has not been run");
        return;
    }
    let (source, rate) = read_capture(&sample).expect("read the bundled sample");
    let speech = resample(&source, rate, RATE);
    let mut segmenter = load_segmenter(&config.vad, RATE).expect("load Silero");

    for seconds in [30, 60, 270] {
        // Speech and silence in turn, so the detector does the work it would
        // do on a real tail rather than skating over a flat buffer.
        let mut tail = Vec::with_capacity(seconds * RATE as usize);
        while tail.len() < seconds * RATE as usize {
            tail.extend_from_slice(&speech);
            tail.extend(std::iter::repeat_n(0.0, RATE as usize / 2));
        }
        tail.truncate(seconds * RATE as usize);
        // One pass to warm the model, three to time.
        segmenter.split(&tail).expect("split");
        let mut worst = Duration::ZERO;
        for _ in 0..3 {
            let started = Instant::now();
            let split = segmenter.split(&tail).expect("split");
            worst = worst.max(started.elapsed());
            assert!(!split.segments.is_empty());
        }
        println!(
            "Silero split over a {seconds}s tail: {:.0} ms",
            worst.as_secs_f64() * 1000.0
        );
    }
}

/// Dropping settled silence leaves the audio that follows covered by the
/// same detector windows, and the keep-back leaves Silero enough silence to
/// find the same speech in it. The first is arithmetic — the drop is a whole
/// number of `VAD_WINDOW`s — the second is a property of the model, which is
/// what this measures against the real one.
#[test]
#[ignore = "loads the real Silero model from the default model directory"]
fn dropping_settled_silence_does_not_move_the_detector_spans() {
    let _alone = one_heavy_test_at_a_time();
    let config = Config::default();
    let sample = config.asr.model_dir.join("test_en.wav");
    if !sample.is_file() || !config.vad.model.is_file() {
        eprintln!("skipping: `spokenpad fetch-models` has not been run");
        return;
    }
    let (source, rate) = read_capture(&sample).expect("read the bundled sample");
    let speech = resample(&source, rate, RATE);
    let mut segmenter = load_segmenter(&config.vad, RATE).expect("load Silero");

    let keep = settling_silence(&config.vad, RATE);
    let forgotten = keep + 56 * RATE as usize;
    assert_eq!(
        (forgotten - keep) % VAD_WINDOW,
        0,
        "the drop has to be a whole number of detector windows"
    );
    let mut with_prefix = |silence: usize| {
        let mut samples = vec![0.0_f32; silence];
        samples.extend_from_slice(&speech);
        samples.extend(std::iter::repeat_n(0.0, RATE as usize));
        segmenter.split(&samples).expect("split")
    };

    let trimmed = with_prefix(keep);
    let whole = with_prefix(forgotten);
    let shift = forgotten - keep;
    assert!(!trimmed.segments.is_empty(), "the sample has speech in it");
    assert_eq!(
        trimmed.segments.len(),
        whole.segments.len(),
        "a minute of extra leading silence changed how many windows there are"
    );
    for (short, long) in trimmed.segments.iter().zip(&whole.segments) {
        assert_eq!(
            (
                short.window.start + shift,
                short.window.end + shift,
                short.speech_end + shift,
                short.settled
            ),
            (
                long.window.start,
                long.window.end,
                long.speech_end,
                long.settled
            ),
            "the window moved by more than the silence that was dropped"
        );
    }
}

#[test]
#[ignore = "loads the real CPU models from the default model directory; about ten seconds"]
fn real_models_transcribe_the_kennedy_sample() {
    let _alone = one_heavy_test_at_a_time();
    if !nvim_available() {
        return;
    }
    let mut config = Config::default();
    let sample = config.asr.model_dir.join("test_en.wav");
    if !sample.is_file() || !config.vad.model.is_file() {
        eprintln!("skipping: `spokenpad fetch-models` has not been run");
        return;
    }
    let (source, rate) = read_capture(&sample).expect("read the bundled sample");
    let speech = resample(&source, rate, RATE);

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    config.recording.dir = root.join("audio");
    // No editor: the transcript goes to the pending passage, a file like any
    // other in the dictation directory.
    config.nvim.mode = Mode::Attach;
    config.nvim.editor = [
        "nvim",
        "-u",
        "NONE",
        "-i",
        "NONE",
        "--cmd",
        TEST_CLIPBOARD_CMD,
    ]
    .map(str::to_owned)
    .into();
    config.nvim.socket_path = root.join("nvim.sock");
    config.nvim.dictation_dir = root.join("dictation");
    config.nvim.startup_timeout_s = 10.0;
    config.validate().unwrap();

    let mut transcriber = Transcriber::new(&config.asr, RATE).expect("load the CPU recognizer");
    transcriber.warm_up().expect("warm up");
    let pipeline = PipelineSource::Ready(Pipeline {
        recognizer: transcriber,
        segmenter: load_segmenter(&config.vad, RATE),
    });
    let microphone = FakeMicrophone::default();
    let capture = AudioCapture::with_backend(
        microphone.clone(),
        config.audio.clone(),
        config.recording.clone(),
    );
    let (requests, received) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let dictation = config.nvim.dictation_dir.clone();
    let served = thread::spawn(move || {
        serve(
            &config,
            Devices {
                capture,
                requests: received,
                pipeline,
                reload: Box::new({
                    let config = config.clone();
                    move || Ok(config.clone())
                }),
            },
            stop,
            None,
        )
    });

    requests
        .send(Received {
            request: Request::Start,
            at: Instant::now(),
        })
        .unwrap();
    microphone.speak(&speech);
    wait_until("the sample is delivered", || microphone.spoken());
    thread::sleep(Duration::from_millis(100));
    requests
        .send(Received {
            request: Request::Stop,
            at: Instant::now(),
        })
        .unwrap();

    let text = || {
        let mut files: Vec<_> = fs::read_dir(&dictation)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .pop()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    wait_until("the real decode lands", || !text().is_empty());
    let transcript = text();
    stopping.store(true, Ordering::Release);
    let _ = served.join();

    assert_eq!(
        transcript.trim(),
        "Ask not what your country can do for you. Ask what you can do for your country."
    );
}

/// Linear resampling, adequate for a speech fixture and dependency-free.
fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let frames = (samples.len() as f64 / ratio) as usize;
    (0..frames)
        .map(|i| {
            let position = i as f64 * ratio;
            let left = position as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (position - left as f64) as f32;
            samples[left] * (1.0 - fraction) + samples[right] * fraction
        })
        .collect()
}
