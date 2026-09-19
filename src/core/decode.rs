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

/// Whether the recognizer appends its trailing silence to the samples it is
/// given. `Padded` is the normal presentation. Parakeet sometimes decodes a
/// short utterance to "" with the silence and to the right words without it,
/// and sometimes the other way round, so neither is safe on its own; `Bare`
/// exists for the retry in [`Pipeline::transcribe_speech`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailingSilence {
    Padded,
    Bare,
}
pub trait Recognizer {
    fn transcribe(&mut self, samples: &[f32], trailing: TrailingSilence) -> Result<String>;
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
    /// Decodes one segment's window. A window the VAD marked as speech that
    /// decodes to nothing is decoded once more without the trailing silence,
    /// and that result stands. This is the only second decode of committed
    /// audio: it runs only when the first decode produced no text, so every
    /// window's output still comes from exactly one decode. Without a
    /// segmenter nothing claims the window holds speech, so it is not retried.
    fn transcribe_speech(&mut self, window: &[f32]) -> Result<String> {
        let text = self
            .recognizer
            .transcribe(window, TrailingSilence::Padded)?;
        if !text.trim().is_empty() || self.segmenter.is_none() {
            return Ok(text);
        }
        let retry = self.recognizer.transcribe(window, TrailingSilence::Bare)?;
        let seconds = Frames(window.len()).seconds(crate::config::REQUIRED_SAMPLE_RATE);
        if retry.trim().is_empty() {
            log::debug!("{seconds:.1}s of speech decoded empty with and without trailing silence");
        } else {
            log::info!(
                "{seconds:.1}s of speech decoded empty; without trailing silence: {retry:?}"
            );
        }
        Ok(retry)
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
        // Silence is not decoded. `split` only returns nothing when a VAD model
        // found no speech at all; the no-segmenter path always yields one
        // whole-buffer segment, so nothing that knows less than the VAD is
        // silenced here. The rate is the validated project-wide constant.
        if segments.is_empty() {
            log::debug!(
                "VAD found no speech in {:.1}s; nothing to decode",
                Frames(samples.len()).seconds(crate::config::REQUIRED_SAMPLE_RATE)
            );
            return Ok(String::new());
        }
        let mut texts = vec![];
        for s in &segments {
            if abandoned() {
                return Ok(texts.join(" "));
            }
            let text = self.transcribe_speech(&samples[s.window.clone()])?;
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
        if texts.is_empty() && segments.len() > 1 && !abandoned() {
            log::warn!(
                "all {} chunks empty; retrying the complete remainder",
                segments.len()
            );
            let text = self
                .recognizer
                .transcribe(samples, TrailingSilence::Padded)?;
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
            let window = &remainder[segment.window.clone()];
            // The preview is cosmetic and redrawn every tick, so an empty open
            // tail is not retried; only a settled chunk's text is permanent.
            if !segment.settled {
                let text = self
                    .pipeline
                    .recognizer
                    .transcribe(window, TrailingSilence::Padded)?;
                return Ok(utterance.ticking().then(|| self.open_tail(text)));
            }
            let text = self.pipeline.transcribe_speech(window)?;
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
        fn transcribe(&mut self, s: &[f32], _: TrailingSilence) -> Result<String> {
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
        // Two chunks, each decoded padded and then bare, then the remainder.
        w.pipeline.recognizer.replies = vec![
            "".into(),
            "".into(),
            " ".into(),
            "".into(),
            "recovered".into(),
        ];
        let mut commits = vec![];
        let (text, _) = w
            .finish(&[1.; 8], &Utterance::new(1), |c| commits.push(c))
            .unwrap();
        assert_eq!(text, "recovered");
        assert_eq!(commits.len(), 1);
        assert_eq!(w.pipeline.recognizer.calls.len(), 5);
    }
    /// Parakeet's failure mode: nothing with the trailing silence, `bare`
    /// without it.
    struct DeafWhenPadded {
        bare: &'static str,
        calls: Vec<TrailingSilence>,
    }
    impl Recognizer for DeafWhenPadded {
        fn transcribe(&mut self, _: &[f32], trailing: TrailingSilence) -> Result<String> {
            self.calls.push(trailing);
            Ok(match trailing {
                TrailingSilence::Padded => "",
                TrailingSilence::Bare => self.bare,
            }
            .into())
        }
    }
    fn deaf_worker(bare: &'static str) -> Worker<DeafWhenPadded, Split> {
        Worker::new(Pipeline {
            recognizer: DeafWhenPadded {
                bare,
                calls: vec![],
            },
            segmenter: Some(Split),
        })
    }
    #[test]
    fn empty_speech_is_decoded_again_without_trailing_silence() {
        use TrailingSilence::{Bare, Padded};
        let mut w = deaf_worker("words");
        let u = Utterance::new(1);
        let samples = [1.; 10];
        let mut commits = vec![];
        let preview = w
            .tick(&samples, Frames::ZERO, &u, |c| commits.push(c))
            .unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|c| (c.text.as_str(), c.through))
                .collect::<Vec<_>>(),
            vec![("words", Frames(4)), ("words", Frames(8))],
            "each settled chunk commits its retried text"
        );
        assert_eq!(
            preview.map(|p| p.text),
            Some(String::new()),
            "the preview is not retried"
        );
        assert_eq!(
            w.pipeline.recognizer.calls,
            [Padded, Bare, Padded, Bare, Padded]
        );
        u.release();
        let (text, tail) = w.finish(&samples, &u, |c| commits.push(c)).unwrap();
        assert_eq!(
            (text.as_str(), tail),
            ("words", 2),
            "the release path retries"
        );
        assert_eq!(commits.len(), 3);
    }
    #[test]
    fn speech_empty_either_way_commits_nothing_and_still_advances() {
        use TrailingSilence::{Bare, Padded};
        let mut w = deaf_worker("");
        let u = Utterance::new(1);
        let mut commits = vec![];
        w.tick(&[1.; 10], Frames::ZERO, &u, |c| commits.push(c))
            .unwrap();
        assert!(commits.iter().all(|c| c.text.is_empty()));
        assert_eq!(w.progress.through, Frames(8));
        let fresh = Utterance::new(2);
        let (text, _) = w.finish(&[1.; 8], &fresh, |_| panic!()).unwrap();
        assert_eq!(text, "");
        assert_eq!(
            w.pipeline.recognizer.calls[5..],
            [Padded, Bare, Padded, Bare, Padded],
            "both chunks retried once, then the whole remainder once"
        );
    }
    #[test]
    fn without_a_segmenter_empty_text_is_not_retried() {
        let mut w = deaf_worker("words");
        w.pipeline.segmenter = None;
        let (text, _) = w.finish(&[1.; 8], &Utterance::new(1), |_| {}).unwrap();
        assert_eq!(text, "");
        assert_eq!(w.pipeline.recognizer.calls, [TrailingSilence::Padded]);
    }
    #[test]
    fn a_capture_the_vad_hears_no_speech_in_is_not_decoded() {
        struct Silent;
        impl Segmenter for Silent {
            fn split(&mut self, _: &[f32]) -> Result<Vec<Segment>> {
                Ok(vec![])
            }
        }
        let mut w = Worker::new(Pipeline {
            recognizer: Fake {
                calls: vec![],
                replies: vec![],
                stop: None,
            },
            segmenter: Some(Silent),
        });
        let u = Utterance::new(1);
        assert_eq!(
            w.tick(&[0.; 10], Frames::ZERO, &u, |_| panic!("committed silence"))
                .unwrap(),
            Some(Preview {
                text: String::new(),
                through: Frames::ZERO
            }),
            "an empty preview, and the committed offset stays where it was"
        );
        u.release();
        let (text, tail) = w
            .finish(&[0.; 10], &u, |_| panic!("committed silence"))
            .unwrap();
        assert_eq!((text.as_str(), tail), ("", 10));
        assert!(
            w.pipeline.recognizer.calls.is_empty(),
            "no chunk decode and no whole-buffer retry: {:?}",
            w.pipeline.recognizer.calls
        );
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
            fn transcribe(&mut self, _: &[f32], _: TrailingSilence) -> Result<String> {
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
