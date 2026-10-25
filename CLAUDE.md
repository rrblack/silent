# CLAUDE.md

Guidance for Claude working in this repo. Read [HANDOFF.md](HANDOFF.md) for the current state and what to do next. This file covers how the code is organized and the rules to keep.

## What Silent is

- Encrypted, account-free voice and video calls, screen sharing and chat for **up to 4 people**.
- Rooms are temporary, joined with a code like `4071-amber-falcon-river`. **Nothing is stored or logged**, anywhere.
- **Peer to peer:** a WebRTC mesh in which every pair connects directly, with TURN only as a fallback.
- **Targets:** Windows, macOS, Linux (desktop app), Android (built, untested on a device), iOS (skeleton, builds for the simulator in CI).
- **Owner:** Kyle (`rrblack` on GitHub), working on Windows with an Android phone. No Mac and no iPhone.

## Layout

| Path | What | Notes |
|---|---|---|
| `crates/silent-core` | Invite codes, SPAKE2 pair handshake, sealed envelopes, safety words, wire types | Pure, no I/O. Security-critical; every change needs tests. |
| `crates/silent-engine` | Room actor: relay client, per-peer `PeerConnection`, tracks, chat, stats, rendering helpers | Wraps LiveKit's `libwebrtc` crate (pinned `=0.3.52`, `webrtc-sys` locked at 0.3.49). |
| `crates/silent-app` | Desktop app (`silent` binary), Slint UI in `ui/app.slint` | Red and black palette in the `Palette` global. |
| `crates/silent-ffi` | UniFFI bindings for mobile, plus the JNI init in `src/android.rs` | Kotlin package `app.silent.ffi`; Swift module `SilentFFI` (configured in `uniffi.toml`). |
| `crates/uniffi-bindgen` | Binding generator binary | Kept separate so it doesn't build libwebrtc for the host. |
| `crates/silent-relay` | Rust signaling relay (dev, LAN, self-hosting) | Listens on `0.0.0.0:8787` by default. The desktop app also hosts one on loopback when none is running. |
| `signal/` | The same relay as a Cloudflare Worker with Durable Objects | Must keep the exact wire protocol of `silent-relay`. Not deployed yet. |
| `android/` | Kotlin + Jetpack Compose app, CameraX, MediaProjection | `build-native.sh` must run before Gradle. |
| `ios/` | SwiftUI app, AVFoundation camera | XcodeGen `project.yml`; `build-native.sh` makes the XCFramework and Swift bindings (Mac only). See `ios/README.md`. |
| `packaging/` | Icons, macOS `Info.plist`, Linux `.desktop`, `package.sh` | `package.sh` turns a release build into `dist/` (dmg, deb + tarball, zip). CI runs it. |
| `third_party/` | webrtc-sys 0.3.49 (log sink at warnings, not verbose; remote track volume) and libwebrtc 0.3.52 (exposes that volume) | Patched in via `[patch.crates-io]`. Read its README before upgrading libwebrtc. |
| `docs/SECURITY.md` | Protocol and threat model | Update it whenever the protocol changes. |

## How a call works (short version)

1. The relay puts peers in a room by nameplate (`4071`). The secret words never leave the device.
2. Every pair runs symmetric SPAKE2 with the code as the password, then HKDF derives per-direction ChaCha20-Poly1305 keys.
3. All signaling after SPAKE2 is sealed: `Hello{name}`, SDP, ICE. A wrong code shows up as a sealed message that fails to open.
4. The higher peer id sends the offer. DTLS is authenticated by the fingerprints in that sealed SDP, so media and chat are end to end.
5. Safety words come from the pair key plus both DTLS fingerprints.
6. A dropped connection is ICE-restarted by the offerer (the answerer asks with a sealed `Restart`). If the relay drops, calls continue peer to peer and the room rejoins the relay on its own; the sealed session id in `Hello` lets friends swap the old connection for the new pair (`PeerVerified { replaces }`). Event peer ids are local to the room (`room::Ids`), not relay ids.

## Commands

```sh
# Linux needs clang >= 21 for libwebrtc's prebuilt archive:
export CC=clang-21 CXX=clang++-21

cargo test --workspace                 # 61 tests incl. real in-process WebRTC calls (~35 s after first build)
cargo clippy --workspace --all-targets # must stay warning-free
cargo fmt --all
cargo run --release -p silent-app      # twice, to call yourself
cargo run -p silent-engine --example bot -- http://127.0.0.1:8787 <code> [file]   # a test participant: beeps, test pattern, echoes chat, sends a file; BOT_SCREEN=1 also shares a screen

cd signal && npx tsc --noEmit          # Worker typecheck
SILENT_TEST_RELAY=http://127.0.0.1:8787 cargo test -p silent-relay   # protocol tests against `wrangler dev`

# Android (needs NDK r28, cargo-ndk, aarch64-linux-android target)
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/28.2.13676358
android/build-native.sh && (cd android && ./gradlew assembleDebug)

# Packages (after cargo build --release -p silent-app -p silent-relay)
packaging/package.sh                   # dist/: dmg on macOS, deb + tarball on Linux, zip on Windows

# iOS (Mac only)
ios/build-native.sh && (cd ios && xcodegen generate)
```

