use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use cpal::{
    Device, FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::Receiver;

use super::{AudioChunk, voice::Voice};

pub const MAX_VOLUME: f32 = 2.0;

fn apply_volume(sample: f32, volume: f32) -> f32 {
    let amplified = sample * volume;
    if volume <= 1.0 {
        return amplified.clamp(-1.0, 1.0);
    }

    // Compress only the highest peaks and blend in the knee as boost increases.
    let headroom = 0.15 * (volume - 1.0);
    let knee = 1.0 - headroom;
    let peak = amplified.abs() - knee;
    if peak > 0.0 {
        (1.0 - headroom * (headroom / (headroom + peak))).copysign(amplified)
    } else {
        amplified
    }
}

#[derive(Clone, Copy)]
pub struct Clock {
    pub position: f64,
    pub start: Option<Instant>,
    pub rate: f64,
    pub volume: f32,
    pub voice: bool,
    pub epoch: u64,
    pub cut: Option<LoopCut>,
}

#[derive(Clone, Copy)]
pub(super) struct LoopCut {
    pub start: f64,
    pub end: f64,
    pub epoch: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            position: 0.0,
            start: None,
            rate: 1.0,
            volume: 1.0,
            voice: false,
            epoch: 0,
            cut: None,
        }
    }
}

impl Clock {
    pub fn position_at(self, now: Instant) -> f64 {
        self.start.map_or(self.position, |start| {
            self.position + now.saturating_duration_since(start).as_secs_f64() * self.rate
        })
    }

    fn audio_position(self, time: f64) -> (f64, u64) {
        match self.cut {
            Some(cut) if time >= cut.end => (cut.start + (time - cut.end), cut.epoch),
            _ => (time, self.epoch),
        }
    }
}

pub struct AudioOutput {
    stream: Stream,
    queue: Receiver<AudioChunk>,
    pub failed: Arc<AtomicBool>,
    staged: StagedAudio,
}

type StagedAudio = Arc<Mutex<Option<(u64, Receiver<AudioChunk>)>>>;

impl AudioOutput {
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    pub fn pause(&self) {
        let _ = self.stream.pause();
    }

    pub fn stage(&self, epoch: u64, queue: Receiver<AudioChunk>) {
        *self.staged.lock().unwrap() = Some((epoch, queue));
    }

    pub fn clear_staged(&self) {
        self.staged.lock().unwrap().take();
    }

    pub fn open(rx: Receiver<AudioChunk>, clock: Arc<Mutex<Clock>>) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("No default audio output device")?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let mut config = supported.config();
        if let cpal::SupportedBufferSize::Range { min, max } = *supported.buffer_size() {
            config.buffer_size = cpal::BufferSize::Fixed((config.sample_rate / 50).clamp(min, max));
        }
        let failed = Arc::new(AtomicBool::new(false));
        let staged = StagedAudio::default();
        let epoch = clock.lock().unwrap().epoch;
        let open = |config: &StreamConfig| -> Result<Stream> {
            let rx = rx.clone();
            let clock = clock.clone();
            let failed = failed.clone();
            let staged = staged.clone();
            let input = AudioInput { epoch, rx, staged };
            Ok(match format {
                SampleFormat::F32 => build::<f32>(&device, config, input, clock, failed),
                SampleFormat::F64 => build::<f64>(&device, config, input, clock, failed),
                SampleFormat::I16 => build::<i16>(&device, config, input, clock, failed),
                SampleFormat::I32 => build::<i32>(&device, config, input, clock, failed),
                SampleFormat::U16 => build::<u16>(&device, config, input, clock, failed),
                _ => bail!("Unsupported audio output format {format}"),
            }?)
        };
        let stream = open(&config).or_else(|err| {
            if config.buffer_size == cpal::BufferSize::Default {
                Err(err)
            } else {
                open(&supported.config())
            }
        })?;
        stream.play()?;
        Ok(Self {
            stream,
            failed,
            queue: rx,
            staged,
        })
    }
}

struct AudioInput {
    epoch: u64,
    rx: Receiver<AudioChunk>,
    staged: StagedAudio,
}

