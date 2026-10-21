#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod video;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use silent_core::proto::MediaState;
use silent_core::InviteCode;
use silent_engine::libwebrtc::video_track::RtcVideoTrack;
use silent_engine::{
    camera, capture, AudioInput, CameraProfile, CameraTarget, Engine, PeerId, Room, RoomError,
    RoomEvent, ScreenProfile, ScreenTarget, Settings, VideoKind,
};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};
use tokio::task::JoinHandle;

slint::include_modules!();

const DEFAULT_RELAY: &str = match option_env!("SILENT_RELAY_URL") {
    Some(url) if !url.is_empty() => url,
    _ => "http://127.0.0.1:8787",
};

/// Everything that lives on the tokio side.
struct Backend {
    rt: tokio::runtime::Runtime,
    engine: Engine,
    room: Mutex<Option<Room>>,
    pumps: Mutex<HashMap<String, JoinHandle<()>>>,
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

fn backend() -> &'static Backend {
    BACKEND.get().expect("backend initialised in main")
}

fn with_room(f: impl FnOnce(&Room)) {
    if let Some(room) = backend().room.lock().unwrap().as_ref() {
        f(room);
    }
}

fn tile_key(peer: PeerId, kind: VideoKind) -> String {
    match kind {
        VideoKind::Camera => format!("{peer}:camera"),
        VideoKind::Screen => format!("{peer}:screen"),
    }
}

fn start_pump(peer: PeerId, kind: VideoKind, track: RtcVideoTrack) {
    let key = tile_key(peer, kind);
    let (max_w, max_h) = match kind {
        VideoKind::Camera => (1280, 720),
        VideoKind::Screen => (1920, 1080),
    };
    let ui_key = key.clone();
    let handle = backend()
        .rt
        .spawn(video::pump(track, max_w, max_h, move |pixels| {
            ui(|s| s.set_frame(&ui_key, Image::from_rgba8(pixels)))
        }));
    if let Some(old) = backend().pumps.lock().unwrap().insert(key, handle) {
        old.abort();
    }
}

fn stop_pumps(peer: Option<PeerId>) {
    let prefix = peer.map(|p| format!("{p}:"));
    backend().pumps.lock().unwrap().retain(|key, handle| {
        let matches = prefix.as_ref().is_none_or(|p| key.starts_with(p.as_str()));
        if matches {
            handle.abort();
        }
        !matches
    });
}

struct PeerView {
    name: String,
    status: &'static str,
    media: MediaState,
    words: Option<String>,
    verified: bool,
}

/// Everything that lives on the UI thread.
struct UiState {
    window: slint::Weak<AppWindow>,
    tiles: Rc<VecModel<Tile>>,
    chat: Rc<VecModel<ChatLine>>,
    verifications: Rc<VecModel<Verification>>,
    me: PeerId,
    my_name: String,
    created: bool,
    local: MediaState,
    peers: BTreeMap<PeerId, PeerView>,
    frames: HashMap<String, Image>,
}

thread_local! {
    static UI: RefCell<Option<UiState>> = const { RefCell::new(None) };
}

fn ui(f: impl FnOnce(&mut UiState)) {
    UI.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            f(state);
        }
    });
}

fn initial(name: &str) -> SharedString {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_default()
        .into()
}

impl UiState {
    fn window(&self) -> Option<AppWindow> {
        self.window.upgrade()
    }

    fn tile(
        &self,
        peer: PeerId,
        kind: VideoKind,
        name: &str,
        mic: bool,
        has_video: bool,
        status: &str,
    ) -> Tile {
        let key = tile_key(peer, kind);
        Tile {
            frame: self.frames.get(&key).cloned().unwrap_or_default(),
            has_video: has_video && self.frames.contains_key(&key),
            key: key.into(),
            name: name.into(),
            initial: initial(name),
            kind: match kind {
                VideoKind::Camera => "camera".into(),
                VideoKind::Screen => "screen".into(),
            },
            mic,
            status: status.into(),
            is_me: peer == self.me,
        }
    }

