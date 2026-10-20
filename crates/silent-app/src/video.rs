//! Turns WebRTC video tracks into Slint images.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use silent_engine::libwebrtc::prelude::*;
use silent_engine::libwebrtc::video_frame::native::VideoFrameBufferExt;
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
        let Some(pixels) = to_rgba(frame.buffer.as_ref(), max_w, max_h) else {
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

fn any_to_i420(buf: &dyn VideoBuffer) -> Option<I420Buffer> {
    if let Some(b) = buf.as_i420() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_nv12() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_i420a() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_i422() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_i444() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_i010() {
        return Some(b.to_i420());
    }
    if let Some(b) = buf.as_native() {
        return Some(b.to_i420());
    }
    None
}

fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let scale = f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64);
    (
        ((w as f64 * scale) as u32).max(2) & !1,
        ((h as f64 * scale) as u32).max(2) & !1,
    )
}

fn to_rgba(buf: &dyn VideoBuffer, max_w: u32, max_h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (w, h) = (buf.width(), buf.height());
    if w < 2 || h < 2 {
        return None;
    }
    let mut i420 = any_to_i420(buf)?;
    let (tw, th) = fit(w, h, max_w, max_h);
    if (tw, th) != (w, h) {
        i420 = i420.scale(tw as i32, th as i32);
    }
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
    // libyuv's "ABGR" is R, G, B, A in memory order.
    i420.to_argb(
        VideoFormatType::ABGR,
        pixels.make_mut_bytes(),
        tw * 4,
        tw as i32,
        th as i32,
    );
    Some(pixels)
}
