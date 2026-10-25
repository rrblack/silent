//! Local media: microphone, camera and screen tracks, plus the encoding
//! settings applied to each. One set of tracks per room; every peer
//! connection sends the same tracks (libwebrtc encodes once per connection,
//! which keeps per-friend bitrate adaptation independent).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// The computer's own sound, sent along with a screen share.
pub const STREAM_SCREEN_AUDIO: &str = "silent-screen-audio";

/// Computer audio is sent as 48 kHz stereo.
pub const SCREEN_AUDIO_RATE: u32 = 48_000;
pub const SCREEN_AUDIO_CHANNELS: u32 = 2;

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
    /// No audio device. Sends a 440 Hz beep, half a second on, half a
    /// second off; for diagnostics and tests.
    TestTone,
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

impl Encoding {
    /// How much a camera is scaled down in a call with `friends` others.
    /// Every friend gets their own encoder in a mesh, so bigger calls send
    /// smaller video: 720p, then 540p, then 480p (1080p: 720p, then 540p).
    pub(crate) fn mesh_scale(&self, friends: usize) -> f64 {
        let heights: [u32; 3] = if self.max_height >= 1080 {
            [1080, 720, 540]
        } else {
            [720, 540, 480]
        };
        let target = heights[friends.saturating_sub(1).min(2)];
        (self.max_height as f64 / target as f64).max(1.0)
    }
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
    /// Computer audio for screen shares: silent until the app pushes some.
    pub screen_audio: RtcAudioTrack,
    pub screen_audio_input: ScreenAudioInput,
    screen_audio_feed: tokio::task::JoinHandle<()>,
    _silence: Option<NativeAudioSource>,
    tone: Option<tokio::task::JoinHandle<()>>,
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
        let audio_choice = audio;
        let platform_audio = audio == AudioInput::Platform && factory.acquire_platform_adm();
        if audio == AudioInput::Platform && !platform_audio {
            tracing::warn!("no usable audio device; sending silence");
        }
        if platform_audio {
            let chosen = CHOSEN_AUDIO
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(id) = &chosen.microphone {
                factory.set_recording_device_by_guid(id);
            }
            if let Some(id) = &chosen.speaker {
                factory.set_playout_device_by_guid(id);
            }
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
        let tone = match (&silence, audio_choice == AudioInput::TestTone) {
            (Some(source), true) => Some(tokio::spawn(test_tone(source.clone()))),
            _ => None,
        };
        // No echo cancellation or noise suppression: it's music and games,
        // and it never came through a microphone.
        // No queue: frames go straight to the encoders at the capture's own
        // pace, so the delay can't build up when the two clocks drift.
        let screen_source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            SCREEN_AUDIO_RATE,
            SCREEN_AUDIO_CHANNELS,
            0,
        );
        let screen_audio = factory.create_audio_track(STREAM_SCREEN_AUDIO, screen_source.clone());
        let (chunks, queued) = tokio::sync::mpsc::channel(SCREEN_AUDIO_QUEUE);
        Self {
            factory: factory.clone(),
            audio,
            screen_audio,
            screen_audio_input: ScreenAudioInput { chunks },
            screen_audio_feed: tokio::spawn(feed_screen_audio(screen_source, queued)),
            _silence: silence,
            tone,
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
        if let Some(tone) = self.tone.take() {
            tone.abort();
        }
        self.screen_audio_feed.abort();
        if self.platform_audio {
            self.factory.release_platform_adm();
        }
    }
}

/// Marks the computer audio m-line of our own offer or answer as stereo,
/// which is how libwebrtc learns the other side may send it in stereo (it
/// mixes Opus down to mono otherwise), with room for music-grade bitrate.
pub(crate) fn stereo_computer_audio(sdp: &str) -> String {
    let marker = format!("a=msid:{STREAM_SCREEN_AUDIO} ");
    let mut out = String::with_capacity(sdp.len() + 64);
    for (i, section) in sdp.split("\r\nm=").enumerate() {
        if i > 0 {
            out.push_str("\r\nm=");
        }
        if !section.lines().any(|l| l.starts_with(&marker)) {
            out.push_str(section);
            continue;
        }
        let opus = section.lines().find_map(|l| {
            let (pt, codec) = l.strip_prefix("a=rtpmap:")?.split_once(' ')?;
            codec.starts_with("opus/").then(|| pt.to_string())
        });
        let fmtp = opus.map(|pt| format!("a=fmtp:{pt} "));
        let lines: Vec<String> = section
            .split("\r\n")
            .map(|l| match &fmtp {
                Some(f) if l.starts_with(f.as_str()) && !l.contains("stereo=") => {
                    format!("{l};stereo=1;sprop-stereo=1;maxaveragebitrate=128000")
                }
                _ => l.to_string(),
            })
            .collect();
        out.push_str(&lines.join("\r\n"));
    }
    out
}