    fn rebuild(&mut self) {
        let me = self.me;
        let mut tiles = vec![self.tile(
            me,
            VideoKind::Camera,
            &self.my_name,
            self.local.mic,
            self.local.camera,
            "",
        )];
        for (&id, p) in &self.peers {
            tiles.push(self.tile(
                id,
                VideoKind::Camera,
                &p.name,
                p.media.mic,
                p.media.camera,
                p.status,
            ));
        }
        if self.local.screen {
            tiles.push(self.tile(me, VideoKind::Screen, &self.my_name, true, true, ""));
        }
        for (&id, p) in &self.peers {
            if p.media.screen {
                tiles.push(self.tile(id, VideoKind::Screen, &p.name, true, true, ""));
            }
        }
        self.tiles.set_vec(tiles);

        let verifications: Vec<Verification> = self
            .peers
            .values()
            .filter_map(|p| {
                let words = p.words.as_ref()?;
                Some(Verification {
                    name: p.name.as_str().into(),
                    words: words.as_str().into(),
                })
            })
            .collect();
        self.verifications.set_vec(verifications);

        if let Some(w) = self.window() {
            w.set_mic_on(self.local.mic);
            w.set_camera_on(self.local.camera);
            w.set_screen_on(self.local.screen);
        }
    }

    fn set_frame(&mut self, key: &str, image: Image) {
        if self.frames.insert(key.to_string(), image.clone()).is_none() {
            self.rebuild();
            return;
        }
        for i in 0..self.tiles.row_count() {
            if let Some(mut t) = self.tiles.row_data(i) {
                if t.key == key {
                    t.frame = image;
                    self.tiles.set_row_data(i, t);
                    return;
                }
            }
        }
    }

    fn system(&self, text: impl Into<SharedString>) {
        self.chat.push(ChatLine {
            system: true,
            text: text.into(),
            ..Default::default()
        });
    }

    fn apply(&mut self, event: RoomEvent) {
        match event {
            RoomEvent::PeerJoining { peer } => {
                let view = PeerView {
                    name: "Friend".into(),
                    status: "Connecting…",
                    media: MediaState::default(),
                    words: None,
                    verified: false,
                };
                self.peers.insert(peer, view);
            }
            RoomEvent::PeerVerified { peer, name } => {
                self.system(format!("{name} joined"));
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.name = name;
                    p.status = "Securing connection…";
                    p.verified = true;
                }
            }
            RoomEvent::PeerWrongCode { peer } => {
                self.peers.remove(&peer);
                if self.created {
                    self.system("Someone tried to join with the wrong code and was kept out.");
                } else if self.peers.values().all(|p| !p.verified) {
                    let all_rejected = self.peers.is_empty();
                    if all_rejected {
                        // Runs after this borrow of the UI state ends.
                        slint::Timer::single_shot(Duration::ZERO, || {
                            leave_room(Some(
                                "That code doesn't match the room. Check the words and try again.",
                            ))
                        });
                        return;
                    }
                }
            }
            RoomEvent::PeerConnected { peer, safety_words } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.status = "";
                    p.words = Some(safety_words.join(" "));
                }
            }
            RoomEvent::PeerConnectionLost { peer } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.status = "Connection lost";
                }
            }
            RoomEvent::PeerLeft { peer } => {
                if let Some(p) = self.peers.remove(&peer) {
                    self.system(format!("{} left", p.name));
                }
                let prefix = format!("{peer}:");
                self.frames.retain(|k, _| !k.starts_with(&prefix));
            }
            RoomEvent::PeerMedia { peer, state } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.media = state;
                }
                if !state.camera {
                    self.frames.remove(&tile_key(peer, VideoKind::Camera));
                }
                if !state.screen {
                    self.frames.remove(&tile_key(peer, VideoKind::Screen));
                }
            }
            RoomEvent::Chat { from, text, .. } => {
                let who = self
                    .peers
                    .get(&from)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "Friend".into());
                self.chat.push(ChatLine {
                    who: who.into(),
                    text: text.into(),
                    ..Default::default()
                });
            }
            RoomEvent::RemoteVideo { .. } => {}
            RoomEvent::LocalMediaError { kind, message } => {
                match kind {
                    VideoKind::Camera => self.local.camera = false,
                    VideoKind::Screen => self.local.screen = false,
                }
                self.system(message);
            }
            RoomEvent::RelayLost => {
                if let Some(w) = self.window() {
                    w.set_notice("Relay offline: the call continues, nobody new can join".into());
                }
            }
        }
        self.rebuild();
    }
}

