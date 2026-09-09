//! Four owners: main/session, input, inference, and editor; recorder owns disk I/O.
use crate::{
    audio::{AudioCapture, Watchdog},
    config::Config,
    decode::{Commit, Pipeline, Recognizer, Tick, Utterance, UtteranceId, Worker},
    hotkey::HotkeyWatcher,
    inference::{Transcriber, load_segmenter},
    nvim::{IndicatorPhase, IndicatorUpdate, NvimSession},
    recorder::RecordingStatus,
    session::Session,
    state::{Command, Event, State},
    text::Processor,
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

enum Work {
    Tick {
        audio: Vec<f32>,
        start: usize,
        utterance: Arc<Utterance>,
    },
    Finish {
        audio: Vec<f32>,
        utterance: Arc<Utterance>,
    },
    Quit,
}
enum ResultEvent {
    Commit(Commit),
    Tick {
        id: UtteranceId,
        result: Result<Tick>,
        elapsed: Duration,
    },
    Finished {
        id: UtteranceId,
        result: Result<(String, usize)>,
        elapsed: Duration,
    },
}
enum EditorWork {
    Ensure,
    Append {
        utterance: Arc<Utterance>,
        text: String,
    },
    Quit,
}
#[derive(Clone, PartialEq)]
struct Indicator {
    phase: &'static str,
    preview: String,
    latched: bool,
    previewing: bool,
    level: f64,
}
impl Indicator {
    fn from_session(s: &Session, level: f32) -> Self {
        Self {
            phase: s.state.phase(),
            preview: s.shown_preview().to_owned(),
            latched: s.state.latched(),
            previewing: s.previewing,
            level: f64::from(level),
        }
    }
}

fn send<T>(tx: &Sender<T>, value: T) -> Result<()> {
    tx.send(value)
        .map_err(|_| anyhow::anyhow!("a background thread stopped unexpectedly"))
}
fn join_bounded(handle: JoinHandle<()>, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        if handle.join().is_err() {
            log::error!("{name} thread panicked");
        }
    } else {
        log::warn!("{name} did not stop within 3s; process exit will release it");
    }
}

fn editor_thread(
    config: crate::config::Nvim,
    rx: Receiver<EditorWork>,
    indicator: Arc<Mutex<Indicator>>,
    stopping: Arc<AtomicBool>,
) {
    let mut nvim = NvimSession::new(config);
    let mut paragraph: Option<UtteranceId> = None;
    let mut previous: Option<Indicator> = None;
    while !stopping.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(66)) {
            Ok(EditorWork::Ensure) => match nvim.ensure() {
                Ok(path) => {
                    log::info!("dictating into {}", path.display());
                    previous = None;
                }
                Err(e) => log::error!("could not open dictation window: {e:#}"),
            },
            Ok(EditorWork::Append { utterance, text }) => {
                if utterance.cancelled() {
                    continue;
                }
                let now = Instant::now();
                match nvim.append(&text, paragraph == Some(utterance.id)) {
                    Ok(line) => {
                        paragraph = Some(utterance.id);
                        previous = None;
                        log::info!(
                            "appended in {:.0}ms (line {line})",
                            now.elapsed().as_secs_f64() * 1000.
                        );
                    }
                    Err(e) => {
                        paragraph = None;
                        log::error!(
                            "append failed: {e:#}; transcript retained in diagnostic log; recover from the capture WAV if available"
                        );
                        log::debug!("undelivered text: {text:?}");
                    }
                }
            }
            Ok(EditorWork::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if nvim.connected() {
            let current = indicator.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if previous.as_ref() != Some(&current) {
                let phase = match current.phase {
                    "recording" => IndicatorPhase::Recording,
                    "transcribing" => IndicatorPhase::Transcribing,
                    _ => IndicatorPhase::Idle,
                };
                if let Err(e) = nvim.set_state(IndicatorUpdate {
                    phase: Some(phase),
                    level: Some(current.level),
                    preview: Some(&current.preview),
                    latched: Some(current.latched),
                    previewing: Some(current.previewing),
                }) {
                    log::warn!("editor indicator unavailable: {e:#}");
                }
                previous = Some(current);
            }
        }
    }
    nvim.close();
}