/// Chunks of computer audio waiting for the source; more are dropped.
const SCREEN_AUDIO_QUEUE: usize = 32;

/// Where an app pushes the computer's own sound while it shares its screen
/// (on Windows: everything playing except Silent itself, so friends don't
/// hear themselves). Cheap to clone and safe to use from any thread.
#[derive(Clone)]
pub struct ScreenAudioInput {
    chunks: tokio::sync::mpsc::Sender<Vec<i16>>,
}

impl ScreenAudioInput {
    /// Interleaved 16-bit stereo at 48 kHz, in chunks of any length. Never
    /// blocks: if the call can't keep up, the chunk is dropped.
    pub fn push(&self, samples: Vec<i16>) {
        if samples.len() >= SCREEN_AUDIO_CHANNELS as usize {
            let _ = self.chunks.try_send(samples);
        }
    }
}

/// Hands pushed computer audio to `source` in order, cut into the exact
/// 10 ms frames its unqueued path takes.
async fn feed_screen_audio(
    source: NativeAudioSource,
    mut chunks: tokio::sync::mpsc::Receiver<Vec<i16>>,
) {
    const FRAME: usize = (SCREEN_AUDIO_RATE / 100 * SCREEN_AUDIO_CHANNELS) as usize;
    let mut pending: Vec<i16> = Vec::with_capacity(FRAME * 4);
    while let Some(chunk) = chunks.recv().await {
        pending.extend_from_slice(&chunk);
        let whole = pending.len() / FRAME * FRAME;
        for frame in pending[..whole].chunks_exact(FRAME) {
            let frame = AudioFrame {
                data: frame.into(),
                sample_rate: SCREEN_AUDIO_RATE,
                num_channels: SCREEN_AUDIO_CHANNELS,
                samples_per_channel: SCREEN_AUDIO_RATE / 100,
            };
            let _ = source.capture_frame(&frame).await;
        }
        pending.drain(..whole);
    }
}

/// Feeds `source` a beep: 10 ms frames, paced by the source's queue.
async fn test_tone(source: NativeAudioSource) {
    const RATE: u32 = 48_000;
    const FRAME: usize = (RATE / 100) as usize;
    let mut n: u64 = 0;
    loop {
        let data: Vec<i16> = (0..FRAME)
            .map(|i| {
                let t = n + i as u64;
                let on = (t / (RATE as u64 / 2)) % 2 == 0;
                let phase = t as f64 * 440.0 * std::f64::consts::TAU / RATE as f64;
                if on {
                    (phase.sin() * 8000.0) as i16
                } else {
                    0
                }
            })
            .collect();
        n += FRAME as u64;
        let frame = AudioFrame {
            data: data.into(),
            sample_rate: RATE,
            num_channels: 1,
            samples_per_channel: FRAME as u32,
        };
        if source.capture_frame(&frame).await.is_err() {
            return;
        }
    }
}

/// A microphone or speaker the OS offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// Stable across hot-plugging; pass it back to choose the device.
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDevices {
    pub microphones: Vec<AudioDevice>,
    pub speakers: Vec<AudioDevice>,
}

#[derive(Clone, Default)]
struct ChosenAudio {
    microphone: Option<String>,
    speaker: Option<String>,
}

/// The user's device choice, applied whenever the audio device module is
/// opened. Process-wide, like the module itself; kept in memory only.
static CHOSEN_AUDIO: Mutex<ChosenAudio> = Mutex::new(ChosenAudio {
    microphone: None,
    speaker: None,
});

