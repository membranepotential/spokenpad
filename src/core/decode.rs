//! Shared live/recovery pipeline. Preview output has a separate event type.
use crate::core::frames::Frames;
use anyhow::{Result, ensure};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

pub trait Recognizer {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String>;
}
pub trait Segmenter {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>>;
}
/// One recognizer input. `window` indexes the slice that was segmented;
/// `speech_end` is the unpadded speech boundary inside the same slice, which
/// is what a settled commit advances the offset to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub window: Range<usize>,
    pub speech_end: usize,
    pub settled: bool,
}

/// An utterance moves forwards only: recording, then either released for its
/// final decode or cancelled outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Lifecycle {
    Live = 0,
    Released = 1,
    Cancelled = 2,
}

impl Lifecycle {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Live,
            1 => Self::Released,
            _ => Self::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtteranceId(pub u64);
#[derive(Debug)]
pub struct Utterance {
    pub id: UtteranceId,
    lifecycle: AtomicU8,
}
impl Utterance {
    pub fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id: UtteranceId(id),
            lifecycle: AtomicU8::new(Lifecycle::Live as u8),
        })
    }
    fn advance(&self, to: Lifecycle) {
        self.lifecycle.fetch_max(to as u8, Ordering::AcqRel);
    }
    pub fn release(&self) {
        self.advance(Lifecycle::Released);
    }
    pub fn cancel(&self) {
        self.advance(Lifecycle::Cancelled);
    }
    pub fn lifecycle(&self) -> Lifecycle {
        Lifecycle::from_bits(self.lifecycle.load(Ordering::Acquire))
    }
    pub fn cancelled(&self) -> bool {
        self.lifecycle() == Lifecycle::Cancelled
    }
    /// Still recording: preview ticks may be issued and their text shown.
    pub fn ticking(&self) -> bool {
        self.lifecycle() == Lifecycle::Live
    }
}

pub struct Pipeline<R, S> {
    pub recognizer: R,
    pub segmenter: Option<S>,
}
impl<R: Recognizer, S: Segmenter> Pipeline<R, S> {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>> {
        let segments = match &mut self.segmenter {
            Some(s) => s.split(samples)?,
            None => vec![Segment {
                window: 0..samples.len(),
                speech_end: samples.len(),
                settled: false,
            }],
        };
        let mut end = 0;
        for s in &segments {
            ensure!(
                s.window.start <= s.speech_end
                    && s.speech_end <= s.window.end
                    && s.window.end <= samples.len()
                    && s.speech_end >= end,
                "invalid segment boundaries"
            );
            end = s.speech_end;
        }
        Ok(segments)
    }
    pub fn decode(
        &mut self,
        samples: &[f32],
        mut abandoned: impl FnMut() -> bool,
        mut on_segment: impl FnMut(String),
    ) -> Result<String> {
        if samples.is_empty() || abandoned() {
            return Ok(String::new());
        }
        let segments = self.split(samples)?;
        let mut texts = vec![];
        for s in &segments {
            if abandoned() {
                return Ok(texts.join(" "));
            }
            let text = self.recognizer.transcribe(&samples[s.window.clone()])?;
            log::debug!("chunk {:?}: {text:?}", s.window);
            if abandoned() {
                return Ok(texts.join(" "));
            }
            if !text.trim().is_empty() {
                texts.push(text.clone());
                on_segment(text);
            }
        }
        // Explicit failure recovery exception: don't let segmentation erase speech.
        if texts.is_empty() && segments.len() != 1 && !abandoned() {
            log::warn!(
                "all {} chunks empty; retrying the complete remainder",
                segments.len()
            );
            let text = self.recognizer.transcribe(samples)?;
            if !text.trim().is_empty() && !abandoned() {
                texts.push(text.clone());
                on_segment(text);
            }
        }
        Ok(texts.join(" "))
    }
}

