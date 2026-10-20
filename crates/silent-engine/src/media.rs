//! Local media: microphone, camera and screen tracks, plus the encoding
//! settings applied to each. One set of tracks per room; every peer
//! connection sends the same tracks (libwebrtc encodes once per connection,
//! which keeps per-friend bitrate adaptation independent).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_track::RtcAudioTrack;
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection_factory::PeerConnectionFactory;
use libwebrtc::prelude::*;
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_track::RtcVideoTrack;

pub const STREAM_AUDIO: &str = "silent-audio";
pub const STREAM_CAMERA: &str = "silent-camera";
pub const STREAM_SCREEN: &str = "silent-screen";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoKind {
    Camera,
    Screen,
}

impl VideoKind {
    pub fn stream_id(self) -> &'static str {
        match self {
            VideoKind::Camera => STREAM_CAMERA,
            VideoKind::Screen => STREAM_SCREEN,
        }
    }

    pub fn from_stream_id(id: &str) -> Option<Self> {
        match id {
            STREAM_CAMERA => Some(VideoKind::Camera),
            STREAM_SCREEN => Some(VideoKind::Screen),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioInput {
    /// Microphone and speakers through the OS audio stack, with WebRTC's echo
    /// cancellation, noise suppression and gain control.
    Platform,
    /// No audio device. Sends silence; used by tests and headless runs.
    Silent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CameraProfile {
    /// 1280x720 at 30 fps, about 1.5 Mbps per friend.
    #[default]
    Standard,
    /// 1920x1080 at 60 fps, about 5 Mbps per friend.
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenProfile {
    /// Games and video: 1920x1080 at 60 fps, keeps frame rate under pressure.
    #[default]
    Motion,
    /// Code and documents: up to 2560x1440 at 30 fps, keeps sharpness.
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Encoding {
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
    pub max_bitrate_bps: u64,
    pub degradation: DegradationPreference,
}

impl CameraProfile {
    pub fn encoding(self) -> Encoding {
        match self {
            CameraProfile::Standard => Encoding {
                max_width: 1280,
                max_height: 720,
                fps: 30,
                max_bitrate_bps: 1_500_000,
                degradation: DegradationPreference::Balanced,
            },
            CameraProfile::High => Encoding {
                max_width: 1920,
                max_height: 1080,
                fps: 60,
                max_bitrate_bps: 5_000_000,
                degradation: DegradationPreference::Balanced,
            },
        }
    }
}

impl ScreenProfile {
    pub fn encoding(self) -> Encoding {
        match self {
            ScreenProfile::Motion => Encoding {
                max_width: 1920,
                max_height: 1080,
                fps: 60,
                max_bitrate_bps: 8_000_000,
                degradation: DegradationPreference::MaintainFramerate,
            },
            ScreenProfile::Detail => Encoding {
                max_width: 2560,
                max_height: 1440,
                fps: 30,
                max_bitrate_bps: 6_000_000,
                degradation: DegradationPreference::MaintainResolution,
            },
        }
    }
}

/// What to put on the screen-share track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenTarget {
    /// Let the OS show its own picker (macOS ScreenCaptureKit, Wayland portal).
    SystemPicker,
    Screen(u64),
    Window(u64),
    /// A moving test pattern at the profile's full resolution and frame rate.
    TestPattern,
    /// Frames are pushed by the app through [`crate::Room::video_input`]
    /// (Android MediaProjection, iOS ReplayKit).
    External,
}

/// What to put on the camera track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraTarget {
    /// A device index from [`crate::camera::list_cameras`].
    Device(u32),
    /// A moving test pattern at the profile's full resolution and frame rate.
    TestPattern,
    /// Frames are pushed by the app through [`crate::Room::video_input`]
    /// (Android CameraX, iOS AVFoundation).
    External,
}

/// A thread pushing frames into a video source. Stops when dropped.
pub(crate) struct Producer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Producer {
    pub(crate) fn spawn(name: &str, body: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || body(flag))
            .expect("spawn producer thread");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub(crate) struct LocalVideo {
    pub source: NativeVideoSource,
    pub track: RtcVideoTrack,
    pub encoding: Encoding,
    pub producer: Option<Producer>,
}

impl LocalVideo {
    fn new(factory: &PeerConnectionFactory, kind: VideoKind, encoding: Encoding) -> Self {
        let resolution = VideoResolution {
            width: encoding.max_width,
            height: encoding.max_height,
        };
        let source = NativeVideoSource::new(resolution, kind == VideoKind::Screen);
        let track = factory.create_video_track(kind.stream_id(), source.clone());
        Self {
            source,
            track,
            encoding,
            producer: None,
        }
    }
}

pub(crate) struct LocalMedia {
    factory: PeerConnectionFactory,
    pub audio: RtcAudioTrack,
    _silence: Option<NativeAudioSource>,
    platform_audio: bool,
    pub camera: LocalVideo,
    pub screen: LocalVideo,
}

impl LocalMedia {
    pub(crate) fn new(
        factory: &PeerConnectionFactory,
        audio: AudioInput,
        camera: CameraProfile,
        screen: ScreenProfile,
    ) -> Self {
        let platform_audio = audio == AudioInput::Platform && factory.acquire_platform_adm();
        if audio == AudioInput::Platform && !platform_audio {
            tracing::warn!("no usable audio device; sending silence");
        }
        let (audio, silence) = if platform_audio {
            (factory.create_device_audio_track(STREAM_AUDIO), None)
        } else {
            let source = NativeAudioSource::new(AudioSourceOptions::default(), 48_000, 1, 100);
            (
                factory.create_audio_track(STREAM_AUDIO, source.clone()),
                Some(source),
            )
        };
        Self {
            factory: factory.clone(),
            audio,
            _silence: silence,
            platform_audio,
            camera: LocalVideo::new(factory, VideoKind::Camera, camera.encoding()),
            screen: LocalVideo::new(factory, VideoKind::Screen, screen.encoding()),
        }
    }

    pub(crate) fn has_platform_audio(&self) -> bool {
        self.platform_audio
    }

    pub(crate) fn video(&mut self, kind: VideoKind) -> &mut LocalVideo {
        match kind {
            VideoKind::Camera => &mut self.camera,
            VideoKind::Screen => &mut self.screen,
        }
    }
}

impl Drop for LocalMedia {
    fn drop(&mut self) {
        self.camera.producer = None;
        self.screen.producer = None;
        if self.platform_audio {
            self.factory.release_platform_adm();
        }
    }
}

/// Applies the profile's limits to a video sender and pauses or resumes it.
pub(crate) fn apply_encoding(sender: &RtpSender, encoding: &Encoding, active: bool) {
    let mut params = sender.parameters();
    for enc in params.encodings.iter_mut() {
        enc.active = active;
        enc.max_bitrate = Some(encoding.max_bitrate_bps);
        enc.max_framerate = Some(encoding.fps as f64);
        enc.priority = Priority::High;
    }
    params.set_degradation_preference(encoding.degradation);
    if let Err(e) = sender.set_parameters(params) {
        tracing::warn!("could not apply encoding limits: {e}");
    }
}

/// Scales `(w, h)` down to fit `max_w x max_h`, keeping aspect ratio and even
/// dimensions (required by I420).
pub(crate) fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w & !1, h & !1);
    }
    let scale = f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64);
    let fw = ((w as f64 * scale).round() as u32).max(2) & !1;
    let fh = ((h as f64 * scale).round() as u32).max(2) & !1;
    (fw, fh)
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Runs `tick` at `fps` until `stop` is set, without drifting.
pub(crate) fn paced(fps: u32, stop: &AtomicBool, mut tick: impl FnMut()) {
    let period = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        tick();
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
}

pub(crate) fn push_i420(source: &NativeVideoSource, buffer: I420Buffer) {
    push_rotated(source, buffer, VideoRotation::VideoRotation0);
}

fn push_rotated(source: &NativeVideoSource, buffer: I420Buffer, rotation: VideoRotation) {
    let frame = VideoFrame {
        rotation,
        timestamp_us: now_us(),
        frame_metadata: None,
        buffer,
    };
    source.capture_frame(&frame);
}

/// Where an app pushes its own camera or screen frames (mobile platforms,
/// where capture lives in Kotlin or Swift). Cheap to clone; frames are
/// scaled down to the profile's limits before encoding.
#[derive(Clone)]
pub struct VideoInput {
    pub(crate) source: NativeVideoSource,
    pub(crate) encoding: Encoding,
}

impl VideoInput {
    /// YUV 4:2:0 with arbitrary strides, as Android's `YUV_420_888` and most
    /// camera APIs deliver it. `uv_pixel_stride` is 1 for planar (I420) and
    /// 2 for semi-planar (NV12/NV21) chroma. `rotation_degrees` is how far
    /// the image must be rotated clockwise to be upright.
    #[allow(clippy::too_many_arguments)]
    pub fn push_yuv420(
        &self,
        width: u32,
        height: u32,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        y_stride: u32,
        uv_stride: u32,
        uv_pixel_stride: u32,
        rotation_degrees: u32,
    ) {
        let (w, h) = (width & !1, height & !1);
        if w < 2 || h < 2 {
            return;
        }
        let mut buf = I420Buffer::new(w, h);
        let (sy, su, sv) = buf.strides();
        let (dy, du, dv) = buf.data_mut();
        for row in 0..h as usize {
            let src = row * y_stride as usize;
            let dst = row * sy as usize;
            if src + w as usize > y.len() {
                return;
            }
            dy[dst..dst + w as usize].copy_from_slice(&y[src..src + w as usize]);
        }
        let (cw, ch) = ((w / 2) as usize, (h / 2) as usize);
        let ps = uv_pixel_stride.max(1) as usize;
        for row in 0..ch {
            let src = row * uv_stride as usize;
            if src + (cw - 1) * ps >= u.len().min(v.len()) {
                return;
            }
            let (du_row, dv_row) = (row * su as usize, row * sv as usize);
            if ps == 1 {
                du[du_row..du_row + cw].copy_from_slice(&u[src..src + cw]);
                dv[dv_row..dv_row + cw].copy_from_slice(&v[src..src + cw]);
            } else {
                for col in 0..cw {
                    du[du_row + col] = u[src + col * ps];
                    dv[dv_row + col] = v[src + col * ps];
                }
            }
        }
        self.push(buf, rotation_degrees);
    }

    /// 32-bit RGBA pixels (R, G, B, A in memory), as Android's `ImageReader`
    /// with `PixelFormat.RGBA_8888` delivers screen captures.
    pub fn push_rgba(&self, width: u32, height: u32, stride: u32, data: &[u8]) {
        let (w, h) = (width & !1, height & !1);
        if w < 2 || h < 2 || data.len() < (stride * (h - 1) + w * 4) as usize {
            return;
        }
        let mut buf = I420Buffer::new(w, h);
        let (sy, su, sv) = buf.strides();
        let (dy, du, dv) = buf.data_mut();
        // RGBA bytes in memory are what libyuv calls ABGR.
        libwebrtc::native::yuv_helper::abgr_to_i420(
            data, stride, dy, sy, du, su, dv, sv, w as i32, h as i32,
        );
        self.push(buf, 0);
    }

    fn push(&self, buf: I420Buffer, rotation_degrees: u32) {
        let rotation = match rotation_degrees % 360 {
            90 => VideoRotation::VideoRotation90,
            180 => VideoRotation::VideoRotation180,
            270 => VideoRotation::VideoRotation270,
            _ => VideoRotation::VideoRotation0,
        };
        let (w, h) = (buf.width(), buf.height());
        // Limits apply to the upright image.
        let sideways = matches!(
            rotation,
            VideoRotation::VideoRotation90 | VideoRotation::VideoRotation270
        );
        let (max_w, max_h) = if sideways {
            (self.encoding.max_height, self.encoding.max_width)
        } else {
            (self.encoding.max_width, self.encoding.max_height)
        };
        let (tw, th) = fit(w, h, max_w, max_h);
        let mut buf = buf;
        if (tw, th) != (w, h) {
            buf = buf.scale(tw as i32, th as i32);
        }
        push_rotated(&self.source, buf, rotation);
    }
}

/// Horizontal bands that scroll plus a sweeping bar, so encoders see real
/// motion. Built from row fills to stay cheap even in debug builds.
pub(crate) fn test_pattern(source: NativeVideoSource, encoding: Encoding, stop: Arc<AtomicBool>) {
    let (w, h) = (encoding.max_width, encoding.max_height);
    let bar_w = (w / 40).max(2);
    let mut n: u32 = 0;
    paced(encoding.fps, &stop, || {
        let mut buf = I420Buffer::new(w, h);
        let (sy, su, sv) = buf.strides();
        let (y, u, v) = buf.data_mut();
        let bar = ((n * 8) % (w - bar_w)) as usize;
        for row in 0..h {
            let start = (row * sy) as usize;
            let line = &mut y[start..start + w as usize];
            line.fill((((row / 8 + n) % 200) + 16) as u8);
            line[bar..bar + bar_w as usize].fill(235);
        }
        for row in 0..h / 2 {
            let start = (row * su) as usize;
            u[start..start + (w / 2) as usize].fill(((row / 4 + n) % 255) as u8);
            let start = (row * sv) as usize;
            v[start..start + (w / 2) as usize].fill((255 - (n % 255)) as u8);
        }
        push_i420(&source, buf);
        n = n.wrapping_add(1);
    });
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit(1279, 721, 1920, 1080), (1278, 720));
        assert_eq!(fit(5120, 1440, 1920, 1080), (1920, 540));
    }
}
