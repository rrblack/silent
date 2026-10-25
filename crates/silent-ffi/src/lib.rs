//! UniFFI bindings for the mobile apps (Kotlin on Android, Swift on iOS).
//!
//! The mobile UI owns capture (CameraX, MediaProjection, AVFoundation,
//! ReplayKit) and pushes frames in; everything else, from the PAKE to the
//! WebRTC mesh, runs in the same Rust engine as the desktop app.
//!
//! Calls that talk to the network block; call them off the main thread.
//! Listener callbacks arrive on background threads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use silent_core::InviteCode;
use silent_engine::libwebrtc::video_stream::native::NativeVideoStream;
use silent_engine::libwebrtc::video_track::RtcVideoTrack;
use silent_engine::{
    AudioInput, CameraProfile, CameraTarget, Engine, PeerId, Room, RoomError, RoomEvent,
    ScreenProfile, ScreenTarget, Settings, VideoInput, VideoKind,
};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

#[cfg(target_os = "android")]
mod android;

uniffi::setup_scaffolding!();

/// Remote video handed to the UI is capped in size and rate: phone screens
/// are small and every frame crosses the FFI boundary as RGBA. The cap is on
/// the long edge, so portrait phone video is not squeezed.
const CAMERA_RENDER: u32 = 960;
const SCREEN_RENDER: u32 = 1920;
const RENDER_INTERVAL: Duration = Duration::from_millis(33);

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("silent")
            .build()
            .expect("tokio runtime")
    })
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| runtime().block_on(async { Engine::new() }))
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct RoomSettings {
    pub relay_url: String,
    pub name: String,
    /// 1080p60 camera instead of 720p30.
    pub camera_high: bool,
    /// Sharp 1440p30 screen share instead of smooth 1080p60.
    pub screen_detail: bool,
    /// Route media through TURN so friends never see this device's IP.
    pub relay_only: bool,
    /// Use the microphone and speaker. Off sends silence.
    pub use_audio: bool,
}

