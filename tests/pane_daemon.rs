//! P2: the daemon in `nvim.mode = "pane"`, end to end.
//!
//! The real `shell::daemon::serve`, with a synthetic microphone and a
//! recogniser that always says the same thing, on an Xvfb and an i3 this test
//! starts itself. What it asserts is the wiring, not the drawing —
//! `tests/pane_render.rs` covers the grid:
//!
//! - with no `DISPLAY` the daemon says so and the transcript goes to the
//!   pending passage, which is what happens when a terminal is missing too;
//! - with one, a dictation opens a window that i3 does not focus, and the
//!   text lands in the dictation file;
//! - closing that window ends the passage: the next dictation opens a new
//!   window on a new file;
//! - stopping the daemon leaves no window and no editor behind.
//!
//! One test, in that order, because it is one story: the same daemon code
//! with and without a display to open on.
mod harness;

use anyhow::Result;
use harness::{I3, SETTLE, XServer, close_window, wait_for};
use spokenpad::{
    config::{Config, Mode},
    core::{
        control::{Received, Request},
        decode::{Pipeline, Recognizer, Segmenter, TrailingSilence, Worker},
    },
    shell::{
        audio::{AudioCapture, CallbackCore, InputBackend, InputStream, Teardown},
        daemon::{Devices, serve},
    },
};
use std::{
    collections::VecDeque,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// What the stand-in recogniser says for every capture.
const SPOKEN: &str = "Der Pane hoert zu";
const PATIENCE: Duration = Duration::from_secs(20);

#[test]
fn the_daemon_opens_a_pane_dictates_into_it_and_cleans_up() {
    if !harness::tools_or_skip(&["Xvfb", "i3", "nvim", "fc-match"]) {
        return;
    }

    // ---------------------------------------- with no display at all
    // The state a systemd user service is in when nobody imported DISPLAY.
    // Said in the configuration rather than in the environment: the daemon
    // reads `$DISPLAY` once, where it reads everything else, so a test can
    // say what it means instead of mutating a process-global.
    let blind = Daemon::start(None);
    let _ = blind.dictate();
    let pending = wait_for(
        PATIENCE,
        "the transcript to reach the pending passage",
        || blind.dictation_files().into_iter().next(),
    );
    let text = std::fs::read_to_string(&pending).expect("read the pending passage");
    assert!(
        text.contains(SPOKEN),
        "with no display the transcript must still be kept:\n{text}"
    );
    blind.stop();

    // ---------------------------------------- with one
    let server = XServer::start();
    let i3 = I3::start(&server);
    let base = server.plain_window();
    i3.wait_until_managed(base);
    std::thread::sleep(SETTLE);
    assert!(
        i3.node(base).expect("the plain window").focused,
        "the plain window should hold the focus before the pane opens"
    );
    let daemon = Daemon::start(Some(server.display.clone()));
    let before = resident_kilobytes();
    let released = daemon.dictate();
    let first = wait_for(PATIENCE, "the transcript to reach a dictation file", || {
        daemon
            .dictation_files()
            .into_iter()
            .find(|path| contains(path, SPOKEN))
    });
    let landed = released.elapsed();
    let after = resident_kilobytes();
    println!(
        "key-up to the first transcript in the file, opening a pane on the way: {} ms; \
         resident memory {before} kB -> {after} kB ({} kB for the window, its font and its editor's client)",
        landed.as_millis(),
        after.saturating_sub(before)
    );
    let pane = wait_for(PATIENCE, "i3 to manage the pane", || pane_window(&i3));
    std::thread::sleep(SETTLE);
    let node = i3.node(pane).expect("the pane");
    assert!(!node.focused, "the daemon's pane took the focus");
    assert!(node.floating, "the daemon's pane is not floating");
    assert!(
        i3.node(base).expect("the plain window").focused,
        "the focus moved away from the window that had it"
    );
    assert_eq!(
        server.input_focus(),
        base,
        "the X input focus moved when the pane opened"
    );
    println!(
        "the daemon opened a pane on {}, unfocused and floating",
        first.display()
    );

    // ---------------------------------------- the clipboard copy
    // `nvim.copy_to_clipboard` goes through Neovim's own provider, so pane
    // mode needs nothing of its own for it. Read back from this test's X
    // server, never the user's: the pane's editor inherited this DISPLAY.
    match clipboard(&server.display) {
        Some(clipboard) => {
            assert!(
                clipboard.contains(SPOKEN),
                "the clipboard copy did not reach this display's selection: {clipboard:?}"
            );
            println!(
                "the release copied the buffer to the clipboard on {}",
                server.display
            );
        }
        None => println!(
            "no clipboard provider on PATH: the copy could not be read back (not a failure)"
        ),
    }

    // ---------------------------------------- closing it ends the passage
    close_window(&server, pane);
    wait_for(PATIENCE, "the pane to go away", || {
        pane_window(&i3).is_none().then_some(())
    });
    let _ = daemon.dictate();
    let second = wait_for(PATIENCE, "a second dictation file", || {
        daemon
            .dictation_files()
            .into_iter()
            .find(|path| *path != first && contains(path, SPOKEN))
    });
    assert_ne!(first, second);
    wait_for(PATIENCE, "i3 to manage the second pane", || {
        pane_window(&i3)
    });
    println!(
        "closing the window ended the passage: the next dictation opened {}",
        second.display()
    );

    // ---------------------------------------- and shutting down cleans up
    let socket = daemon.socket.clone();
    daemon.stop();
    assert!(
        UnixStream::connect(&socket).is_err(),
        "the editor is still listening on {} after the daemon stopped",
        socket.display()
    );
    wait_for(PATIENCE, "the pane's window to be gone", || {
        pane_window(&i3).is_none().then_some(())
    });
    println!("stopping the daemon left no window and no editor behind");
}

/// What the clipboard selection holds on one display, when a tool to read it
/// is installed. Never the user's display: the caller passes the one this
/// test started.
fn clipboard(display: &str) -> Option<String> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let output = std::process::Command::new("xclip")
            .args(["-display", display, "-selection", "clipboard", "-o"])
            .output()
            .ok()?;
        if output.status.success()
            && let Ok(text) = String::from_utf8(output.stdout)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// This process's resident set, in kilobytes, from `/proc`.
fn resident_kilobytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    pages * 4
}

