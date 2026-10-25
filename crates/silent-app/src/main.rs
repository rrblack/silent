#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod files;
mod video;

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use silent_core::proto::MediaState;
use silent_core::InviteCode;
use silent_engine::libwebrtc::video_track::RtcVideoTrack;
use silent_engine::{
    camera, capture, AudioDevice, AudioInput, CameraProfile, CameraTarget, Engine, PeerId, Room,
    RoomError, RoomEvent, ScreenProfile, ScreenTarget, Settings, VideoKind,
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
    /// Bumped whenever a room is opened or left, so a room's late events
    /// and file dialogs can tell they are no longer current.
    generation: AtomicU64,
    pumps: Mutex<HashMap<String, JoinHandle<()>>>,
}

fn generation() -> u64 {
    backend().generation.load(Ordering::SeqCst)
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
    muted_for_me: bool,
    /// Their volume for you, in percent.
    volume: i32,
    /// The user said the safety words matched.
    confirmed: bool,
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
    /// Key of the spotlighted tile.
    focus: Option<String>,
    /// Friends' screen shares already on screen, so only new ones take the
    /// spotlight by themselves.
    screens_seen: HashSet<String>,
    speaking: HashSet<PeerId>,
    /// Received files, by chat line key, until the room is left.
    files: HashMap<String, (String, Vec<u8>)>,
    sent_files: u32,
}

thread_local! {
    static UI: RefCell<Option<UiState>> = const { RefCell::new(None) };
    static DEVICES: RefCell<Devices> = RefCell::new(Devices::default());
}

/// The devices the pickers list, after their first "default" entry, and the
/// choice last applied. Kept in memory only.
#[derive(Default)]
struct Devices {
    microphones: Vec<AudioDevice>,
    speakers: Vec<AudioDevice>,
    cameras: Vec<camera::CameraDevice>,
    microphone: Option<String>,
    speaker: Option<String>,
    camera: Option<u32>,
}

impl Devices {
    /// The camera to open: the chosen one, else the first.
    fn camera_target(&self) -> Option<CameraTarget> {
        if test_patterns() {
            return Some(CameraTarget::TestPattern);
        }
        match self.camera {
            Some(index) => Some(CameraTarget::Device(index)),
            None => camera::list_cameras()
                .first()
                .map(|c| CameraTarget::Device(c.index)),
        }
    }
}

/// Lists devices off the UI thread (opening the audio module can take a
/// moment), then fills the pickers, keeping the current choices selected.
fn refresh_devices(window: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let audio = backend().engine.audio_devices();
        let cameras = camera::list_cameras();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = window.upgrade() else { return };
            DEVICES.with(|d| {
                let mut d = d.borrow_mut();
                let position = |list: &[AudioDevice], id: &Option<String>| {
                    id.as_ref()
                        .and_then(|id| list.iter().position(|x| &x.id == id))
                        .map_or(0, |i| i as i32 + 1)
                };
                let names = |first: &str, rest: Vec<String>| {
                    let mut all: Vec<SharedString> = vec![first.into()];
                    all.extend(rest.into_iter().map(SharedString::from));
                    ModelRc::new(VecModel::from(all))
                };
                w.set_microphones(names(
                    "System default",
                    audio.microphones.iter().map(|m| m.name.clone()).collect(),
                ));
                w.set_speakers(names(
                    "System default",
                    audio.speakers.iter().map(|m| m.name.clone()).collect(),
                ));
                w.set_cameras(names(
                    "First camera",
                    cameras.iter().map(|c| c.name.clone()).collect(),
                ));
                w.set_microphone(position(&audio.microphones, &d.microphone));
                w.set_speaker(position(&audio.speakers, &d.speaker));
                w.set_camera(
                    d.camera
                        .and_then(|i| cameras.iter().position(|c| c.index == i))
                        .map_or(0, |i| i as i32 + 1),
                );
                d.microphones = audio.microphones;
                d.speakers = audio.speakers;
                d.cameras = cameras;
            });
        });
    });
}

/// Applies what the pickers show. Audio switches at once; a camera that is
/// on restarts with the new device.
fn apply_devices(w: &AppWindow) {
    let pick = |index: i32| usize::try_from(index - 1).ok();
    let (microphone, speaker, camera_changed) = DEVICES.with(|d| {
        let mut d = d.borrow_mut();
        let microphone =
            pick(w.get_microphone()).and_then(|i| d.microphones.get(i).map(|m| m.id.clone()));
        let speaker = pick(w.get_speaker()).and_then(|i| d.speakers.get(i).map(|m| m.id.clone()));
        let camera = pick(w.get_camera()).and_then(|i| d.cameras.get(i).map(|c| c.index));
        let changed = (
            (d.microphone != microphone).then(|| microphone.clone()),
            (d.speaker != speaker).then(|| speaker.clone()),
            d.camera != camera,
        );
        d.microphone = microphone;
        d.speaker = speaker;
        d.camera = camera;
        changed
    });
    if microphone.is_some() || speaker.is_some() {
        std::thread::spawn(move || {
            if let Some(id) = microphone {
                backend().engine.choose_microphone(id);
            }
            if let Some(id) = speaker {
                backend().engine.choose_speaker(id);
            }
        });
    }
    let camera_on = UI.with(|c| c.borrow().as_ref().is_some_and(|s| s.local.camera));
    if camera_changed && camera_on {
        let target = DEVICES.with(|d| d.borrow().camera_target());
        with_room(|r| r.set_camera(target));
    }
}

