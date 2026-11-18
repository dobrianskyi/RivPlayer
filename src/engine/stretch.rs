use anyhow::{Context, Result};

use super::{
    AudioChunk,
    decode::{DialogueLayout, Samples},
};

pub struct Stretch {
    dsp: wsola::TimeStretch,
    rate: f64,
    time: f64,
    end: f64,
    buffer: Vec<f32>,
    layout: DialogueLayout,
}

impl Stretch {
    pub fn new(chunk: &AudioChunk, rate: f64) -> Result<Self> {
        let mut dsp = wsola::TimeStretch::new(
            chunk
                .samples
                .rate
                .try_into()
                .context("Invalid audio sample rate")?,
            (chunk.samples.channels as u16)
                .try_into()
                .context("Invalid audio channel count")?,
        )?;
        dsp.set_tempo(rate as f32);
        let buffer = vec![0.0; dsp.min_output_buff_size() * 8];
        Ok(Self {
            dsp,
            rate,
            time: chunk.time,
            end: chunk.time,
            buffer,
            layout: chunk.samples.layout,
        })
    }

    pub fn contiguous(&self, chunk: &AudioChunk) -> bool {
        self.dsp.sample_rate().get() == chunk.samples.rate
            && usize::from(self.dsp.channels().get()) == chunk.samples.channels
            && self.layout == chunk.samples.layout
            && (self.end - chunk.time).abs() <= 0.004
    }

    pub fn push(&mut self, chunk: &AudioChunk) {
        self.end = chunk.end();
        self.dsp.push(&chunk.samples.data);
    }

    pub fn drain(&mut self) {
        self.dsp.set_draining();
    }

    pub fn pull(&mut self) -> Option<AudioChunk> {
        let count = self.dsp.pull(&mut self.buffer).ok()?;
        if count == 0 {
            return None;
        }
        let rate = self.dsp.sample_rate().get();
        let channels = usize::from(self.dsp.channels().get());
        let available = ((self.end - self.time) * f64::from(rate) / self.rate)
            .round()
            .max(0.0) as usize;
        let count = count.min(available.saturating_mul(channels));
        if count == 0 {
            return None;
        }
        let chunk = AudioChunk {
            time: self.time,
            samples: Samples {
                rate,
                channels,
                layout: self.layout,
                data: self.buffer[..count].to_vec(),
            },
        };
        self.time += (chunk.end() - chunk.time) * self.rate;
        Some(chunk)
    }
}
