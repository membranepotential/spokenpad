//! Crash-tolerant WAV recording for every microphone capture.
//!
//! The PortAudio callback only enqueues shared sample buffers. A dedicated
//! writer owns each WAV, checkpoints its header after every buffer, and may be
//! abandoned after a bounded wait without affecting live capture or ASR.

use crate::{config::Recording, core::session::RecordingStatus};
use anyhow::{Context, Result, bail, ensure};
use chrono::Local;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::BufWriter,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const CAPTURE_PREFIX: &str = "capture-";
const CAPTURE_SUFFIX: &str = ".wav";
const MAX_SAME_SECOND: usize = 100;
const FULL_SCALE: f32 = 32_767.0;
const WRITER_JOIN: Duration = Duration::from_secs(3);
const QUEUE_SECONDS: usize = 60;
/// Recovery refuses implausibly long WAVs rather than allocating for them.
const MAX_RECOVERY_SECONDS: f64 = 4.0 * 3600.0;

trait WavSink: Send {
    fn write(&mut self, samples: &[f32]) -> Result<()>;
    fn duration(&self) -> u32;
    fn sample_rate(&self) -> u32;
    fn finalize(self: Box<Self>) -> Result<()>;
}

struct HoundSink {
    wav: WavWriter<BufWriter<File>>,
}

impl WavSink for HoundSink {
    fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &sample in samples {
            self.wav.write_sample(to_pcm16_sample(sample))?;
        }
        // This updates both RIFF/data lengths and flushes the userspace
        // buffer, leaving a valid WAV after every received callback.
        self.wav.flush()?;
        Ok(())
    }

    fn duration(&self) -> u32 {
        self.wav.duration()
    }

    fn sample_rate(&self) -> u32 {
        self.wav.spec().sample_rate
    }

    fn finalize(self: Box<Self>) -> Result<()> {
        self.wav.finalize().map_err(Into::into)
    }
}

#[derive(Debug)]
struct RecordingState {
    path: PathBuf,
    sender: Sender<Arc<[f32]>>,
    stopping: AtomicBool,
    broken: AtomicBool,
    queued: AtomicUsize,
    dropped: AtomicUsize,
    drop_reported: AtomicBool,
    max_queued: usize,
}

struct ActiveRecording {
    state: Arc<RecordingState>,
    writer: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Lifecycle {
    current: Option<ActiveRecording>,
    last: Option<Arc<RecordingState>>,
}

/// One asynchronous WAV writer per capture.
pub struct CaptureRecorder {
    config: Recording,
    sample_rate: u32,
    lifecycle: Mutex<Lifecycle>,
    open_paths: Arc<Mutex<HashSet<PathBuf>>>,
    writer_join: Duration,
}

impl CaptureRecorder {
    pub fn new(config: Recording, sample_rate: u32) -> Self {
        let recorder = Self {
            config,
            sample_rate,
            lifecycle: Mutex::new(Lifecycle::default()),
            open_paths: Arc::new(Mutex::new(HashSet::new())),
            writer_join: WRITER_JOIN,
        };
        if recorder.config.enabled {
            prune_recordings(
                &recorder.config.dir,
                recorder.config.max_total_bytes,
                |_| false,
            );
        }
        recorder
    }

    pub fn status(&self) -> RecordingStatus {
        let last = lock(&self.lifecycle).last.clone();
        match last {
            None => RecordingStatus::NotRecorded,
            Some(recording)
                if recording.broken.load(Ordering::Acquire)
                    || recording.dropped.load(Ordering::Acquire) > 0 =>
            {
                RecordingStatus::Truncated(recording.path.clone())
            }
            Some(recording) => RecordingStatus::Recorded(recording.path.clone()),
        }
    }