fn settings_from(w: &AppWindow) -> Settings {
    Settings {
        relay_url: w.get_relay_url().trim().to_string(),
        name: w.get_name().to_string(),
        audio: if std::env::var_os("SILENT_NO_AUDIO").is_some() {
            AudioInput::Silent
        } else {
            AudioInput::Platform
        },
        camera: if w.get_camera_quality() == 1 {
            CameraProfile::High
        } else {
            CameraProfile::Standard
        },
        screen: if w.get_screen_quality() == 1 {
            ScreenProfile::Detail
        } else {
            ScreenProfile::Motion
        },
        relay_only: w.get_relay_only(),
    }
}

fn room_error_text(e: &RoomError) -> String {
    match e {
        RoomError::NoSuchRoom => {
            "No room is open with that number. Ask your friend for a fresh code.".into()
        }
        RoomError::RoomFull => "That room already has four people.".into(),
        RoomError::Signal(e) => format!("Couldn't reach the relay ({e})."),
    }
}

fn open_room(window: slint::Weak<AppWindow>, settings: Settings, join: Option<InviteCode>) {
    let my_name = silent_core::proto::clean_name(&settings.name);
    backend().rt.spawn(async move {
        let created = join.is_none();
        let result = match join {
            None => backend().engine.create_room(settings).await,
            Some(code) => backend().engine.join_room(settings, code).await,
        };
        let mut room = match result {
            Ok(room) => room,
            Err(e) => {
                let text = room_error_text(&e);
                let _ = window.upgrade_in_event_loop(move |w| {
                    w.set_busy(false);
                    w.set_home_error(text.into());
                });
                return;
            }
        };

        let me = room.me();
        let code = room.code().to_string();
        let has_mic = room.has_microphone();
        let mut events = room.take_events();
        start_pump(me, VideoKind::Camera, room.local_video(VideoKind::Camera));
        start_pump(me, VideoKind::Screen, room.local_video(VideoKind::Screen));
        *backend().room.lock().unwrap() = Some(room);

        let win = window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = win.upgrade() else { return };
            let state = UiState {
                window: win.clone(),
                tiles: Rc::new(VecModel::default()),
                chat: Rc::new(VecModel::default()),
                verifications: Rc::new(VecModel::default()),
                me,
                my_name,
                created,
                local: MediaState {
                    mic: true,
                    camera: false,
                    screen: false,
                },
                peers: BTreeMap::new(),
                frames: HashMap::new(),
            };
            w.set_tiles(ModelRc::from(state.tiles.clone()));
            w.set_chat(ModelRc::from(state.chat.clone()));
            w.set_verifications(ModelRc::from(state.verifications.clone()));
            w.set_code(code.into());
            w.set_notice(if has_mic {
                "".into()
            } else {
                "No microphone found".into()
            });
            w.set_busy(false);
            w.set_home_error("".into());
            w.set_screen("room".into());
            UI.with(|cell| *cell.borrow_mut() = Some(state));
            ui(|s| {
                if created {
                    s.system("Room created. Share the code above with up to three friends.");
                }
                s.rebuild();
            });
        });

        while let Some(event) = events.recv().await {
            match &event {
                RoomEvent::RemoteVideo { peer, kind, track } => {
                    start_pump(*peer, *kind, track.clone())
                }
                RoomEvent::PeerLeft { peer } => stop_pumps(Some(*peer)),
                _ => {}
            }
            let _ = slint::invoke_from_event_loop(move || ui(|s| s.apply(event)));
        }
    });
}

/// Leaves the current room and returns to the home screen, optionally with
/// an error message. Call from the UI thread.
fn leave_room(error: Option<&str>) {
    let room = backend().room.lock().unwrap().take();
    stop_pumps(None);
    if let Some(room) = room {
        backend().rt.spawn(room.leave());
    }
    let window = UI.with(|cell| cell.borrow_mut().take().and_then(|s| s.window()));
    if let Some(w) = window {
        w.set_screen("home".into());
        w.set_picker_open(false);
        w.set_verify_open(false);
        w.set_notice("".into());
        w.set_draft("".into());
        w.set_home_error(error.unwrap_or("").into());
    }
}