fn ui(f: impl FnOnce(&mut UiState)) {
    UI.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            f(state);
        }
    });
}

/// The local time of day for a chat line, like "14:05".
fn clock() -> SharedString {
    chrono::Local::now().format("%H:%M").to_string().into()
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
            confirmed: self.peers.get(&peer).is_some_and(|p| p.confirmed),
            speaking: kind == VideoKind::Camera && self.speaking.contains(&peer),
            muted_for_me: self.peers.get(&peer).is_some_and(|p| p.muted_for_me),
            volume: self.peers.get(&peer).map_or(100, |p| p.volume),
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
        let screens: HashSet<String> = tiles
            .iter()
            .filter(|t| t.kind == "screen" && !t.is_me)
            .map(|t| t.key.to_string())
            .collect();
        if self.focus.is_none() {
            self.focus = screens.difference(&self.screens_seen).next().cloned();
        }
        self.screens_seen = screens;
        let focus_index = self
            .focus
            .as_ref()
            .and_then(|key| tiles.iter().position(|t| t.key == key.as_str()));
        if focus_index.is_none() {
            self.focus = None;
        }
        self.tiles.set_vec(tiles);

        let verifications: Vec<Verification> = self
            .peers
            .iter()
            .filter_map(|(&id, p)| {
                let words = p.words.as_ref()?;
                Some(Verification {
                    peer: id as i32,
                    name: p.name.as_str().into(),
                    words: words.as_str().into(),
                    confirmed: p.confirmed,
                })
            })
            .collect();
        self.verifications.set_vec(verifications);

        if let Some(w) = self.window() {
            w.set_focus_index(focus_index.map_or(-1, |i| i as i32));
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

    /// Updates the chat line of file `key`.
    fn file_line(&self, key: &str, update: impl FnOnce(&mut ChatLine)) {
        for i in (0..self.chat.row_count()).rev() {
            if let Some(mut line) = self.chat.row_data(i) {
                if line.file == key {
                    update(&mut line);
                    self.chat.set_row_data(i, line);
                    return;
                }
            }
        }
    }

    /// Decodes an image preview off the UI thread and shows it on line `key`.
    fn show_preview(&self, key: String, data: Vec<u8>) {
        backend().rt.spawn_blocking(move || {
            if let Some(pixels) = files::preview(&data) {
                let _ = slint::invoke_from_event_loop(move || {
                    ui(|s| {
                        s.file_line(&key, |line| {
                            line.image = Image::from_rgba8(pixels);
                            line.has_image = true;
                        })
                    })
                });
            }
        });
    }

    /// Flashes the taskbar entry (bounces the Dock icon on macOS) if the
    /// window is in the background, for a message or someone joining.
    fn ask_attention(&self) {
        use slint::winit_030::{winit::window::UserAttentionType, WinitWindowAccessor};
        if let Some(window) = self.window.upgrade() {
            window.window().with_winit_window(|w| {
                if !w.has_focus() {
                    w.request_user_attention(Some(UserAttentionType::Informational));
                }
            });
        }
    }

    fn friend_name(&self, peer: PeerId) -> String {
        self.peers
            .get(&peer)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Friend".into())
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
                    muted_for_me: false,
                    volume: 100,
                    confirmed: false,
                };
                self.peers.insert(peer, view);
            }
            RoomEvent::PeerVerified {
                peer,
                name,
                replaces,
            } => {
                let returning = replaces.and_then(|old| self.peers.remove(&old));
                if let Some(old) = replaces {
                    let prefix = format!("{old}:");
                    self.frames.retain(|k, _| !k.starts_with(&prefix));
                }
                if returning.is_none() {
                    self.system(format!("{name} joined"));
                    self.ask_attention();
                }
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.name = name;
                    p.status = "Securing connection…";
                    p.verified = true;
                    // The engine keeps a friend's mute and volume across a reconnect.
                    p.muted_for_me = returning.as_ref().is_some_and(|old| old.muted_for_me);
                    p.volume = returning.as_ref().map_or(100, |old| old.volume);
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
                    p.status = "Reconnecting…";
                }
            }
            RoomEvent::PeerLeft { peer } => {
                if let Some(p) = self.peers.remove(&peer).filter(|p| p.verified) {
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
            RoomEvent::FileIncoming {
                from,
                id,
                name,
                size,
            } => {
                self.chat.push(ChatLine {
                    who: self.friend_name(from).into(),
                    time: clock(),
                    text: format!(
                        "File: {name} ({}) · receiving…",
                        files::size_text(size as usize)
                    )
                    .into(),
                    file: format!("{from}:{id}").into(),
                    ..Default::default()
                });
                self.ask_attention();
            }
            RoomEvent::FileReceived {
                from,
                id,
                name,
                data,
            } => {
                let key = format!("{from}:{id}");
                let text = format!("File: {name} ({})", files::size_text(data.len()));
                self.file_line(&key, |line| {
                    line.text = text.into();
                    line.file_ready = true;
                });
                self.show_preview(key.clone(), data.clone());
                self.files.insert(key, (name, data));
            }
            RoomEvent::FileFailed { from, id } => {
                self.file_line(&format!("{from}:{id}"), |line| {
                    line.text = format!(
                        "{} · didn't arrive",
                        line.text.trim_end_matches(" · receiving…")
                    )
                    .into();
                });
            }
            RoomEvent::Chat { from, text, .. } => {
                let who = self.friend_name(from);
                self.chat.push(ChatLine {
                    who: who.into(),
                    time: clock(),
                    text: text.into(),
                    ..Default::default()
                });
                self.ask_attention();
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
                    w.set_notice("Relay offline: the call continues, reconnecting…".into());
                }
            }
            RoomEvent::Speaking { peers } => {
                self.speaking = peers.into_iter().collect();
            }
            RoomEvent::Removed => {
                // Runs after this borrow of the UI state ends.
                slint::Timer::single_shot(Duration::ZERO, || {
                    leave_room(Some(
                        "You were removed from the room: your code doesn't match. Check the words and try again.",
                    ))
                });
                return;
            }
            RoomEvent::RelayRestored => {
                if let Some(w) = self
                    .window()
                    .filter(|w| w.get_notice().starts_with("Relay"))
                {
                    w.set_notice("".into());
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
        audio: match (
            std::env::var_os("SILENT_NO_AUDIO").is_some(),
            test_patterns(),
        ) {
            (true, true) => AudioInput::TestTone,
            (true, false) => AudioInput::Silent,
            (false, _) => AudioInput::Platform,
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
        RoomError::TooManyJoins => {
            "Too many attempts to join that room. Wait a minute and try again.".into()
        }
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
        let mine = backend().generation.fetch_add(1, Ordering::SeqCst) + 1;

        let win = window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = win.upgrade() else { return };
            // With push to talk, the microphone is off until Space is held.
            let push_to_talk = w.get_push_to_talk();
            if push_to_talk {
                with_room(|r| r.set_mic(false));
            }
            let state = UiState {
                window: win.clone(),
                tiles: Rc::new(VecModel::default()),
                chat: Rc::new(VecModel::default()),
                verifications: Rc::new(VecModel::default()),
                me,
                my_name,
                created,
                local: MediaState {
                    mic: !push_to_talk,
                    camera: false,
                    screen: false,
                },
                peers: BTreeMap::new(),
                frames: HashMap::new(),
                focus: None,
                screens_seen: HashSet::new(),
                speaking: HashSet::new(),
                files: HashMap::new(),
                sent_files: 0,
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
            if generation() != mine {
                break;
            }
            match &event {
                RoomEvent::RemoteVideo { peer, kind, track } => {
                    start_pump(*peer, *kind, track.clone())
                }
                RoomEvent::PeerLeft { peer }
                | RoomEvent::PeerVerified {
                    replaces: Some(peer),
                    ..
                } => stop_pumps(Some(*peer)),
                _ => {}
            }
            let _ = slint::invoke_from_event_loop(move || {
                if generation() == mine {
                    ui(|s| s.apply(event))
                }
            });
        }
    });
}

/// Leaves the current room and returns to the home screen, optionally with
/// an error message. Call from the UI thread.
fn leave_room(error: Option<&str>) {
    backend().generation.fetch_add(1, Ordering::SeqCst);
    let room = backend().room.lock().unwrap().take();
    stop_pumps(None);
    if let Some(room) = room {
        backend().rt.spawn(room.leave());
    }
    let window = UI.with(|cell| cell.borrow_mut().take().and_then(|s| s.window()));
    if let Some(w) = window {
        w.set_screen("home".into());
        w.window().set_fullscreen(false);
        w.set_focus_index(-1);
        w.set_picker_open(false);
        w.set_verify_open(false);
        w.set_devices_open(false);
        w.set_stats_open(false);
        w.set_notice("".into());
        w.set_draft("".into());
        w.set_home_error(error.unwrap_or("").into());
    }
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
                        lines.push(format!("    you send: {}", v.summary()).into());
                    }
                    for v in &p.receiving {
                        lines.push(format!("    you get: {}", v.summary()).into());
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
        generation: AtomicU64::new(0),
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
        if w.get_busy() || backend().room.lock().unwrap().is_some() {
            return;
        }
        w.set_busy(true);
        w.set_home_error("".into());
        open_room(weak.clone(), settings_from(&w), None);
    });

    let weak = window.as_weak();
    window.on_join_room(move || {
        let Some(w) = weak.upgrade() else { return };
        if w.get_busy() || backend().room.lock().unwrap().is_some() {
            return;
        }
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
        let target = DEVICES.with(|d| d.borrow().camera_target());
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
    window.on_devices_requested(move || refresh_devices(weak.clone()));

    let weak = window.as_weak();
    window.on_devices_changed(move || {
        if let Some(w) = weak.upgrade() {
            apply_devices(&w);
        }
    });

    window.on_push_to_talk_held(|held| {
        ui(|s| {
            if s.local.mic != held {
                s.local.mic = held;
                with_room(|r| r.set_mic(held));
                s.rebuild();
            }
        })
    });

    window.on_confirm_words(|peer| {
        ui(|s| {
            if let Some(p) = s.peers.get_mut(&(peer as PeerId)) {
                p.confirmed = !p.confirmed;
            }
            s.rebuild();
        })
    });

    window.on_tile_clicked(|key| {
        ui(|s| {
            s.focus = (s.focus.as_deref() != Some(key.as_str())).then(|| key.to_string());
            s.rebuild();
        })
    });

    window.on_mute_friend(|key| {
        let Some(peer) = key.split(':').next().and_then(|p| p.parse::<PeerId>().ok()) else {
            return;
        };
        ui(|s| {
            if let Some(p) = s.peers.get_mut(&peer) {
                p.muted_for_me = !p.muted_for_me;
                let muted = p.muted_for_me;
                with_room(|r| r.set_friend_muted(peer, muted));
            }
            s.rebuild();
        })
    });

    window.on_friend_volume(|key, volume| {
        let Some(peer) = key.split(':').next().and_then(|p| p.parse::<PeerId>().ok()) else {
            return;
        };
        ui(|s| {
            if let Some(p) = s.peers.get_mut(&peer) {
                // The slider shows the new value itself; rebuilding the tiles
                // mid-drag would end the drag.
                if p.volume != volume {
                    p.volume = volume;
                    with_room(|r| r.set_friend_volume(peer, f64::from(volume) / 100.0));
                }
            }
        })
    });

    window.on_unfocus_tile(|| {
        ui(|s| {
            s.focus = None;
            s.rebuild();
        })
    });

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
                time: clock(),
                text: text.into(),
                mine: true,
                ..Default::default()
            });
        });
        w.set_draft("".into());
    });

    window.on_send_file(|| {
        let picked_in = generation();
        backend().rt.spawn(async move {
            let Some((name, data)) = files::pick().await else {
                return;
            };
            // The room may have been left (or another joined) meanwhile.
            if generation() != picked_in {
                return;
            }
            let size = data.len();
            let mut sent = None;
            with_room(|r| sent = Some(r.send_file(&name, data.clone())));
            let _ = slint::invoke_from_event_loop(move || {
                ui(|s| match sent {
                    Some(Ok(())) => {
                        s.sent_files += 1;
                        let key = format!("me:{}", s.sent_files);
                        s.chat.push(ChatLine {
                            who: s.my_name.as_str().into(),
                            time: clock(),
                            text: format!("File: {name} ({})", files::size_text(size)).into(),
                            mine: true,
                            file: key.as_str().into(),
                            file_ready: true,
                            ..Default::default()
                        });
                        s.show_preview(key, data);
                    }
                    Some(Err(e)) => s.system(format!("Couldn't send {name}: {e}.")),
                    None => {}
                })
            });
        });
    });

    window.on_save_file(|key| {
        let Some((name, data)) = UI.with(|c| {
            c.borrow()
                .as_ref()
                .and_then(|s| s.files.get(key.as_str()).cloned())
        }) else {
            return;
        };
        backend().rt.spawn(async move {
            let result = files::save(name.clone(), data).await;
            let _ = slint::invoke_from_event_loop(move || {
                ui(|s| match result {
                    Ok(true) => s.system(format!("Saved {name}.")),
                    Ok(false) => {}
                    Err(e) => s.system(format!("Couldn't save {name}: {e}.")),
                })
            });
        });
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
