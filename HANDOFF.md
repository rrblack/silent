# Silent: handoff for hardware testing

This file is for the next Claude session (on a desktop machine) and for whoever is testing.
It covers what is built and verified, how to run it, and exactly what needs real hardware.

## Where things stand

**Built and verified in a Linux cloud container (no GPU, camera, microphone or speakers):**

| Area | Status | How it was verified |
|---|---|---|
| Invite codes, SPAKE2, sealed envelopes, safety words | Done | 23 unit tests (`silent-core`) |
| Rust relay | Done | 5 protocol tests (`silent-relay/tests/relay.rs`) |
| Cloudflare Worker relay | Done, not deployed | The same 5 tests pass against `wrangler dev`; the call tests also pass through it |
| 1:1 call: connect, verify code, chat, screen share | Done | `two_friends_connect_chat_and_share_screen`: 1080p60 test pattern received at **~55-58 fps**; sender stats `1920x1080 @ 60.0 fps via libvpx` |
| Wrong code is detected and kept out | Done | `wrong_code_is_detected_and_isolated` and the UI flow |
| 4-person full mesh, 5th turned away | Done | `four_friends_full_mesh_and_fifth_is_turned_away` |
| Desktop app UI | Done | Two app instances driven with xdotool on Xvfb: create, copy code, join, camera (test pattern), real X11 screen capture, chat, Verify (matching words on both sides), Stats, wrong-code error, leave |
| Webcam capture (nokhwa) | Written, **never run with a camera** | Compiles on Linux only |
| Microphone, speakers, echo cancellation | Written, **never run with audio devices** | Falls back to silence when no device is present |
| Hardware video encoding | **Unknown** | Only software (libvpx) available in the container |
| Windows and macOS builds | **Never compiled** | CI workflow written but never run (no GitHub repo yet) |

## Running it

```sh
# Linux only: libwebrtc's prebuilt archive needs clang >= 21
export CC=clang-21 CXX=clang++-21

cargo test --workspace                    # ~10 s after the first build
cargo run -p silent-relay                 # relay on 127.0.0.1:8787
cargo run --release -p silent-app         # run twice to call yourself
```

The app's default relay is `http://127.0.0.1:8787`. To test across machines, run the relay on one of them:

```sh
SILENT_RELAY_ADDR=0.0.0.0:8787 cargo run -p silent-relay
```

Then set Settings → Relay server to `http://<that machine's LAN IP>:8787` on the others. For different networks, deploy the Worker (below).

Useful environment variables:

- `SILENT_TEST_PATTERN=1`: the camera sends a moving 60 fps pattern.
- `SILENT_NO_AUDIO=1`: don't open audio devices.
- `SILENT_LOG=debug,silent_engine=trace`: verbose logs.
- `SILENT_RELAY_URL=...`: default relay.

The screen share picker always includes **"Test pattern (diagnostics)"**. Use it to separate encoder and network problems from capture problems.

The **Stats** button in a call shows, per friend:

- resolution and fps sent and received
- the encoder and decoder names
- target bitrate
- what limits quality (`cpu` or `bandwidth`)
- round-trip time

## Test plan (needs real machines)

Report the Stats sheet text and anything odd for each step.

### 1. Builds
- [ ] Windows 11: `cargo build --release -p silent-app`. This is the first Windows build ever. webrtc-sys may need a specific MSVC or Windows SDK version; follow its error messages.
- [ ] macOS (Apple Silicon): same.
- [ ] Linux (Ubuntu 24.04 desktop): same, with clang 21.

### 2. Same machine, two instances
- [ ] Start a room, copy the code, join from the second instance. Both show the same safety words under Verify.
- [ ] Chat both ways.
- [ ] Screen share → "Test pattern (diagnostics)". Expect ~60 fps received in Stats.
- [ ] Screen share → a real screen and then a window:
  - **macOS:** expect the Screen Recording permission prompt, then a restart of the app.
  - **Wayland:** use "Choose with the system picker…" (portal dialog).
  - **Windows:** no prompt.
- [ ] Camera on. **macOS:** camera permission prompt on first launch.

### 3. Two machines, same network
- [ ] Voice both ways. Check for echo with speakers (not headphones) on both ends; WebRTC's echo canceller should handle it.
- [ ] Mute, then unmute.
- [ ] 1080p60 screen share of a game or video (Settings → Screen share → Motion). In Stats, check:
  - Is `you send` around 60 fps?
  - What encoder does it name: `libvpx`/`OpenH264` (software), or `NVENC`/`VideoToolbox`/`MediaFoundation` (hardware)?
  - Does it say "limited by cpu"?
