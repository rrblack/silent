//! Turns decoded frames into upright RGBA images for any UI toolkit.

use std::sync::atomic::{AtomicU64, Ordering};

use libwebrtc::prelude::*;
use libwebrtc::video_frame::native::VideoFrameBufferExt;

/// How big a video is shown, in physical pixels (0 until the UI lays it
/// out). The UI sets it; the thread converting frames reads it.
#[derive(Debug, Default)]
pub struct ViewSize(AtomicU64);

impl ViewSize {
    pub fn set(&self, width: u32, height: u32) {
        self.0.store(
            u64::from(width) << 32 | u64::from(height),
            Ordering::Relaxed,
        );
    }

    pub fn get(&self) -> (u32, u32) {
        let v = self.0.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }
}

/// How a picture fills its view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Fills the view, cropping the edges (cameras).
    Cover,
    /// Shows all of it (screens).
    Contain,
}

/// The box to pass to [`to_rgba`] so `frame` comes out just big enough for
/// a `view` sized view: never larger than the picture or `max` (upright
/// width and height), and `max` while the view size is unknown.
pub fn view_box(frame: &BoxVideoFrame, view: (u32, u32), fit: Fit, max: (u32, u32)) -> (u32, u32) {
    let buf = frame.buffer.as_ref();
    let upright = if is_sideways(frame.rotation) {
        (buf.height(), buf.width())
    } else {
        (buf.width(), buf.height())
    };
    fit_view(upright, view, fit, max)
}

fn fit_view(picture: (u32, u32), view: (u32, u32), fit: Fit, max: (u32, u32)) -> (u32, u32) {
    if view.0 == 0 || view.1 == 0 {
        return max;
    }
    let (pw, ph) = (f64::from(picture.0.max(1)), f64::from(picture.1.max(1)));
    let (sx, sy) = (f64::from(view.0) / pw, f64::from(view.1) / ph);
    let scale = match fit {
        Fit::Cover => sx.max(sy),
        Fit::Contain => sx.min(sy),
    }
    .min(1.0);
    (
        ((pw * scale).ceil() as u32).min(max.0),
        ((ph * scale).ceil() as u32).min(max.1),
    )
}

fn is_sideways(rotation: VideoRotation) -> bool {
    matches!(
        rotation,
        VideoRotation::VideoRotation90 | VideoRotation::VideoRotation270
    )
}

/// An upright RGBA image (R, G, B, A bytes per pixel, rows tightly packed).
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Converts `frame` to RGBA, scaled down to fit `max_w x max_h` after
/// applying the frame's rotation (phone cameras send sideways frames plus a
/// rotation flag).
pub fn to_rgba(frame: &BoxVideoFrame, max_w: u32, max_h: u32) -> Option<RgbaImage> {
    let buf = frame.buffer.as_ref();
    let (w, h) = (buf.width(), buf.height());
    if w < 2 || h < 2 {
        return None;
    }
    let (fit_w, fit_h) = if is_sideways(frame.rotation) {
        (max_h, max_w)
    } else {
        (max_w, max_h)
    };
    let mut i420 = any_to_i420(buf)?;
    let (tw, th) = fit(w, h, fit_w, fit_h);
    if (tw, th) != (w, h) {
        i420 = i420.scale(tw as i32, th as i32);
    }
    let mut data = vec![0u8; (tw * th * 4) as usize];
    // libyuv's "ABGR" is R, G, B, A in memory order.
    i420.to_argb(
        VideoFormatType::ABGR,
        &mut data,
        tw * 4,
        tw as i32,
        th as i32,
    );
    Some(rotate(
        RgbaImage {
            width: tw,
            height: th,
            data,
        },
        frame.rotation,
    ))
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
        return (w & !1, h & !1);
    }
    let scale = f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64);
    (
        ((w as f64 * scale) as u32).max(2) & !1,
        ((h as f64 * scale) as u32).max(2) & !1,
    )
}

fn rotate(img: RgbaImage, rotation: VideoRotation) -> RgbaImage {
    let (w, h) = (img.width as usize, img.height as usize);
    let (nw, nh) = match rotation {
        VideoRotation::VideoRotation0 => return img,
        VideoRotation::VideoRotation180 => (w, h),
        VideoRotation::VideoRotation90 | VideoRotation::VideoRotation270 => (h, w),
    };
    let mut data = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        for x in 0..nw {
            // Destination (x, y) reads the source pixel that lands there.
            let (sx, sy) = match rotation {
                VideoRotation::VideoRotation90 => (y, h - 1 - x),
                VideoRotation::VideoRotation180 => (w - 1 - x, h - 1 - y),
                _ => (w - 1 - y, x),
            };
            let (src, dst) = ((sy * w + sx) * 4, (y * nw + x) * 4);
            data[dst..dst + 4].copy_from_slice(&img.data[src..src + 4]);
        }
    }
    RgbaImage {
        width: nw as u32,
        height: nh as u32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> RgbaImage {
        let data = (0..w * h).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        RgbaImage {
            width: w,
            height: h,
            data,
        }
    }

    fn at(img: &RgbaImage, x: u32, y: u32) -> u8 {
        img.data[((y * img.width + x) * 4) as usize]
    }

    const MAX: (u32, u32) = (1280, 720);

    #[test]
    fn small_views_get_small_pictures() {
        assert_eq!(
            fit_view((1280, 720), (640, 360), Fit::Cover, MAX),
            (640, 360)
        );
        // A wide view: cover keeps the width, contain the height.
        assert_eq!(
            fit_view((1280, 720), (640, 200), Fit::Cover, MAX),
            (640, 360)
        );
        assert_eq!(
            fit_view((1280, 720), (640, 200), Fit::Contain, MAX),
            (356, 200)
        );
    }

    #[test]
    fn never_upscaled_or_over_the_cap() {
        assert_eq!(
            fit_view((640, 360), (1920, 1080), Fit::Cover, MAX),
            (640, 360)
        );
        assert_eq!(fit_view((1920, 1080), (2560, 1440), Fit::Cover, MAX), MAX);
    }

    #[test]
    fn unknown_view_uses_the_cap() {
        assert_eq!(fit_view((1920, 1080), (0, 0), Fit::Cover, MAX), MAX);
    }

    #[test]
    fn view_size_round_trips() {
        let size = ViewSize::default();
        size.set(1234, 567);
        assert_eq!(size.get(), (1234, 567));
    }

    #[test]
    fn rotates_clockwise() {
        // 3x2 source:  0 1 2
        //              3 4 5
        let r = rotate(img(3, 2), VideoRotation::VideoRotation90);
        assert_eq!((r.width, r.height), (2, 3));
        // Rotated 90 clockwise: 3 0 / 4 1 / 5 2
        assert_eq!([at(&r, 0, 0), at(&r, 1, 0)], [3, 0]);
        assert_eq!([at(&r, 0, 2), at(&r, 1, 2)], [5, 2]);

        let r = rotate(img(3, 2), VideoRotation::VideoRotation270);
        // Rotated 270 clockwise: 2 5 / 1 4 / 0 3
        assert_eq!([at(&r, 0, 0), at(&r, 1, 0)], [2, 5]);
        assert_eq!([at(&r, 0, 2), at(&r, 1, 2)], [0, 3]);

        let r = rotate(img(3, 2), VideoRotation::VideoRotation180);
        assert_eq!([at(&r, 0, 0), at(&r, 2, 1)], [5, 0]);
    }
}
