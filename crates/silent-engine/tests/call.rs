//! End-to-end calls inside one process: a local relay plus several engines
//! talking over real WebRTC (host candidates only, no STUN/TURN).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
use tokio::sync::watch;

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

/// A TCP proxy in front of the relay that can cut every connection and
/// refuse new ones, the way a relay restart or a network blip would.
struct Proxy {
    url: String,
    down: Arc<AtomicBool>,
    cut: watch::Sender<u32>,
}

impl Proxy {
    async fn start(upstream: &str) -> Self {
        let target = upstream
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let down = Arc::new(AtomicBool::new(false));
        let (cut, _) = watch::channel(0u32);
        let (is_down, cuts) = (down.clone(), cut.clone());
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if is_down.load(Ordering::SeqCst) {
                    continue;
                }
                let target = target.clone();
                let mut cut = cuts.subscribe();
                tokio::spawn(async move {
                    let Ok(mut server) = tokio::net::TcpStream::connect(target).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = cut.changed() => {}
                    }
                });
            }
        });
        Self { url, down, cut }
    }

    fn cut(&self) {
        self.down.store(true, Ordering::SeqCst);
        self.cut.send_modify(|n| *n += 1);
    }

    fn restore(&self) {
        self.down.store(false, Ordering::SeqCst);
    }
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
    no_departures: bool,
}

impl Member {
    fn new(mut room: Room) -> Self {
        let events = room.take_events();
        Self {
            room,
            events,
            videos: HashMap::new(),
            no_departures: false,
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
            if self.no_departures {
                assert!(
                    !matches!(ev, RoomEvent::PeerLeft { .. }),
                    "nobody should leave"
                );
            }
        }
    }