    /// Start a capture. Failure is deliberately swallowed: disk recording is
    /// a safety net and must never make microphone capture fail.
    pub fn start(&self) {
        if !self.config.enabled {
            lock(&self.lifecycle).last = None;
            return;
        }
        // The key-press path must never wait on a sick filesystem, so the
        // previous writer is abandoned rather than joined; it owns its WAV and
        // finalises itself.
        self.detach();

        let opened = (|| -> Result<_> {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(&self.config.dir)
                .with_context(|| {
                    format!("create recording directory {}", self.config.dir.display())
                })?;
            narrow_directory(&self.config.dir);
            let (path, file) = open_capture(&self.config.dir)?;
            let spec = mono_pcm16(self.sample_rate);
            let mut wav = WavWriter::new(BufWriter::new(file), spec)
                .with_context(|| format!("start recording {}", path.display()))?;
            // Materialise a valid zero-frame header before handing ownership
            // to the thread. Every later chunk is checkpointed the same way.
            wav.flush()
                .with_context(|| format!("write WAV header {}", path.display()))?;
            Ok((path, Box::new(HoundSink { wav }) as Box<dyn WavSink>))
        })();
        let (path, sink) = match opened {
            Ok(value) => value,
            Err(error) => {
                log::error!(
                    "could not open a recording in {}; this capture exists only in memory: {error:#}",
                    self.config.dir.display()
                );
                lock(&self.lifecycle).last = None;
                return;
            }
        };

        self.launch(path, sink, self.sample_rate as usize * QUEUE_SECONDS);
    }

    fn launch(&self, path: PathBuf, sink: Box<dyn WavSink>, max_queued: usize) {
        // The unbounded channel has no callback-count ceiling: its RAM is
        // bounded by the atomic frame quota below, independent of the device's
        // chosen callback size.
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(RecordingState {
            path: path.clone(),
            sender,
            stopping: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            queued: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            drop_reported: AtomicBool::new(false),
            max_queued,
        });
        lock(&self.open_paths).insert(path);

        let writer_state = Arc::clone(&state);
        let open_paths = Arc::clone(&self.open_paths);
        let directory = self.config.dir.clone();
        let max_total_bytes = self.config.max_total_bytes;
        let writer = thread::Builder::new()
            .name("spokenpad-recorder".into())
            .spawn(move || {
                drain(
                    writer_state,
                    receiver,
                    sink,
                    &directory,
                    max_total_bytes,
                    open_paths,
                );
            });
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                state.broken.store(true, Ordering::Release);
                lock(&self.open_paths).remove(&state.path);
                log::error!(
                    "could not start recording thread for {}: {error}",
                    state.path.display()
                );
                lock(&self.lifecycle).last = Some(state);
                return;
            }
        };

        let mut lifecycle = lock(&self.lifecycle);
        lifecycle.last = Some(Arc::clone(&state));
        lifecycle.current = Some(ActiveRecording {
            state,
            writer: Some(writer),
        });
    }

    /// Copy and enqueue samples. Production callers hand over the immutable
    /// callback buffer they already own through `write_shared`.
    #[cfg(test)]
    fn write(&self, samples: &[f32]) {
        self.write_shared(Arc::from(samples));
    }

    pub(crate) fn write_shared(&self, samples: Arc<[f32]>) {
        if samples.is_empty() {
            return;
        }
        // Keeping this lock through send closes the write-vs-stop race.
        // It is never held across I/O or a blocking channel operation.
        let lifecycle = lock(&self.lifecycle);
        let Some(active) = lifecycle.current.as_ref() else {
            return;
        };
        let state = &active.state;
        if state.broken.load(Ordering::Acquire) {
            return;
        }
        let frames = samples.len();
        if !reserve_frames(&state.queued, frames, state.max_queued) {
            state.dropped.fetch_add(frames, Ordering::Release);
            return;
        }
        if let Err(error) = state.sender.send(samples) {
            state.queued.fetch_sub(error.0.len(), Ordering::AcqRel);
            state.broken.store(true, Ordering::Release);
        }
    }

    /// Stop feeding the current writer and let it finish on its own thread.
    fn detach(&self) {
        let active = lock(&self.lifecycle).current.take();
        let Some(mut active) = active else { return };
        active.state.stopping.store(true, Ordering::Release);
        // Dropping the handle detaches: `drain` still finalises the WAV and
        // removes the path from the open registry when it is done.
        drop(active.writer.take());
        report_dropped(&active.state, self.sample_rate);
    }

    /// Flush the current capture, but never wait on a sick filesystem for
    /// longer than three seconds.
    pub fn stop(&self) {
        let active = {
            let mut lifecycle = lock(&self.lifecycle);
            lifecycle.current.take()
        };
        let Some(mut active) = active else { return };
        active.state.stopping.store(true, Ordering::Release);

        let Some(writer) = active.writer.take() else {
            return;
        };
        let deadline = Instant::now() + self.writer_join;
        while !writer.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if writer.is_finished() {
            if writer.join().is_err() {
                active.state.broken.store(true, Ordering::Release);
                log::error!(
                    "recording writer for {} panicked",
                    active.state.path.display()
                );
            }
        } else {
            active.state.broken.store(true, Ordering::Release);
            log::error!(
                "recording {} is still being written after {:.1}s; carrying on with a truncated file",
                active.state.path.display(),
                self.writer_join.as_secs_f64(),
            );
            drop(writer); // detach; the thread still owns and finalises its WAV
        }

        report_dropped(&active.state, self.sample_rate);
    }
}

