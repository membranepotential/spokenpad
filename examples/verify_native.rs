//! Prints the segments, transcripts and a simulated progressive passage of
//! each WAV as JSON, for inspection by hand. Local only; never uploads audio
//! or transcripts.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use spokenpad::{
    config::Config,
    core::{
        decode::{Pipeline, Segmenter, Utterance, Worker},
        frames::Frames,
    },
    shell::inference::{SpeechSegmenter, Transcriber},
};
use std::{path::Path, time::Instant};

fn read(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let s = r.spec();
    ensure!(
        s.channels == 1
            && s.sample_rate == 16000
            && s.bits_per_sample == 16
            && s.sample_format == hound::SampleFormat::Int,
        "expected mono16/16k"
    );
    Ok(r.samples::<i16>()
        .map(|s| s.map(|s| (f32::from(s) / 32767.).clamp(-1., 1.)))
        .collect::<Result<_, _>>()?)
}
fn main() -> Result<()> {
    let config = Config::default();
    let mut recognizer = Transcriber::new(&config.asr, 16000)?;
    recognizer.warm_up()?;
    let mut pipeline = Pipeline {
        recognizer,
        segmenter: Some(SpeechSegmenter::new(&config.vad, 16000)?),
    };
    let mut cases = vec![];
    let mut passage = vec![];
    for arg in std::env::args().skip(1) {
        let samples = read(Path::new(&arg))?;
        let segments = pipeline
            .segmenter
            .as_mut()
            .context("segmenter")?
            .split(&samples)?;
        let t = Instant::now();
        let text = pipeline.decode(&samples, || false, drop)?;
        cases.push(json!({"file":Path::new(&arg).file_name().context("filename")?.to_string_lossy(),"text":text,"elapsed":t.elapsed().as_secs_f64(),"segments":segments.iter().map(|s|json!([s.window.start,s.window.end,s.speech_end,s.settled])).collect::<Vec<_>>() }));
        if samples.len() > 32000 {
            passage.extend(samples);
            passage.extend(vec![0.; 16000]);
        }
    }
    let mut worker = Worker::new(pipeline);
    let u = Utterance::new(1);
    let mut through = Frames::ZERO;
    let mut commits = vec![];
    let mut texts = vec![];
    let step = 17600;
    for end in (step..passage.len()).step_by(step) {
        worker.tick(&passage[through.get()..end], through, &u, |c| {
            through = c.through;
            commits.push(json!([end, c.through.get(), c.text]));
            if !c.text.trim().is_empty() {
                texts.push(c.text);
            }
        })?;
    }
    u.release();
    let t = Instant::now();
    let (_, tail) = worker.finish(&passage, &u, |c| {
        if !c.text.trim().is_empty() {
            texts.push(c.text);
        }
    })?;
    let release = t.elapsed().as_secs_f64();
    println!(
        "{}",
        json!({"cases":cases,"progressive":{"text":texts.join(" "),"commits":commits,"tail_frames":tail,"release_seconds":release,"frames":passage.len()}})
    );
    Ok(())
}