/// Holds the lock for the daemon lifetime. Never unlink a lock another process may hold.
fn daemon_lock() -> Result<File> {
    let dir = crate::config::state_dir();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("daemon.lock"))?;
    // SAFETY: flock borrows a valid owned file descriptor and retains no pointer.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ensure!(
        result == 0,
        "another spokenpad Rust daemon is running ({})",
        std::io::Error::last_os_error()
    );
    Ok(file)
}

pub fn run(config: Config, dump_dir: Option<&Path>) -> Result<()> {
    let _lock = daemon_lock()?;
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))?;
    log::info!("loading CPU recognizer");
    let mut transcriber = Transcriber::new(&config.asr, config.audio.sample_rate)?;
    transcriber.transcribe(&vec![0.; config.audio.sample_rate as usize])?;
    let segmenter = load_segmenter(&config.vad, config.audio.sample_rate);
    let mut worker = Worker::new(Pipeline {
        recognizer: transcriber,
        segmenter,
    });
    let processor = Processor::new(&config.text)?;
    let mut audio = AudioCapture::new(config.audio.clone(), config.recording.clone())?;
    let (keys_tx, keys) = mpsc::channel();
    let mut watcher = HotkeyWatcher::start(config.hotkey.clone(), keys_tx)
        .context("hotkey watcher could not start")?;
    let (work_tx, work_rx) = mpsc::channel();
    let (result_tx, results) = mpsc::channel();
    let stop = Arc::clone(&stopping);
    let engine = thread::Builder::new()
        .name("spokenpad-asr".into())
        .spawn(move || {
            while let Ok(work) = work_rx.recv() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                match work {
                    Work::Tick {
                        audio,
                        start,
                        utterance,
                    } => {
                        let started = Instant::now();
                        let result = worker.tick(&audio, start, &utterance, |c| {
                            let _ = result_tx.send(ResultEvent::Commit(c));
                        });
                        if result_tx
                            .send(ResultEvent::Tick {
                                id: utterance.id,
                                result,
                                elapsed: started.elapsed(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Work::Finish { audio, utterance } => {
                        let start = Instant::now();
                        let result = worker.finish(&audio, &utterance, |c| {
                            let _ = result_tx.send(ResultEvent::Commit(c));
                        });
                        if result_tx
                            .send(ResultEvent::Finished {
                                id: utterance.id,
                                result,
                                elapsed: start.elapsed(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Work::Quit => break,
                }
            }
        })?;
    let mut session = Session::new(
        config.preview.enabled,
        Duration::from_millis(config.preview.interval_ms),
    );
    let indicator = Arc::new(Mutex::new(Indicator::from_session(&session, 0.)));
    let (editor_tx, editor_rx) = mpsc::channel();
    let editor_config = config.nvim.clone();
    let editor_indicator = Arc::clone(&indicator);
    let stop = Arc::clone(&stopping);
    let editor = thread::Builder::new()
        .name("spokenpad-nvim".into())
        .spawn(move || editor_thread(editor_config, editor_rx, editor_indicator, stop))?;
    log::info!(
        "model ready; listening for key code {}",
        config.hotkey.key_code
    );
    let mut next_health = Instant::now();
    let mut gap_notice = false;
    let mut capped = false;
    let mut tick_failed = false;
    let result = (|| -> Result<()> {
        while !stopping.load(Ordering::Acquire) {
            // Give key release/cancel priority over delivery of inference results.
            for event in keys.try_iter().take(128) {
                let before = session.state;
                let command = session.event(event);
                match command {
                    Command::Start => {
                        gap_notice = false;
                        capped = false;
                        tick_failed = false;
                        if let Err(e) = audio.start_capture() {
                            session.event(Event::Cancel);
                            log::error!("capture could not start: {e:#}");
                            continue;
                        }
                        send(&editor_tx, EditorWork::Ensure)?;
                    }
                    Command::Decode => {
                        let samples = audio.stop_capture();
                        let held = match before {
                            State::Recording { started, .. } => Instant::now()
                                .saturating_duration_since(started)
                                .as_secs_f64(),
                            _ => 0.,
                        };
                        let seconds = samples.len() as f64 / f64::from(config.audio.sample_rate);
                        let peak = samples.iter().fold(0_f32, |a, &s| a.max(s.abs()));
                        log::info!(
                            "captured {held:.1}s held -> {seconds:.1}s audio ({} samples), peak={peak:.4}",
                            samples.len()
                        );
                        log_recording(audio.recording_status());
                        if audio.take_cap_notice() {
                            session.cap(cap_message(audio.recording_status()));
                        }
                        if held > 1. && seconds < held * 0.5 {
                            log::error!(
                                "microphone delivered less than half the expected audio; capture is incomplete"
                            );
                        }
                        if peak < 0.01 {
                            log::warn!("capture is nearly silent; check microphone gain/device");
                        }
                        if let Some(dir) = dump_dir {
                            let path = dir.join(format!(
                                "capture-{}.wav",
                                chrono::Local::now().format("%Y-%m-%d-%H%M%S-%f")
                            ));
                            if let Err(e) = crate::recorder::dump_capture(
                                &path,
                                &samples,
                                config.audio.sample_rate,
                            ) {
                                log::error!("could not dump capture: {e:#}");
                            }
                        }
                        let utterance = session
                            .current
                            .as_ref()
                            .context("capture has no utterance")?
                            .clone();
                        send(
                            &work_tx,
                            Work::Finish {
                                audio: samples,
                                utterance,
                            },
                        )?;
                    }
                    Command::Discard => {
                        audio.stop_capture();
                        log::info!("capture cancelled or too short; settled text and WAV retained");
                    }
                    Command::Abort => log::info!("decode cancelled; settled text and WAV retained"),
                    Command::None => {}
                }
            }
            for event in results.try_iter().take(128) {
                match event {
                    ResultEvent::Commit(c) => {
                        if !session.accept_commit(&c) {
                            continue;
                        }
                        let text = processor.process(&c.text);
                        log::debug!(
                            "utterance {} committed through {}: {text:?}",
                            c.utterance.id.0,
                            c.through
                        );
                        if !text.trim().is_empty() {
                            send(
                                &editor_tx,
                                EditorWork::Append {
                                    utterance: c.utterance,
                                    text,
                                },
                            )?;
                        }
                    }
                    ResultEvent::Tick {
                        id,
                        result,
                        elapsed,
                    } => {
                        match &result {
                            Ok(Tick::Preview { text, through }) => log::debug!(
                                "preview {}: {} characters, committed through {}, decoded in {:.2}s",
                                id.0,
                                text.chars().count(),
                                through,
                                elapsed.as_secs_f64()
                            ),
                            Ok(Tick::Stopped) => log::debug!(
                                "preview {} stopped after {:.2}s",
                                id.0,
                                elapsed.as_secs_f64()
                            ),
                            Err(_) => {}
                        }
                        let tick = match result {
                            Ok(Tick::Preview { text, through }) => Some(Tick::Preview {
                                text: processor.process(&text),
                                through,
                            }),
                            Ok(Tick::Stopped) => Some(Tick::Stopped),
                            Err(e) => {
                                if session.is_current(id) && !tick_failed {
                                    log::warn!("preview tick failed: {e:#}; will retry");
                                    tick_failed = true;
                                }
                                None
                            }
                        };
                        session.tick_finished(id, tick, Instant::now());
                    }
                    ResultEvent::Finished {
                        id,
                        result,
                        elapsed,
                    } => {
                        match result {
                            Ok((text, frames)) => {
                                log::info!(
                                    "decoded the last {:.1}s in {:.2}s (utterance {})",
                                    frames as f64 / f64::from(config.audio.sample_rate),
                                    elapsed.as_secs_f64(),
                                    id.0
                                );
                                log::debug!("release transcript: {text:?}");
                            }
                            Err(e) => log::error!("decode failed for utterance {}: {e:#}", id.0),
                        }
                        session.finish(id);
                    }
                }
            }
            let now = Instant::now();
            if now >= next_health {
                next_health = now + Duration::from_millis(100);
                match audio.watchdog() {
                    Ok(Watchdog::Restarted {
                        gap_seconds,
                        during_capture,
                    }) => {
                        log::warn!("microphone restarted after {gap_seconds:.1}s without audio");
                        if during_capture {
                            gap_notice = true;
                            session.warn("microphone stopped delivering audio; reopened, but this recording has a gap".into());
                        }
                    }
                    Err(e) => {
                        log::error!("microphone recovery failed: {e:#}");
                        next_health = now + Duration::from_secs(2);
                        if session.state.recording() {
                            gap_notice = true;
                            session.warn("microphone unavailable; audio is missing".into());
                        }
                    }
                    Ok(Watchdog::Idle | Watchdog::Healthy) => {}
                }
                if let Some(status) = audio.take_stream_status() {
                    log::warn!("PortAudio: {status}");
                }
                if audio.take_cap_notice() {
                    capped = true;
                    let message = cap_message(audio.recording_status());
                    log::warn!("{message}");
                    session.cap(message);
                }
                // A writer can fail after the cap; never leave a false recovery promise.
                if capped && !gap_notice {
                    session.warn(cap_message(audio.recording_status()));
                }
                if gap_notice {
                    session.warn(
                        "microphone stopped delivering audio; this recording has a gap".into(),
                    );
                }
            }
            if session.tick_due(now) {
                let samples = audio.snapshot_capture(session.committed_hint);
                if samples.len() as f64 / f64::from(config.audio.sample_rate)
                    > config.preview.max_seconds
                {
                    session.pause_previews();
                    log::warn!(
                        "preview paused: uncommitted audio exceeds preview.max_seconds; still recording"
                    );
                } else {
                    let utterance = session
                        .current
                        .as_ref()
                        .context("recording has no utterance")?
                        .clone();
                    session.requested(now);
                    log::debug!(
                        "preview {} requested: {:.2}s audio starting at {}",
                        utterance.id.0,
                        samples.len() as f64 / f64::from(config.audio.sample_rate),
                        session.committed_hint
                    );
                    send(
                        &work_tx,
                        Work::Tick {
                            audio: samples,
                            start: session.committed_hint,
                            utterance,
                        },
                    )?;
                }
            }
            *indicator.lock().unwrap_or_else(|e| e.into_inner()) = Indicator::from_session(
                &session,
                if session.state.recording() {
                    audio.level()
                } else {
                    0.
                },
            );
            if watcher.is_finished() || engine.is_finished() || editor.is_finished() {
                anyhow::bail!("a required worker stopped unexpectedly");
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })();
    watcher.stop();
    if let Some(u) = &session.current {
        u.cancel();
    }
    stopping.store(true, Ordering::Release);
    audio.shutdown();
    let _ = work_tx.send(Work::Quit);
    let _ = editor_tx.send(EditorWork::Quit);
    join_bounded(engine, "inference");
    join_bounded(editor, "editor");
    result
}

fn log_recording(status: RecordingStatus) {
    match status {
        RecordingStatus::Recorded(p) => log::info!("recorded to {}", p.display()),
        RecordingStatus::Truncated(p) => log::error!("recording {} is INCOMPLETE", p.display()),
        RecordingStatus::NotRecorded => log::warn!("no recovery WAV for this capture"),
    }
}
fn cap_message(status: RecordingStatus) -> String {
    match status {
    RecordingStatus::Recorded(p)=>format!("past the 60-minute memory limit; remaining audio is in {} — recover with spokenpad transcribe",p.display()),
    RecordingStatus::Truncated(_)=>"past the 60-minute memory limit AND recording failed; stop and start a new recording".into(),
    RecordingStatus::NotRecorded=>"past the 60-minute memory limit with no recovery recording; new audio is being discarded".into(),
}
}
