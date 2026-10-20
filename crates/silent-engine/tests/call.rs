//! End-to-end calls inside one process: a local relay plus several engines
//! talking over real WebRTC (host candidates only, no STUN/TURN).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use libwebrtc::video_stream::native::NativeVideoStream;
use libwebrtc::video_track::RtcVideoTrack;
use silent_core::proto::IceConfig;
use silent_engine::{
    AudioInput, CameraProfile, CameraTarget, Engine, PeerId, Room, RoomError, RoomEvent,
    ScreenProfile, ScreenTarget, Settings, VideoKind,
};
use tokio::sync::mpsc::UnboundedReceiver;

const STEP: Duration = Duration::from_secs(20);

/// One call at a time, as in the real app. Several 1080p60 software-encoded
/// meshes in parallel on a small CI machine only measure CPU starvation.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// In-process relay, or `SILENT_TEST_RELAY` (e.g. `wrangler dev`) to run the
/// same calls through the Cloudflare Worker.
async fn relay() -> String {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
    if let Ok(base) = std::env::var("SILENT_TEST_RELAY") {
        return base;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = silent_relay::RelayConfig {
        ice: IceConfig {
            ice_servers: vec![],
        },
    };
    tokio::spawn(silent_relay::serve(listener, config));
    format!("http://{addr}")
}

fn settings(relay: &str, name: &str) -> Settings {
    Settings {
        relay_url: relay.into(),
        name: name.into(),
        audio: AudioInput::Silent,
        camera: CameraProfile::Standard,
        screen: ScreenProfile::Motion,
        relay_only: false,
    }
}

struct Member {
    room: Room,
    events: UnboundedReceiver<RoomEvent>,
    videos: HashMap<(PeerId, VideoKind), RtcVideoTrack>,
}

impl Member {
    fn new(mut room: Room) -> Self {
        let events = room.take_events();
        Self {
            room,
            events,
            videos: HashMap::new(),
        }
    }

    async fn video(&mut self, peer: PeerId, kind: VideoKind) -> RtcVideoTrack {
        if let Some(track) = self.videos.get(&(peer, kind)) {
            return track.clone();
        }
        self.until(|e| match e {
            RoomEvent::RemoteVideo {
                peer: p,
                kind: k,
                track,
            } if *p == peer && *k == kind => Some(track.clone()),
            _ => None,
        })
        .await
    }

    async fn until<T>(&mut self, mut pick: impl FnMut(&RoomEvent) -> Option<T>) -> T {
        let deadline = Instant::now() + STEP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = tokio::time::timeout(left, self.events.recv())
                .await
                .expect("timed out waiting for room event")
                .expect("room closed");
            if std::env::var("SILENT_TEST_TRACE").is_ok() {
                eprintln!("[{}] {ev:?}", self.room.me());
            }
            if let RoomEvent::RemoteVideo { peer, kind, track } = &ev {
                self.videos.insert((*peer, *kind), track.clone());
            }
            if let Some(v) = pick(&ev) {
                return v;
            }
        }
    }

    async fn connected(&mut self, n: usize) -> HashMap<PeerId, [&'static str; 4]> {
        let mut out = HashMap::new();
        while out.len() < n {
            let (peer, words) = self
                .until(|e| match e {
                    RoomEvent::PeerConnected { peer, safety_words } => Some((*peer, *safety_words)),
                    _ => None,
                })
                .await;
            out.insert(peer, words);
        }
        out
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_friends_connect_chat_and_share_screen() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut alice = Member::new(engine.create_room(settings(&relay, "Alice")).await.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );

    let bob_name = alice
        .until(|e| match e {
            RoomEvent::PeerVerified { name, .. } => Some(name.clone()),
            _ => None,
        })
        .await;
    assert_eq!(bob_name, "Bob");

    let a_words = alice.connected(1).await;
    let b_words = bob.connected(1).await;
    assert_eq!(a_words[&bob.room.me()], b_words[&alice.room.me()]);
    assert!(!a_words[&bob.room.me()].contains(&"?"));

    bob.until(|e| matches!(e, RoomEvent::PeerMedia { .. }).then_some(()))
        .await;
    alice.room.send_chat("hi bob");
    let (from, text) = bob
        .until(|e| match e {
            RoomEvent::Chat { from, text, .. } => Some((*from, text.clone())),
            _ => None,
        })
        .await;
    assert_eq!((from, text.as_str()), (alice.room.me(), "hi bob"));

    alice.room.share_screen(Some(ScreenTarget::TestPattern));
    let track = bob.video(alice.room.me(), VideoKind::Screen).await;
    let screen_on = bob
        .until(|e| match e {
            RoomEvent::PeerMedia { state, .. } if state.screen => Some(true),
            _ => None,
        })
        .await;
    assert!(screen_on);

    let mut stream = NativeVideoStream::new(track);
    let warmup = tokio::time::timeout(STEP, stream.next())
        .await
        .expect("no screen frames")
        .unwrap();
    assert!(warmup.buffer.width() > 0);
    let started = Instant::now();
    let mut frames = 0u32;
    let mut max_size = (0, 0);
    while started.elapsed() < Duration::from_secs(4) {
        if let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            frames += 1;
            max_size = max_size.max((frame.buffer.width(), frame.buffer.height()));
        }
    }
    let fps = frames as f64 / started.elapsed().as_secs_f64();
    println!(
        "screen share received: {fps:.1} fps, up to {}x{}",
        max_size.0, max_size.1
    );
    assert!(frames > 10, "only {frames} frames in 4s");

    let stats = alice.room.stats().await;
    let screen = stats[0]
        .sending
        .iter()
        .find(|v| v.kind == Some(VideoKind::Screen))
        .expect("screen in outbound stats");
    println!(
        "sender stats: {}x{} @ {:.1} fps via {} (limited by {}, rtt {:?} ms)",
        screen.width,
        screen.height,
        screen.fps,
        screen.implementation,
        screen.limited_by,
        stats[0].rtt_ms
    );
    assert_eq!((screen.width, screen.height), (1920, 1080));
    assert!(!screen.implementation.is_empty());

    alice.room.share_screen(None);
    bob.until(|e| match e {
        RoomEvent::PeerMedia { state, .. } if !state.screen => Some(()),
        _ => None,
    })
    .await;

    let alice_id = alice.room.me();
    alice.room.leave().await;
    let left = bob
        .until(|e| match e {
            RoomEvent::PeerLeft { peer } => Some(*peer),
            _ => None,
        })
        .await;
    assert_eq!(left, alice_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_code_is_detected_and_isolated() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut alice = Member::new(engine.create_room(settings(&relay, "Alice")).await.unwrap());
    let real = alice.room.code().to_string();
    let mut parts: Vec<&str> = real.split('-').collect();
    parts[3] = if parts[3] == "oak" { "owl" } else { "oak" };
    let wrong = parts.join("-").parse().unwrap();

    let mut mallory = Member::new(
        engine
            .join_room(settings(&relay, "Mallory"), wrong)
            .await
            .unwrap(),
    );
    alice
        .until(|e| matches!(e, RoomEvent::PeerWrongCode { .. }).then_some(()))
        .await;
    mallory
        .until(|e| matches!(e, RoomEvent::PeerWrongCode { .. }).then_some(()))
        .await;

    let quiet = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match alice.events.recv().await {
                Some(RoomEvent::PeerVerified { .. } | RoomEvent::PeerConnected { .. }) => {
                    return true
                }
                Some(_) => continue,
                None => return false,
            }
        }
    })
    .await;
    assert!(
        quiet.is_err() || !quiet.unwrap(),
        "wrong-code peer must never be verified"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_friends_full_mesh_and_fifth_is_turned_away() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engines: Vec<Engine> = (0..5).map(|_| Engine::new()).collect();

    let first = engines[0]
        .create_room(settings(&relay, "P1"))
        .await
        .unwrap();
    let code = first.code().clone();
    let mut members = vec![Member::new(first)];
    for (i, engine) in engines.iter().enumerate().take(4).skip(1) {
        let name = format!("P{}", i + 1);
        let room = engine
            .join_room(settings(&relay, &name), code.clone())
            .await
            .unwrap();
        members.push(Member::new(room));
    }

    let mut words: Vec<HashMap<PeerId, [&str; 4]>> = Vec::new();
    for m in members.iter_mut() {
        words.push(m.connected(3).await);
    }
    for (i, a) in members.iter().enumerate() {
        for (j, b) in members.iter().enumerate() {
            if i != j {
                assert_eq!(
                    words[i][&b.room.me()],
                    words[j][&a.room.me()],
                    "pair {i}-{j}"
                );
            }
        }
    }

    members[2].room.set_camera(Some(CameraTarget::TestPattern));
    let cam_owner = members[2].room.me();
    for (i, m) in members.iter_mut().enumerate() {
        if i == 2 {
            continue;
        }
        let peer = m
            .until(|e| match e {
                RoomEvent::PeerMedia { peer, state } if state.camera => Some(*peer),
                _ => None,
            })
            .await;
        assert_eq!(peer, cam_owner);
    }

    match engines[4].join_room(settings(&relay, "P5"), code).await {
        Err(RoomError::RoomFull) => {}
        Err(e) => panic!("expected RoomFull, got {e}"),
        Ok(_) => panic!("expected RoomFull, got a room"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushed_frames_arrive_upright() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut alice = Member::new(engine.create_room(settings(&relay, "Alice")).await.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );
    alice.connected(1).await;
    bob.connected(1).await;

    // A phone held upright: the sensor delivers 640x480 landscape frames
    // that must be turned 90 degrees clockwise.
    alice.room.set_camera(Some(CameraTarget::External));
    let input = alice.room.video_input(VideoKind::Camera);
    let pusher = std::thread::spawn(move || {
        let (w, h) = (640u32, 480u32);
        let y = vec![120u8; (w * h) as usize];
        let uv = vec![128u8; (w * h / 2) as usize];
        for _ in 0..120 {
            // Semi-planar chroma (NV12-style), as Android cameras deliver it.
            input.push_yuv420(w, h, &y, &uv, &uv[1..], w, w, 2, 90);
            std::thread::sleep(Duration::from_millis(33));
        }
    });

    let track = bob.video(alice.room.me(), VideoKind::Camera).await;
    let mut stream = NativeVideoStream::new(track);
    let mut upright = None;
    let deadline = Instant::now() + STEP;
    while Instant::now() < deadline {
        let Ok(Some(frame)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await
        else {
            continue;
        };
        if let Some(img) = silent_engine::render::to_rgba(&frame, 1920, 1920) {
            if img.width > 2 {
                upright = Some((img.width, img.height));
                break;
            }
        }
    }
    pusher.join().unwrap();
    assert_eq!(
        upright,
        Some((480, 640)),
        "receiver should see a portrait image"
    );
}