fn contains(path: &Path, text: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|content| content.contains(text))
}

/// The pane's window, as i3 sees it. The pane names itself
/// `spokenpad-pane`, which no rule of the user's can match.
fn pane_window(i3: &I3) -> Option<x11rb::protocol::xproto::Window> {
    fn walk(node: &serde_json::Value, found: &mut Option<u32>) {
        let class = node
            .get("window_properties")
            .and_then(|properties| properties.get("instance"))
            .and_then(serde_json::Value::as_str);
        if class == Some(spokenpad::shell::pane::x11::INSTANCE)
            && let Some(id) = node.get("window").and_then(serde_json::Value::as_u64)
        {
            *found = Some(id as u32);
            return;
        }
        for key in ["nodes", "floating_nodes"] {
            for child in node
                .get(key)
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                if found.is_none() {
                    walk(child, found);
                }
            }
        }
    }
    let mut found = None;
    walk(&i3.tree(), &mut found);
    found
}

// ------------------------------------------------------------- the daemon

struct Daemon {
    requests: Sender<Received>,
    microphone: Microphone,
    stopping: Arc<AtomicBool>,
    served: Option<JoinHandle<Result<()>>>,
    dictation: PathBuf,
    socket: PathBuf,
    _directory: tempfile::TempDir,
}

