//! Webcam capture through nokhwa: Media Foundation on Windows, AVFoundation
//! on macOS, V4L2 on Linux.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libwebrtc::native::yuv_helper;
use libwebrtc::prelude::*;
use libwebrtc::video_source::native::NativeVideoSource;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::Camera;

use crate::media::{fit, push_i420, Encoding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    pub index: u32,
    pub name: String,
}

pub fn list_cameras() -> Vec<CameraDevice> {
    match nokhwa::query(ApiBackend::Auto) {
        Ok(list) => list
            .into_iter()
            .filter_map(|info| match info.index() {
                CameraIndex::Index(i) => Some(CameraDevice {
                    index: *i,
                    name: info.human_name(),
                }),
                CameraIndex::String(_) => None,
            })
            .collect(),
        Err(e) => {
            tracing::warn!("could not list cameras: {e}");
            Vec::new()
        }
    }
}

/// On macOS the camera permission prompt must be triggered before the first
/// capture. `on_done` receives whether access was granted. Elsewhere this
/// reports `true` immediately.
pub fn request_permission(on_done: impl Fn(bool) + Send + Sync + 'static) {
    #[cfg(target_os = "macos")]
    {
        nokhwa::nokhwa_initialize(on_done);
    }
    #[cfg(not(target_os = "macos"))]
    {
        on_done(true);
    }
}

pub(crate) fn run(
    source: NativeVideoSource,
    encoding: Encoding,
    index: u32,
    stop: Arc<AtomicBool>,
    on_fatal: impl Fn(String) + Send + 'static,
) {
    let wanted = CameraFormat::new(
        Resolution::new(encoding.max_width, encoding.max_height),
        FrameFormat::MJPEG,
        encoding.fps,
    );
    let format = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(wanted));
    let mut camera = match Camera::new(CameraIndex::Index(index), format) {
        Ok(c) => c,
        Err(e) => return on_fatal(format!("could not open camera: {e}")),
    };
    if let Err(e) = camera.open_stream() {
        return on_fatal(format!("could not start camera: {e}"));
    }
    tracing::info!(format = %camera.camera_format(), "camera started");

    let mut rgba = Vec::new();
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        let frame = match camera.frame() {
            Ok(f) => f,
            Err(e) => {
                failures += 1;
                if failures > 30 {
                    on_fatal(format!("camera stopped: {e}"));
                    break;
                }
                continue;
            }
        };
        failures = 0;
        let res = frame.resolution();
        let (w, h) = (res.width_x & !1, res.height_y & !1);
        if w < 2 || h < 2 {
            continue;
        }
        rgba.resize((res.width_x * res.height_y * 4) as usize, 0);
        if frame
            .decode_image_to_buffer::<RgbAFormat>(&mut rgba)
            .is_err()
        {
            continue;
        }
        let mut full = I420Buffer::new(w, h);
        let (sy, su, sv) = full.strides();
        let (y, u, v) = full.data_mut();
        // RGBA bytes in memory are what libyuv calls ABGR.
        yuv_helper::abgr_to_i420(
            &rgba,
            res.width_x * 4,
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
        let buffer = if (tw, th) == (w, h) {
            full
        } else {
            full.scale(tw as i32, th as i32)
        };
        push_i420(&source, buffer);
    }
    let _ = camera.stop_stream();
}
