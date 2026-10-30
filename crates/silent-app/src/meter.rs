//! The microphone level meter on the settings page. It listens on its own
//! stream, apart from the call, on the microphone the sound system knows by
//! the chosen name, else the default one. Nothing is recorded: each buffer
//! only updates the loudest recent sample.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

/// A running meter. Dropping it closes the microphone.
pub struct Meter {
    peak: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl Meter {
    /// Starts listening on `microphone`; failures (no input device) leave the
    /// meter at zero.
    pub fn start(microphone: Option<String>) -> Self {
        let peak = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (peak.clone(), stop.clone());
        let _ = std::thread::Builder::new()
            .name("mic meter".into())
            .spawn(move || {
                if let Err(e) = run(microphone, p, s) {
                    tracing::debug!("mic meter not started: {e:#}");
                }
            });
        Self { peak, stop }
    }

    /// The loudest sample since the last call, from 0 to 1 on a decibel
    /// scale (-60 dBFS and below is 0).
    pub fn take_level(&self) -> f32 {
        let peak = f32::from_bits(self.peak.swap(0, Ordering::Relaxed));
        level(peak)
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn level(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn run(
    microphone: Option<String>,
    peak: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let named = microphone.and_then(|name| {
        host.input_devices()
            .ok()?
            .find(|d| d.name().is_ok_and(|n| n == name))
    });
    let device = named
        .or_else(|| host.default_input_device())
        .context("no input device")?;
    let supported = device.default_input_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, peak),
        SampleFormat::I16 => build::<i16>(&device, &config, peak),
        SampleFormat::U16 => build::<u16>(&device, &config, peak),
        SampleFormat::I32 => build::<i32>(&device, &config, peak),
        other => anyhow::bail!("unsupported sample format {other}"),
    }?;
    stream.play()?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    peak: Arc<AtomicU32>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            let loudest = data
                .iter()
                .fold(0.0f32, |m, s| m.max(s.to_sample::<f32>().abs()));
            let _ = peak.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                (loudest > f32::from_bits(old)).then(|| loudest.to_bits())
            });
        },
        |e| tracing::debug!("mic meter stream: {e}"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_is_a_decibel_scale() {
        assert_eq!(level(0.0), 0.0);
        assert_eq!(level(0.0005), 0.0);
        assert!((level(1.0) - 1.0).abs() < 1e-6);
        assert!((level(0.1) - 2.0 / 3.0).abs() < 1e-3);
    }
}
