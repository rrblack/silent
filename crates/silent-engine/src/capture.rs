//! Screen and window capture through libwebrtc's DesktopCapturer: Windows
//! Graphics Capture / DXGI on Windows, ScreenCaptureKit on macOS, PipeWire
//! (via the xdg-desktop-portal) or X11 on Linux.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use libwebrtc::desktop_capturer::{
    CaptureError, DesktopCaptureSourceType, DesktopCapturer, DesktopCapturerOptions, DesktopFrame,
};
use libwebrtc::native::yuv_helper;
use libwebrtc::prelude::*;
use libwebrtc::video_source::native::NativeVideoSource;

use crate::media::{fit, paced, push_i420, Encoding, ScreenTarget};

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
        .map(|s| CaptureChoice { id: s.id(), title: s.title(), kind })
        .collect()
}

/// Whether this platform shows its own picker instead of a source list.
pub fn has_system_picker() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

pub(crate) fn run(
    source: NativeVideoSource,
    encoding: Encoding,
    target: ScreenTarget,
    stop: Arc<AtomicBool>,
    on_fatal: impl Fn(String) + Send + 'static,
) {
    let (source_type, wanted) = match target {
        ScreenTarget::Screen(id) => (DesktopCaptureSourceType::Screen, Some(id)),
        ScreenTarget::Window(id) => (DesktopCaptureSourceType::Window, Some(id)),
        ScreenTarget::SystemPicker => (system_picker_type(), None),
        ScreenTarget::TestPattern => unreachable!("handled by media::test_pattern"),
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
            let found = capturer.get_source_list().into_iter().find(|s| s.id() == id);
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
    capturer.start_capture(chosen, move |result: Result<DesktopFrame, CaptureError>| match result {
        Ok(frame) => {
            if let Some(buffer) = to_i420(&frame, &encoding) {
                push_i420(&source, buffer);
            }
        }
        Err(CaptureError::Temporary) => {}
        Err(CaptureError::Permanent) => {
            fatal_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });

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
    yuv_helper::argb_to_i420(frame.data(), frame.stride(), y, sy, u, su, v, sv, w as i32, h as i32);

    let (tw, th) = fit(w, h, encoding.max_width, encoding.max_height);
    if (tw, th) == (w, h) {
        Some(full)
    } else {
        Some(full.scale(tw as i32, th as i32))
    }
}
