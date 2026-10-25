//! A call participant without a screen, for testing: it joins a room with a
//! beeping microphone and a test-pattern camera, echoes chat back, and can
//! send a file once someone is connected. With `BOT_SCREEN=1` it also shares
//! a test-pattern screen (1080p, 60 fps). With `BOT_VIEW=640x360` it tells
//! friends it shows their cameras at that size, as the apps do with tiles.
//!
//! ```sh
//! cargo run -p silent-engine --example bot -- http://127.0.0.1:8787 1234-word-word-word [file]
//! ```

use std::time::Duration;

use futures_util::StreamExt;
use silent_engine::libwebrtc::video_stream::native::NativeVideoStream;
use silent_engine::{
    AudioInput, CameraProfile, CameraTarget, Engine, RoomEvent, ScreenProfile, ScreenTarget,
    Settings, VideoKind,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(relay_url), Some(code)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: bot <relay url> <code> [file to send]");
    };
    let file = args.next();

    let settings = Settings {
        relay_url,
        name: "Bot".into(),
        audio: AudioInput::TestTone,
        camera: CameraProfile::Standard,
        screen: ScreenProfile::Motion,
        relay_only: false,
    };
    let mut room = Engine::new().join_room(settings, code.parse()?).await?;
    let mut events = room.take_events();
    room.set_camera(Some(CameraTarget::TestPattern));
    if std::env::var_os("BOT_SCREEN").is_some() {
        room.share_screen(Some(ScreenTarget::TestPattern));
    }
    println!("joined as {}", room.me());
    let view: Option<(u32, u32)> = std::env::var("BOT_VIEW").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });

    let mut sent = false;
    while let Some(event) = events.recv().await {
        match event {
            RoomEvent::PeerConnected { peer, safety_words } => {
                println!("connected to {peer}: {}", safety_words.join(" "));
                if let (Some(path), false) = (&file, sent) {
                    sent = true;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let data = std::fs::read(path)?;
                    room.send_file(path, data)?;
                    println!("sent {path}");
                }
            }
            RoomEvent::Chat { text, .. } => room.send_chat(format!("you said: {text}")),
            RoomEvent::FileReceived { name, data, .. } => {
                println!("got {name}, {} bytes", data.len());
            }
            RoomEvent::PeerLeft { peer } => println!("{peer} left"),
            RoomEvent::RemoteVideo {
                peer,
                kind: VideoKind::Camera,
                track,
            } => {
                if let Some(size) = view {
                    let mut camera_view = room.camera_view(peer);
                    tokio::spawn(async move {
                        let mut frames = NativeVideoStream::new(track);
                        while let Some(frame) = frames.next().await {
                            camera_view.report(&frame, size);
                        }
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}