fn describe(v: &silent_engine::stats::VideoStats) -> String {
    let what = match v.kind {
        Some(VideoKind::Camera) => "camera",
        Some(VideoKind::Screen) => "screen",
        None => "video",
    };
    let mut line = format!(
        "{what} {}x{} @ {:.0} fps, {}",
        v.width, v.height, v.fps, v.implementation
    );
    if v.target_bitrate_bps > 0.0 {
        line.push_str(&format!(", {:.1} Mbps", v.target_bitrate_bps / 1e6));
    }
    if !v.limited_by.is_empty() && v.limited_by != "none" {
        line.push_str(&format!(", limited by {}", v.limited_by));
    }
    line
}

fn refresh_stats(window: slint::Weak<AppWindow>) {
    let mut request = None;
    with_room(|r| request = Some(r.stats()));
    let Some(request) = request else { return };
    backend().rt.spawn(async move {
        let stats = request.await;
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = window.upgrade() else { return };
            let mut lines: Vec<SharedString> = Vec::new();
            UI.with(|cell| {
                let cell = cell.borrow();
                let names = cell.as_ref().map(|s| &s.peers);
                for p in &stats {
                    let name = names
                        .and_then(|n| n.get(&p.peer))
                        .map(|v| v.name.as_str())
                        .unwrap_or("Friend");
                    let rtt = p
                        .rtt_ms
                        .map(|r| format!(" (round trip {r:.0} ms)"))
                        .unwrap_or_default();
                    lines.push(format!("{name}{rtt}").into());
                    for v in &p.sending {
                        lines.push(format!("    you send: {}", describe(v)).into());
                    }
                    for v in &p.receiving {
                        lines.push(format!("    you get: {}", describe(v)).into());
                    }
                }
            });
            if lines.is_empty() {
                lines.push("No video flowing yet.".into());
            }
            w.set_stats_lines(ModelRc::new(VecModel::from(lines)));
        });
    });
}

/// When the relay is on this machine and nothing is serving it yet, the
/// first app instance hosts it. Two windows on one computer can then call
/// each other with nothing else running. Only binds to loopback.
fn start_local_relay(relay_url: &str) {
    let Some(port) = relay_url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| relay_url.strip_prefix("http://localhost:"))
        .and_then(|p| p.trim_end_matches('/').parse::<u16>().ok())
    else {
        return;
    };
    backend().rt.spawn(async move {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            tracing::info!("hosting local relay on 127.0.0.1:{port}");
            let _ = silent_relay::serve(listener, Default::default()).await;
        }
    });
}

