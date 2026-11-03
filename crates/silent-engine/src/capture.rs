//! Screen and window capture through libwebrtc's DesktopCapturer: Windows
//! Graphics Capture / DXGI on Windows, ScreenCaptureKit on macOS, PipeWire
//! (via the xdg-desktop-portal) or X11 on Linux.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libwebrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use libwebrtc::native::yuv_helper;
use libwebrtc::prelude::*;

use crate::media::{fit, paced, push_i420, Encoding, ScreenTarget, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Screen,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureChoice {
    pub id: u64,
    pub title: String,
    pub kind: SourceKind,
}

/// Lists screens or windows that can be shared. Empty on platforms where only
/// the system picker is available (Wayland) or when capture is not permitted.
pub fn list_sources(kind: SourceKind) -> Vec<CaptureChoice> {
    let source_type = match kind {
        SourceKind::Screen => DesktopCaptureSourceType::Screen,
        SourceKind::Window => DesktopCaptureSourceType::Window,
    };
    let Some(capturer) = DesktopCapturer::new(DesktopCapturerOptions::new(source_type)) else {
        return Vec::new();
    };
    capturer
        .get_source_list()
        .into_iter()
        .map(|s| CaptureChoice {
            id: s.id(),
            title: s.title(),
            kind,
        })
        .collect()
}

/// A small still of a screen or window for the share picker.
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, 4 bytes a pixel.
    pub rgba: Vec<u8>,
}

/// How long [`thumbnail`] waits for a first frame; window capture on
/// Windows delivers it asynchronously.
const THUMBNAIL_WAIT: Duration = Duration::from_millis(600);

/// Grabs one frame of `choice` and shrinks it to fit `max_width` x
/// `max_height`. `None` when the source gives no frame in time (a minimized
/// window, say) or can't be captured.
pub fn thumbnail(choice: &CaptureChoice, max_width: u32, max_height: u32) -> Option<Thumbnail> {
    let source_type = match choice.kind {
        SourceKind::Screen => DesktopCaptureSourceType::Screen,
        SourceKind::Window => DesktopCaptureSourceType::Window,
    };
    let mut capturer = DesktopCapturer::new(DesktopCapturerOptions::new(source_type))?;
    let source = capturer
        .get_source_list()
        .into_iter()
        .find(|s| s.id() == choice.id)?;
    let grabbed = Arc::new(Mutex::new(None));
    let slot = grabbed.clone();
    capturer.start_capture(
        Some(source),
        move |result: Result<DesktopFrame, CaptureError>| {
            if let Ok(frame) = result {
                let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
                if slot.is_none() {
                    *slot = shrink(&frame, max_width, max_height);
                }
            }
        },
    );
    let deadline = Instant::now() + THUMBNAIL_WAIT;
    loop {
        capturer.capture_frame();
        if let Some(t) = grabbed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            return Some(t);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Box-filters a BGRA frame down to fit the bounds, as RGBA.
fn shrink(frame: &DesktopFrame, max_width: u32, max_height: u32) -> Option<Thumbnail> {
    let (w, h) = (frame.width().max(0) as u32, frame.height().max(0) as u32);
    if w == 0 || h == 0 {
        return None;
    }
    let (tw, th) = fit(w, h, max_width, max_height);
    let (tw, th) = (tw.max(1), th.max(1));
    Some(Thumbnail {
        width: tw,
        height: th,
        rgba: box_filter(frame.data(), frame.stride() as usize, w, h, tw, th),
    })
}

/// Averages up to 4x4 samples of each target pixel's area of a BGRA image
/// into RGBA.
fn box_filter(bgra: &[u8], stride: usize, w: u32, h: u32, tw: u32, th: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((tw * th * 4) as usize);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let mut sum = [0u32; 3];
            let mut n = 0;
            for sy in 0..4 {
                let y = (y0 + (y1 - y0) * sy / 4).min(h - 1) as usize;
                for sx in 0..4 {
                    let x = (x0 + (x1 - x0) * sx / 4).min(w - 1) as usize;
                    let at = y * stride + x * 4;
                    if let Some(px) = bgra.get(at..at + 3) {
                        sum[0] += u32::from(px[2]);
                        sum[1] += u32::from(px[1]);
                        sum[2] += u32::from(px[0]);
                        n += 1;
                    }
                }
            }
            let n = n.max(1);
            out.extend_from_slice(&[
                (sum[0] / n) as u8,
                (sum[1] / n) as u8,
                (sum[2] / n) as u8,
                255,
            ]);
        }
    }
    out
}

