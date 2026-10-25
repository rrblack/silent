//! Short chimes when a friend joins or leaves. They are synthesized here and
//! played on the system's default output device, apart from the call audio.

use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

/// Which chime to play.
#[derive(Clone, Copy)]
pub enum Chime {
    Joined,
    Left,
}

impl Chime {
    /// Notes as (frequency in Hz, start in s): rising for a join, falling
    /// for a leave.
    fn notes(self) -> [(f32, f32); 2] {
        match self {
            Chime::Joined => [(659.3, 0.0), (880.0, 0.09)],
            Chime::Left => [(880.0, 0.0), (587.3, 0.09)],
        }
    }
}

const NOTE: f32 = 0.22;
const LENGTH: f32 = 0.09 + NOTE;
const VOLUME: f32 = 0.16;

/// Plays `chime` on its own thread; failures (no output device) are ignored.
pub fn play(chime: Chime) {
    let _ = std::thread::Builder::new()
        .name("chime".into())
        .spawn(move || {
            if let Err(e) = run(chime) {
                tracing::debug!("chime not played: {e:#}");
            }
        });
}

fn run(chime: Chime) -> anyhow::Result<()> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let samples = render(chime, config.sample_rate.0 as f32);
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, samples),
        SampleFormat::I16 => build::<i16>(&device, &config, samples),
        SampleFormat::U16 => build::<u16>(&device, &config, samples),
        SampleFormat::I32 => build::<i32>(&device, &config, samples),
        other => anyhow::bail!("unsupported sample format {other}"),
    }?;
    stream.play()?;
    std::thread::sleep(Duration::from_secs_f32(LENGTH + 0.1));
    Ok(())
}

/// The chime as mono samples: each note a sine with a quick attack and an
/// exponential fade.
fn render(chime: Chime, rate: f32) -> Vec<f32> {
    let len = (LENGTH * rate) as usize;
    let mut out = vec![0.0; len];
    for (freq, start) in chime.notes() {
        let first = (start * rate) as usize;
        for (i, sample) in out.iter_mut().enumerate().skip(first) {
            let t = (i - first) as f32 / rate;
            if t > NOTE {
                break;
            }
            let attack = (t / 0.005).min(1.0);
            let fade = (-t * 18.0).exp();
            *sample += VOLUME * attack * fade * (std::f32::consts::TAU * freq * t).sin();
        }
    }
    out
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    samples: Vec<f32>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let mut next = 0;
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| {
            for frame in out.chunks_mut(channels) {
                let value = T::from_sample(samples.get(next).copied().unwrap_or(0.0));
                frame.fill(value);
                next += 1;
            }
        },
        |e| tracing::debug!("chime stream: {e}"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chimes_are_short_and_never_clip() {
        for chime in [Chime::Joined, Chime::Left] {
            let samples = render(chime, 48_000.0);
            assert_eq!(samples.len(), (LENGTH * 48_000.0) as usize);
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.05 && peak < 1.0, "peak {peak}");
        }
    }
}
