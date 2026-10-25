//! Short chimes when a friend joins or leaves. They are synthesized here and
//! played apart from the call audio: on the speaker chosen in the app when
//! the sound system knows it by the same name, else the default one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Chimes closer together than this play once (three friends already in a
/// room you join, say).
const MIN_GAP: Duration = Duration::from_millis(300);

/// Plays `chime` on its own thread, on the output device called `speaker`
/// if there is one; failures (no output device) are ignored.
pub fn play(chime: Chime, speaker: Option<String>) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < MIN_GAP) {
            return;
        }
        *last = Some(now);
    }
    let _ = std::thread::Builder::new()
        .name("chime".into())
        .spawn(move || {
            if let Err(e) = run(chime, speaker) {
                tracing::debug!("chime not played: {e:#}");
            }
        });
}

fn run(chime: Chime, speaker: Option<String>) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let named = speaker.and_then(|name| {
        host.output_devices()
            .ok()?
            .find(|d| d.name().is_ok_and(|n| n == name))
    });
    let device = named
        .or_else(|| host.default_output_device())
        .context("no output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let samples = render(chime, config.sample_rate.0 as f32);
    let done = Arc::new(AtomicBool::new(false));
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, samples, done.clone()),
        SampleFormat::I16 => build::<i16>(&device, &config, samples, done.clone()),
        SampleFormat::U16 => build::<u16>(&device, &config, samples, done.clone()),
        SampleFormat::I32 => build::<i32>(&device, &config, samples, done.clone()),
        other => anyhow::bail!("unsupported sample format {other}"),
    }?;
    stream.play()?;
    // Until the device has taken the last sample (a waking Bluetooth headset
    // can take a moment to start), then a little more for its buffer.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(150));
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
    done: Arc<AtomicBool>,
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
            if next >= samples.len() {
                done.store(true, Ordering::Relaxed);
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
