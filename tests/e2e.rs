//! Headless end-to-end tests of the real daemon loop.
//!
//! [`spokenpad::shell::daemon::serve`] runs with three substitutions and nothing
//! else: a synthetic microphone in place of PortAudio, an ordinary channel in
//! place of evdev, and (outside the ignored real-model test) a counting
//! recognizer. Session policy, capture arithmetic, the recovery WAV, the
//! decode worker, the editor thread and a genuine `nvim --headless` over
//! msgpack-RPC are the production code paths.
//!
//! Nothing here touches `/dev/input`, PortAudio, the per-user daemon lock or
//! the real state directory, so the tests pass while the user's own service is
//! running. Each test owns a temporary directory and kills the editor it
//! spawned: the editor's socket path is unique to that directory, so the
//! process is found by scanning `/proc/*/cmdline` for it and its process group
//! — `nvim` calls `setsid`, so its pid is its process-group id — is signalled.
//! Spawning a graphical editor is never attempted; `nvim.terminal` is empty and
//! the editor command is explicitly `--headless`.

use anyhow::Result;
use spokenpad::{
    config::{Audio, Config},
    core::{
        decode::{Pipeline, Recognizer, Segment, Segmenter, TrailingSilence, Worker},
        state::Event,
    },
    shell::{
        audio::{AudioCapture, CallbackCore, InputBackend, InputStream, Teardown},
        daemon::{Devices, serve},
        inference::{Transcriber, load_segmenter},
        recorder::read_capture,
    },
};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::Command,
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
    fn open(&self, config: &Audio, core: Arc<CallbackCore>) -> Result<FakeStream> {
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

/// Fixed-length chunks: everything but the last one has settled. Audio with
/// no loud sample in it is silence, and a real VAD reports no speech there —
/// which, since 2026-09-11, means no segments and therefore no decode.
struct Fixed(usize);

impl Segmenter for Fixed {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>> {
        if loud_samples(samples) == 0 {
            return Ok(vec![]);
        }
        Ok((0..samples.len())
            .step_by(self.0)
            .map(|start| {
                let end = (start + self.0).min(samples.len());
                Segment {
                    window: start..end,
                    speech_end: end,
                    settled: start + self.0 < samples.len(),
                }
            })
            .collect())
    }
}

// ------------------------------------------------------------------- harness

struct Harness {
    _directory: TempDir,
    microphone: FakeMicrophone,
    keys: Sender<Event>,
    stopping: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<usize>>>,
    socket: PathBuf,
    dictation: PathBuf,
    recordings: PathBuf,
    served: Option<JoinHandle<Result<()>>>,
}

struct Settings {
    preroll_ms: u32,
    postroll_ms: u32,
    interval_ms: u64,
    chunk: Option<usize>,
    words: bool,
    delay: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            preroll_ms: 250,
            postroll_ms: 250,
            interval_ms: 200,
            chunk: None,
            words: true,
            delay: Duration::ZERO,
        }
    }
}

impl Harness {
    fn start(settings: Settings) -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let mut config = Config::default();
        config.audio.preroll_ms = settings.preroll_ms;
        config.audio.postroll_ms = settings.postroll_ms;
        config.recording.dir = root.join("audio");
        config.nvim.terminal = Vec::new();
        config.nvim.editor = [
            "nvim",
            "--headless",
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
        config.validate().expect("harness config is valid");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let microphone = FakeMicrophone::default();
        let capture = AudioCapture::with_backend(
            microphone.clone(),
            config.audio.clone(),
            config.recording.clone(),
        )
        .expect("open the synthetic microphone");
        let worker = Worker::new(Pipeline {
            recognizer: Counting {
                words: settings.words,
                delay: settings.delay,
                calls: Arc::clone(&calls),
            },
            segmenter: settings.chunk.map(Fixed),
        });
        let (keys, key_rx) = mpsc::channel();
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
                    keys: key_rx,
                    worker,
                },
                stop,
                None,
            )
        });
        Self {
            _directory: directory,
            microphone,
            keys,
            stopping,
            calls,
            socket,
            dictation,
            recordings,
            served: Some(served),
        }
    }

    fn send(&self, event: Event) {
        self.keys.send(event).expect("serve is running");
    }
    /// Press, and do not return until the capture is actually running: every
    /// capture opens its recovery WAV the moment it starts, so counting those
    /// files is a precise, cheap signal that speech will now be recorded.
    fn press(&self, latch: bool) {
        let before = self.captures();
        self.press_only(latch);
        wait_until("the capture starts", || self.captures() > before);
    }
    /// A press that ends a latched recording rather than starting one.
    fn press_only(&self, latch: bool) {
        self.send(Event::Down {
            at: Instant::now(),
            latch,
        });
    }
    fn captures(&self) -> usize {
        fs::read_dir(&self.recordings)
            .map(|entries| entries.flatten().count())
            .unwrap_or(0)
    }
    fn release(&self) {
        self.send(Event::Up { at: Instant::now() });
    }
    fn cancel(&self) {
        self.send(Event::Cancel);
    }
    /// Press and release inside the minimum hold, without waiting for it.
    fn tap(&self) {
        let at = Instant::now();
        self.send(Event::Down { at, latch: false });
        self.send(Event::Up {
            at: at + Duration::from_millis(50),
        });
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
        let mut files: Vec<_> = fs::read_dir(&self.recordings)
            .expect("recording directory")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        files.sort();
        read_capture(files.last().expect("a recovery WAV")).expect("readable WAV")
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
    /// Block until the dictation editor is up and pinned to its file.
    fn wait_for_editor(&self) {
        assert_eq!(
            self.ask("luaeval('tostring(Spokenpad ~= nil)')").trim(),
            "true"
        );
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
    }
}