impl Drop for CaptureRecorder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn reserve_frames(queued: &AtomicUsize, frames: usize, maximum: usize) -> bool {
    let mut current = queued.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(frames).filter(|next| *next <= maximum) else {
            return false;
        };
        match queued.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn drain(
    state: Arc<RecordingState>,
    receiver: Receiver<Arc<[f32]>>,
    mut sink: Box<dyn WavSink>,
    directory: &Path,
    max_total_bytes: u64,
    open_paths: Arc<Mutex<HashSet<PathBuf>>>,
) {
    prune_recordings(directory, max_total_bytes, |path| {
        lock(&open_paths).contains(path)
    });
    secure_recordings(directory);

    loop {
        report_dropped(&state, sink.sample_rate());
        let message = if state.stopping.load(Ordering::Acquire) {
            receiver.try_recv().ok()
        } else {
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(samples) => Some(samples),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        };
        let Some(samples) = message else { break };
        state.queued.fetch_sub(samples.len(), Ordering::AcqRel);

        if let Err(error) = sink.write(&samples) {
            state.broken.store(true, Ordering::Release);
            log::error!(
                "recording to {} failed after {:.1}s; the rest is not on disk: {error:#}",
                state.path.display(),
                sink.duration() as f64 / f64::from(sink.sample_rate()),
            );
            break;
        }
    }

    report_dropped(&state, sink.sample_rate());
    if let Err(error) = sink.finalize() {
        state.broken.store(true, Ordering::Release);
        log::error!(
            "could not finalise recording {}: {error}",
            state.path.display()
        );
    }
    lock(&open_paths).remove(&state.path);
}

fn report_dropped(state: &RecordingState, sample_rate: u32) {
    let dropped = state.dropped.load(Ordering::Acquire);
    if dropped == 0 || state.drop_reported.swap(true, Ordering::AcqRel) {
        return;
    }
    log::error!(
        "dropped {:.1}s of audio that the writer could not keep up with: {} is incomplete",
        dropped as f64 / f64::from(sample_rate),
        state.path.display()
    );
}

fn mono_pcm16(sample_rate: u32) -> WavSpec {
    WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    }
}

/// NumPy-compatible clipping and IEEE ties-to-even rounding.
fn to_pcm16_sample(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * FULL_SCALE).round_ties_even() as i16
}

pub fn dump_capture(path: &Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    ensure!(sample_rate > 0, "sample rate must be positive");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(FILE_MODE))
        .with_context(|| format!("secure {}", path.display()))?;
    let mut wav = WavWriter::new(BufWriter::new(file), mono_pcm16(sample_rate))?;
    for &sample in samples {
        wav.write_sample(to_pcm16_sample(sample))?;
    }
    wav.finalize()
        .with_context(|| format!("finalise {}", path.display()))
}

