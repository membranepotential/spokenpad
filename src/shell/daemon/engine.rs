//! The inference thread: it builds the pipeline if it was not handed one,
//! then decodes whatever the event loop sends it, in order, one piece of
//! work at a time.
use super::{Loader, PipelineSource, ResultEvent};
use crate::{
    core::{
        decode::{Commit, Recognizer, Segmenter, TickKind, Utterance, Worker},
        frames::Frames,
    },
    shell::recorder::CaptureReader,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{Receiver, Sender},
    },
    time::Instant,
};

/// A recording ends, or stops being readable, before the text already written
/// from it does: it was cut or replaced since, and its rest is gone.
#[derive(Debug)]
pub(super) struct ShorterThanItsText {
    recorded: f64,
    through: f64,
}

impl std::fmt::Display for ShorterThanItsText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "it ends at {:.2}s, before the {:.2}s its text already reaches",
            self.recorded, self.through
        )
    }
}

impl std::error::Error for ShorterThanItsText {}

pub(super) enum Work {
    Tick {
        audio: Vec<f32>,
        start: Frames,
        utterance: Arc<Utterance>,
        kind: TickKind,
    },
    Finish(Tail),
    /// Transcribe a recording made before the model was ready, or left by
    /// the last daemon, from `from` on: its text before that is written.
    Recording {
        path: PathBuf,
        utterance: Arc<Utterance>,
        from: Frames,
    },
    /// Try the loader again: the last attempt failed.
    Load,
    Quit,
}

/// What a released capture still holds for its final decode: the audio
/// after `start`, everything before it having been committed.
pub(super) struct Tail {
    pub(super) audio: Vec<f32>,
    pub(super) start: Frames,
    pub(super) utterance: Arc<Utterance>,
}