- [ ] The Detail profile with text and code: is it sharp?
- [ ] Camera at High (1080p60).

### 4. Different networks (needs the Worker deployed, ideally with TURN)
- [ ] Friend on another home network or mobile hotspot connects.
- [ ] "Hide my IP from friends" on: the call still connects (needs TURN credentials on the Worker).

### 5. Four people
- [ ] 4 machines (or 2 machines × 2 instances). All tiles show, Verify lists 3 friends each.
- [ ] CPU usage with everyone's camera on plus one 1080p60 screen share.

## Deploying the relay

```sh
cd signal && npm ci
npx wrangler login
npx wrangler deploy                       # prints https://silent-signal.<account>.workers.dev
# optional TURN (Cloudflare dashboard → Realtime → TURN → create key):
npx wrangler secret put TURN_KEY_ID
npx wrangler secret put TURN_KEY_API_TOKEN
```

Then build the app with that URL as the default:

```sh
SILENT_RELAY_URL=https://silent-signal.<account>.workers.dev cargo build --release -p silent-app
```

The TURN endpoint in `signal/src/index.ts` is `POST https://rtc.live.cloudflare.com/v1/turn/keys/{id}/credentials/generate-ice-servers`, written from memory of Cloudflare's docs. Check it against current docs when enabling TURN. The parser accepts both the array and the object response shape.

## libwebrtc binding gotchas found the hard way

The crate is `libwebrtc 0.3.52` (LiveKit, pinned with `=`). These are worth knowing before touching `crates/silent-engine/src/room.rs`:

1. **`OfferOptions::default()` disables receiving.** Its `offer_to_receive_*: false` turns every transceiver send-only. Always pass `true`.
2. **The answerer must add its tracks *after* `set_remote_description(offer)`.** Only then are they attached to the offer's transceivers. The offerer uses `add_transceiver(.., SendRecv)`.
3. **`RtpSender::set_track(None)` segfaults** (webrtc-sys dereferences the null track). To stop video, set `encodings[].active = false` instead (`media::apply_encoding`).
4. **`NativeVideoSource::new` feeds 10 fps black keepalive frames** until the first real frame. They were encoded on every connection and saturated 4 cores in a 4-person test, hence point 3.
5. **`to_i420` and `to_argb` can't be called on `dyn VideoBuffer`** from outside the crate. Downcast with `as_i420()`, `as_nv12()` and so on (`silent-app/src/video.rs`).
6. **The Linux link needs GLib/GIO** for the Wayland portal (`silent-engine/build.rs`).

## Next steps, roughly in order

1. **Get it on GitHub.** This session couldn't create the repo (see below). Push, then let CI produce Windows and macOS builds.
2. **Hardware test pass**, the test plan above.
3. **Hardware encoder and codec choice.** If Stats shows software encoding on capable GPUs, prefer H.264 via `RtpTransceiver::set_codec_preferences` and try `RtpSender::set_video_encoder_backend(Hardware)`. Encoding once and sending to all peers (`NativeVideoSource::new_encoded`) is a later optimization.
4. **Device pickers:** microphone and speaker selection (factory `set_recording_device`/`set_playout_device`), camera selection, per-friend volume.
5. **Rendering:** frames are converted to RGBA on the CPU and uploaded by Slint. Fine for 720p tiles; for 1080p60 several times over, move to a GPU path (Slint wgpu interop with an I420 shader).
6. **Resilience:** ICE restart on network change (`PeerConnectionLost` currently just shows a status).
7. **Screen share audio** (system audio capture).
8. **Packaging:** Velopack for installers and signed auto-updates, Apple Developer ID plus notarization, Windows code signing (Azure Trusted Signing), AppImage or Flatpak.
9. **Room squatting mitigation** and an **independent security review** (see docs/SECURITY.md).

## Decisions waiting on the owner

- **Repository:** create `rrblack/silent` (private) on GitHub. This session's GitHub integration lacked permission to create repos.
- **License:** none set yet (`Cargo.toml` has no `license`).
- **Relay domain:** a `workers.dev` URL or a custom domain.
- **Local settings:** whether to remember the display name and quality settings between launches. Currently nothing is written to disk.
- **Signing accounts:** Apple Developer ($99/yr) and Windows code signing, needed before sharing builds with friends.