struct Feed {
    epoch: u64,
    queue: Receiver<AudioChunk>,
    pending: Option<AudioChunk>,
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &Device,
    config: &StreamConfig,
    input: AudioInput,
    clock: Arc<Mutex<Clock>>,
    failed: Arc<AtomicBool>,
) -> Result<Stream, cpal::BuildStreamError> {
    let channels = usize::from(config.channels);
    let sample_rate = f64::from(config.sample_rate);
    let mut feed = Feed {
        epoch: input.epoch,
        queue: input.rx,
        pending: None,
    };
    let mut previous: Option<Feed> = None;
    let mut voice = Voice::new(sample_rate as f32);
    device.build_output_stream(
        config,
        move |output: &mut [T], info| {
            output.fill(T::EQUILIBRIUM);
            let Ok(guard) = clock.try_lock() else {
                return;
            };
            let state = *guard;
            drop(guard);
            let Some(anchor) = state.start else {
                return;
            };
            let timestamp = info.timestamp();
            let latency = timestamp
                .playback
                .duration_since(&timestamp.callback)
                .unwrap_or_default()
                .min(Duration::from_millis(500));
            let render_at = Instant::now() + latency;
            let offset = if render_at >= anchor {
                render_at.duration_since(anchor).as_secs_f64()
            } else {
                -anchor.duration_since(render_at).as_secs_f64()
            };
            let mut time = state.position + offset * state.rate;
            let step = state.rate / sample_rate;
            for frame in output.chunks_exact_mut(channels) {
                if time >= state.position {
                    let (media_time, epoch) = state.audio_position(time);
                    if feed.epoch != epoch {
                        if let Some(other) = &mut previous
                            && other.epoch == epoch
                        {
                            std::mem::swap(&mut feed, other);
                        } else if let Ok(mut staged) = input.staged.try_lock()
                            && staged.as_ref().is_some_and(|(id, _)| *id == epoch)
                            && let Some((_, queue)) = staged.take()
                        {
                            previous = Some(std::mem::replace(
                                &mut feed,
                                Feed {
                                    epoch,
                                    queue,
                                    pending: None,
                                },
                            ));
                        } else {
                            time += step;
                            continue;
                        }
                    }
                    loop {
                        if feed.pending.is_none() {
                            feed.pending = feed.queue.try_recv().ok();
                        }
                        let Some(chunk) = feed.pending.as_ref() else {
                            break;
                        };
                        let samples = &chunk.samples;
                        let index =
                            (media_time - chunk.time) / state.rate * f64::from(samples.rate);
                        let count = samples.data.len() / samples.channels;
                        if index >= count as f64 {
                            feed.pending = None;
                            continue;
                        }
                        if index >= 0.0 {
                            let i = index as usize;
                            let frac = (index - i as f64) as f32;
                            let j = (i + 1).min(count - 1);
                            let sample = |ch: usize| {
                                let a = samples.data[i * samples.channels + ch];
                                let b = samples.data[j * samples.channels + ch];
                                let value = a + (b - a) * frac;
                                if value.is_finite() { value } else { 0.0 }
                            };
                            let (l, r) = samples
                                .layout
                                .front
                                .unwrap_or((0, usize::from(samples.channels > 1)));
                            let left = sample(l);
                            let right = sample(r);
                            let center = samples
                                .layout
                                .center
                                .filter(|&ch| samples.channels > 2 && ch < samples.channels)
                                .map(sample);
                            let extra = (0..samples.channels)
                                .filter(|&ch| {
                                    ch != l && ch != r && samples.layout.center != Some(ch)
                                })
                                .map(sample)
                                .sum::<f32>()
                                * (0.5 / samples.channels.saturating_sub(2).max(1) as f32);
                            let mix = center.unwrap_or_default() * std::f32::consts::FRAC_1_SQRT_2
                                + extra;
                            let wet = voice.process(
                                [left + mix, right + mix],
                                [left, right],
                                center,
                                state.voice,
                            );
                            for (ch, out) in frame.iter_mut().enumerate().take(2) {
                                let val = if channels == 1 {
                                    (wet[0] + wet[1]) * 0.5
                                } else {
                                    wet[ch]
                                };
                                *out = T::from_sample(
                                    voice
                                        .limit(val * state.volume, apply_volume(val, state.volume)),
                                );
                            }
                        }
                        break;
                    }
                }
                time += step;
            }
        },
        move |err| {
            if !failed.swap(true, Ordering::Relaxed) {
                eprintln!("Audio output: {err}");
            }
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_amplifies_quiet_audio_and_compresses_peaks() {
        assert_eq!(apply_volume(0.25, 0.5), 0.125);
        assert_eq!(apply_volume(0.25, 1.0), 0.25);
        assert_eq!(apply_volume(0.25, 1.5), 0.375);
        assert_eq!(apply_volume(-0.25, MAX_VOLUME), -0.5);
        let peak = apply_volume(0.9, MAX_VOLUME);
        assert!((0.9..1.0).contains(&peak));
        assert_eq!(apply_volume(-0.9, MAX_VOLUME), -peak);
    }

    #[test]
    fn prearmed_audio_cut_uses_the_first_sample_at_b_and_preserves_fractional_time() {
        let clock = Clock {
            rate: 1.25,
            epoch: 10,
            cut: Some(LoopCut {
                start: 0.45,
                end: 1.14,
                epoch: 11,
            }),
            ..Clock::default()
        };
        let sample = clock.rate / 48_000.0;
        assert_eq!(clock.audio_position(1.14 - sample), (1.14 - sample, 10));
        assert_eq!(clock.audio_position(1.14), (0.45, 11));
        let (wrapped, epoch) = clock.audio_position(1.14 + sample);
        assert_eq!(epoch, 11);
        assert!((wrapped - 0.45 - sample).abs() < 1e-12);
    }
}
