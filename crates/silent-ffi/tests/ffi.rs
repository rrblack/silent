//! Drives the mobile API the way the Kotlin app does, on the host.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use silent_core::proto::IceConfig;
use silent_ffi::{check_code, create_room, join_room, RoomListener, RoomSettings, RoomUpdate};

enum Seen {
    Update(RoomUpdate),
    Frame {
        screen: bool,
        width: u32,
        height: u32,
        bytes: usize,
    },
}

struct Listener(Mutex<Sender<Seen>>);

impl RoomListener for Listener {
    fn on_update(&self, update: RoomUpdate) {
        let _ = self.0.lock().unwrap().send(Seen::Update(update));
    }
    fn on_video_frame(&self, _peer: u32, screen: bool, width: u32, height: u32, rgba: Vec<u8>) {
        let _ = self.0.lock().unwrap().send(Seen::Frame {
            screen,
            width,
            height,
            bytes: rgba.len(),
        });
    }
}

fn listener() -> (Arc<Listener>, Receiver<Seen>) {
    let (tx, rx) = channel();
    (Arc::new(Listener(Mutex::new(tx))), rx)
}

fn settings(relay: &str, name: &str) -> RoomSettings {
    RoomSettings {
        relay_url: relay.into(),
        name: name.into(),
        camera_high: false,
        screen_detail: false,
        relay_only: false,
        use_audio: false,
    }
}

fn wait<T>(rx: &Receiver<Seen>, mut pick: impl FnMut(&Seen) -> Option<T>) -> T {
    loop {
        let seen = rx.recv_timeout(Duration::from_secs(20)).expect("timed out");
        if let Some(v) = pick(&seen) {
            return v;
        }
    }
}

#[test]
fn mobile_api_round_trip() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let relay = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = silent_relay::RelayConfig {
            ice: IceConfig {
                ice_servers: vec![],
            },
        };
        tokio::spawn(silent_relay::serve(listener, config));
        format!("http://{addr}")
    });

    assert!(check_code("nope".into()).is_some());

    let (la, ra) = listener();
    let alice = create_room(settings(&relay, "Alice"), la).unwrap();
    assert!(check_code(alice.code()).is_none());

    let (lb, rb) = listener();
    let bob = join_room(alice.code(), settings(&relay, "Bob"), lb).unwrap();

    let name = wait(&ra, |s| match s {
        Seen::Update(RoomUpdate::PeerVerified { name, .. }) => Some(name.clone()),
        _ => None,
    });
    assert_eq!(name, "Bob");
    let words_a = wait(&ra, |s| match s {
        Seen::Update(RoomUpdate::PeerConnected { safety_words, .. }) => Some(safety_words.clone()),
        _ => None,
    });
    let words_b = wait(&rb, |s| match s {
        Seen::Update(RoomUpdate::PeerConnected { safety_words, .. }) => Some(safety_words.clone()),
        _ => None,
    });
    assert_eq!(words_a, words_b);

    bob.send_chat("hi from the phone".into());
    let text = wait(&ra, |s| match s {
        Seen::Update(RoomUpdate::Chat { text, .. }) => Some(text.clone()),
        _ => None,
    });
    assert_eq!(text, "hi from the phone");

    // Bob's phone camera: 640x480 sensor frames, rotated 90 degrees.
    bob.set_camera(true);
    let pusher = {
        let bob = bob.clone();
        std::thread::spawn(move || {
            let (w, h) = (640u32, 480u32);
            for _ in 0..90 {
                let y = vec![100u8; (w * h) as usize];
                let uv = vec![128u8; (w * h / 2) as usize];
                bob.push_camera_frame(w, h, y, uv.clone(), uv[1..].to_vec(), w, w, 2, 90);
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let (screen, width, height, bytes) = wait(&ra, |s| match s {
        Seen::Frame {
            screen,
            width,
            height,
            bytes,
        } if *width > 2 => Some((*screen, *width, *height, *bytes)),
        _ => None,
    });
    assert!(!screen);
    assert_eq!((width, height), (480, 640));
    assert_eq!(bytes, (480 * 640 * 4) as usize);
    pusher.join().unwrap();

    let stats = alice.stats();
    assert_eq!(stats.len(), 1);
    assert!(
        stats[0]
            .lines
            .iter()
            .any(|l| l.starts_with("you get: camera")),
        "{:?}",
        stats[0].lines
    );

    bob.leave();
    wait(&ra, |s| {
        matches!(s, Seen::Update(RoomUpdate::PeerLeft { .. })).then_some(())
    });
    alice.leave();
}
