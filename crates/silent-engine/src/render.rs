//! Turns decoded frames into upright RGBA images for any UI toolkit.

use libwebrtc::prelude::*;
use libwebrtc::video_frame::native::VideoFrameBufferExt;

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
    let sideways = matches!(
        frame.rotation,
        VideoRotation::VideoRotation90 | VideoRotation::VideoRotation270
    );
    let (fit_w, fit_h) = if sideways {
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