Windows needs no extra setup beyond Rust plus VS Build Tools. `.cargo/config.toml` forces the static CRT, which the prebuilt libwebrtc requires.

CI (`.github/workflows/ci.yml`) runs:
- fmt, clippy, the core and relay tests
- the Worker protocol tests
- the engine call tests on Linux
- release builds and packages for Linux, Windows and macOS
- the Android APK

Every push to `main` uploads `silent-Windows`, `silent-macOS`, `silent-Linux` and `silent-android` artifacts. `.github/workflows/ios.yml` builds the iOS app for the simulator, only when `ios/` or `crates/silent-ffi` change (macOS minutes cost 10x on a private repo).

## Rules

- **Nothing on disk, nothing in logs.** Don't persist chat, codes, names or settings without the owner deciding to. Never log SDP, ICE candidates (they contain IPs), codes, names or chat text. Worker observability stays off.
- **Don't weaken the protocol for convenience.** No sending the secret words anywhere, no unsealed signaling, no skipping the strict counter check in `seal.rs`. Protocol changes bump `PROTOCOL_VERSION`, update `docs/SECURITY.md`, and land in both relays.
- **Both relays speak one protocol.** If you change `silent-core/src/proto.rs` or relay behavior, change `signal/src/index.ts` too and run the relay tests against `wrangler dev`.
- **Mobile captures, Rust encodes.** On Android and iOS the app pushes frames through `Room::video_input` (`CameraTarget::External` / `ScreenTarget::External`). Desktop capture code is behind `#[cfg(desktop)]`, set in `silent-engine/build.rs`.
- **Bindings follow the library.** After changing `silent-ffi`'s API, rerun `android/build-native.sh`. The Kotlin checksums must match the `.so`. Generated bindings and `jniLibs` are gitignored.
- **Verify before claiming.** There are in-process call tests for engine behavior; add one when you change room logic. For the desktop UI, the previous session drove two app instances on Xvfb with xdotool and screenshots. Hardware-dependent things (mic, camera, GPU encoders, phones) cannot be verified in a cloud container: say so.
- Match the surrounding style: doc comments on modules and public items, few inline comments, no dead code.

## libwebrtc gotchas (each one cost hours)

1. `OfferOptions::default()` sets `offer_to_receive_*: false`, which makes every transceiver send-only. Always pass `true`.
2. The **answerer** must call `add_track` **after** `set_remote_description(offer)` so tracks attach to the offer's transceivers. The **offerer** uses `add_transceiver(.., SendRecv)`.
3. `RtpSender::set_track(None)` **segfaults** (null dereference in webrtc-sys). Pause video with `encodings[].active = false` (`media::apply_encoding`).
4. `NativeVideoSource::new` sends 10 fps black keepalive frames until the first real frame. Unpaused, they get encoded once per connection and saturate the CPU.
5. `to_i420` and `to_argb` can't be called on `dyn VideoBuffer` from outside the crate. Downcast first (`render::any_to_i420`).
6. Linux links GLib/GIO for the Wayland portal (`silent-engine/build.rs`). Android must call `webrtc_sys_build::configure_jni_symbols()` (`silent-ffi/build.rs`) or libwebrtc's Java classes can't find their native methods.
7. The prebuilt Android `libwebrtc.a` defines `JNI_OnLoad`. Don't define another; init goes through `SilentNative.init(context)`.
8. Release builds strip symbols, which removes UniFFI's metadata. Mobile libraries use `--profile mobile` (`strip = "debuginfo"`).
9. Windows: the static CRT is required (`.cargo/config.toml`). macOS: needs a recent Xcode (CI picks the latest stable).
10. **Never drop a `PeerConnection`/`DataChannel` right after `close()`.** libwebrtc can still deliver queued callbacks (e.g. ICE state), and webrtc-sys frees the C++ observer with the last Rust handle, so the callback hits freed memory (intermittent SIGSEGV). `room::retire` clears handlers, closes, and parks handles for 10 s; `Peer`'s `Drop` goes through it. All engines share one process-wide factory for the same reason.
11. **Each side sends with the first codec the *other* side lists.** Codec preferences on your transceivers decide what your friend sends you, not what you send. That's why a device announces `send_h264` in its `Hello` and the receiver orders its codecs for it (`Actor::prefer_codecs`).
12. `ScrollView` content in Slint stretches to the viewport; give the inner layout `height: self.preferred-height`.
13. **Upstream webrtc-sys logs at verbose.** Every log line gets formatted and dcSCTP hex-dumps each data channel packet (CPU in every call, files under 1 MB/s). `third_party/webrtc-sys` patches the sink to warnings; keep that patch when upgrading.
14. Hardware encoders are compiled in only when their headers are present at build time: `libva-dev` for VAAPI on Linux (CI installs it), the CUDA toolkit for NVENC on Linux. There is no Windows hardware encoder in webrtc-sys.
15. **libwebrtc's task queues run at real-time priority** where allowed. Saturating the CPU with several video senders then starved its worker thread and leaked outgoing frames (GBs in minutes). `third_party/webrtc-sys` gives it a `FairTaskQueueFactory`; keep it when upgrading. To check: the desktop app plus three bots for several minutes, watching RSS (`ps -o rss=`).
