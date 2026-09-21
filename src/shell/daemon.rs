//! Four owners: main/session, input, inference, and editor; recorder owns disk I/O.
//!
//! [`run`] is the imperative shell: it takes the per-user lock, registers
//! signals, opens PortAudio and evdev, and loads the models. [`serve`] is the
//! event loop over whatever devices it is handed, which is what the headless
//! end-to-end tests drive.
use crate::{
    config::Config,
    core::{
        decode::{
            Commit, Pipeline, Preview, Recognizer, Segmenter, Utterance, UtteranceId, Worker,
        },
        frames::Frames,
        session::{Notice, RecordingStatus, Session},
        state::{Command, DiscardReason, Event, State},
        text::Processor,
    },
    shell::{
        audio::{AudioCapture, CaptureEvent, InputBackend, PostRoll},
        hotkey::HotkeyWatcher,
        inference::{Transcriber, load_segmenter},
        nvim::{AppendFailure, CopyOutcome, IndicatorState, NvimSession},
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    fmt,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long the shutdown waits for a background thread, and therefore how
/// long queued editor appends have to reach the file.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const LOOP_INTERVAL: Duration = Duration::from_millis(20);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Below this a capture that is much shorter than the hold is reported.
const INCOMPLETE_HOLD: Duration = Duration::from_secs(1);
const NEARLY_SILENT_PEAK: f32 = 0.01;

/// The hotkey device could not be opened. systemd treats exit code 3 as a
/// permanent failure, so this is matched by type, never by message.
#[derive(Debug)]
pub struct HotkeyUnavailable;

impl fmt::Display for HotkeyUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("hotkey watcher could not start")
    }
}

impl std::error::Error for HotkeyUnavailable {}

/// Everything the event loop reads from the outside world.
pub struct Devices<B: InputBackend, R, S> {
    pub capture: AudioCapture<B>,
    /// Key events. The sender being dropped means the input source ended.
    pub keys: Receiver<Event>,
    pub worker: Worker<R, S>,
}

/// Key events in arrival order. A release's post-roll ends as soon as a key
/// press is waiting; it looks by moving every received event into `early`,
/// which the loop drains before receiving anything newer, so nothing is lost
/// or reordered.
struct Keys {
    receiver: Receiver<Event>,
    early: VecDeque<Event>,
    /// False once the sender is gone: the input source ended.
    alive: bool,
}

impl Keys {
    fn new(receiver: Receiver<Event>) -> Self {
        Self {
            receiver,
            early: VecDeque::new(),
            alive: true,
        }
    }

    fn next(&mut self) -> Option<Event> {
        self.early.pop_front().or_else(|| self.receive())
    }

    /// Whether a key press is waiting, or no event can ever arrive again.
    /// Only a press would start a new capture: the key-up of the press that
    /// stopped a latched recording, a cancel, or a lost keyboard must not
    /// shorten the post-roll of the capture that is ending.
    fn press_waiting(&mut self) -> bool {
        while let Some(event) = self.receive() {
            self.early.push_back(event);
        }
        self.early
            .iter()
            .any(|event| matches!(event, Event::Down { .. }))
            || !self.alive
    }

    /// No event is queued and none can arrive again. Disconnection alone is
    /// not enough: `press_waiting` may have seen it while events it moved into
    /// `early` are still owed to the loop.
    fn exhausted(&self) -> bool {
        !self.alive && self.early.is_empty()
    }

    fn receive(&mut self) -> Option<Event> {
        match self.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.alive = false;
                None
            }
        }
    }
}

enum Work {
    Tick {
        audio: Vec<f32>,
        start: Frames,
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
        result: Result<Option<Preview>>,
        elapsed: Duration,
    },
    Finished {
        id: UtteranceId,
        result: Result<(String, usize)>,
        elapsed: Duration,
    },
}

/// Everything the editor's winbar shows, replaced wholesale, never merged.
fn indicator_of(session: &Session, level: f32) -> IndicatorState {
    IndicatorState {
        phase: session.state.indicator_phase(),
        level: f64::from(level),
        preview: session.preview().to_owned(),
        notice: session.notice().map(Notice::text),
        latched: session.state.latched(),
        previewing: session.previewing(),
    }
}

