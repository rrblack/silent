//! A call participant without a screen, for testing: it joins a room with a
//! beeping microphone and a test-pattern camera, echoes chat back, and can
//! send a file once someone is connected.
//!
//! ```sh
//! cargo run -p silent-engine --example bot -- http://127.0.0.1:8787 1234-word-word-word [file]
//! ```

use std::time::Duration;

use silent_engine::{
    AudioInput, CameraProfile, CameraTarget, Engine, RoomEvent, ScreenProfile, Settings,
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
    println!("joined as {}", room.me());

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
            _ => {}
        }
    }
    Ok(())
}