impl From<RoomSettings> for Settings {
    fn from(s: RoomSettings) -> Self {
        Settings {
            relay_url: s.relay_url,
            name: s.name,
            audio: if s.use_audio {
                AudioInput::Platform
            } else {
                AudioInput::Silent
            },
            camera: if s.camera_high {
                CameraProfile::High
            } else {
                CameraProfile::Standard
            },
            screen: if s.screen_detail {
                ScreenProfile::Detail
            } else {
                ScreenProfile::Motion
            },
            relay_only: s.relay_only,
        }
    }
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum RoomUpdate {
    PeerJoining {
        peer: u32,
    },
    /// `replaces` is set when someone already in the call came back under a
    /// new id after reconnecting; drop the old id without a "left" message.
    PeerVerified {
        peer: u32,
        name: String,
        replaces: Option<u32>,
    },
    PeerWrongCode {
        peer: u32,
    },
    PeerConnected {
        peer: u32,
        safety_words: Vec<String>,
    },
    PeerConnectionLost {
        peer: u32,
    },
    PeerLeft {
        peer: u32,
    },
    PeerMedia {
        peer: u32,
        mic: bool,
        camera: bool,
        screen: bool,
    },
    Chat {
        from: u32,
        text: String,
        sent_at_ms: u64,
    },
    LocalMediaError {
        screen: bool,
        reason: String,
    },
    RelayLost,
    RelayRestored,
    /// The others removed you: you typed a different code. Leave the room.
    Removed,
    /// Who is talking now, yourself included as `me()`.
    Speaking {
        peers: Vec<u32>,
    },
    /// A friend's connection got poor (choppy or late audio) or good again.
    ConnectionQuality {
        peer: u32,
        poor: bool,
    },
    FileIncoming {
        from: u32,
        id: u32,
        name: String,
        size: u64,
    },
    /// The whole file, in memory only. Save it only if the user asks.
    FileReceived {
        from: u32,
        id: u32,
        name: String,
        data: Vec<u8>,
    },
    FileFailed {
        from: u32,
        id: u32,
    },
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SilentError {
    #[error("{reason}")]
    InvalidCode { reason: String },
    #[error("No room is open with that number.")]
    NoSuchRoom,
    #[error("That room already has four people.")]
    RoomFull,
    #[error("Too many attempts to join that room. Wait a minute and try again.")]
    TooManyJoins,
    #[error("Couldn't reach the relay: {reason}")]
    Network { reason: String },
    #[error("Files can be up to 64 MB.")]
    FileTooLarge,
}

impl From<RoomError> for SilentError {
    fn from(e: RoomError) -> Self {
        match e {
            RoomError::NoSuchRoom => SilentError::NoSuchRoom,
            RoomError::RoomFull => SilentError::RoomFull,
            RoomError::TooManyJoins => SilentError::TooManyJoins,
            RoomError::Signal(e) => SilentError::Network {
                reason: e.to_string(),
            },
        }
    }
}

#[uniffi::export(with_foreign)]
pub trait RoomListener: Send + Sync {
    fn on_update(&self, update: RoomUpdate);
    /// An upright RGBA frame (4 bytes per pixel, rows packed) of a friend's
    /// camera or screen.
    fn on_video_frame(&self, peer: u32, screen: bool, width: u32, height: u32, rgba: Vec<u8>);
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct FriendStats {
    pub peer: u32,
    pub rtt_ms: Option<f64>,
    pub lines: Vec<String>,
}

/// Returns why `code` isn't a valid invite code, or `None` if it is.
#[uniffi::export]
pub fn check_code(code: String) -> Option<String> {
    code.parse::<InviteCode>().err().map(|e| e.to_string())
}

/// Opens a new room. Blocks until the relay answers.
#[uniffi::export]
pub fn create_room(
    settings: RoomSettings,
    listener: Arc<dyn RoomListener>,
) -> Result<Arc<SilentRoom>, SilentError> {
    let room = runtime().block_on(engine().create_room(settings.into()))?;
    Ok(SilentRoom::start(room, listener))
}

/// Joins a friend's room. Blocks until the relay answers. A mistyped secret
/// shows up afterwards as `PeerWrongCode` for every peer.
#[uniffi::export]
pub fn join_room(
    code: String,
    settings: RoomSettings,
    listener: Arc<dyn RoomListener>,
) -> Result<Arc<SilentRoom>, SilentError> {
    let code: InviteCode =
        code.parse()
            .map_err(|e: silent_core::CodeError| SilentError::InvalidCode {
                reason: e.to_string(),
            })?;
    let room = runtime().block_on(engine().join_room(settings.into(), code))?;
    Ok(SilentRoom::start(room, listener))
}

type Pumps = Arc<Mutex<HashMap<(PeerId, VideoKind), JoinHandle<()>>>>;

#[derive(uniffi::Object)]
pub struct SilentRoom {
    room: Mutex<Option<Room>>,
    code: String,
    me: PeerId,
    camera: VideoInput,
    screen: VideoInput,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// One video pump per friend and kind; stopped when they leave.
    pumps: Pumps,
}

impl SilentRoom {
    fn start(mut room: Room, listener: Arc<dyn RoomListener>) -> Arc<Self> {
        let mut events = room.take_events();
        let tasks: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let pumps: Pumps = Arc::default();
        let this = Arc::new(SilentRoom {
            code: room.code().to_string(),
            me: room.me(),
            camera: room.video_input(VideoKind::Camera),
            screen: room.video_input(VideoKind::Screen),
            room: Mutex::new(Some(room)),
            tasks: tasks.clone(),
            pumps: pumps.clone(),
        });

        let handle = runtime().spawn(async move {
            while let Some(event) = events.recv().await {
                let gone = match &event {
                    RoomEvent::RemoteVideo { peer, kind, track } => {
                        let pump =
                            runtime().spawn(pump(*peer, *kind, track.clone(), listener.clone()));
                        if let Some(old) = pumps.lock().unwrap().insert((*peer, *kind), pump) {
                            old.abort();
                        }
                        continue;
                    }
                    RoomEvent::PeerLeft { peer }
                    | RoomEvent::PeerVerified {
                        replaces: Some(peer),
                        ..
                    } => Some(*peer),
                    _ => None,
                };
                if let Some(gone) = gone {
                    pumps.lock().unwrap().retain(|(peer, _), pump| {
                        let keep = *peer != gone;
                        if !keep {
                            pump.abort();
                        }
                        keep
                    });
                }
                if let Some(update) = to_update(event) {
                    listener.on_update(update);
                }
            }
        });
        tasks.lock().unwrap().push(handle);
        this
    }

    fn with_room(&self, f: impl FnOnce(&Room)) {
        if let Some(room) = self.room.lock().unwrap().as_ref() {
            f(room);
        }
    }
}

#[uniffi::export]
impl SilentRoom {
    pub fn code(&self) -> String {
        self.code.clone()
    }

    pub fn me(&self) -> u32 {
        self.me
    }

    pub fn send_chat(&self, text: String) {
        self.with_room(|r| r.send_chat(text));
    }

    pub fn set_mic(&self, on: bool) {
        self.with_room(|r| r.set_mic(on));
    }

    /// After turning the camera on, feed frames with `push_camera_frame`.
    pub fn set_camera(&self, on: bool) {
        self.with_room(|r| r.set_camera(on.then_some(CameraTarget::External)));
    }

    /// After turning sharing on, feed frames with `push_screen_frame`.
    pub fn set_screen(&self, on: bool) {
        self.with_room(|r| r.share_screen(on.then_some(ScreenTarget::External)));
    }

    /// Sends a file to everyone in the room over the encrypted data channels.
    pub fn send_file(&self, name: String, data: Vec<u8>) -> Result<(), SilentError> {
        let mut result = Ok(());
        self.with_room(|r| {
            result = r
                .send_file(&name, data)
                .map_err(|_| SilentError::FileTooLarge)
        });
        result
    }

    /// Silences a friend for this device only.
    pub fn set_friend_muted(&self, peer: u32, muted: bool) {
        self.with_room(|r| r.set_friend_muted(peer, muted));
    }

    /// How loud a friend plays on this device: 1.0 normal, 0.0 silent, up to
    /// 2.0. Kept if they reconnect.
    pub fn set_friend_volume(&self, peer: u32, volume: f64) {
        self.with_room(|r| r.set_friend_volume(peer, volume));
    }

    /// Call when the phone switched networks, so calls move over at once.
    pub fn restart_connections(&self) {
        self.with_room(|r| r.restart_connections());
    }

    /// One camera frame in YUV 4:2:0 (Android `YUV_420_888`). See
    /// `VideoInput::push_yuv420` for the meaning of each argument.
    #[allow(clippy::too_many_arguments)]
    pub fn push_camera_frame(
        &self,
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        y_stride: u32,
        uv_stride: u32,
        uv_pixel_stride: u32,
        rotation_degrees: u32,
    ) {
        self.camera.push_yuv420(
            width,
            height,
            &y,
            &u,
            &v,
            y_stride,
            uv_stride,
            uv_pixel_stride,
            rotation_degrees,
        );
    }

    /// One screen frame in RGBA (Android `ImageReader` with `RGBA_8888`).
    pub fn push_screen_frame(&self, width: u32, height: u32, row_stride: u32, rgba: Vec<u8>) {
        self.screen.push_rgba(width, height, row_stride, &rgba);
    }

    /// One screen frame in YUV 4:2:0 (iOS ReplayKit delivers NV12). Same
    /// arguments as `push_camera_frame`.
    #[allow(clippy::too_many_arguments)]
    pub fn push_screen_yuv(
        &self,
        width: u32,
        height: u32,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        y_stride: u32,
        uv_stride: u32,
        uv_pixel_stride: u32,
        rotation_degrees: u32,
    ) {
        self.screen.push_yuv420(
            width,
            height,
            &y,
            &u,
            &v,
            y_stride,
            uv_stride,
            uv_pixel_stride,
            rotation_degrees,
        );
    }

    pub fn stats(&self) -> Vec<FriendStats> {
        let mut request = None;
        self.with_room(|r| request = Some(r.stats()));
        let Some(request) = request else {
            return Vec::new();
        };
        runtime()
            .block_on(request)
            .into_iter()
            .map(|p| FriendStats {
                peer: p.peer,
                rtt_ms: p.rtt_ms,
                lines: p
                    .sending
                    .iter()
                    .map(|v| format!("you send: {}", v.summary()))
                    .chain(
                        p.receiving
                            .iter()
                            .map(|v| format!("you get: {}", v.summary())),
                    )
                    .collect(),
            })
            .collect()
    }

    pub fn leave(&self) {
        let room = self.room.lock().unwrap().take();
        self.stop_tasks();
        if let Some(room) = room {
            runtime().block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(2), room.leave()).await;
            });
        }
    }
}