/// The inference thread: builds the pipeline if it was not handed one ready,
/// then decodes what the event loop sends it. `window` is the worker's: the
/// most audio one tick reads, live or from a recording.
pub(super) fn engine_thread<R: Recognizer, S: Segmenter>(
    pipeline: PipelineSource<R, S>,
    window: usize,
    rate: u32,
    work_rx: &Receiver<Work>,
    result_tx: &Sender<ResultEvent>,
) {
    let mut worker = match pipeline {
        PipelineSource::Ready(pipeline) => Worker::new(pipeline, window),
        PipelineSource::Load(loader) => match load(loader, window, work_rx, result_tx) {
            Some(worker) => worker,
            None => return,
        },
    };
    while let Ok(work) = work_rx.recv() {
        let sent = match work {
            Work::Tick {
                audio,
                start,
                utterance,
                kind,
            } => {
                let started = Instant::now();
                let result = worker.tick(&audio, start, &utterance, kind, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Tick {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Finish(Tail {
                audio,
                start,
                utterance,
            }) => {
                let started = Instant::now();
                let result = worker.finish(&audio, start, &utterance, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                });
                result_tx.send(ResultEvent::Finished {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            Work::Recording {
                path,
                utterance,
                from,
            } => {
                let started = Instant::now();
                let result = decode_recording(&mut worker, &path, &utterance, from, rate, |c| {
                    let _ = result_tx.send(ResultEvent::Commit(c));
                })
                .with_context(|| format!("transcribe {}", path.display()));
                result_tx.send(ResultEvent::Finished {
                    id: utterance.id,
                    result,
                    elapsed: started.elapsed(),
                })
            }
            // Loaded already.
            Work::Load => Ok(()),
            Work::Quit => break,
        };
        if sent.is_err() {
            break;
        }
    }
}

/// Runs `loader` until it builds the pipeline: once now, and again at every
/// [`Work::Load`] after a failure. `None` when told to stop first.
fn load<R: Recognizer, S: Segmenter>(
    mut loader: Loader<R, S>,
    window: usize,
    work_rx: &Receiver<Work>,
    result_tx: &Sender<ResultEvent>,
) -> Option<Worker<R, S>> {
    loop {
        let started = Instant::now();
        let loaded = loader(&mut |step| {
            let _ = result_tx.send(ResultEvent::Preparing(step));
        });
        match loaded {
            Ok(pipeline) => {
                let ready = ResultEvent::Ready {
                    elapsed: started.elapsed(),
                };
                return result_tx
                    .send(ready)
                    .is_ok()
                    .then(|| Worker::new(pipeline, window));
            }
            Err(e) => result_tx
                .send(ResultEvent::Unavailable(format!("{e:#}")))
                .ok()?,
        }
        // Nothing to decode can come before the pipeline is built: the event
        // loop keeps every capture on disk until it hears `Ready`.
        loop {
            match work_rx.recv().ok()? {
                Work::Load => break,
                Work::Quit => return None,
                Work::Tick { .. } | Work::Finish(_) | Work::Recording { .. } => {
                    log::error!("inference work arrived before the speech model loaded; dropped");
                }
            }
        }
    }
}

/// Transcribes a recording made before the model was ready, the way a live
/// capture of it would have been: settled chunks committed a window at a
/// time, then the tail decoded as the release decodes it. Only the worker's
/// window and the open tail are ever in memory. Everything before `from` is committed
/// already, and not read.
fn decode_recording<R: Recognizer, S: Segmenter>(
    worker: &mut Worker<R, S>,
    path: &Path,
    utterance: &Arc<Utterance>,
    from: Frames,
    rate: u32,
    mut commit: impl FnMut(Commit),
) -> Result<(String, usize)> {
    let mut reader = CaptureReader::open(path, rate)?;
    let skipped = reader.skip(from.get());
    if skipped < from.get() {
        return Err(ShorterThanItsText {
            recorded: Frames(skipped).seconds(rate),
            through: from.seconds(rate),
        }
        .into());
    }
    worker.resume(utterance, from);
    let window = worker.window();
    let mut held = Vec::with_capacity(window);
    let mut start = from;
    loop {
        let wanted = window.saturating_sub(held.len());
        if reader.read(&mut held, wanted)? < wanted {
            break;
        }
        // A full window, as a live tick past `preview.max_seconds` has: one
        // with nothing settled in it -- speech the detector never breaks, or
        // pauses too short to settle it -- is committed through its last
        // pause, or whole, which keeps this to a window.
        let Some(tail) = worker.tick(&held, start, utterance, TickKind::Window, &mut commit)?
        else {
            break;
        };
        let settled = tail.through.since(start).min(held.len());
        ensure!(settled > 0, "a window of the recording settled nothing");
        held.drain(..settled);
        start += settled;
    }
    utterance.release();
    worker.finish(&held, start, utterance, commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::decode::{Pipeline, Segment, Split, TrailingSilence};

    /// Counts the samples it is given.
    struct Count;
    impl Recognizer for Count {
        fn transcribe(&mut self, samples: &[f32], _: TrailingSilence) -> Result<String> {
            Ok(samples.len().to_string())
        }
    }
    /// Blocks of a fixed size, each settled once audio follows it.
    struct Blocks(usize);
    impl Segmenter for Blocks {
        fn split(&mut self, samples: &[f32]) -> Result<Split> {
            let segments = (0..samples.len())
                .step_by(self.0)
                .map(|start| {
                    let end = (start + self.0).min(samples.len());
                    Segment {
                        window: start..end,
                        speech_end: end,
                        settled: end < samples.len(),
                    }
                })
                .collect();
            Ok(Split {
                segments,
                ..Split::default()
            })
        }
    }

    /// A recording made before the model was ready is committed a window at
    /// a time, never read whole, and every sample is decoded exactly once.
    #[test]
    fn a_recording_is_transcribed_a_window_at_a_time_every_sample_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(
            Pipeline {
                recognizer: Count,
                segmenter: Blocks(1_000),
            },
            3_000,
        );
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames::ZERO,
            16_000,
            |c| commits.push((c.text.parse::<usize>().unwrap(), c.through)),
        )
        .unwrap();
        assert_eq!(
            commits,
            (1..=10)
                .map(|block| (1_000, Frames(block * 1_000)))
                .collect::<Vec<_>>(),
            "one commit per block, each once, each reaching as far as its block"
        );
    }

    /// A recording whose text reaches partway already is transcribed from
    /// there: the rest once, nothing before it again.
    #[test]
    fn a_recording_is_transcribed_from_where_its_text_reaches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(
            Pipeline {
                recognizer: Count,
                segmenter: Blocks(1_000),
            },
            3_000,
        );
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames(4_000),
            16_000,
            |c| commits.push((c.text.parse::<usize>().unwrap(), c.through)),
        )
        .unwrap();
        assert_eq!(commits.iter().map(|c| c.0).sum::<usize>(), 6_000);
        assert_eq!(commits.first().map(|c| c.1), Some(Frames(5_000)));
        assert_eq!(commits.last().map(|c| c.1), Some(Frames(10_000)));

        let beyond = decode_recording(
            &mut worker,
            &path,
            &Utterance::new(2),
            Frames(20_000),
            16_000,
            |_| panic!("nothing to commit"),
        );
        assert!(beyond.is_err(), "a recording shorter than its offset");
    }

    /// With nothing ever settling -- speech the detector never breaks -- a
    /// recording is still read and decoded a window at a time, never whole,
    /// and every sample is still decoded once.
    #[test]
    fn a_recording_with_nothing_settled_is_held_a_window_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.wav");
        crate::shell::recorder::dump_capture(&path, &[0.5; 10_000], 16_000).unwrap();
        let mut worker = Worker::new(
            Pipeline {
                recognizer: Count,
                segmenter: Blocks(usize::MAX),
            },
            3_000,
        );
        let mut commits = Vec::new();
        decode_recording(
            &mut worker,
            &path,
            &Utterance::new(1),
            Frames::ZERO,
            16_000,
            |c| commits.push(c.text.parse::<usize>().unwrap()),
        )
        .unwrap();
        assert!(
            commits.iter().all(|&decoded| decoded <= 3_000),
            "a decode larger than the window: {commits:?}"
        );
        assert_eq!(commits.iter().sum::<usize>(), 10_000, "{commits:?}");
    }
}
