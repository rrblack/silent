//! A pretend friend for trying the app alone. It joins a room like anyone
//! else, with a beeping microphone and a test-pattern camera, answers chat,
//! and sends every file back, so one device can check sound, video, chat,
//! pictures and videos without a second person.

use silent_core::InviteCode;
use tokio::sync::oneshot;

use crate::{
    AudioInput, CameraProfile, CameraTarget, Engine, RoomError, RoomEvent, ScreenProfile, Settings,
};

/// The name the test friend shows under.
pub const TEST_FRIEND_NAME: &str = "Test friend";

/// A test friend in a call. It leaves when dropped.
pub struct TestFriend {
    stop: Option<oneshot::Sender<()>>,
}

impl TestFriend {
    /// Joins the room `code` on `relay_url`. It never plays what it hears, so
    /// your own microphone doesn't come back to you through its speaker.
    pub async fn join(
        engine: &Engine,
        relay_url: &str,
        code: InviteCode,
    ) -> Result<Self, RoomError> {
        let settings = Settings {
            relay_url: relay_url.into(),
            name: TEST_FRIEND_NAME.into(),
            audio: AudioInput::TestTone,
            camera: CameraProfile::Standard,
            screen: ScreenProfile::Motion,
            relay_only: false,
        };
        let mut room = engine.join_room(settings, code).await?;
        let mut events = room.take_events();
        room.set_camera(Some(CameraTarget::TestPattern));
        let (stop, mut stopped) = oneshot::channel();
        tokio::spawn(async move {
            let mut greeted = false;
            loop {
                let event = tokio::select! {
                    _ = &mut stopped => break,
                    event = events.recv() => match event {
                        Some(event) => event,
                        None => break,
                    },
                };
                match event {
                    RoomEvent::PeerConnected { peer, .. } => {
                        room.set_friend_muted(peer, true);
                        if !std::mem::replace(&mut greeted, true) {
                            room.send_chat("Hi! I'm a test friend. Write to me, or send me a picture or a video and I'll send it back.");
                        }
                    }
                    RoomEvent::Chat { text, .. } => room.send_chat(format!("You said: {text}")),
                    RoomEvent::FileReceived { name, data, .. } => {
                        let _ = room.send_file(&name, data);
                    }
                    _ => {}
                }
            }
            room.leave().await;
        });
        Ok(Self { stop: Some(stop) })
    }
}

impl Drop for TestFriend {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
