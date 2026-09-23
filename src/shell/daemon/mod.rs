//! Four owners: the event loop with its session, the control socket, the
//! inference thread, and the editor thread; the recorder owns disk I/O.
//!
//! [`run`] is the composition root: it takes the per-user lock (`lock`),
//! listens on the control socket, registers signals and opens PortAudio,
//! and hands the inference thread a loader that downloads and loads the
//! models while presses are already accepted. [`serve`] is the event loop
//! over whatever devices it is handed, which is what the headless end-to-end
//! tests drive; each pass is a method of `Loop`. The threads it starts are
//! `engine` and `editor`; `capture` is what it does with the microphone,
//! `requests` how it reads the control requests, and `transcriptions` its
//! list of recordings whose text is not all written.
mod capture;
mod editor;
mod engine;
mod lock;
mod requests;
mod transcriptions;

pub use lock::AnotherDaemon;

use self::{
    capture::{apply_capture_event, discard, note_postroll, release},
    editor::{EditorWork, editor_thread, indicator_of},
    engine::{Work, engine_thread},
    lock::{daemon_lock, lock_under_activation},
    requests::Requests,
    transcriptions::{Stage, Transcription, Transcriptions, begin, finished, keep, persist},
};
use crate::{
    config::{Config, Nvim, Source},
    core::{
        control::{Received, Request},
        decode::{
            Commit, Pipeline, Preview, Recognizer, Segmenter, TickKind, Utterance, UtteranceId,
        },
        frames::Frames,
        session::{Notice, Preparing, Recognition, Session},
        state::{Cause, Command, Event},
        text::Processor,
    },
    shell::{
        audio::{AudioCapture, InputBackend, MAX_UTTERANCE_SECONDS},
        control::{ControlServer, Socket},
        inference::{SpeechSegmenter, Transcriber},
        models::{Repair, ensure_defaults, repair_defaults},
        nvim::NvimSession,
        recorder::Unfinished,
    },
};
use anyhow::{Context, Result, ensure};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The most control requests one pass of the loop carries out before it
/// delivers results: a bound that keeps a burst from starving the rest of
/// the pass. Requests past it wait for the next pass; none is dropped.
const MAX_REQUESTS_PER_PASS: usize = 256;
/// The most inference results one pass delivers, for the same reason.
const MAX_RESULTS_PER_PASS: usize = 128;

/// How long the shutdown waits for a background thread, and therefore how
/// long queued editor appends have to reach the file.
///
/// It is also the whole budget a pane has to write what is in its buffers and
/// close: when this runs out the process exits, and a pane thread stops
/// wherever it had got to. `shell::pane` divides it and checks that its
/// pieces fit.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

const LOOP_INTERVAL: Duration = Duration::from_millis(20);

const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Everything the event loop reads from the outside world.
pub struct Devices<B: InputBackend, R, S> {
    pub capture: AudioCapture<B>,
    /// Control requests. The sender being dropped means the socket closed.
    pub requests: Receiver<Received>,
    pub pipeline: PipelineSource<R, S>,
    /// The config file, read again for each new dictation window.
    pub reload: Reload,
}

/// Builds the recognizer and the segmenter on the inference thread, telling
/// it each step as it begins. Called again, at the next press, after it
/// failed.
pub type Loader<R, S> = Box<dyn FnMut(&mut dyn FnMut(Preparing)) -> Result<Pipeline<R, S>> + Send>;

/// Where the event loop's pipeline comes from.
pub enum PipelineSource<R, S> {
    /// Built already: the loop is ready from its first press.
    Ready(Pipeline<R, S>),
    /// Built by the inference thread while the loop already accepts presses.
    /// Until it is, captures are kept on disk and transcribed afterwards.
    Load(Loader<R, S>),
}