enum EditorWork {
    Ensure,
    Append {
        utterance: UtteranceId,
        text: String,
    },
    Indicator(IndicatorState),
    /// Copy the whole dictation buffer to `+`. Sent once per utterance, after
    /// its `Finished` result -- see the comment where it is sent, in
    /// `serve`, for why every `Append` of that utterance is already ahead of
    /// it in this same queue.
    Copy,
    Quit,
}

fn send<T>(tx: &Sender<T>, value: T) -> Result<()> {
    tx.send(value)
        .map_err(|_| anyhow::anyhow!("a background thread stopped unexpectedly"))
}

fn join_bounded(handle: JoinHandle<()>, name: &str) {
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        if handle.join().is_err() {
            log::error!("{name} thread panicked");
        }
    } else {
        log::warn!(
            "{name} did not stop within {}s; process exit will release it",
            SHUTDOWN_GRACE.as_secs()
        );
    }
}

/// Owns the editor connection. It runs until its channel says to stop, never
/// on a shared flag: text already queued must reach the file even while the
/// daemon is shutting down or the capture it came from was cancelled.
/// Where an utterance's last commit went. A later commit of the same
/// utterance continues its line only in the same place: joined onto another
/// file's last line, it would run into unrelated text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sink {
    Editor,
    Passage,
}