impl SilentRoom {
    fn stop_tasks(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        for (_, pump) in self.pumps.lock().unwrap().drain() {
            pump.abort();
        }
    }
}

impl Drop for SilentRoom {
    fn drop(&mut self) {
        self.stop_tasks();
    }
}

async fn pump(
    peer: PeerId,
    kind: VideoKind,
    track: RtcVideoTrack,
    listener: Arc<dyn RoomListener>,
) {
    let long_edge = match kind {
        VideoKind::Camera => CAMERA_RENDER,
        VideoKind::Screen => SCREEN_RENDER,
    };
    let screen = kind == VideoKind::Screen;
    let mut stream = NativeVideoStream::new(track);
    let mut last = Instant::now() - RENDER_INTERVAL;
    while let Some(frame) = stream.next().await {
        if last.elapsed() < RENDER_INTERVAL {
            continue;
        }
        last = Instant::now();
        let Some(img) = silent_engine::render::to_rgba(&frame, long_edge, long_edge) else {
            continue;
        };
        let listener = listener.clone();
        // The UI callback may block briefly; keep it off the async workers.
        let _ = tokio::task::spawn_blocking(move || {
            listener.on_video_frame(peer, screen, img.width, img.height, img.data)
        })
        .await;
    }
}

fn to_update(event: RoomEvent) -> Option<RoomUpdate> {
    Some(match event {
        RoomEvent::PeerJoining { peer } => RoomUpdate::PeerJoining { peer },
        RoomEvent::PeerVerified {
            peer,
            name,
            replaces,
        } => RoomUpdate::PeerVerified {
            peer,
            name,
            replaces,
        },
        RoomEvent::PeerWrongCode { peer } => RoomUpdate::PeerWrongCode { peer },
        RoomEvent::PeerConnected { peer, safety_words } => RoomUpdate::PeerConnected {
            peer,
            safety_words: safety_words.iter().map(|w| w.to_string()).collect(),
        },
        RoomEvent::PeerConnectionLost { peer } => RoomUpdate::PeerConnectionLost { peer },
        RoomEvent::PeerLeft { peer } => RoomUpdate::PeerLeft { peer },
        RoomEvent::PeerMedia { peer, state } => RoomUpdate::PeerMedia {
            peer,
            mic: state.mic,
            camera: state.camera,
            screen: state.screen,
        },
        RoomEvent::Chat {
            from,
            text,
            sent_at_ms,
        } => RoomUpdate::Chat {
            from,
            text,
            sent_at_ms,
        },
        RoomEvent::LocalMediaError { kind, message } => RoomUpdate::LocalMediaError {
            screen: kind == VideoKind::Screen,
            reason: message,
        },
        RoomEvent::RelayLost => RoomUpdate::RelayLost,
        RoomEvent::RelayRestored => RoomUpdate::RelayRestored,
        RoomEvent::Removed => RoomUpdate::Removed,
        RoomEvent::Speaking { peers } => RoomUpdate::Speaking { peers },
        RoomEvent::ConnectionQuality { peer, quality } => RoomUpdate::ConnectionQuality {
            peer,
            poor: quality == silent_engine::Quality::Poor,
        },
        RoomEvent::FileIncoming {
            from,
            id,
            name,
            size,
        } => RoomUpdate::FileIncoming {
            from,
            id,
            name,
            size,
        },
        RoomEvent::FileReceived {
            from,
            id,
            name,
            data,
        } => RoomUpdate::FileReceived {
            from,
            id,
            name,
            data,
        },
        RoomEvent::FileFailed { from, id } => RoomUpdate::FileFailed { from, id },
        RoomEvent::RemoteVideo { .. } => return None,
    })
}