pub fn read_capture(path: &Path) -> Result<(Vec<f32>, u32)> {
    let mut wav =
        WavReader::open(path).with_context(|| format!("{}: not a readable wav", path.display()))?;
    let spec = wav.spec();
    ensure!(
        spec.sample_rate > 0,
        "{}: wav declares a zero sample rate",
        path.display()
    );
    ensure!(
        spec.channels == 1,
        "{}: expected mono audio, got {} channels",
        path.display(),
        spec.channels
    );
    ensure!(
        spec.bits_per_sample == 16 && spec.sample_format == SampleFormat::Int,
        "{}: expected 16-bit PCM, got {}-bit {:?}",
        path.display(),
        spec.bits_per_sample,
        spec.sample_format
    );
    let seconds = f64::from(wav.duration()) / f64::from(spec.sample_rate);
    ensure!(
        seconds <= MAX_RECOVERY_SECONDS,
        "{}: {seconds:.0}s of audio exceeds the {MAX_RECOVERY_SECONDS:.0}s recovery limit",
        path.display()
    );
    let samples = wav
        .samples::<i16>()
        .map(|sample| sample.map(|value| (f32::from(value) / FULL_SCALE).clamp(-1.0, 1.0)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("{}: unreadable PCM samples", path.display()))?;
    Ok((samples, spec.sample_rate))
}

pub(crate) fn prune_recordings(
    directory: &Path,
    max_total_bytes: u64,
    mut keep: impl FnMut(&Path) -> bool,
) {
    let read_dir = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            log::error!(
                "could not list {} to prune it: {error}",
                directory.display()
            );
            return;
        }
    };
    let mut entries = Vec::new();
    for entry in read_dir {
        let candidate = (|| -> Result<_> {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(CAPTURE_PREFIX) || !name.ends_with(CAPTURE_SUFFIX) {
                return Ok(None);
            }
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() {
                return Ok(None);
            }
            Ok(Some((metadata.modified()?, metadata.len(), path)))
        })();
        match candidate {
            Ok(Some(candidate)) => entries.push(candidate),
            Ok(None) => {}
            Err(error) => {
                log::error!(
                    "could not inspect a recording in {}: {error:#}",
                    directory.display()
                );
                return;
            }
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.2.cmp(&right.2)));
    let mut total = entries.iter().map(|entry| entry.1).sum::<u64>();
    for (_, size, path) in entries {
        if total <= max_total_bytes {
            break;
        }
        if keep(&path) {
            continue;
        }
        // Ask both questions at deletion time: a stale listing must not delete
        // a newly-opened path or something swapped to a symlink/non-file.
        let still_regular = fs::symlink_metadata(&path)
            .map(|metadata| metadata.file_type().is_file())
            .unwrap_or(false);
        if !still_regular {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => total = total.saturating_sub(size),
            Err(error) => log::error!("could not prune {}: {error}", path.display()),
        }
    }
}

