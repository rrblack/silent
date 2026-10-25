//! Turns WebRTC video tracks into Slint images.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use silent_engine::libwebrtc::prelude::*;
use silent_engine::libwebrtc::video_stream::native::NativeVideoStream;
use silent_engine::libwebrtc::video_track::RtcVideoTrack;
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// How big a tile is on screen, in physical pixels (0 until it is laid out).
#[derive(Default)]
pub struct TileSize(AtomicU64);

impl TileSize {
    pub fn set(&self, width: u32, height: u32) {
        self.0.store(
            u64::from(width) << 32 | u64::from(height),
            Ordering::Relaxed,
        );
    }

    fn get(&self) -> (u32, u32) {
        let v = self.0.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
}

/// How a picture fills its tile.
#[derive(Clone, Copy)]
pub enum Fit {
    /// Fills the tile, cropping the edges (cameras).
    Cover,
    /// Shows all of it (screens).
    Contain,
}

/// Reads frames from `track`, scales them to the size `tile` is shown at
/// (never above `max_w x max_h`) and runs `deliver` with the pixels on the
/// UI thread. Frames are dropped while the UI is still busy with the previous
/// one, so a slow UI never builds up latency.
pub async fn pump<F>(
    track: RtcVideoTrack,
    max_w: u32,
    max_h: u32,
    tile: Arc<TileSize>,
    fit: Fit,
    deliver: F,
) where
    F: Fn(SharedPixelBuffer<Rgba8Pixel>) + Send + Sync + 'static,
{
    let deliver = Arc::new(deliver);
    let mut stream = NativeVideoStream::new(track);
    let busy = Arc::new(AtomicBool::new(false));
    while let Some(frame) = stream.next().await {
        if busy.load(Ordering::Acquire) {
            continue;
        }
        let buf = frame.buffer.as_ref();
        let sideways = matches!(
            frame.rotation,
            VideoRotation::VideoRotation90 | VideoRotation::VideoRotation270
        );
        let upright = if sideways {
            (buf.height(), buf.width())
        } else {
            (buf.width(), buf.height())
        };
        let (w, h) = target(upright, tile.get(), fit, (max_w, max_h));
        let Some(pixels) = to_rgba(&frame, w, h) else {
            continue;
        };
        busy.store(true, Ordering::Release);
        let (flag, deliver) = (busy.clone(), deliver.clone());
        let sent = slint::invoke_from_event_loop(move || {
            deliver(pixels);
            flag.store(false, Ordering::Release);
        });
        if sent.is_err() {
            break;
        }
    }
}

/// The box an upright `frame` sized picture is scaled to fit in: just big
/// enough for the tile, never larger than the picture or `max`.
fn target(frame: (u32, u32), tile: (u32, u32), fit: Fit, max: (u32, u32)) -> (u32, u32) {
    let (fw, fh) = (f64::from(frame.0.max(1)), f64::from(frame.1.max(1)));
    if tile.0 == 0 || tile.1 == 0 {
        return max;
    }
    let (sx, sy) = (f64::from(tile.0) / fw, f64::from(tile.1) / fh);
    let scale = match fit {
        Fit::Cover => sx.max(sy),
        Fit::Contain => sx.min(sy),
    }
    .min(1.0);
    (
        ((fw * scale).ceil() as u32).min(max.0),
        ((fh * scale).ceil() as u32).min(max.1),
    )
}

fn to_rgba(frame: &BoxVideoFrame, max_w: u32, max_h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = silent_engine::render::to_rgba(frame, max_w, max_h)?;
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(img.width, img.height);
    pixels.make_mut_bytes().copy_from_slice(&img.data);
    Some(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: (u32, u32) = (1280, 720);

    #[test]
    fn small_tiles_get_small_pictures() {
        assert_eq!(target((1280, 720), (640, 360), Fit::Cover, MAX), (640, 360));
        // A wide tile: cover keeps the width, contain the height.
        assert_eq!(target((1280, 720), (640, 200), Fit::Cover, MAX), (640, 360));
        assert_eq!(
            target((1280, 720), (640, 200), Fit::Contain, MAX),
            (356, 200)
        );
    }

    #[test]
    fn never_upscaled_or_over_the_cap() {
        assert_eq!(
            target((640, 360), (1920, 1080), Fit::Cover, MAX),
            (640, 360)
        );
        assert_eq!(target((1920, 1080), (2560, 1440), Fit::Cover, MAX), MAX);
    }

    #[test]
    fn unknown_tile_uses_the_cap() {
        assert_eq!(target((1920, 1080), (0, 0), Fit::Cover, MAX), MAX);
    }

    #[test]
    fn tile_size_round_trips() {
        let size = TileSize::default();
        size.set(1234, 567);
        assert_eq!(size.get(), (1234, 567));
    }
}
