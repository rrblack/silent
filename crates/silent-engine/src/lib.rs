//! Silent's call engine: relay signaling, pairwise PAKE, and a WebRTC mesh of
//! up to four peers carrying audio, camera, screen share and chat.

#[cfg(desktop)]
pub mod camera;
#[cfg(desktop)]
pub mod capture;
pub mod media;
pub mod render;
mod room;
mod signaling;
pub mod stats;

use libwebrtc::peer_connection_factory::PeerConnectionFactory;
use silent_core::proto::{Intent, RelayErrorCode};
use silent_core::InviteCode;

pub use libwebrtc;
pub use media::{
    AudioDevice, AudioDevices, AudioInput, CameraProfile, CameraTarget, ScreenAudioInput,
    ScreenProfile, ScreenTarget, VideoInput, VideoKind, SCREEN_AUDIO_CHANNELS, SCREEN_AUDIO_RATE,
};
pub use room::{CameraView, FileTooLarge, Room, RoomEvent, Settings, MAX_FRIEND_VOLUME};
pub use signaling::SignalError;
pub use silent_core::proto::MediaState;
pub use silent_core::PeerId;
pub use stats::Quality;

const NAMEPLATE_ATTEMPTS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("no open room matches that code")]
    NoSuchRoom,
    #[error("that room already has four people")]
    RoomFull,
    #[error("too many attempts to join that room, try again in a minute")]
    TooManyJoins,
    #[error(transparent)]
    Signal(#[from] SignalError),
}

/// Entry point for rooms. Cheap to create: every engine shares one
/// process-wide WebRTC factory (threads, audio device), which is never torn
/// down because libwebrtc does not cope with factories being destroyed while
/// others are in use.
pub struct Engine {
    factory: PeerConnectionFactory,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        static FACTORY: std::sync::OnceLock<PeerConnectionFactory> = std::sync::OnceLock::new();
        Self {
            factory: FACTORY.get_or_init(PeerConnectionFactory::default).clone(),
        }
    }

    /// Microphones and speakers the OS offers.
    pub fn audio_devices(&self) -> AudioDevices {
        media::audio_devices(&self.factory)
    }

    /// Chooses the microphone by [`AudioDevice::id`], or `None` for the system
    /// default. Applies to the current call and later ones.
    pub fn choose_microphone(&self, id: Option<String>) {
        media::choose_microphone(&self.factory, id);
    }

    /// Chooses the speaker by [`AudioDevice::id`], or `None` for the system
    /// default. Applies to the current call and later ones.
    pub fn choose_speaker(&self, id: Option<String>) {
        media::choose_speaker(&self.factory, id);
    }

    /// Opens a new room with a fresh invite code.
    pub async fn create_room(&self, settings: Settings) -> Result<Room, RoomError> {
        let mut code = InviteCode::generate();
        for _ in 0..NAMEPLATE_ATTEMPTS {
            match signaling::connect(&settings.relay_url, code.nameplate(), Intent::Create).await {
                Ok((welcome, link)) => {
                    return Ok(Room::start(
                        self.factory.clone(),
                        settings,
                        code,
                        welcome,
                        link,
                    ))
                }
                Err(SignalError::Relay(RelayErrorCode::RoomTaken)) => {
                    code = code.with_new_nameplate();
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(SignalError::Relay(RelayErrorCode::RoomTaken).into())
    }

    /// Joins the room a friend shared. A mistyped secret is not detected
    /// here: it surfaces as [`RoomEvent::PeerWrongCode`] for every peer.
    pub async fn join_room(&self, settings: Settings, code: InviteCode) -> Result<Room, RoomError> {
        match signaling::connect(&settings.relay_url, code.nameplate(), Intent::Join).await {
            Ok((welcome, link)) => Ok(Room::start(
                self.factory.clone(),
                settings,
                code,
                welcome,
                link,
            )),
            Err(SignalError::Relay(RelayErrorCode::NoRoom)) => Err(RoomError::NoSuchRoom),
            Err(SignalError::Relay(RelayErrorCode::RoomFull)) => Err(RoomError::RoomFull),
            Err(SignalError::Relay(RelayErrorCode::TooManyJoins)) => Err(RoomError::TooManyJoins),
            Err(e) => Err(e.into()),
        }
    }
}
