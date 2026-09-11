//! Turning VAD spans into decode windows: merge, padding, settlement.
//!
//! Pure policy shared by the live daemon and the offline tools; the Silero
//! detector that produces the raw spans is
//! [`shell::inference`](crate::shell::inference).
use crate::{config::Vad, core::decode::Segment};
use std::ops::Range;

/// Identical merge/padding/settlement policy to the reference implementation.
pub fn merge_spans(spans: &[Range<usize>], len: usize, config: &Vad, rate: u32) -> Vec<Segment> {
    if spans.is_empty() {
        return vec![Segment {
            window: 0..len,
            speech_end: len,
            settled: false,
        }];
    }
    struct Chunk {
        start: usize,
        end: usize,
        speech: usize,
        closed: bool,
        edge_lead: bool,
        edge_trail: bool,
    }
    let mut chunks = vec![];
    let mut pending: Option<Chunk> = None;
    let target = config.chunk_seconds * f64::from(rate);
    // A pause longer than both outer context windows is a safe decode
    // boundary even before the speech target is reached. This targets long
    // dead-air gaps that can make the transducer discard later speech while
    // preserving normal sentence pauses for contextual decoding.
    let split_silence = (rate as usize).max(
        (2.0 * config.edge_pad_seconds.max(config.pad_seconds) * f64::from(rate))
            .ceil()
            .min(usize::MAX as f64) as usize,
    );
    for span in spans {
        let prior_end = pending
            .as_ref()
            .map(|chunk| chunk.end)
            .or_else(|| chunks.last().map(|chunk: &Chunk| chunk.end));
        let edge_lead =
            prior_end.is_some_and(|end| span.start.saturating_sub(end) >= split_silence);
        if edge_lead {
            if pending.is_some() {
                let mut chunk = pending.take().expect("pending chunk");
                chunk.closed = true;
                chunks.push(chunk);
            }
            chunks.last_mut().expect("prior chunk").edge_trail = true;
        }
        let c = pending.get_or_insert(Chunk {
            start: span.start,
            end: span.start,
            speech: 0,
            closed: false,
            edge_lead,
            edge_trail: false,
        });
        c.end = span.end;
        c.speech += span.end - span.start;
        if c.speech as f64 >= target {
            c.closed = true;
            chunks.push(pending.take().expect("pending chunk"));
        }
    }
    if let Some(c) = pending {
        chunks.push(c);
    }
    let pad = (config.pad_seconds * f64::from(rate)) as usize;
    let edge = (config.edge_pad_seconds * f64::from(rate)) as usize;
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let wide = c.speech >= 3 * rate as usize;
            let lead = if wide && (i == 0 || c.edge_lead) {
                edge
            } else {
                pad
            };
            let trail = if wide && (i == last || c.edge_trail) {
                edge
            } else {
                pad
            };
            Segment {
                window: c.start.saturating_sub(lead)..len.min(c.end + trail),
                speech_end: c.end,
                settled: c.closed && (i < last || len.saturating_sub(c.end) >= rate as usize),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_and_edge_padding() {
        let c = Vad::default();
        let s = merge_spans(&[200..1200, 1500..1800], 1900, &c, 100);
        assert_eq!(
            s[0],
            Segment {
                window: 0..1250,
                speech_end: 1200,
                settled: true
            }
        );
        assert_eq!(
            s[1],
            Segment {
                window: 1450..1900,
                speech_end: 1800,
                settled: false
            }
        );
        assert!(!merge_spans(std::slice::from_ref(&(200..1200)), 1299, &c, 100)[0].settled);
        assert!(merge_spans(std::slice::from_ref(&(200..1200)), 1300, &c, 100)[0].settled);
        assert_eq!(
            merge_spans(std::slice::from_ref(&(500..550)), 1000, &c, 100)[0].window,
            450..600
        );
    }

    #[test]
    fn long_silence_splits_short_speech_without_padding_overlap() {
        let c = Vad::default();
        let split = merge_spans(&[100..300, 700..800], 900, &c, 100);
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].window, 50..350);
        assert_eq!(split[0].speech_end, 300);
        assert!(split[0].settled);
        assert_eq!(split[1].window, 650..850);
        assert!(!split[1].settled);

        let merged = merge_spans(&[100..300, 699..800], 900, &c, 100);
        assert_eq!(merged.len(), 1, "ordinary pauses still aggregate");
        assert_eq!(merged[0].window, 0..900);

        let mut wide_pad = c.clone();
        wide_pad.pad_seconds = 3.0;
        assert_eq!(
            merge_spans(&[100..300, 700..800], 900, &wide_pad, 100).len(),
            1,
            "custom inner padding raises the safe split threshold"
        );

        let wide = merge_spans(&[100..500, 900..1300], 1400, &c, 100);
        assert_eq!(wide[0].window, 0..700);
        assert_eq!(wide[1].window, 700..1400);

        let target_then_gap = merge_spans(&[100..1100, 1500..1600], 1700, &c, 100);
        assert_eq!(target_then_gap[0].window, 0..1300);
        assert_eq!(target_then_gap[1].window, 1450..1650);
        let target_then_short_gap = merge_spans(&[100..1100, 1499..1600], 1700, &c, 100);
        assert_eq!(target_then_short_gap[0].window, 0..1150);
        assert_eq!(target_then_short_gap[1].window, 1449..1650);
    }
}
