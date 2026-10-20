//! Turns WebRTC video tracks into Slint images.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use silent_engine::libwebrtc::prelude::*;
use silent_engine::libwebrtc::video_stream::native::NativeVideoStream;
use silent_engine::libwebrtc::video_track::RtcVideoTrack;
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Reads frames from `track`, scales them to fit `max_w x max_h` and runs
/// `deliver` with the pixels on the UI thread. Frames are dropped while the
/// UI is still busy with the previous one, so a slow UI never builds up
/// latency.
pub async fn pump<F>(track: RtcVideoTrack, max_w: u32, max_h: u32, deliver: F)
where
    F: Fn(SharedPixelBuffer<Rgba8Pixel>) + Send + Sync + 'static,
{
    let deliver = Arc::new(deliver);
    let mut stream = NativeVideoStream::new(track);
    let busy = Arc::new(AtomicBool::new(false));
    while let Some(frame) = stream.next().await {
        if busy.load(Ordering::Acquire) {
            continue;
        }
        let Some(pixels) = to_rgba(&frame, max_w, max_h) else {
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

fn to_rgba(frame: &BoxVideoFrame, max_w: u32, max_h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let img = silent_engine::render::to_rgba(frame, max_w, max_h)?;
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(img.width, img.height);
    pixels.make_mut_bytes().copy_from_slice(&img.data);
    Some(pixels)
}