/// Whether this platform shows its own picker instead of a source list.
pub fn has_system_picker() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

pub(crate) fn run(
    source: Source,
    encoding: Encoding,
    target: ScreenTarget,
    stop: Arc<AtomicBool>,
    on_fatal: impl Fn(String) + Send + 'static,
) {
    let (source_type, wanted) = match target {
        ScreenTarget::Screen(id) => (DesktopCaptureSourceType::Screen, Some(id)),
        ScreenTarget::Window(id) => (DesktopCaptureSourceType::Window, Some(id)),
        ScreenTarget::SystemPicker => (system_picker_type(), None),
        ScreenTarget::TestPattern | ScreenTarget::External => {
            unreachable!("handled in Room without a capturer")
        }
    };

    let mut options = DesktopCapturerOptions::new(source_type);
    options.set_include_cursor(true);
    #[cfg(target_os = "macos")]
    if wanted.is_none() {
        options.set_sck_system_picker(true);
    }

    let Some(mut capturer) = DesktopCapturer::new(options) else {
        on_fatal("screen capture is not available on this system".into());
        return;
    };

    let chosen = match wanted {
        Some(id) => {
            let found = capturer
                .get_source_list()
                .into_iter()
                .find(|s| s.id() == id);
            if found.is_none() {
                on_fatal("that screen or window is no longer available".into());
                return;
            }
            found
        }
        None if cfg!(target_os = "windows") => capturer.get_source_list().into_iter().next(),
        None => None,
    };

    let fatal = Arc::new(AtomicBool::new(false));
    let fatal_flag = fatal.clone();
    let mut reported = false;
    capturer.start_capture(
        chosen,
        move |result: Result<DesktopFrame, CaptureError>| match result {
            Ok(frame) => {
                if let Some(buffer) = to_i420(&frame, &encoding) {
                    push_i420(&source, buffer);
                }
            }
            Err(CaptureError::Temporary) => {}
            Err(CaptureError::Permanent) => {
                fatal_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
    );

    paced(encoding.fps, &stop, || {
        if fatal.load(std::sync::atomic::Ordering::Relaxed) {
            if !reported {
                reported = true;
                on_fatal("screen sharing stopped (permission denied or source closed)".into());
            }
            return;
        }
        capturer.capture_frame();
    });
}

fn system_picker_type() -> DesktopCaptureSourceType {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        DesktopCaptureSourceType::Generic
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        DesktopCaptureSourceType::Screen
    }
}

/// DesktopFrame pixels are 32-bit BGRA in memory, which libyuv calls ARGB.
fn to_i420(frame: &DesktopFrame, encoding: &Encoding) -> Option<I420Buffer> {
    let (w, h) = (frame.width(), frame.height());
    if w < 2 || h < 2 {
        return None;
    }
    let (w, h) = (w as u32 & !1, h as u32 & !1);
    let mut full = I420Buffer::new(w, h);
    let (sy, su, sv) = full.strides();
    let (y, u, v) = full.data_mut();
    yuv_helper::argb_to_i420(
        frame.data(),
        frame.stride(),
        y,
        sy,
        u,
        su,
        v,
        sv,
        w as i32,
        h as i32,
    );

    let (tw, th) = fit(w, h, encoding.max_width, encoding.max_height);
    if (tw, th) == (w, h) {
        Some(full)
    } else {
        Some(full.scale(tw as i32, th as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_filter_averages_and_swaps_to_rgba() {
        // 4x2 BGRA: left half pure blue, right half pure red, with padding.
        let stride = 4 * 4 + 8;
        let mut bgra = vec![0u8; stride * 2];
        for y in 0..2 {
            for x in 0..4 {
                let px = &mut bgra[y * stride + x * 4..][..4];
                px.copy_from_slice(if x < 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let out = box_filter(&bgra, stride, 4, 2, 2, 1);
        assert_eq!(out, vec![0, 0, 255, 255, 255, 0, 0, 255]);
    }
}