enum ResultEvent {
    Preparing(Preparing),
    Ready {
        elapsed: Duration,
    },
    Unavailable(String),
    /// The config file did not load when a window opened, for this reason.
    /// Sent by the editor thread.
    ConfigInvalid(String),
    /// The user closed the dictation pane at `at`. Sent by the editor thread,
    /// which asks the pane after every piece of work and every 66 ms.
    WindowClosed {
        at: Instant,
    },
    Commit(Commit),
    Tick {
        id: UtteranceId,
        result: Result<Option<Preview>>,
        elapsed: Duration,
    },
    /// A capture's last decode is over; its text went out as `Commit`s.
    Finished {
        id: UtteranceId,
        /// How many samples the last decode had.
        result: Result<usize>,
        elapsed: Duration,
    },
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

/// What each new dictation window opens with. The editor thread asks before
/// each one, so that `[nvim]` changes apply to that window without a
/// restart.
pub struct Reload {
    /// Reads the configuration again.
    pub load: Box<dyn FnMut() -> Result<Config> + Send>,
    /// Gives the `[nvim]` settings a window opens with the session it opens
    /// in: the settings the file has now or, when it no longer loads, those
    /// in use.
    pub session: fn(Nvim) -> Nvim,
}

/// The imperative shell: acquire the machine's resources, then serve.
/// `config` was read from `source`, which each new dictation window reads
/// again. `inherited` is the control socket systemd passed in, if it passed
/// one; otherwise the daemon binds its own.
pub fn run(
    config: Config,
    source: Source,
    inherited: Option<Socket>,
    dump_dir: Option<&Path>,
) -> Result<()> {
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stopping))?;
    let path = crate::config::control_socket();
    let (_lock, socket) = match inherited {
        Some(socket) => match lock_under_activation(&socket, &stopping)? {
            Some(lock) => (lock, socket),
            // Stopped while it waited.
            None => return Ok(()),
        },
        None => (daemon_lock()?, Socket::bind(&path)?),
    };
    let reload = reload_from(source, &socket);
    // Served first, before anything slow: under socket activation the press
    // that started this daemon is waiting on the socket, and a request is
    // stamped when it is read. The model loads later, on the inference
    // thread, and what is recorded meanwhile is kept until it has.
    let (requests_tx, requests) = mpsc::channel();
    let _control = ControlServer::start(socket, requests_tx)?;
    log::info!("listening on {}", path.display());
    let capture = AudioCapture::new(config.audio.clone(), config.recording.clone());
    let (asr, vad, rate) = (
        config.asr.clone(),
        config.vad.clone(),
        config.audio.sample_rate,
    );
    let load: Loader<Transcriber, SpeechSegmenter> = Box::new(move |step| {
        ensure_defaults(&asr, &vad, |done, total| {
            step(Preparing::Downloading { done, total });
        })?;
        let build = || -> Result<Pipeline<Transcriber, SpeechSegmenter>> {
            log::info!("loading CPU recognizer");
            let mut recognizer = Transcriber::new(&asr, rate)?;
            recognizer.warm_up()?;
            Ok(Pipeline {
                recognizer,
                segmenter: SpeechSegmenter::new(&vad, rate)?,
            })
        };
        load_with_repair(
            build,
            |step| {
                repair_defaults(&asr, &vad, |done, total| {
                    step(Preparing::Downloading { done, total });
                })
            },
            step,
        )
    });
    serve(
        &config,
        Devices {
            capture,
            requests,
            pipeline: PipelineSource::Load(load),
            reload,
        },
        stopping,
        dump_dir,
    )
}

/// What each new dictation window reads: `source` again, and for the daemon
/// systemd started on its socket, the display and sockets the user manager
/// has by then (`with_manager_session`), whether or not `source` still
/// loads. A daemon started in a terminal, or by a test, keeps the
/// environment it was given.
fn reload_from(source: Source, socket: &Socket) -> Reload {
    Reload {
        load: Box::new(move || source.load()),
        session: match socket {
            Socket::Inherited(_) => crate::shell::nvim::with_manager_session,
            Socket::Bound { .. } => std::convert::identity,
        },
    }
}

/// Builds the pipeline, and when a model that is in place does not load,
/// repairs it once and builds again. Whether the files are all there is
/// decided by size before this, without reading them; a file of the right
/// size with the wrong bytes is only found here, by `repair` hashing it.
fn load_with_repair<P>(
    mut build: impl FnMut() -> Result<P>,
    repair: impl FnOnce(&mut dyn FnMut(Preparing)) -> Result<Repair>,
    step: &mut dyn FnMut(Preparing),
) -> Result<P> {
    step(Preparing::Loading);
    let failure = match build() {
        Ok(pipeline) => return Ok(pipeline),
        Err(failure) => failure,
    };
    log::warn!("the speech model did not load: {failure:#}");
    let repaired = repair(step).with_context(|| {
        format!(
            "{failure:#}; downloading it again failed too (`spokenpad fetch-models` verifies and repairs the files)"
        )
    })?;
    match repaired {
        Repair::Replaced => {
            step(Preparing::Loading);
            build().context("the speech model was downloaded again and still does not load")
        }
        Repair::Verified => Err(failure.context(
            "the speech model's files match their pinned sha256, so this machine cannot load it",
        )),
        Repair::NotOurs => Err(failure.context("check asr.model_dir")),
    }
}