#[derive(Debug)]
pub struct Commit {
    pub utterance: Arc<Utterance>,
    pub text: String,
    pub through: Frames,
}
/// Cosmetic text for the open tail. Absence of a `Preview` is the single
/// encoding of "this tick produced nothing to show".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub text: String,
    pub through: Frames,
}
#[derive(Default)]
struct Progress {
    id: Option<UtteranceId>,
    through: Frames,
}
pub struct Worker<R, S> {
    pub pipeline: Pipeline<R, S>,
    progress: Progress,
}
impl<R: Recognizer, S: Segmenter> Worker<R, S> {
    pub fn new(pipeline: Pipeline<R, S>) -> Self {
        Self {
            pipeline,
            progress: Progress::default(),
        }
    }
    fn begin(&mut self, utterance: &Utterance) {
        if self.progress.id != Some(utterance.id) {
            self.progress = Progress {
                id: Some(utterance.id),
                through: Frames::ZERO,
            };
        }
    }
    /// Commits every chunk that has settled and returns the open tail's
    /// cosmetic text, or `None` once the utterance stopped ticking.
    pub fn tick(
        &mut self,
        samples: &[f32],
        start: Frames,
        utterance: &Arc<Utterance>,
        mut commit: impl FnMut(Commit),
    ) -> Result<Option<Preview>> {
        if !utterance.ticking() {
            return Ok(None);
        }
        self.begin(utterance);
        ensure!(
            start <= self.progress.through,
            "snapshot begins after the worker's offset"
        );
        let skip = self.progress.through.since(start).min(samples.len());
        let remainder = &samples[skip..];
        if remainder.is_empty() {
            return Ok(Some(self.open_tail(String::new())));
        }
        let base = self.progress.through;
        for segment in self.pipeline.split(remainder)? {
            if !utterance.ticking() {
                return Ok(None);
            }
            let text = self
                .pipeline
                .recognizer
                .transcribe(&remainder[segment.window.clone()])?;
            if !segment.settled {
                return Ok(utterance.ticking().then(|| self.open_tail(text)));
            }
            if utterance.cancelled() {
                return Ok(None);
            }
            // A release during this decode keeps this completed settled chunk.
            self.progress.through = base + segment.speech_end;
            commit(Commit {
                utterance: Arc::clone(utterance),
                text,
                through: self.progress.through,
            });
        }
        Ok(utterance.ticking().then(|| self.open_tail(String::new())))
    }