/// Kill the editor this harness spawned, and only that one: its socket path is
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
        // SAFETY: `nvim` is spawned with setsid, so its pid is its process
        // group id; the negated pid signals exactly that group.
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
/// paragraph an earlier utterance already committed.
#[test]
fn release_copies_the_whole_buffer_to_the_clipboard() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
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

/// The keyboard that would end a latched recording is unplugged: its key is
/// already up and its cancel key left with it, so nothing else could stop it.
#[test]
fn losing_the_hotkey_ends_a_latched_recording() {
    if !nvim_available() {
        return;
    }
    let h = Harness::start(Settings::default());
    h.press(true);
    h.release();
    h.say(&tone(1.0));
    h.send(Event::HotkeyLost { at: Instant::now() });
    wait_until("the lost keyboard ends the latch", || !h.text().is_empty());
    assert_eq!(h.text(), "word word\n");
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
    h.release();
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
    h.release();
    // Still transcribing: press again immediately.
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
/// looking -- and on the first press of a session there is no window yet, so
/// the failure has to open one. Setting the notice and returning left a dead
/// key and an empty screen: nothing recorded, nothing said, nowhere.
#[test]
fn a_press_that_cannot_open_the_microphone_opens_the_window_and_says_so() {
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
#[test]
#[ignore = "loads the real CPU models; needs models/ and about ten seconds"]
fn real_models_transcribe_the_kennedy_sample() {
    if !nvim_available() {
        return;
    }
    let models = Path::new("models");
    let sample = models.join("parakeet-tdt-0.6b-v3-int8/test_en.wav");
    if !sample.is_file() || !models.join("silero_vad.onnx").is_file() {
        eprintln!("skipping: models/ is not present in this checkout");
        return;
    }
    let (source, rate) = read_capture(&sample).expect("read the bundled sample");
    let speech = resample(&source, rate, RATE);

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut config = Config::default();
    config.recording.dir = root.join("audio");
    config.nvim.terminal = Vec::new();
    config.nvim.editor = [
        "nvim",
        "--headless",
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
    let worker = Worker::new(Pipeline {
        recognizer: transcriber,
        segmenter: load_segmenter(&config.vad, RATE),
    });
    let microphone = FakeMicrophone::default();
    let capture = AudioCapture::with_backend(
        microphone.clone(),
        config.audio.clone(),
        config.recording.clone(),
    )
    .unwrap();
    let (keys, key_rx) = mpsc::channel();
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let socket = config.nvim.socket_path.clone();
    let dictation = config.nvim.dictation_dir.clone();
    let served = thread::spawn(move || {
        serve(
            &config,
            Devices {
                capture,
                keys: key_rx,
                worker,
            },
            stop,
            None,
        )
    });

    keys.send(Event::Down {
        at: Instant::now(),
        latch: false,
    })
    .unwrap();
    microphone.speak(&speech);
    wait_until("the sample is delivered", || microphone.spoken());
    thread::sleep(Duration::from_millis(100));
    keys.send(Event::Up { at: Instant::now() }).unwrap();

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
    kill_editor(&socket);

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
