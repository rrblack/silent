//! Webcam capture through nokhwa: Media Foundation on Windows, AVFoundation
//! on macOS, V4L2 on Linux.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libwebrtc::native::yuv_helper;
use libwebrtc::prelude::*;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::Camera;

use crate::capture::{box_filter, Thumbnail};
use crate::media::{fit, push_i420, Encoding, Source};

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

/// Shows camera `index` without a call, for the settings page: until `stop`
/// is set, hands about `fps` frames a second to `on_frame`, shrunk to fit
/// `max_width` x `max_height`. Blocks; run it on its own thread.
pub fn preview(
    index: u32,
    max_width: u32,
    max_height: u32,
    fps: u32,
    stop: Arc<AtomicBool>,
    mut on_frame: impl FnMut(Thumbnail),
    on_fatal: impl Fn(String),
) {
    let wanted = CameraFormat::new(Resolution::new(640, 360), FrameFormat::MJPEG, 30);
    let format = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(wanted));
    let mut camera = match Camera::new(CameraIndex::Index(index), format) {
        Ok(c) => c,
        Err(e) => return on_fatal(format!("could not open camera: {e}")),
    };
    if let Err(e) = camera.open_stream() {
        return on_fatal(format!("could not start camera: {e}"));
    }
    let every = Duration::from_secs(1) / fps.max(1);
    let mut last: Option<Instant> = None;
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
        if last.is_some_and(|at| at.elapsed() < every) {
            continue;
        }
        let res = frame.resolution();
        let (w, h) = (res.width_x, res.height_y);
        if w < 2 || h < 2 {
            continue;
        }
        rgba.resize((w * h * 4) as usize, 0);
        if frame
            .decode_image_to_buffer::<RgbAFormat>(&mut rgba)
            .is_err()
        {
            continue;
        }
        last = Some(Instant::now());
        let (tw, th) = fit(w, h, max_width, max_height);
        on_frame(Thumbnail {
            width: tw,
            height: th,
            rgba: box_filter(&rgba, (w * 4) as usize, w, h, tw, th, false),
        });
    }
    let _ = camera.stop_stream();
}

pub(crate) fn run(
    source: Source,
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