    fn open_tail(&self, text: String) -> Preview {
        Preview {
            text,
            through: self.progress.through,
        }
    }
    pub fn finish(
        &mut self,
        samples: &[f32],
        utterance: &Arc<Utterance>,
        mut commit: impl FnMut(Commit),
    ) -> Result<(String, usize)> {
        self.begin(utterance);
        let remainder = &samples[self.progress.through.get().min(samples.len())..];
        let tail_frames = remainder.len();
        let result = self.pipeline.decode(
            remainder,
            || utterance.cancelled(),
            |text| {
                commit(Commit {
                    utterance: Arc::clone(utterance),
                    text,
                    through: Frames(samples.len()),
                })
            },
        );
        self.progress = Progress::default();
        Ok((result?, tail_frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        calls: Vec<Vec<f32>>,
        replies: Vec<String>,
        stop: Option<Arc<Utterance>>,
    }
    impl Recognizer for Fake {
        fn transcribe(&mut self, s: &[f32]) -> Result<String> {
            self.calls.push(s.to_vec());
            if let Some(u) = &self.stop {
                u.release();
            }
            Ok(if self.replies.is_empty() {
                "text".into()
            } else {
                self.replies.remove(0)
            })
        }
    }
    struct Split;
    impl Segmenter for Split {
        fn split(&mut self, s: &[f32]) -> Result<Vec<Segment>> {
            Ok((0..s.len())
                .step_by(4)
                .map(|i| Segment {
                    window: i..(i + 4).min(s.len()),
                    speech_end: (i + 4).min(s.len()),
                    settled: i + 4 < s.len(),
                })
                .collect())
        }
    }
    fn worker() -> Worker<Fake, Split> {
        Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Some(Split),
        })
    }
    #[test]
    fn progressive_release_never_redecodes_committed_speech() {
        let mut w = worker();
        let u = Utterance::new(1);
        let samples: Vec<_> = (0..10).map(|i| i as f32).collect();
        let mut commits = vec![];
        assert_eq!(
            w.tick(&samples, Frames::ZERO, &u, |c| commits.push(c))
                .unwrap(),
            Some(Preview {
                text: "text".into(),
                through: Frames(8)
            })
        );
        assert_eq!(
            commits.iter().map(|c| c.through).collect::<Vec<_>>(),
            vec![Frames(4), Frames(8)]
        );
        u.release();
        let (_, tail) = w.finish(&samples, &u, |c| commits.push(c)).unwrap();
        assert_eq!(tail, 2);
        assert_eq!(
            w.pipeline.recognizer.calls,
            vec![
                vec![0., 1., 2., 3.],
                vec![4., 5., 6., 7.],
                vec![8., 9.],
                vec![8., 9.]
            ]
        );
        assert_eq!(commits.len(), 3);
    }
    #[test]
    fn lagging_snapshot_and_stale_tick() {
        let mut w = worker();
        let a = Utterance::new(1);
        let s: Vec<_> = (0..10).map(|i| i as f32).collect();
        w.tick(&s, Frames::ZERO, &a, |_| {}).unwrap();
        w.tick(&s[4..], Frames(4), &a, |_| panic!("duplicate commit"))
            .unwrap();
        assert_eq!(w.pipeline.recognizer.calls.last().unwrap(), &vec![8., 9.]);
        a.release();
        let b = Utterance::new(2);
        assert_eq!(w.tick(&s, Frames::ZERO, &a, |_| panic!()).unwrap(), None);
        w.finish(&s, &a, |_| {}).unwrap();
        w.tick(&s, Frames::ZERO, &b, |_| {}).unwrap();
        assert_eq!(w.progress.through, Frames(8));
    }
    #[test]
    fn release_in_settled_decode_commits_current_chunk_then_stops() {
        let mut w = worker();
        let u = Utterance::new(1);
        w.pipeline.recognizer.stop = Some(u.clone());
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[1.; 10], Frames::ZERO, &u, |c| commits.push(c))
                .unwrap(),
            None
        );
        assert_eq!(commits.len(), 1);
        assert_eq!(w.progress.through, Frames(4));
    }
    #[test]
    fn cancel_and_no_vad() {
        let mut w = worker();
        let u = Utterance::new(1);
        u.cancel();
        w.finish(&[1.; 10], &u, |_| panic!()).unwrap();
        assert!(w.pipeline.recognizer.calls.is_empty());
        let u = Utterance::new(2);
        w.pipeline.segmenter = None;
        w.tick(&[1.; 10], Frames::ZERO, &u, |_| panic!()).unwrap();
        assert_eq!(w.progress.through, Frames::ZERO);
        u.release();
        assert_eq!(w.finish(&[1.; 10], &u, |_| {}).unwrap().1, 10);
    }
    #[test]
    fn empty_chunks_retry_whole_buffer() {
        let mut w = worker();
        w.pipeline.recognizer.replies = vec!["".into(), "".into(), "recovered".into()];
        let mut commits = vec![];
        let (text, _) = w
            .finish(&[1.; 8], &Utterance::new(1), |c| commits.push(c))
            .unwrap();
        assert_eq!(text, "recovered");
        assert_eq!(commits.len(), 1);
        assert_eq!(w.pipeline.recognizer.calls.len(), 3);
    }
    #[test]
    fn lifecycle_only_moves_forwards() {
        let u = Utterance::new(1);
        assert_eq!(u.lifecycle(), Lifecycle::Live);
        assert!(u.ticking() && !u.cancelled());
        u.release();
        assert_eq!(u.lifecycle(), Lifecycle::Released);
        assert!(!u.ticking() && !u.cancelled());
        u.cancel();
        assert_eq!(u.lifecycle(), Lifecycle::Cancelled);
        u.release();
        assert_eq!(u.lifecycle(), Lifecycle::Cancelled, "a cancel is final");
        assert!(u.cancelled() && !u.ticking());
    }
    #[test]
    fn cancel_during_decode_suppresses_result() {
        struct Cancel(Arc<Utterance>);
        impl Recognizer for Cancel {
            fn transcribe(&mut self, _: &[f32]) -> Result<String> {
                self.0.cancel();
                Ok("late".into())
            }
        }
        let u = Utterance::new(1);
        let mut p = Pipeline {
            recognizer: Cancel(u.clone()),
            segmenter: Some(Split),
        };
        assert_eq!(
            p.decode(&[1.; 8], || u.cancelled(), |_| panic!("late commit"))
                .unwrap(),
            ""
        );
    }
}