    async fn chat(&mut self) -> (PeerId, String) {
        self.until(|e| match e {
            RoomEvent::Chat { from, text, .. } => Some((*from, text.clone())),
            _ => None,
        })
        .await
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

    // Alice was there first, so her app has Mallory removed from the relay.
    mallory
        .until(|e| matches!(e, RoomEvent::Removed).then_some(()))
        .await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_survive_the_relay_dropping() {
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&relay().await).await;
    let engine = Engine::new();

    let mut alice = Member::new(
        engine
            .create_room(settings(&proxy.url, "Alice"))
            .await
            .unwrap(),
    );
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&proxy.url, "Bob"), code)
            .await
            .unwrap(),
    );
    let old_bob = *alice.connected(1).await.keys().next().unwrap();
    let old_alice = *bob.connected(1).await.keys().next().unwrap();
    alice.no_departures = true;
    bob.no_departures = true;

    proxy.cut();
    for m in [&mut alice, &mut bob] {
        m.until(|e| matches!(e, RoomEvent::RelayLost).then_some(()))
            .await;
    }
    // Without the relay, the direct connection carries on.
    alice.room.send_chat("still here");
    assert_eq!(bob.chat().await, (old_alice, "still here".to_string()));

    proxy.restore();
    for (m, old) in [(&mut alice, old_bob), (&mut bob, old_alice)] {
        m.until(|e| matches!(e, RoomEvent::RelayRestored).then_some(()))
            .await;
        let replaced = m
            .until(|e| match e {
                RoomEvent::PeerVerified { replaces, .. } => Some(*replaces),
                _ => None,
            })
            .await;
        assert_eq!(replaced, Some(old), "the same friend, back under a new id");
    }
    let new_bob = *alice.connected(1).await.keys().next().unwrap();
    let new_alice = *bob.connected(1).await.keys().next().unwrap();
    assert_ne!(new_bob, old_bob);
    assert_ne!(new_alice, old_alice);

    bob.room.send_chat("back");
    assert_eq!(alice.chat().await, (new_bob, "back".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarting_ice_keeps_the_call() {
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
    alice.no_departures = true;
    bob.no_departures = true;

    alice.room.share_screen(Some(ScreenTarget::TestPattern));
    let track = bob.video(alice.room.me(), VideoKind::Screen).await;

    // Both sides at once, a few times over: the offerer restarts directly,
    // the answerer asks for it, and the requests cross.
    for _ in 0..3 {
        alice.room.restart_connections();
        bob.room.restart_connections();
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    alice.room.send_chat("after restart");
    assert_eq!(
        bob.chat().await,
        (alice.room.me(), "after restart".to_string())
    );
    let mut stream = NativeVideoStream::new(track);
    let mut frames = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while frames < 20 && Instant::now() < deadline {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            frames += 1;
        }
    }
    assert!(
        frames >= 20,
        "video stalled after the restart ({frames} frames)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn h264_is_negotiated_when_preferred() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    // What a machine with a hardware encoder picks on its own.
    std::env::set_var("SILENT_VIDEO_CODEC", "h264");
    let alice = engine.create_room(settings(&relay, "Alice")).await;
    std::env::remove_var("SILENT_VIDEO_CODEC");
    let mut alice = Member::new(alice.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );
    alice.connected(1).await;
    bob.connected(1).await;

    alice.room.set_camera(Some(CameraTarget::TestPattern));
    let track = bob.video(alice.room.me(), VideoKind::Camera).await;
    let mut stream = NativeVideoStream::new(track);
    let mut frames = 0;
    let deadline = Instant::now() + STEP;
    while frames < 30 && Instant::now() < deadline {
        if let Ok(Some(_)) = tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
            frames += 1;
        }
    }
    assert!(frames >= 30, "only {frames} frames");

    let sent = alice.room.stats().await;
    let camera = sent[0]
        .sending
        .iter()
        .find(|v| v.kind == Some(VideoKind::Camera))
        .expect("camera in outbound stats");
    println!("camera: {}", camera.summary());
    assert_eq!(camera.codec, "H264");
    let received = bob.room.stats().await;
    assert!(received[0].receiving.iter().any(|v| v.codec == "H264"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speaking_is_detected_and_friends_can_be_muted() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut beeping = settings(&relay, "Alice");
    beeping.audio = AudioInput::TestTone;
    let mut alice = Member::new(engine.create_room(beeping).await.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );
    alice.connected(1).await;
    bob.connected(1).await;
    let alice_id = alice.room.me();

    let speaking = |who: PeerId| {
        move |e: &RoomEvent| match e {
            RoomEvent::Speaking { peers } if peers.contains(&who) => Some(()),
            _ => None,
        }
    };
    bob.until(speaking(alice_id)).await;
    alice.until(speaking(alice_id)).await;

    // Muted for Bob, Alice no longer shows as speaking to him.
    bob.room.set_friend_muted(alice_id, true);
    bob.until(|e| match e {
        RoomEvent::Speaking { peers } if peers.is_empty() => Some(()),
        _ => None,
    })
    .await;
    let quiet = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(RoomEvent::Speaking { peers }) = bob.events.recv().await {
                if peers.contains(&alice_id) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "a muted friend must not show as speaking");

    // Alice mutes her mic: she stops showing as speaking to herself too.
    alice.room.set_mic(false);
    alice
        .until(|e| match e {
            RoomEvent::Speaking { peers } if !peers.contains(&alice_id) => Some(()),
            _ => None,
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_volume_changes_what_you_hear() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut beeping = settings(&relay, "Alice");
    beeping.audio = AudioInput::TestTone;
    let mut alice = Member::new(engine.create_room(beeping).await.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );
    alice.connected(1).await;
    bob.connected(1).await;
    let alice_id = alice.room.me();
    let speaking = |on: bool| {
        move |e: &RoomEvent| match e {
            RoomEvent::Speaking { peers } if peers.contains(&alice_id) == on => Some(()),
            _ => None,
        }
    };
    bob.until(speaking(true)).await;

    // The level Bob measures is after his volume: at zero she's silent.
    bob.room.set_friend_volume(alice_id, 0.0);
    bob.until(speaking(false)).await;
    let quiet = tokio::time::timeout(Duration::from_secs(2), bob.until(speaking(true))).await;
    assert!(quiet.is_err(), "a friend at volume 0 must be silent");

    // Turned far down she's quiet, but still shows as talking.
    bob.room.set_friend_volume(alice_id, 0.05);
    bob.until(speaking(true)).await;
}

/// Waits until `pick` finds what it wants in Bob's stats of Alice.
async fn until_stats(
    room: &Room,
    friend: PeerId,
    mut pick: impl FnMut(&silent_engine::stats::PeerStats) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let stats = room.stats().await;
        if stats.iter().any(|s| s.peer == friend && pick(s)) {
            return;
        }
        assert!(Instant::now() < deadline, "stats never matched");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn computer_audio_arrives_apart_from_the_voice() {
    let _serial = SERIAL.lock().await;
    let relay = relay().await;
    let engine = Engine::new();

    let mut sharing = settings(&relay, "Alice");
    sharing.audio = AudioInput::Silent;
    let mut alice = Member::new(engine.create_room(sharing).await.unwrap());
    let code = alice.room.code().clone();
    let mut bob = Member::new(
        engine
            .join_room(settings(&relay, "Bob"), code)
            .await
            .unwrap(),
    );
    alice.connected(1).await;
    bob.connected(1).await;
    let alice_id = alice.room.me();

    // A stereo tone as Alice's computer audio, 10 ms at a time.
    let input = alice.room.screen_audio_input();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let feeder = tokio::spawn(async move {
        let mut n = 0u64;
        while !stopped.load(Ordering::Relaxed) {
            let chunk: Vec<i16> = (0..480u64)
                .flat_map(|i| {
                    let t = (n + i) as f64 / 48_000.0;
                    let v = ((t * 330.0 * std::f64::consts::TAU).sin() * 9000.0) as i16;
                    [v, v]
                })
                .collect();
            n += 480;
            input.push(chunk);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    until_stats(&bob.room, alice_id, |s| {
        s.computer_audio_level.is_some_and(|l| l > 0.05)
    })
    .await;
    // Her microphone is silent, so she doesn't show as talking.
    let talking = tokio::time::timeout(
        Duration::from_secs(2),
        bob.until(|e| match e {
            RoomEvent::Speaking { peers } if peers.contains(&alice_id) => Some(()),
            _ => None,
        }),
    )
    .await;
    assert!(talking.is_err(), "computer audio must not count as talking");

    // Her volume covers her computer audio too.
    bob.room.set_friend_volume(alice_id, 0.0);
    until_stats(&bob.room, alice_id, |s| {
        s.computer_audio_level.is_some_and(|l| l < 0.001)
    })
    .await;

    stop.store(true, Ordering::Relaxed);
    feeder.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_arrive_whole_and_chat_keeps_flowing() {
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
    bob.until(|e| matches!(e, RoomEvent::PeerMedia { .. }).then_some(()))
        .await;

    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    alice.room.send_file("../holiday.bin", big.clone()).unwrap();
    alice.room.send_file("empty.txt", Vec::new()).unwrap();
    // Chat has its own channel, so it is not stuck behind the file.
    alice.room.send_chat("sent you a file");

    let mut chat_seen = false;
    let mut files = HashMap::new();
    let started = Instant::now();
    while files.len() < 2 {
        let event = bob
            .until(|e| match e {
                RoomEvent::FileReceived { name, data, .. } => {
                    Some(Some((name.clone(), data.clone())))
                }
                RoomEvent::Chat { text, .. } if text == "sent you a file" => Some(None),
                RoomEvent::FileFailed { .. } => panic!("transfer failed"),
                _ => None,
            })
            .await;
        match event {
            Some((name, data)) => {
                files.insert(name, data);
            }
            // The empty file may well come first; it's the big one chat
            // mustn't wait for.
            None => chat_seen = !files.contains_key("holiday.bin"),
        }
    }
    println!("3 MiB arrived in {:?}", started.elapsed());
    assert!(chat_seen, "chat should arrive before the big file finishes");
    assert_eq!(files["holiday.bin"], big, "file content must match");
    assert!(files["empty.txt"].is_empty());

    let too_big = vec![0u8; silent_core::proto::MAX_FILE_BYTES as usize + 1];
    assert!(alice.room.send_file("huge", too_big).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_files_at_once_all_arrive() {
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
    bob.until(|e| matches!(e, RoomEvent::PeerMedia { .. }).then_some(()))
        .await;

    // More than a receiver takes at once; the sender queues the rest.
    for i in 0..6u8 {
        let size = if i == 0 { 2 * 1024 * 1024 } else { 10_000 };
        alice
            .room
            .send_file(&format!("file{i}.bin"), vec![i; size])
            .unwrap();
    }
    let mut names = Vec::new();
    while names.len() < 6 {
        let (name, data) = bob
            .until(|e| match e {
                RoomEvent::FileReceived { name, data, .. } => Some((name.clone(), data.clone())),
                RoomEvent::FileFailed { .. } => panic!("transfer failed"),
                _ => None,
            })
            .await;
        let i: u8 = name[4..5].parse().unwrap();
        assert!(data.iter().all(|&b| b == i), "{name} content");
        names.push(name);
    }
    names.sort();
    assert_eq!(names.len(), 6);
}