fn secure_recordings(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(CAPTURE_PREFIX) || !name.ends_with(CAPTURE_SUFFIX) {
            continue;
        }
        let result = (|| -> Result<()> {
            // Narrow the file we actually hold open, never a path that could be
            // swapped for a symlink between the check and the chmod. O_NOFOLLOW
            // refuses symlinks and O_NONBLOCK refuses to wait on a FIFO.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)?;
            let metadata = file.metadata()?;
            if metadata.file_type().is_file() && metadata.permissions().mode() & 0o777 != FILE_MODE
            {
                file.set_permissions(fs::Permissions::from_mode(FILE_MODE))?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            log::error!(
                "could not narrow permissions on {}: {error:#}",
                path.display()
            );
        }
    }
}

fn narrow_directory(directory: &Path) {
    if let Err(error) = fs::set_permissions(directory, fs::Permissions::from_mode(DIR_MODE)) {
        log::warn!(
            "could not set permissions on {}: {error}",
            directory.display()
        );
    }
}

fn open_capture(directory: &Path) -> Result<(PathBuf, File)> {
    let stamp = Local::now().format("%Y-%m-%d-%H%M%S");
    for index in 0..MAX_SAME_SECOND {
        let tail = if index == 0 {
            String::new()
        } else {
            format!("-{index}")
        };
        let path = directory.join(format!("{CAPTURE_PREFIX}{stamp}{tail}{CAPTURE_SUFFIX}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    bail!(
        "{}: {MAX_SAME_SECOND} recordings already have timestamp {stamp}",
        directory.display()
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::{Condvar, mpsc as test_mpsc},
        time::SystemTime,
    };
    use tempfile::tempdir;

    fn config(directory: PathBuf) -> Recording {
        Recording {
            enabled: true,
            dir: directory,
            max_total_bytes: 1024 * 1024,
        }
    }

    #[derive(Default)]
    struct SinkState {
        samples: Mutex<Vec<f32>>,
        finalized: AtomicBool,
    }

    struct TestSink {
        state: Arc<SinkState>,
        write_fails: bool,
        finalize_fails: bool,
    }

    impl WavSink for TestSink {
        fn write(&mut self, samples: &[f32]) -> Result<()> {
            if self.write_fails {
                bail!("injected write failure");
            }
            lock(&self.state.samples).extend_from_slice(samples);
            Ok(())
        }

        fn duration(&self) -> u32 {
            lock(&self.state.samples).len() as u32
        }

        fn sample_rate(&self) -> u32 {
            1_000
        }

        fn finalize(self: Box<Self>) -> Result<()> {
            self.state.finalized.store(true, Ordering::Release);
            if self.finalize_fails {
                bail!("injected finalize failure");
            }
            Ok(())
        }
    }

    struct BlockingSink {
        entered: test_mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl WavSink for BlockingSink {
        fn write(&mut self, _samples: &[f32]) -> Result<()> {
            let _ = self.entered.send(());
            let (released, condition) = &*self.release;
            let mut released = lock(released);
            while !*released {
                released = condition
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(())
        }

        fn duration(&self) -> u32 {
            0
        }

        fn sample_rate(&self) -> u32 {
            1_000
        }

        fn finalize(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    fn test_sink(state: Arc<SinkState>) -> Box<dyn WavSink> {
        Box::new(TestSink {
            state,
            write_fails: false,
            finalize_fails: false,
        })
    }

    fn injected_recorder(directory: &Path) -> CaptureRecorder {
        let mut recorder = CaptureRecorder::new(config(directory.to_owned()), 1_000);
        recorder.writer_join = Duration::from_millis(25);
        recorder
    }

    #[test]
    fn pcm_uses_ties_even_and_clips() {
        let half = 0.5 / FULL_SCALE;
        assert_eq!(to_pcm16_sample(half), 0);
        assert_eq!(to_pcm16_sample(3.0 * half), 2);
        assert_eq!(to_pcm16_sample(-half), 0);
        assert_eq!(to_pcm16_sample(2.0), i16::MAX);
        assert_eq!(to_pcm16_sample(-2.0), -32_767);
    }

    #[test]
    fn recording_round_trips_and_is_private() {
        let temporary = tempdir().unwrap();
        let recorder = CaptureRecorder::new(config(temporary.path().join("audio")), 16_000);
        let samples = [-1.2, -0.5, 0.0, 0.5, 1.2];
        recorder.start();
        recorder.write(&samples[..2]);
        recorder.write(&samples[2..]);
        recorder.stop();
        let RecordingStatus::Recorded(path) = recorder.status() else {
            panic!("not recorded")
        };
        let (actual, rate) = read_capture(&path).unwrap();
        assert_eq!(rate, 16_000);
        let expected: Vec<_> = samples
            .iter()
            .map(|&sample| f32::from(to_pcm16_sample(sample)) / FULL_SCALE)
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            DIR_MODE
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            FILE_MODE
        );
    }

    #[test]
    fn intermediate_header_is_valid() {
        let temporary = tempdir().unwrap();
        let recorder = CaptureRecorder::new(config(temporary.path().join("audio")), 16_000);
        recorder.start();
        recorder.write(&[0.25; 64]);
        let path = match recorder.status() {
            RecordingStatus::Recorded(path) => path,
            _ => panic!(),
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if read_capture(&path).is_ok_and(|(samples, _)| samples.len() == 64) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "writer did not checkpoint the WAV"
            );
            thread::sleep(Duration::from_millis(5));
        }
        recorder.stop();
    }

    #[test]
    fn disabled_recorder_is_a_noop() {
        let temporary = tempdir().unwrap();
        let recorder = CaptureRecorder::new(
            Recording {
                enabled: false,
                dir: temporary.path().join("absent"),
                max_total_bytes: 1,
            },
            16_000,
        );
        recorder.start();
        recorder.write(&[1.0]);
        recorder.stop();
        assert_eq!(recorder.status(), RecordingStatus::NotRecorded);
        assert!(!temporary.path().join("absent").exists());
    }

    #[test]
    fn pruning_is_oldest_first_and_never_follows_symlinks() {
        let temporary = tempdir().unwrap();
        let directory = temporary.path();
        let old = directory.join("capture-old.wav");
        let new = directory.join("capture-new.wav");
        let stranger = directory.join("notes.txt");
        fs::write(&old, [0; 64]).unwrap();
        thread::sleep(Duration::from_millis(10));
        fs::write(&new, [0; 64]).unwrap();
        fs::write(&stranger, [0; 128]).unwrap();
        let link = directory.join("capture-link.wav");
        symlink(&stranger, &link).unwrap();
        assert!(fs::metadata(&old).unwrap().modified().unwrap() <= SystemTime::now());
        prune_recordings(directory, 64, |_| false);
        assert!(!old.exists());
        assert!(new.exists() && stranger.exists() && link.symlink_metadata().is_ok());
    }

    #[test]
    fn same_second_captures_get_distinct_names_until_the_ceiling() {
        let temporary = tempdir().unwrap();
        let directory = temporary.path();
        let mut opened = Vec::new();
        for _ in 0..3 {
            let (path, file) = open_capture(directory).unwrap();
            drop(file);
            opened.push(path);
        }
        let names: HashSet<_> = opened.iter().collect();
        assert_eq!(names.len(), 3, "same-second captures must not collide");
        assert!(
            opened[1]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-1.wav")
        );

        // Fill the whole second, then prove the ceiling is an error, not a
        // silent overwrite of somebody else's recording.
        let stamp = opened[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches(CAPTURE_PREFIX)
            .trim_end_matches(CAPTURE_SUFFIX)
            .to_owned();
        for index in 3..MAX_SAME_SECOND {
            fs::write(
                directory.join(format!("{CAPTURE_PREFIX}{stamp}-{index}{CAPTURE_SUFFIX}")),
                [],
            )
            .unwrap();
        }
        let error = open_capture(directory).unwrap_err().to_string();
        assert!(error.contains("already have timestamp"), "{error}");
    }

    #[test]
    fn pruning_keeps_every_file_a_live_writer_still_owns() {
        let temporary = tempdir().unwrap();
        let directory = temporary.path();
        for name in ["capture-a.wav", "capture-b.wav"] {
            fs::write(directory.join(name), [0; 64]).unwrap();
            thread::sleep(Duration::from_millis(5));
        }
        // Over quota by a wide margin, but nothing may be deleted.
        prune_recordings(directory, 0, |_| true);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 2);
        prune_recordings(directory, 0, |_| false);
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }

    #[test]
    fn read_rejects_implausibly_long_recordings() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("long.wav");
        // One sample per second: four hours of "audio" in five frames.
        let mut writer = WavWriter::create(&path, mono_pcm16(1)).unwrap();
        for _ in 0..(4 * 3600 + 1) {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        let error = read_capture(&path).unwrap_err().to_string();
        assert!(error.contains("recovery limit"), "{error}");
    }

    #[test]
    fn starting_a_capture_does_not_wait_for_a_stalled_writer() {
        let temporary = tempdir().unwrap();
        let mut recorder = CaptureRecorder::new(config(temporary.path().join("audio")), 1_000);
        recorder.writer_join = Duration::from_secs(3);
        let stalled = temporary.path().join("capture-stalled.wav");
        fs::write(&stalled, []).unwrap();
        let (entered_tx, entered_rx) = test_mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        recorder.launch(
            stalled,
            Box::new(BlockingSink {
                entered: entered_tx,
                release: Arc::clone(&release),
            }),
            1_000,
        );
        recorder.write(&[1.0]);
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        recorder.start();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "the key-press path joined the previous writer"
        );
        assert!(matches!(recorder.status(), RecordingStatus::Recorded(_)));

        let (released, condition) = &*release;
        *lock(released) = true;
        condition.notify_all();
    }

    #[test]
    fn read_rejects_stereo() {
        let temporary = tempdir().unwrap();
        let stereo = temporary.path().join("stereo.wav");
        let file = File::create(&stereo).unwrap();
        let mut writer = WavWriter::new(
            BufWriter::new(file),
            WavSpec {
                channels: 2,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: SampleFormat::Int,
            },
        )
        .unwrap();
        writer.write_sample(0_i16).unwrap();
        writer.write_sample(0_i16).unwrap();
        writer.finalize().unwrap();
        assert!(
            read_capture(&stereo)
                .unwrap_err()
                .to_string()
                .contains("mono")
        );
    }

    #[test]
    fn read_rejects_float_wav() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("float.wav");
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .unwrap();
        writer.write_sample(0.25_f32).unwrap();
        writer.finalize().unwrap();
        assert!(read_capture(&path).unwrap_err().to_string().contains("PCM"));
    }

    #[test]
    fn read_rejects_malformed_and_truncated_wavs() {
        let temporary = tempdir().unwrap();
        let malformed = temporary.path().join("malformed.wav");
        fs::write(&malformed, b"not a wav").unwrap();
        assert!(
            read_capture(&malformed)
                .unwrap_err()
                .to_string()
                .contains("not a readable wav")
        );

        let truncated = temporary.path().join("truncated.wav");
        dump_capture(&truncated, &[0.25; 32], 16_000).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&truncated)
            .unwrap()
            .set_len(45)
            .unwrap();
        assert!(read_capture(&truncated).is_err());
    }

    #[test]
    fn dump_capture_refuses_to_follow_symlinks() {
        let temporary = tempdir().unwrap();
        let target = temporary.path().join("target");
        fs::write(&target, b"private").unwrap();
        let link = temporary.path().join("capture.wav");
        symlink(&target, &link).unwrap();
        assert!(dump_capture(&link, &[0.5], 16_000).is_err());
        assert_eq!(fs::read(target).unwrap(), b"private");
    }

    #[test]
    fn asynchronous_write_and_finalize_failures_are_truncated() {
        let temporary = tempdir().unwrap();
        let recorder = injected_recorder(temporary.path());

        let write_state = Arc::new(SinkState::default());
        let write_path = temporary.path().join("capture-write.wav");
        recorder.launch(
            write_path.clone(),
            Box::new(TestSink {
                state: write_state,
                write_fails: true,
                finalize_fails: false,
            }),
            1_000,
        );
        recorder.write(&[0.5]);
        recorder.stop();
        assert_eq!(recorder.status(), RecordingStatus::Truncated(write_path));

        let finalize_state = Arc::new(SinkState::default());
        let finalize_path = temporary.path().join("capture-finalize.wav");
        recorder.launch(
            finalize_path.clone(),
            Box::new(TestSink {
                state: Arc::clone(&finalize_state),
                write_fails: false,
                finalize_fails: true,
            }),
            1_000,
        );
        recorder.write(&[0.25]);
        recorder.stop();
        assert!(finalize_state.finalized.load(Ordering::Acquire));
        assert_eq!(recorder.status(), RecordingStatus::Truncated(finalize_path));
    }

    #[test]
    fn frame_quota_truncates_immediately_and_accepts_later_audio() {
        let temporary = tempdir().unwrap();
        let recorder = injected_recorder(temporary.path());
        let state = Arc::new(SinkState::default());
        let path = temporary.path().join("capture-quota.wav");
        recorder.launch(path.clone(), test_sink(Arc::clone(&state)), 2);

        // One oversized callback is dropped, but does not poison the writer;
        // a later buffer still reaches the sink once it fits the frame quota.
        recorder.write(&[1.0, 2.0, 3.0]);
        assert_eq!(recorder.status(), RecordingStatus::Truncated(path.clone()));
        recorder.write(&[4.0, 5.0]);
        recorder.stop();
        assert_eq!(&*lock(&state.samples), &[4.0, 5.0]);
        assert_eq!(recorder.status(), RecordingStatus::Truncated(path));
    }

    #[test]
    fn stalled_writer_stop_is_bounded_and_new_capture_is_isolated() {
        let temporary = tempdir().unwrap();
        let recorder = injected_recorder(temporary.path());
        let first_path = temporary.path().join("capture-first.wav");
        fs::write(&first_path, []).unwrap();
        let (entered_tx, entered_rx) = test_mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        recorder.launch(
            first_path.clone(),
            Box::new(BlockingSink {
                entered: entered_tx,
                release: Arc::clone(&release),
            }),
            1_000,
        );
        recorder.write(&[1.0]);
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        recorder.stop();
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(
            recorder.status(),
            RecordingStatus::Truncated(first_path.clone())
        );
        assert!(lock(&recorder.open_paths).contains(&first_path));

        let second_path = temporary.path().join("capture-second.wav");
        fs::write(&second_path, []).unwrap();
        let second = Arc::new(SinkState::default());
        recorder.launch(second_path.clone(), test_sink(Arc::clone(&second)), 1_000);
        recorder.write(&[2.0, 3.0]);

        // A prune while both paths are open must keep the abandoned writer's
        // file. Once released, it removes only its own registry entry.
        prune_recordings(temporary.path(), 0, |candidate| {
            lock(&recorder.open_paths).contains(candidate)
        });
        assert!(first_path.exists());
        assert!(second_path.exists());
        recorder.stop();
        assert_eq!(&*lock(&second.samples), &[2.0, 3.0]);
        assert_eq!(recorder.status(), RecordingStatus::Recorded(second_path));
        let (released, condition) = &*release;
        *lock(released) = true;
        condition.notify_all();
        let deadline = Instant::now() + Duration::from_secs(1);
        while lock(&recorder.open_paths).contains(&first_path) {
            assert!(Instant::now() < deadline, "abandoned writer was not pruned");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn concurrent_stop_and_write_do_not_leak_into_next_capture() {
        let temporary = tempdir().unwrap();
        let recorder = Arc::new(injected_recorder(temporary.path()));
        let first = Arc::new(SinkState::default());
        recorder.launch(
            temporary.path().join("capture-one.wav"),
            test_sink(Arc::clone(&first)),
            10_000,
        );
        thread::scope(|scope| {
            let writer_recorder = Arc::clone(&recorder);
            let writer = scope.spawn(move || {
                for _ in 0..100 {
                    writer_recorder.write(&[1.0]);
                }
            });
            recorder.stop();
            writer.join().unwrap();
        });

        let second = Arc::new(SinkState::default());
        recorder.launch(
            temporary.path().join("capture-two.wav"),
            test_sink(Arc::clone(&second)),
            10_000,
        );
        recorder.write(&[2.0; 10]);
        recorder.stop();
        assert!(lock(&first.samples).iter().all(|sample| *sample == 1.0));
        assert_eq!(&*lock(&second.samples), &[2.0; 10]);
    }
}
