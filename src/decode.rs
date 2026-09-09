//! Shared live/recovery pipeline. Preview output has a separate event type.
use anyhow::{Result, ensure};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub trait Recognizer {
    fn transcribe(&mut self, samples: &[f32]) -> Result<String>;
}
pub trait Segmenter {
    fn split(&mut self, samples: &[f32]) -> Result<Vec<Segment>>;
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub samples: Range<usize>,
    pub end_frame: usize,
    pub settled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtteranceId(pub u64);
#[derive(Debug)]
pub struct Utterance {
    pub id: UtteranceId,
    ticking: AtomicBool,
    cancelled: AtomicBool,
}
impl Utterance {
    pub fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id: UtteranceId(id),
            ticking: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
        })
    }
    pub fn release(&self) {
        self.ticking.store(false, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.release();
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub fn ticking(&self) -> bool {
        self.ticking.load(Ordering::Acquire) && !self.cancelled()
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
                samples: 0..samples.len(),
                end_frame: samples.len(),
                settled: false,
            }],
        };
        let mut end = 0;
        for s in &segments {
            ensure!(
                s.samples.start <= s.end_frame
                    && s.end_frame <= s.samples.end
                    && s.samples.end <= samples.len()
                    && s.end_frame >= end,
                "invalid segment boundaries"
            );
            end = s.end_frame;
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
            let text = self.recognizer.transcribe(&samples[s.samples.clone()])?;
            log::debug!("chunk {:?}: {text:?}", s.samples);
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
    pub through: usize,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Preview { text: String, through: usize },
    Stopped,
}
#[derive(Default)]
struct Progress {
    id: Option<UtteranceId>,
    through: usize,
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
                through: 0,
            };
        }
    }
    pub fn tick(
        &mut self,
        samples: &[f32],
        start: usize,
        utterance: &Arc<Utterance>,
        mut commit: impl FnMut(Commit),
    ) -> Result<Tick> {
        if !utterance.ticking() {
            return Ok(Tick::Stopped);
        }
        self.begin(utterance);
        ensure!(
            start <= self.progress.through,
            "snapshot begins after the worker's offset"
        );
        let skip = (self.progress.through - start).min(samples.len());
        let remainder = &samples[skip..];
        if remainder.is_empty() {
            return Ok(Tick::Preview {
                text: String::new(),
                through: self.progress.through,
            });
        }
        let base = self.progress.through;
        for segment in self.pipeline.split(remainder)? {
            if !utterance.ticking() {
                return Ok(Tick::Stopped);
            }
            let text = self
                .pipeline
                .recognizer
                .transcribe(&remainder[segment.samples.clone()])?;
            if utterance.cancelled() {
                return Ok(Tick::Stopped);
            }
            if !segment.settled {
                return Ok(if utterance.ticking() {
                    Tick::Preview {
                        text,
                        through: self.progress.through,
                    }
                } else {
                    Tick::Stopped
                });
            }
            // A release during this decode keeps this completed settled chunk.
            self.progress.through = base + segment.end_frame;
            commit(Commit {
                utterance: Arc::clone(utterance),
                text,
                through: self.progress.through,
            });
        }
        Ok(if utterance.ticking() {
            Tick::Preview {
                text: String::new(),
                through: self.progress.through,
            }
        } else {
            Tick::Stopped
        })
    }
    pub fn finish(
        &mut self,
        samples: &[f32],
        utterance: &Arc<Utterance>,
        mut commit: impl FnMut(Commit),
    ) -> Result<(String, usize)> {
        self.begin(utterance);
        let remainder = &samples[self.progress.through.min(samples.len())..];
        let tail_frames = remainder.len();
        let result = self.pipeline.decode(
            remainder,
            || utterance.cancelled(),
            |text| {
                commit(Commit {
                    utterance: Arc::clone(utterance),
                    text,
                    through: samples.len(),
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
                    samples: i..(i + 4).min(s.len()),
                    end_frame: (i + 4).min(s.len()),
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
            w.tick(&samples, 0, &u, |c| commits.push(c)).unwrap(),
            Tick::Preview {
                text: "text".into(),
                through: 8
            }
        );
        assert_eq!(
            commits.iter().map(|c| c.through).collect::<Vec<_>>(),
            vec![4, 8]
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
        w.tick(&s, 0, &a, |_| {}).unwrap();
        w.tick(&s[4..], 4, &a, |_| panic!("duplicate commit"))
            .unwrap();
        assert_eq!(w.pipeline.recognizer.calls.last().unwrap(), &vec![8., 9.]);
        a.release();
        let b = Utterance::new(2);
        assert_eq!(w.tick(&s, 0, &a, |_| panic!()).unwrap(), Tick::Stopped);
        w.finish(&s, &a, |_| {}).unwrap();
        w.tick(&s, 0, &b, |_| {}).unwrap();
        assert_eq!(w.progress.through, 8);
    }
    #[test]
    fn release_in_settled_decode_commits_current_chunk_then_stops() {
        let mut w = worker();
        let u = Utterance::new(1);
        w.pipeline.recognizer.stop = Some(u.clone());
        let mut commits = vec![];
        assert_eq!(
            w.tick(&[1.; 10], 0, &u, |c| commits.push(c)).unwrap(),
            Tick::Stopped
        );
        assert_eq!(commits.len(), 1);
        assert_eq!(w.progress.through, 4);
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
        w.tick(&[1.; 10], 0, &u, |_| panic!()).unwrap();
        assert_eq!(w.progress.through, 0);
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