fn test_patterns() -> bool {
    std::env::var_os("SILENT_TEST_PATTERN").is_some()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SILENT_LOG")
                .unwrap_or_else(|_| "warn,libwebrtc::imp::video_stream=error".into()),
        )
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let engine = rt.block_on(async { Engine::new() });
    let state = Backend {
        rt,
        engine,
        room: Mutex::new(None),
        pumps: Mutex::new(HashMap::new()),
    };
    if BACKEND.set(state).is_err() {
        unreachable!("main runs once");
    }
    camera::request_permission(|granted| {
        if !granted {
            tracing::warn!("camera access was not granted");
        }
    });

    let window = AppWindow::new()?;
    let relay = std::env::var("SILENT_RELAY_URL").unwrap_or_else(|_| DEFAULT_RELAY.into());
    start_local_relay(&relay);
    window.set_relay_url(relay.into());

    let weak = window.as_weak();
    window.on_create_room(move || {
        let Some(w) = weak.upgrade() else { return };
        w.set_busy(true);
        w.set_home_error("".into());
        open_room(weak.clone(), settings_from(&w), None);
    });

    let weak = window.as_weak();
    window.on_join_room(move || {
        let Some(w) = weak.upgrade() else { return };
        match w.get_join_code().parse::<InviteCode>() {
            Ok(code) => {
                w.set_busy(true);
                w.set_home_error("".into());
                open_room(weak.clone(), settings_from(&w), Some(code));
            }
            Err(e) => {
                w.set_home_error(format!("That doesn't look like a Silent code: {e}.").into())
            }
        }
    });

    window.on_toggle_mic(|| {
        ui(|s| {
            s.local.mic = !s.local.mic;
            let on = s.local.mic;
            with_room(|r| r.set_mic(on));
            s.rebuild();
        })
    });

    window.on_toggle_camera(|| {
        let turning_on = UI.with(|c| c.borrow().as_ref().is_some_and(|s| !s.local.camera));
        if !turning_on {
            with_room(|r| r.set_camera(None));
            ui(|s| {
                s.local.camera = false;
                s.frames.remove(&tile_key(s.me, VideoKind::Camera));
                s.rebuild();
            });
            return;
        }
        let target = if test_patterns() {
            Some(CameraTarget::TestPattern)
        } else {
            camera::list_cameras()
                .first()
                .map(|c| CameraTarget::Device(c.index))
        };
        match target {
            Some(target) => {
                with_room(|r| r.set_camera(Some(target)));
                ui(|s| {
                    s.local.camera = true;
                    s.rebuild();
                });
            }
            None => ui(|s| s.system("No camera found.")),
        }
    });

    let weak = window.as_weak();
    window.on_toggle_screen(move || {
        let Some(w) = weak.upgrade() else { return };
        if w.get_screen_on() {
            with_room(|r| r.share_screen(None));
            ui(|s| {
                s.local.screen = false;
                s.frames.remove(&tile_key(s.me, VideoKind::Screen));
                s.rebuild();
            });
            return;
        }
        let mut sources = Vec::new();
        if capture::has_system_picker() {
            sources.push(SourceItem {
                id: "picker".into(),
                title: "Choose with the system picker…".into(),
            });
        }
        for s in capture::list_sources(capture::SourceKind::Screen) {
            sources.push(SourceItem {
                id: format!("s:{}", s.id).into(),
                title: format!("Screen: {}", s.title).into(),
            });
        }
        for s in capture::list_sources(capture::SourceKind::Window) {
            if !s.title.trim().is_empty() {
                sources.push(SourceItem {
                    id: format!("w:{}", s.id).into(),
                    title: s.title.into(),
                });
            }
        }
        sources.push(SourceItem {
            id: "test".into(),
            title: "Test pattern (diagnostics)".into(),
        });
        w.set_sources(ModelRc::new(VecModel::from(sources)));
        w.set_picker_open(true);
    });

    window.on_pick_source(|id| {
        let target = match id.as_str() {
            "picker" => Some(ScreenTarget::SystemPicker),
            "test" => Some(ScreenTarget::TestPattern),
            other => match other.split_once(':') {
                Some(("s", n)) => n.parse().ok().map(ScreenTarget::Screen),
                Some(("w", n)) => n.parse().ok().map(ScreenTarget::Window),
                _ => None,
            },
        };
        if let Some(target) = target {
            with_room(|r| r.share_screen(Some(target)));
            ui(|s| {
                s.local.screen = true;
                s.rebuild();
            });
        }
    });

    window.on_leave(|| leave_room(None));

    let weak = window.as_weak();
    window.on_copy_code(move || {
        let Some(w) = weak.upgrade() else { return };
        let code = w.get_code().to_string();
        let copied = arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(code))
            .is_ok();
        w.set_copied(copied);
        let weak = weak.clone();
        slint::Timer::single_shot(Duration::from_secs(2), move || {
            if let Some(w) = weak.upgrade() {
                w.set_copied(false);
            }
        });
    });

    let weak = window.as_weak();
    window.on_send_chat(move || {
        let Some(w) = weak.upgrade() else { return };
        let text = w.get_draft().trim().to_string();
        if text.is_empty() {
            return;
        }
        with_room(|r| r.send_chat(text.clone()));
        ui(|s| {
            let who: SharedString = s.my_name.as_str().into();
            s.chat.push(ChatLine {
                who,
                text: text.into(),
                mine: true,
                ..Default::default()
            });
        });
        w.set_draft("".into());
    });

    let weak = window.as_weak();
    let stats_timer = Rc::new(slint::Timer::default());
    let timer = stats_timer.clone();
    window.on_stats_opened(move || {
        let weak = weak.clone();
        refresh_stats(weak.clone());
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_secs(1),
            move || {
                if weak.upgrade().is_some_and(|w| w.get_stats_open()) {
                    refresh_stats(weak.clone());
                }
            },
        );
    });

    window.run()?;
    drop(stats_timer);

    if let Some(room) = backend().room.lock().unwrap().take() {
        let _ = backend()
            .rt
            .block_on(tokio::time::timeout(Duration::from_secs(2), room.leave()));
    }
    Ok(())
}