fn editor_thread(config: crate::config::Nvim, rx: Receiver<EditorWork>) {
    let mut nvim = NvimSession::new(config);
    let mut paragraph: Option<(UtteranceId, Sink)> = None;
    let mut shown: Option<IndicatorState> = None;
    let mut quit = false;
    while !quit {
        let first = match rx.recv_timeout(Duration::from_millis(66)) {
            Ok(work) => Some(work),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Drain what is already queued, keeping only the newest indicator:
        // the loop offers one 50 times a second and only the last is true.
        let mut indicator = None;
        for work in first.into_iter().chain(rx.try_iter()) {
            match work {
                EditorWork::Indicator(next) => indicator = Some(next),
                EditorWork::Ensure => match nvim.ensure() {
                    Ok(Some(path)) => {
                        log::info!("dictating into {}", path.display());
                        shown = None;
                    }
                    Ok(None) => log::info!(
                        "no dictation editor is open; text goes to the dictation file until `spokenpad editor` opens it"
                    ),
                    Err(e) => log::error!("could not open dictation window: {e:#}"),
                },
                EditorWork::Append { utterance, text } => {
                    // A failed request elsewhere (a clipboard copy that timed
                    // out, an indicator push) drops the connection; text that
                    // is owed to the file reattaches rather than waiting for
                    // the next key-down to do it.
                    if !nvim.connected()
                        && let Err(e) = nvim.ensure()
                    {
                        log::warn!("could not reattach to the dictation window: {e:#}");
                    }
                    if nvim.connected() {
                        let now = Instant::now();
                        let continued = paragraph == Some((utterance, Sink::Editor));
                        match nvim.append(&text, continued) {
                            Ok(line) => {
                                paragraph = Some((utterance, Sink::Editor));
                                shown = None;
                                log::info!(
                                    "appended in {:.0}ms (line {line})",
                                    now.elapsed().as_secs_f64() * 1000.
                                );
                                continue;
                            }
                            // The editor went away before it got the text —
                            // closed while the tail was decoding — so the
                            // text is certainly not there and goes to the
                            // file below.
                            Err(AppendFailure::NotSent(e)) => {
                                log::warn!("the dictation editor did not receive the text: {e:#}");
                            }
                            // The editor may hold the text already; writing
                            // it anywhere else could write it twice.
                            Err(AppendFailure::Unconfirmed(e)) => {
                                paragraph = None;
                                log::error!(
                                    "append failed: {e:#}; transcript retained in diagnostic log; recover from the capture WAV if available"
                                );
                                log::debug!("undelivered text: {text:?}");
                                continue;
                            }
                        }
                    }
                    // With no editor at all, the text goes to the file the
                    // next editor will open on, rather than nowhere.
                    let continued = paragraph == Some((utterance, Sink::Passage));
                    match nvim.append_detached(&text, continued) {
                        Ok(write) => {
                            paragraph = Some((utterance, Sink::Passage));
                            log::info!("no dictation editor: appended to {}", write.path.display());
                            if write.started {
                                nvim.notify_detached(&write.path);
                            }
                        }
                        Err(e) => {
                            paragraph = None;
                            log::error!(
                                "could not write the transcript to a dictation file: {e:#}; recover from the capture WAV if available"
                            );
                            log::debug!("undelivered text: {text:?}");
                        }
                    }
                }
                EditorWork::Copy => {
                    // Never on a session with no editor: nothing has been
                    // spoken into a window that was never opened, and there
                    // is nothing to copy.
                    if nvim.connected() {
                        match nvim.copy_buffer() {
                            Ok(CopyOutcome::Copied) => {
                                log::debug!("copied the dictation buffer to the clipboard")
                            }
                            Ok(CopyOutcome::Empty) => {}
                            Ok(CopyOutcome::Failed(reason)) => {
                                log::warn!("clipboard copy failed: {reason}")
                            }
                            Err(e) => log::warn!("clipboard copy unavailable: {e:#}"),
                        }
                    }
                }
                EditorWork::Quit => {
                    quit = true;
                    break;
                }
            }
        }
        if nvim.connected()
            && let Some(next) = indicator
            && shown.as_ref() != Some(&next)
        {
            if let Err(e) = nvim.set_indicator(&next) {
                log::warn!("editor indicator unavailable: {e:#}");
            }
            shown = Some(next);
        }
    }
    // Nothing can be written after this point; say what was lost.
    for work in rx.try_iter() {
        if let EditorWork::Append { text, .. } = work {
            log::error!(
                "shutting down with {} characters of undelivered transcript; recover from the capture WAV",
                text.chars().count()
            );
            log::debug!("undelivered text: {text:?}");
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

/// The imperative shell: acquire the machine's resources, then serve.
pub fn run(config: Config, dump_dir: Option<&Path>) -> Result<()> {
    let _lock = daemon_lock()?;
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))?;
    log::info!("loading CPU recognizer");
    let mut transcriber = Transcriber::new(&config.asr, config.audio.sample_rate)?;
    transcriber.warm_up()?;
    let worker = Worker::new(Pipeline {
        recognizer: transcriber,
        segmenter: load_segmenter(&config.vad, config.audio.sample_rate),
    });
    let capture = AudioCapture::new(config.audio.clone(), config.recording.clone())?;
    let (keys_tx, keys) = mpsc::channel();
    let mut watcher =
        HotkeyWatcher::start(config.hotkey.clone(), keys_tx).context(HotkeyUnavailable)?;
    log::info!(
        "model ready; listening for key code {}",
        config.hotkey.key_code
    );
    let result = serve(
        &config,
        Devices {
            capture,
            keys,
            worker,
        },
        stopping,
        dump_dir,
    );
    watcher.stop();
    result
}

/// The event loop. Touches no process-global state: no lock, no signals, no
/// environment, and every path it uses comes from `config`.
pub fn serve<B, R, S>(
    config: &Config,
    devices: Devices<B, R, S>,
    stopping: Arc<AtomicBool>,
    dump_dir: Option<&Path>,
) -> Result<()>
where
    B: InputBackend,
    R: Recognizer + Send + 'static,
    S: Segmenter + Send + 'static,
{
    let Devices {
        mut capture,
        keys,
        mut worker,
    } = devices;
    let mut keys = Keys::new(keys);
    let rate = config.audio.sample_rate;
    // Without a segmenter nothing ever settles, so a preview would re-decode
    // the whole growing capture. That is the one thing this project refuses.
    let previews = config.preview.enabled && worker.pipeline.segmenter.is_some();
    if config.preview.enabled && !previews {
        log::warn!("no VAD model: previews are off and the capture is decoded at release");
    }
    let processor = Processor::new(&config.text)?;

    let (work_tx, work_rx) = mpsc::channel();
    let (result_tx, results) = mpsc::channel();
    let engine = thread::Builder::new()
        .name("spokenpad-asr".into())
        .spawn(move || engine_thread(&mut worker, &work_rx, &result_tx))?;

    let mut session = Session::new(previews, Duration::from_millis(config.preview.interval_ms));
    let (editor_tx, editor_rx) = mpsc::channel();
    let editor_config = config.nvim.clone();
    let editor = thread::Builder::new()
        .name("spokenpad-nvim".into())
        .spawn(move || editor_thread(editor_config, editor_rx))?;

    let mut next_device_poll = Instant::now();
    let result = (|| -> Result<()> {
        while !stopping.load(Ordering::Acquire) {
            // Give key release/cancel priority over delivery of inference results.
            for _ in 0..128 {
                let Some(event) = keys.next() else {
                    break;
                };
                match session.event(event) {
                    Command::Start => {
                        if let Err(e) = capture.start_capture() {
                            // The press cleared the previous notice; cancel
                            // first, then say why nothing is being recorded.
                            session.event(Event::Cancel);
                            session.notify(Notice::MicrophoneUnavailable);
                            log::error!("capture could not start: {e:#}");
                        }
                        // Either way: on the first press of a session there is
                        // no editor yet, and a notice pushed at a window that
                        // does not exist leaves the user with a dead key and
                        // no explanation anywhere.
                        send(&editor_tx, EditorWork::Ensure)?;
                    }
                    Command::Decode => {
                        // The winbar says "transcribing" from the key-up on,
                        // not only once the post-roll has arrived.
                        send(
                            &editor_tx,
                            EditorWork::Indicator(indicator_of(&session, 0.)),
                        )?;
                        let (samples, postroll) = capture.finish_capture(|| {
                            stopping.load(Ordering::Acquire) || keys.press_waiting()
                        });
                        note_postroll(postroll, &mut session);
                        for event in capture.poll() {
                            apply_capture_event(event, &mut session);
                        }
                        release(&mut session, &capture, &samples, config, dump_dir, &work_tx)?;
                    }
                    Command::Discard(reason) => {
                        capture.stop_capture();
                        match reason {
                            DiscardReason::TooShort => log::info!(
                                "capture held under {}ms; discarded, WAV retained",
                                crate::core::state::MINIMUM_HOLD.as_millis()
                            ),
                            DiscardReason::Cancelled => {
                                log::info!("capture cancelled; settled text and WAV retained")
                            }
                        }
                    }
                    Command::Nothing => {}
                }
            }
            for event in results.try_iter().take(128) {
                match event {
                    ResultEvent::Commit(c) => commit(c, &mut session, &processor, &editor_tx)?,
                    ResultEvent::Tick {
                        id,
                        result,
                        elapsed,
                    } => {
                        let preview = match result {
                            Ok(Some(Preview { text, through })) => {
                                log::debug!(
                                    "preview {}: {} characters, committed through {through}, decoded in {:.2}s",
                                    id.0,
                                    text.chars().count(),
                                    elapsed.as_secs_f64()
                                );
                                Some(Preview {
                                    text: processor.process(&text),
                                    through,
                                })
                            }
                            Ok(None) => {
                                log::debug!(
                                    "preview {} stopped after {:.2}s",
                                    id.0,
                                    elapsed.as_secs_f64()
                                );
                                None
                            }
                            Err(e) => {
                                if session.is_current(id) && session.should_warn_tick_failure() {
                                    log::warn!("preview tick failed: {e:#}; will retry");
                                }
                                None
                            }
                        };
                        session.tick_finished(id, preview, Instant::now());
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
                                    Frames(frames).seconds(rate),
                                    elapsed.as_secs_f64(),
                                    id.0
                                );
                                log::debug!("release transcript: {text:?}");
                            }
                            Err(e) => log::error!("decode failed for utterance {}: {e:#}", id.0),
                        }
                        session.finish(id);
                        // Every `Commit` of this utterance was sent on this
                        // same `results` channel, synchronously, before the
                        // engine thread sent `Finished` -- see
                        // `engine_thread`'s `Work::Finish` arm. The `for`
                        // loop above therefore already turned each of them
                        // into an `EditorWork::Append` and queued it on
                        // `editor_tx` ahead of the copy queued here, so the
                        // editor always writes the append before it reads
                        // the buffer to copy. Sent on every `Finished`,
                        // whatever the release decode did: progressively
                        // committed text is in the buffer either way.
                        send(&editor_tx, EditorWork::Copy)?;
                    }
                }
            }
            let now = Instant::now();
            if now >= next_device_poll {
                next_device_poll = now + DEVICE_POLL_INTERVAL;
                for event in capture.poll() {
                    apply_capture_event(event, &mut session);
                }
            }
            if session.tick_due(now) {
                let tail = capture.captured_frames().since(session.committed_hint);
                if Frames(tail).seconds(rate) > config.preview.max_seconds {
                    log::warn!(
                        "preview paused: uncommitted audio exceeds preview.max_seconds; still recording"
                    );
                    session.defer_previews(now + Duration::from_millis(config.preview.interval_ms));
                } else {
                    session.resume_previews();
                    let utterance = session
                        .current
                        .as_ref()
                        .context("recording has no utterance")?
                        .clone();
                    let start = session.committed_hint;
                    let audio = capture.snapshot_capture(start);
                    session.requested(now);
                    log::debug!(
                        "preview {} requested: {:.2}s audio starting at {start}",
                        utterance.id.0,
                        Frames(audio.len()).seconds(rate)
                    );
                    send(
                        &work_tx,
                        Work::Tick {
                            audio,
                            start,
                            utterance,
                        },
                    )?;
                }
            }
            let level = if session.state.recording() {
                capture.level()
            } else {
                0.
            };
            send(
                &editor_tx,
                EditorWork::Indicator(indicator_of(&session, level)),
            )?;
            ensure!(
                !keys.exhausted() && !engine.is_finished() && !editor.is_finished(),
                "a required worker stopped unexpectedly"
            );
            thread::sleep(LOOP_INTERVAL);
        }
        Ok(())
    })();
    // A capture still being held has nothing the user is waiting for. One
    // already released is inside its final decode, and that text is owed to
    // the user: the bounded join below, not a cancel, is what limits the wait.
    if let Some(u) = &session.current
        && u.ticking()
    {
        u.cancel();
    }
    stopping.store(true, Ordering::Release);
    capture.shutdown();
    let _ = work_tx.send(Work::Quit);
    join_bounded(engine, "inference");
    // The engine may have committed, and finished, while the loop was
    // already leaving or during the join above; neither is dropped here.
    for event in results.try_iter() {
        match event {
            ResultEvent::Commit(c) => {
                if let Err(e) = commit(c, &mut session, &processor, &editor_tx) {
                    log::error!("could not queue a commit made during shutdown: {e:#}");
                }
            }
            // Same ordering guarantee as in the main loop: this utterance's
            // commits were drained from the same channel, in order, by the
            // arm above, on an earlier pass of this same `for`.
            ResultEvent::Finished { .. } => {
                if let Err(e) = send(&editor_tx, EditorWork::Copy) {
                    log::error!("could not queue a shutdown clipboard copy: {e:#}");
                }
            }
            ResultEvent::Tick { .. } => {}
        }
    }
    // Sent last: everything the editor still has to write is already queued
    // ahead of it, and the bounded join gives it time to land.
    let _ = editor_tx.send(EditorWork::Quit);
    join_bounded(editor, "editor");
    result
}