pub(crate) fn audio_devices(factory: &PeerConnectionFactory) -> AudioDevices {
    let opened = !factory.is_platform_adm_active() && factory.acquire_platform_adm();
    let list = |count: i16, guid: &dyn Fn(u16) -> String, name: &dyn Fn(u16) -> String| {
        (0..count.max(0) as u16)
            .map(|i| AudioDevice {
                id: guid(i),
                name: name(i),
            })
            .filter(|d| !d.id.is_empty())
            .collect()
    };
    let devices = AudioDevices {
        microphones: list(
            factory.recording_devices(),
            &|i| factory.recording_device_guid(i),
            &|i| factory.recording_device_name(i),
        ),
        speakers: list(
            factory.playout_devices(),
            &|i| factory.playout_device_guid(i),
            &|i| factory.playout_device_name(i),
        ),
    };
    if opened {
        factory.release_platform_adm();
    }
    devices
}

/// Chooses the microphone (`None` for the system default). Takes effect at
/// once in a running call.
pub(crate) fn choose_microphone(factory: &PeerConnectionFactory, id: Option<String>) {
    CHOSEN_AUDIO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .microphone = id.clone();
    if !factory.is_platform_adm_active() {
        return;
    }
    let recording = factory.recording_is_initialized();
    if recording {
        factory.stop_recording();
    }
    match &id {
        Some(id) => factory.set_recording_device_by_guid(id),
        None => factory.set_recording_device(0),
    };
    if recording {
        factory.init_recording();
        factory.start_recording();
    }
}

/// Chooses the speaker (`None` for the system default). Takes effect at once
/// in a running call.
pub(crate) fn choose_speaker(factory: &PeerConnectionFactory, id: Option<String>) {
    CHOSEN_AUDIO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .speaker = id.clone();
    if !factory.is_platform_adm_active() {
        return;
    }
    let playing = factory.playout_is_initialized();
    if playing {
        factory.stop_playout();
    }
    match &id {
        Some(id) => factory.set_playout_device_by_guid(id),
        None => factory.set_playout_device(0),
    };
    if playing {
        factory.init_playout();
        factory.start_playout();
    }
}