/// Turns the ticks and the silence timeout on, once the pipeline is ready:
/// only a tick reports speech, so before then there is no silence to
/// measure.
fn configure(session: &mut Session, config: &Config) {
    let silence = config.capture.silence_timeout();
    match silence {
        Some(timeout) => log::info!(
            "a latched capture ends by itself after {:.0}s without speech",
            timeout.as_secs_f64()
        ),
        None => log::info!(
            "no silence timeout: a capture ends at the {}-hour limit or the in-memory ceiling",
            crate::core::state::MAX_CAPTURE.as_secs() / 3600
        ),
    }
    session.configure(true, silence);
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
        capture,
        requests,
        pipeline,
        reload,
    } = devices;
    let rate = config.audio.sample_rate;
    // No ticks and no silence timeout until the pipeline is ready.
    let mut session = Session::new(false, config.preview.interval(), None);
    let recognition = match &pipeline {
        PipelineSource::Ready(_) => {
            configure(&mut session, config);
            Recognition::Ready
        }
        PipelineSource::Load(_) => Recognition::Preparing(Preparing::Loading),
    };
    // First those the last daemon left, from where their text reached.
    let transcriptions = Transcriptions(
        capture
            .take_unfinished()
            .into_iter()
            .map(|Unfinished { path, through }| {
                log::info!(
                    "{} was left untranscribed by the last run; it is transcribed from {:.2}s once the speech model is ready",
                    path.display(),
                    through.seconds(rate)
                );
                Transcription {
                    path,
                    id: session.reserve_id(),
                    through,
                    from: through,
                    stage: Stage::Kept,
                }
            })
            .collect(),
    );
    // `preview.max_seconds` in samples: the longest tail the loop has
    // previewed, and how much of a recording the engine reads at a time.
    let window = config.preview.window(rate);

    let (work_tx, work_rx) = mpsc::channel();
    let (result_tx, results) = mpsc::channel();
    let editor_events = result_tx.clone();
    let engine = thread::Builder::new()
        .name("spokenpad-asr".into())
        .spawn(move || engine_thread(pipeline, window, rate, &work_rx, &result_tx))?;
    let (editor_tx, editor_rx) = mpsc::channel();
    let running = config.clone();
    let nvim = NvimSession::new(running.nvim.clone());
    let editor_quitting = nvim.quitting();
    let editor = thread::Builder::new()
        .name("spokenpad-nvim".into())
        .spawn(move || editor_thread(nvim, running, reload, editor_rx, editor_events))?;

    let mut state = Loop {
        config,
        rate,
        window,
        capture,
        requests: Requests::new(requests),
        session,
        recognition,
        transcriptions,
        live: false,
        processor: Processor::new(&config.text)?,
        work_tx,
        editor_tx,
        stopping,
        dump_dir,
        next_device_poll: Instant::now(),
    };
    let result = state.run(&results, &engine, &editor);
    state.shut_down(engine, &results);
    // Sent last: everything the editor still has to write is already queued
    // ahead of it, and the bounded join gives it time to land. The flag first,
    // so a call Neovim is holding behind a half-typed command is given up on
    // rather than waited out past the grace.
    editor_quitting.store(true, Ordering::Release);
    let _ = state.editor_tx.send(EditorWork::Quit);
    join_bounded(editor, "editor");
    result
}

/// What the event loop holds between passes. Each pass is a sequence of the
/// methods below, in [`Loop::run`]'s order.
struct Loop<'a, B: InputBackend> {
    config: &'a Config,
    rate: u32,
    /// `preview.max_seconds` in samples: the longest tail a tick previews.
    window: usize,
    capture: AudioCapture<B>,
    requests: Requests,
    session: Session,
    recognition: Recognition,
    transcriptions: Transcriptions,
    /// Whether the capture that is recording, or was last, decodes as it
    /// goes. One made before the model was ready does not: it is on disk
    /// only.
    live: bool,
    processor: Processor,
    work_tx: Sender<Work>,
    editor_tx: Sender<EditorWork>,
    stopping: Arc<AtomicBool>,
    dump_dir: Option<&'a Path>,
    next_device_poll: Instant,
}