/// Hands one decoded commit to the editor thread. The event loop and the
/// shutdown drain share it, so text cannot take a different path depending on
/// when the recognizer produced it.
fn commit(
    c: Commit,
    session: &mut Session,
    processor: &Processor,
    editor_tx: &Sender<EditorWork>,
) -> Result<()> {
    session.note_commit(&c);
    let text = processor.process(&c.text);
    log::debug!(
        "utterance {} committed through {}: {text:?}",
        c.utterance.id.0,
        c.through
    );
    if text.trim().is_empty() {
        return Ok(());
    }
    send(
        editor_tx,
        EditorWork::Append {
            utterance: c.utterance.id,
            text,
        },
    )
}

fn engine_thread<R: Recognizer, S: Segmenter>(
    worker: &mut Worker<R, S>,
    work_rx: &Receiver<Work>,
    result_tx: &Sender<ResultEvent>,
) {
    while let Ok(work) = work_rx.recv() {
        let sent = match work {
            Work::Tick {
                audio,
                start,
                utterance,
            } => {
                let started = Instant::now();
                let result = worker.tick(&audio, start, &utterance, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Tick {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Finish { audio, utterance } => {
                let started = Instant::now();
                let result = worker.finish(&audio, &utterance, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Finished {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Quit => break,
        };
        if sent.is_err() {
            break;
        }
    }
}

/// Everything that happens between the key release and the decode request.
fn release<B: InputBackend>(
    session: &mut Session,
    capture: &AudioCapture<B>,
    samples: &[f32],
    config: &Config,
    dump_dir: Option<&Path>,
    work_tx: &Sender<Work>,
) -> Result<()> {
    let rate = config.audio.sample_rate;
    let held = match session.state {
        State::Transcribing { started, released } => released.saturating_duration_since(started),
        _ => Duration::ZERO,
    };
    let captured = Frames(samples.len()).seconds(rate);
    let peak = samples.iter().fold(0_f32, |a, &s| a.max(s.abs()));
    log::info!(
        "captured {:.1}s held -> {captured:.1}s audio ({} samples), peak={peak:.4}",
        held.as_secs_f64(),
        samples.len()
    );
    log_recording(capture.recording_status());
    // Neither of these can displace the memory-cap notice, which outranks them
    // (`Notice::priority`): past the in-memory ceiling a capture is *expected*
    // to be far shorter than the hold, and the cap notice already says what to
    // do about it. The measurements are still logged, because they are true.
    if held > INCOMPLETE_HOLD && captured < held.as_secs_f64() * 0.5 {
        log::error!(
            "microphone delivered less than half the expected audio; capture is incomplete"
        );
        session.notify(Notice::CaptureIncomplete);
    } else if peak < NEARLY_SILENT_PEAK {
        log::warn!("capture is nearly silent; check microphone gain/device");
        session.notify(Notice::NearlySilent);
    }
    if let Some(dir) = dump_dir {
        let path = dir.join(format!(
            "capture-{}.wav",
            chrono::Local::now().format("%Y-%m-%d-%H%M%S-%f")
        ));
        if let Err(e) = crate::shell::recorder::dump_capture(&path, samples, rate) {
            log::error!("could not dump capture: {e:#}");
        }
    }
    let utterance = session
        .current
        .as_ref()
        .context("capture has no utterance")?
        .clone();
    send(
        work_tx,
        Work::Finish {
            audio: samples.to_vec(),
            utterance,
        },
    )
}

fn note_postroll(postroll: PostRoll, session: &mut Session) {
    match postroll {
        PostRoll::Complete => {}
        PostRoll::Interrupted => {
            log::debug!("post-roll ended early by a key event or shutdown")
        }
        PostRoll::TimedOut => {
            log::warn!(
                "the device delivered less than the post-roll in time; decoding what arrived"
            )
        }
        // The watchdog's own report comes after the capture has ended, when
        // it can no longer tell that this capture lost audio; this can.
        PostRoll::StreamStale => {
            log::warn!(
                "input stream stopped delivering before the post-roll arrived; the end of this capture may be missing"
            );
            session.notify(Notice::MicrophoneGap);
        }
    }
}

fn apply_capture_event(event: CaptureEvent, session: &mut Session) {
    match event {
        CaptureEvent::StreamRestarted {
            gap,
            during_capture,
        } => {
            log::warn!(
                "microphone restarted after {:.1}s without audio",
                gap.as_secs_f64()
            );
            if during_capture {
                session.notify(Notice::MicrophoneGap);
            }
        }
        CaptureEvent::StreamUnavailable {
            reason,
            during_capture,
        } => {
            log::error!("microphone recovery failed: {reason}");
            if during_capture {
                session.notify(Notice::MicrophoneUnavailable);
            }
        }
        CaptureEvent::MemoryCapReached { recovery } => {
            // The notice names the recovery WAV by file name, so that a narrow
            // window still has room for it; the whole path goes here instead.
            let notice = Notice::MemoryCap(recovery.clone());
            match &recovery {
                RecordingStatus::Recorded(path) | RecordingStatus::Truncated(path) => {
                    log::warn!("{notice} ({})", path.display())
                }
                RecordingStatus::NotRecorded => log::warn!("{notice}"),
            }
            session.cap(recovery);
        }
        CaptureEvent::Flags(flags) => log::warn!("PortAudio: {flags}"),
    }
}

fn log_recording(status: RecordingStatus) {
    match status {
        RecordingStatus::Recorded(p) => log::info!("recorded to {}", p.display()),
        RecordingStatus::Truncated(p) => log::error!("recording {} is INCOMPLETE", p.display()),
        RecordingStatus::NotRecorded => log::warn!("no recovery WAV for this capture"),
    }
}