impl Daemon {
    fn start(display: Option<String>) -> Self {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let root = directory.path();
        let mut config = Config::default();
        config.nvim.mode = Mode::Pane;
        config.nvim.notify = false;
        config.nvim.display = display;
        // The editor's own provider does this — xclip, xsel or wl-copy,
        // inside nvim. It writes to whichever display that nvim is on, which
        // here is the Xvfb this test started.
        config.nvim.copy_to_clipboard = true;
        config.nvim.socket_path = root.join("nvim.sock");
        config.nvim.dictation_dir = root.join("dictation");
        config.nvim.startup_timeout_s = 15.0;
        // The bundled configuration, read from the repository rather than
        // materialised into the user's state directory.
        config.nvim.init = Some(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/lua/dictation_init.lua"
        )));
        config.recording.dir = root.join("audio");
        config.recording.enabled = false;
        config.preview.enabled = false;
        config.validate().expect("the test configuration is valid");

        let microphone = Microphone::default();
        let capture = AudioCapture::with_backend(
            microphone.clone(),
            config.audio.clone(),
            config.recording.clone(),
        )
        .expect("open the synthetic microphone");
        let worker = Worker::new(Pipeline {
            recognizer: Fixed,
            segmenter: None::<Never>,
        });
        let (requests, received) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let dictation = config.nvim.dictation_dir.clone();
        let socket = config.nvim.socket_path.clone();
        let served = thread::spawn(move || {
            serve(
                &config,
                Devices {
                    capture,
                    requests: received,
                    worker,
                },
                stop,
                None,
            )
        });
        Self {
            requests,
            microphone,
            stopping,
            served: Some(served),
            dictation,
            socket,
            _directory: directory,
        }
    }

    /// One push-to-talk: press, speak, release. Returns the moment the key
    /// came up, which is when the daemon starts working.
    fn dictate(&self) -> Instant {
        self.send(Request::Start);
        self.microphone.speak(&vec![0.4_f32; 16_000]);
        wait_for(PATIENCE, "the microphone to be drained", || {
            self.microphone.spoken().then_some(())
        });
        self.send(Request::Stop);
        Instant::now()
    }

    fn send(&self, request: Request) {
        self.requests
            .send(Received {
                request,
                at: Instant::now(),
            })
            .expect("the daemon is listening");
    }

    fn dictation_files(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.dictation)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        found.sort();
        found
    }

    fn stop(mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        drop(std::mem::replace(&mut self.requests, mpsc::channel().0));
        if let Some(served) = self.served.take() {
            let _ = served.join().expect("the daemon thread did not panic");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Some(served) = self.served.take() {
            let _ = served.join();
        }
    }
}

// ------------------------------------------------------------ fake devices

/// A microphone whose mouth the test fills. It delivers 20 ms at a time and
/// pads with silence, as a real device does.
#[derive(Clone, Default)]
struct Microphone {
    mouth: Arc<Mutex<VecDeque<f32>>>,
}

impl Microphone {
    fn speak(&self, samples: &[f32]) {
        self.mouth.lock().expect("the mouth").extend(samples);
    }

    fn spoken(&self) -> bool {
        self.mouth.lock().expect("the mouth").is_empty()
    }
}

struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl InputStream for Stream {
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

impl InputBackend for Microphone {
    type Stream = Stream;

    fn open(&self, config: &spokenpad::config::Audio, core: Arc<CallbackCore>) -> Result<Stream> {
        let stop = Arc::new(AtomicBool::new(false));
        let frames = (config.sample_rate as usize / 50).max(1);
        let mouth = Arc::clone(&self.mouth);
        let flag = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let mut buffer = vec![0.0_f32; frames];
            while !flag.load(Ordering::Relaxed) {
                buffer.fill(0.0);
                {
                    let mut queue = mouth.lock().expect("the mouth");
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
        Ok(Stream {
            stop,
            thread: Some(thread),
        })
    }
}

/// Always the same sentence: this test is about where the text goes, not
/// about what it says.
struct Fixed;

impl Recognizer for Fixed {
    fn transcribe(&mut self, _samples: &[f32], _trailing: TrailingSilence) -> Result<String> {
        Ok(SPOKEN.to_owned())
    }
}

/// No segmenter: the whole capture is decoded when the key comes up, which is
/// the simplest path through the pipeline.
struct Never;

impl Segmenter for Never {
    fn split(&mut self, _samples: &[f32]) -> Result<spokenpad::core::decode::Split> {
        unreachable!("this test runs without a segmenter")
    }
}