impl<B: InputBackend> Loop<'_, B> {
    /// Passes until told to stop, or until a thread it needs is gone.
    fn run(
        &mut self,
        results: &Receiver<ResultEvent>,
        engine: &JoinHandle<()>,
        editor: &JoinHandle<()>,
    ) -> Result<()> {
        while !self.stopping.load(Ordering::Acquire) {
            // What the device has to say comes first: the in-memory ceiling
            // ends the capture, and the command it produces is carried out by
            // the same match as a key press's.
            let ceiling = self.poll_device();
            // Then requests and the clock, which have priority over delivery
            // of inference results. The clock is also what ends a capture
            // that has run too long or been quiet too long.
            self.take_requests(ceiling)?;
            for event in results.try_iter().take(MAX_RESULTS_PER_PASS) {
                self.deliver(event)?;
            }
            self.tick()?;
            self.dispatch_recordings()?;
            // Audio the worker has committed is never read again, and a
            // capture that does not decode as it goes is read back from its
            // recording: the recovery WAV keeps the whole capture either way.
            self.capture.discard_before(if self.live {
                self.session.committed_hint
            } else {
                self.capture.captured_frames()
            });
            self.show()?;
            ensure!(
                !self.requests.exhausted() && !engine.is_finished() && !editor.is_finished(),
                "a required worker stopped unexpectedly"
            );
            thread::sleep(LOOP_INTERVAL);
        }
        Ok(())
    }

    /// Hands the device's news to the session, every
    /// [`DEVICE_POLL_INTERVAL`]. Only the ceiling produces a command, and
    /// only once per capture.
    fn poll_device(&mut self) -> Command {
        let at = Instant::now();
        let mut ceiling = Command::Nothing;
        if at < self.next_device_poll {
            return ceiling;
        }
        self.next_device_poll = at + DEVICE_POLL_INTERVAL;
        for event in self.capture.poll() {
            let command = apply_capture_event(event, &mut self.session, at);
            if command != Command::Nothing {
                ceiling = command;
            }
        }
        // A capture kept on disk holds nothing in memory, so the in-memory
        // ceiling cannot end it: its length does, at the same bound.
        if !self.live
            && ceiling == Command::Nothing
            && self.session.state.recording()
            && self.capture.captured_frames().seconds(self.rate) >= MAX_UTTERANCE_SECONDS as f64
        {
            log::warn!(
                "a capture kept on disk reached {} minutes and ends here",
                MAX_UTTERANCE_SECONDS / 60
            );
            ceiling = self
                .session
                .cap_kept((MAX_UTTERANCE_SECONDS / 60) as u64, at);
        }
        ceiling
    }

    /// Carries out `ceiling`, then each request and the clock before it, up
    /// to [`MAX_REQUESTS_PER_PASS`].
    fn take_requests(&mut self, mut ceiling: Command) -> Result<()> {
        for _ in 0..MAX_REQUESTS_PER_PASS {
            let command = if ceiling != Command::Nothing {
                std::mem::replace(&mut ceiling, Command::Nothing)
            } else if let Some(event) = self.requests.next() {
                self.session.event(event)
            } else {
                break;
            };
            match command {
                Command::Start => self.start()?,
                Command::Decode {
                    released,
                    held,
                    cause,
                } => self.decode(released, held, cause)?,
                Command::Discard(reason) => discard(
                    &mut self.capture,
                    &self.session,
                    &mut self.transcriptions,
                    reason,
                ),
                Command::Nothing => {}
            }
        }
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        self.live = self.recognition == Recognition::Ready;
        if let Recognition::Unavailable(_) = self.recognition {
            send(&self.work_tx, Work::Load)?;
            self.recognition = Recognition::Preparing(Preparing::Loading);
        }
        match self.capture.start_capture() {
            Ok(()) => begin(&self.session, &self.capture, &mut self.transcriptions),
            Err(e) => {
                // The press cleared the previous notice; cancel first, then
                // say why nothing is being recorded.
                self.session.event(Event::Request(Received {
                    request: Request::Cancel,
                    at: Instant::now(),
                }));
                self.session.notify(Notice::MicrophoneUnavailable);
                log::error!("capture could not start: {e:#}");
            }
        }
        // Either way: on the first press of a session there is no editor
        // yet, and a notice pushed at a window that does not exist leaves the
        // user with a dead key and no explanation anywhere.
        send(&self.editor_tx, EditorWork::Ensure)
    }

    fn decode(&mut self, released: Instant, held: Duration, cause: Cause) -> Result<()> {
        if cause != Cause::KeyPress {
            log::warn!(
                "no key ended this capture; spokenpad did: {}",
                self.session
                    .notice()
                    .map_or_else(|| format!("{cause:?}"), Notice::to_string)
            );
        }
        // The winbar says "transcribing" from here on, not only once the
        // post-roll has arrived.
        self.show_level(0.)?;
        let (stopping, requests) = (&self.stopping, &mut self.requests);
        let (captured, postroll) = self.capture.finish_capture(released, || {
            stopping.load(Ordering::Acquire) || requests.start_waiting()
        });
        note_postroll(postroll, &mut self.session);
        for event in self.capture.poll() {
            let command = apply_capture_event(event, &mut self.session, Instant::now());
            // This capture has already ended, so the ceiling can only add its
            // notice here.
            debug_assert_eq!(command, Command::Nothing);
        }
        // The dump holds what is decoded, and a capture made before the
        // model was ready decodes from its WAV.
        let live = self.live;
        let tail = release(
            &mut self.session,
            &self.capture,
            captured,
            held,
            self.config,
            self.dump_dir.filter(|_| live),
        )?;
        if live {
            if let Some(t) = self.transcriptions.get_mut(tail.utterance.id) {
                t.stage = Stage::Finishing;
            }
            send(&self.work_tx, Work::Finish(tail))
        } else {
            keep(
                &mut self.session,
                tail.utterance.id,
                &self.capture,
                &mut self.transcriptions,
            );
            let _ = persist(&self.capture, self.transcriptions.listed_while_running());
            Ok(())
        }
    }

    /// One result of the inference or the editor thread.
    fn deliver(&mut self, event: ResultEvent) -> Result<()> {
        match event {
            ResultEvent::Preparing(step) => {
                self.recognition = Recognition::Preparing(step);
            }
            ResultEvent::Ready { elapsed } => {
                log::info!("speech model ready after {:.1}s", elapsed.as_secs_f64());
                configure(&mut self.session, self.config);
                self.recognition = Recognition::Ready;
            }
            ResultEvent::ConfigInvalid(reason) => {
                self.session.notify(Notice::ConfigInvalid(reason));
            }
            // As `spokenpad cancel`: what is committed stays, the tail is not
            // decoded, the WAV is kept. Only a capture that was running when
            // the window closed.
            ResultEvent::WindowClosed { at } => {
                if let Command::Discard(reason) = self.session.event(Event::WindowClosed { at }) {
                    discard(
                        &mut self.capture,
                        &self.session,
                        &mut self.transcriptions,
                        reason,
                    );
                }
            }
            ResultEvent::Unavailable(reason) => {
                log::error!(
                    "no speech model: {reason}; captures are kept, and the next press tries again"
                );
                self.recognition = Recognition::Unavailable(reason);
            }
            ResultEvent::Commit(c) => {
                self.transcriptions.advance(&c);
                commit(c, &mut self.session, &self.processor, &self.editor_tx)?;
            }
            ResultEvent::Tick {
                id,
                result,
                elapsed,
            } => {
                let preview = match result {
                    Ok(Some(preview)) => {
                        log::debug!(
                            "preview {}: {} characters, committed through {}, decoded in {:.2}s",
                            id.0,
                            preview.text.chars().count(),
                            preview.through,
                            elapsed.as_secs_f64()
                        );
                        Some(Preview {
                            text: self.processor.process(&preview.text),
                            ..preview
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
                        if self.session.is_current(id) && self.session.should_warn_tick_failure() {
                            log::warn!("preview tick failed: {e:#}; will retry");
                        }
                        None
                    }
                };
                self.session.tick_finished(id, preview, Instant::now());
            }
            ResultEvent::Finished {
                id,
                result,
                elapsed,
            } => {
                match &result {
                    Ok(frames) => {
                        log::info!(
                            "decoded the last {:.1}s in {:.2}s (utterance {})",
                            Frames(*frames).seconds(self.rate),
                            elapsed.as_secs_f64(),
                            id.0
                        );
                    }
                    Err(e) => log::error!("decode failed for utterance {}: {e:#}", id.0),
                }
                if finished(
                    id,
                    result.as_ref().err(),
                    self.rate,
                    &self.capture,
                    &mut self.session,
                    &mut self.transcriptions,
                ) {
                    log::info!("{} recordings to go", self.transcriptions.waiting());
                    let _ = persist(&self.capture, self.transcriptions.listed_while_running());
                }
                self.session.finish(id);
                // Every `Commit` of this utterance was sent on this same
                // `results` channel, synchronously, before the engine thread
                // sent `Finished` -- see `engine_thread`'s `Work::Finish` arm.
                // An earlier call therefore already turned each of them into
                // an `EditorWork::Append` and queued it on `editor_tx` ahead
                // of the copy queued here, so the editor always writes the
                // append before it reads the buffer to copy. Sent on every
                // `Finished`, whatever the release decode did: progressively
                // committed text is in the buffer either way. The editor
                // thread copies only when the window's
                // `nvim.copy_to_clipboard` says to, off by default.
                send(&self.editor_tx, EditorWork::Copy)?;
            }
        }
        Ok(())
    }

    /// Sends the capture's open tail to the engine when a tick is due.
    fn tick(&mut self) -> Result<()> {
        let now = Instant::now();
        if !self.session.tick_due(now) {
            return Ok(());
        }
        let tail = self
            .capture
            .captured_frames()
            .since(self.session.committed_hint);
        // A tail too long to redraw cheaply stops the cosmetic decode only.
        // The tick still runs, still commits what has settled, and so still
        // lets the shell drop the audio behind it.
        let kind = TickKind::for_tail(tail, self.window);
        if kind == TickKind::Settled {
            if self.session.previewing() {
                log::warn!(
                    "preview paused: uncommitted audio exceeds preview.max_seconds; still recording and committing"
                );
            }
            self.session.defer_previews();
        } else {
            self.session.resume_previews();
        }
        let utterance = self
            .session
            .current
            .as_ref()
            .context("recording has no utterance")?
            .clone();
        // The whole tail, however long: a chunk settles where the detector,
        // reading all of it, closes it, and nowhere else. The detector pass
        // costs about 4 ms per second of tail on an idle machine, and the
        // tail is bounded by the chunk rules (docs/progressive-commit.md).
        let audio = self.capture.snapshot_capture(self.session.committed_hint);
        self.session.requested(now);
        log::debug!(
            "preview {} requested: {:.2}s audio starting at {}",
            utterance.id.0,
            Frames(audio.samples.len()).seconds(self.rate),
            audio.start
        );
        send(
            &self.work_tx,
            Work::Tick {
                audio: audio.samples,
                start: audio.start,
                utterance,
                kind,
            },
        )
    }

    /// Sends every recording kept until the model was ready to the engine,
    /// once it is.
    fn dispatch_recordings(&mut self) -> Result<()> {
        if self.recognition != Recognition::Ready {
            return Ok(());
        }
        for recording in self
            .transcriptions
            .0
            .iter_mut()
            .filter(|t| t.stage == Stage::Kept)
        {
            // Kept since its release; see `keep`.
            send(
                &self.work_tx,
                Work::Recording {
                    path: recording.path.clone(),
                    utterance: Utterance::new(recording.id.0),
                    from: recording.through,
                },
            )?;
            recording.stage = Stage::Sent;
        }
        Ok(())
    }

    /// The winbar, with the microphone's level while a capture records.
    fn show(&mut self) -> Result<()> {
        let level = if self.session.state.recording() {
            self.capture.level()
        } else {
            0.
        };
        self.show_level(level)
    }

    fn show_level(&self, level: f32) -> Result<()> {
        send(
            &self.editor_tx,
            indicator_of(
                &self.session,
                level,
                self.recognition.notice(self.transcriptions.waiting()),
            ),
        )
    }

    /// Stops the capture and the engine, delivers what the engine produced
    /// on its way out, and lists for the next start what is not all
    /// written.
    fn shut_down(&mut self, engine: JoinHandle<()>, results: &Receiver<ResultEvent>) {
        // A capture still being held has nothing the user is waiting for
        // now: it is cancelled, and listed below for the next start. One
        // already released is inside its final decode, and that text is owed
        // to the user: the bounded join below, not a cancel, is what limits
        // the wait.
        if let Some(u) = &self.session.current
            && u.ticking()
        {
            u.cancel();
        }
        self.stopping.store(true, Ordering::Release);
        self.capture.shutdown();
        let _ = self.work_tx.send(Work::Quit);
        join_bounded(engine, "inference");
        // The engine may have committed, and finished, while the loop was
        // already leaving or during the join above; neither is dropped here.
        for event in results.try_iter() {
            match event {
                ResultEvent::Commit(c) => {
                    self.transcriptions.advance(&c);
                    if let Err(e) = commit(c, &mut self.session, &self.processor, &self.editor_tx) {
                        log::error!("could not queue a commit made during shutdown: {e:#}");
                    }
                }
                // Same ordering guarantee as in the loop: this utterance's
                // commits were drained from the same channel, in order, by
                // the arm above, on an earlier pass of this same `for`.
                ResultEvent::Finished { id, result, .. } => {
                    finished(
                        id,
                        result.as_ref().err(),
                        self.rate,
                        &self.capture,
                        &mut self.session,
                        &mut self.transcriptions,
                    );
                    if let Err(e) = send(&self.editor_tx, EditorWork::Copy) {
                        log::error!("could not queue a shutdown clipboard copy: {e:#}");
                    }
                }
                ResultEvent::Tick { .. }
                | ResultEvent::Preparing(_)
                | ResultEvent::Ready { .. }
                | ResultEvent::Unavailable(_)
                | ResultEvent::ConfigInvalid(_)
                | ResultEvent::WindowClosed { .. } => {}
            }
        }
        self.list_what_is_left();
    }

    /// What is left is not all written: captures the stop cut short, and
    /// recordings still waiting. The next start transcribes the rest; only if
    /// it cannot know about them does the user have to.
    fn list_what_is_left(&self) {
        let saved = persist(&self.capture, self.transcriptions.0.iter());
        for Transcription { path, through, .. } in &self.transcriptions.0 {
            let seconds = through.seconds(self.rate);
            match saved {
                Ok(()) => log::warn!(
                    "{} is not transcribed past {seconds:.2}s; the next start transcribes the rest",
                    path.display()
                ),
                Err(()) if *through == Frames::ZERO => log::warn!(
                    "{} is not transcribed; recover it with `spokenpad transcribe {}`",
                    path.display(),
                    path.display()
                ),
                // What was committed is in the dictation file already; the
                // rest is recovered from where it stopped, or it is written
                // twice.
                Err(()) => log::warn!(
                    "{} was transcribed through {seconds:.2}s only; recover the rest with `spokenpad transcribe --from {seconds:.2} {}`",
                    path.display(),
                    path.display()
                ),
            }
        }
    }
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
    session.note_commit(&c, Instant::now());
    let text = processor.process(&c.text);
    log::debug!(
        "utterance {} committed through {}: {} characters",
        c.utterance.id.0,
        c.through,
        text.chars().count()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A model in place that does not load is checked and downloaded again
    /// once; one whose files verify is reported as such, and a configured
    /// one is never touched.
    #[test]
    fn a_model_that_does_not_load_is_repaired_once() {
        let mut steps = Vec::new();
        let mut builds = 0;
        let loaded = load_with_repair(
            || {
                builds += 1;
                if builds == 1 {
                    anyhow::bail!("bad weights")
                } else {
                    Ok("pipeline")
                }
            },
            |step| {
                step(Preparing::Downloading { done: 1, total: 2 });
                Ok(Repair::Replaced)
            },
            &mut |step| steps.push(step),
        );
        assert_eq!(loaded.unwrap(), "pipeline");
        assert_eq!(builds, 2);
        assert_eq!(
            steps,
            [
                Preparing::Loading,
                Preparing::Downloading { done: 1, total: 2 },
                Preparing::Loading
            ]
        );

        for (repair, says) in [
            (Repair::Verified, "match their pinned sha256"),
            (Repair::NotOurs, "check asr.model_dir"),
        ] {
            let error = load_with_repair(
                || -> Result<()> { anyhow::bail!("bad weights") },
                |_| Ok(repair),
                &mut |_| {},
            )
            .unwrap_err();
            let error = format!("{error:#}");
            assert!(
                error.contains(says) && error.contains("bad weights"),
                "{error}"
            );
        }
        let error = load_with_repair(
            || -> Result<()> { anyhow::bail!("bad weights") },
            |_| anyhow::bail!("offline"),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("spokenpad fetch-models"),
            "{error:#}"
        );
    }
}