/// Applies the profile's limits to a video sender and pauses or resumes it.
/// `scale` shrinks the picture (and its bitrate cap) by that factor.
pub(crate) fn apply_encoding(sender: &RtpSender, encoding: &Encoding, active: bool, scale: f64) {
    let mut params = sender.parameters();
    for enc in params.encodings.iter_mut() {
        enc.active = active;
        enc.scale_resolution_down_by = Some(scale);
        enc.max_bitrate = Some((encoding.max_bitrate_bps as f64 / scale) as u64);
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

/// Which video codec a device would like to send. It tells friends in its
/// `Hello`, and they list that codec first, since libwebrtc sends with the
/// first codec the other side lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodecChoice {
    /// libwebrtc's own order (VP8 first).
    Default,
    /// H.264 first: what GPUs and phones encode in hardware.
    H264,
}

impl CodecChoice {
    /// H.264 when this device has a hardware video encoder. The
    /// `SILENT_VIDEO_CODEC` environment variable (`h264` or `vp8`) overrides
    /// the choice, for troubleshooting.
    pub(crate) fn detect() -> Self {
        match std::env::var("SILENT_VIDEO_CODEC").as_deref() {
            Ok("h264") => return CodecChoice::H264,
            Ok("vp8") => return CodecChoice::Default,
            _ => {}
        }
        if hardware_encoder() {
            CodecChoice::H264
        } else {
            CodecChoice::Default
        }
    }

    /// Codec preferences for receiving from a friend who chose `self`, or
    /// `None` to leave libwebrtc's order alone.
    pub(crate) fn preferences(
        self,
        factory: &PeerConnectionFactory,
    ) -> Option<Vec<RtpCodecCapability>> {
        match self {
            CodecChoice::Default => None,
            CodecChoice::H264 => {
                let codecs = factory
                    .get_rtp_receiver_capabilities(MediaType::Video)
                    .codecs;
                let ordered = h264_first(codecs);
                ordered
                    .first()
                    .is_some_and(|c| c.mime_type.eq_ignore_ascii_case("video/H264"))
                    .then_some(ordered)
            }
        }
    }
}

pub(crate) fn hardware_encoder() -> bool {
    VideoEncoderBackend::list_available().into_iter().any(|b| {
        matches!(
            b,
            VideoEncoderBackend::Hardware
                | VideoEncoderBackend::Nvenc
                | VideoEncoderBackend::Vaapi
                | VideoEncoderBackend::VideoToolbox
        )
    })
}

/// Moves H.264 to the front, keeping everything else (VP8, RTX, RED, FEC) in
/// its original order. Among H.264 variants, packetization mode 1 with the
/// constrained baseline profile comes first: it is what every hardware
/// encoder and decoder supports.
fn h264_first(mut codecs: Vec<RtpCodecCapability>) -> Vec<RtpCodecCapability> {
    let rank = |c: &RtpCodecCapability| {
        if !c.mime_type.eq_ignore_ascii_case("video/H264") {
            return 3;
        }
        let fmtp = c.sdp_fmtp_line.as_deref().unwrap_or("");
        match (
            fmtp.contains("packetization-mode=1"),
            fmtp.contains("profile-level-id=42e0"),
        ) {
            (true, true) => 0,
            (true, false) => 1,
            _ => 2,
        }
    };
    codecs.sort_by_key(rank);
    codecs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_computer_audio_line_turns_stereo() {
        let sdp = "v=0\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\na=msid:silent-audio t1\r\n\
            a=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:3\r\na=msid:silent-screen-audio t2\r\n\
            a=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;useinbandfec=1\r\n";
        let out = stereo_computer_audio(sdp);
        let fmtps: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("a=fmtp:111"))
            .collect();
        assert_eq!(fmtps[0], "a=fmtp:111 minptime=10;useinbandfec=1");
        assert_eq!(
            fmtps[1],
            "a=fmtp:111 minptime=10;useinbandfec=1;stereo=1;sprop-stereo=1;maxaveragebitrate=128000"
        );
        assert_eq!(
            out.len(),
            sdp.len() + ";stereo=1;sprop-stereo=1;maxaveragebitrate=128000".len()
        );
        // Applying it twice changes nothing more.
        assert_eq!(stereo_computer_audio(&out), out);
    }

    #[test]
    fn cameras_shrink_as_calls_grow() {
        let standard = CameraProfile::Standard.encoding();
        let heights: Vec<u32> = (1..=3)
            .map(|friends| (720.0 / standard.mesh_scale(friends)).round() as u32)
            .collect();
        assert_eq!(heights, [720, 540, 480]);
        let high = CameraProfile::High.encoding();
        assert_eq!(high.mesh_scale(1), 1.0);
        assert_eq!(high.mesh_scale(3), 2.0);
        assert_eq!(standard.mesh_scale(0), 1.0);
    }

    #[test]
    fn fit_keeps_aspect_and_even_sizes() {
        assert_eq!(fit(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit(2560, 1600, 1920, 1080), (1728, 1080));
        assert_eq!(fit(1279, 721, 1920, 1080), (1278, 720));
        assert_eq!(fit(5120, 1440, 1920, 1080), (1920, 540));
    }

    fn codec(mime: &str, fmtp: Option<&str>) -> RtpCodecCapability {
        RtpCodecCapability {
            channels: None,
            clock_rate: Some(90_000),
            mime_type: mime.into(),
            sdp_fmtp_line: fmtp.map(Into::into),
        }
    }

    #[test]
    fn h264_moves_to_the_front() {
        let ordered = h264_first(vec![
            codec("video/VP8", None),
            codec("video/rtx", Some("apt=96")),
            codec(
                "video/H264",
                Some("level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f"),
            ),
            codec(
                "video/H264",
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f"),
            ),
            codec(
                "video/H264",
                Some("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"),
            ),
            codec("video/AV1", None),
        ]);
        let names: Vec<(&str, Option<&str>)> = ordered
            .iter()
            .map(|c| (c.mime_type.as_str(), c.sdp_fmtp_line.as_deref()))
            .collect();
        assert_eq!(names[0].0, "video/H264");
        assert!(names[0]
            .1
            .unwrap()
            .contains("packetization-mode=1;profile-level-id=42e01f"));
        assert!(names[1].1.unwrap().contains("640c1f"));
        assert!(names[2].1.unwrap().contains("packetization-mode=0"));
        assert_eq!(names[3].0, "video/VP8");
        assert_eq!(names[4].0, "video/rtx");
        assert_eq!(names[5].0, "video/AV1");
    }
}
