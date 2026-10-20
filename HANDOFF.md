# Silent: handoff

The state of the project, how to pick it up, and what to do next. Written at the end of the first build session (October 2026) for Kyle and the next Claude session. Read [CLAUDE.md](CLAUDE.md) for the architecture and rules.

## 1. Where things stand

| Area | Built | Verified | Not verified yet |
|---|---|---|---|
| Crypto and protocol (`silent-core`) | ✅ | 23 unit tests | Independent security review |
| Rust relay (`silent-relay`) | ✅ | 5 protocol tests | |
| Cloudflare Worker relay (`signal/`) | ✅ | Same 5 tests against `wrangler dev`, plus all call tests through it | **Not deployed** |
| Call engine (`silent-engine`) | ✅ | Real WebRTC calls in-process: 1:1 with chat and 1080p60 screen share (received at ~55-59 fps), wrong-code isolation, 4-person mesh plus 5th turned away, rotated phone frames arriving upright | Real networks, NAT, TURN |
| Desktop app (Slint, red and black) | ✅ | Two instances driven on a virtual display: create, copy code, join, camera test pattern, real X11 screen capture, chat, Verify (matching words), Stats, wrong-code error, leave | Real webcam, mic, speakers, echo cancellation, GPU encoding |
| Windows, macOS and Linux builds | ✅ | CI green on all three; artifacts download | Running on real Windows and Mac machines |
| Android app | ✅ | APK builds in CI; the mobile API passes an end-to-end host test | **Never run on a phone** |
| iOS app | ❌ | | Not started (no Mac, no iPhone yet) |

**Repo:** https://github.com/rrblack/silent (private), branch `main`.
**Builds:** https://github.com/rrblack/silent/actions. Open the newest run; the downloads are under Artifacts.

## 2. Getting the code onto your PC

You need:
- **Rust:** https://rustup.rs
- **Git**
- **Visual Studio 2022 Build Tools** with "Desktop development with C++"
- **Node 22**, only if you'll touch the Worker

```powershell
git clone https://github.com/rrblack/silent
cd silent
cargo test --workspace            # first build downloads libwebrtc (~200 MB) and takes a while
cargo run --release -p silent-app
```

For Android work, also install Android Studio. Through it (SDK Manager), install SDK Platform 35 and NDK 28.2.13676358. Then run:

```powershell
rustup target add aarch64-linux-android
cargo install cargo-ndk
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\28.2.13676358"
bash android/build-native.sh      # Git Bash or WSL; builds the Rust engine for Android
cd android; .\gradlew assembleDebug
```

You can skip all of that and just download `silent-android` from CI.

## 3. Testing (do this first; this is where real bugs will show up)

Every check below needs real hardware. When something is off, send the **Stats** sheet text, a screenshot, and for Android the output of `adb logcat -s silent AndroidRuntime`.

### A. One PC, two windows
1. Download `silent-Windows`, unzip it, and open `silent.exe` twice.
   - Windows SmartScreen will warn about an unknown publisher: click **More info**, then **Run anyway**.
2. Window 1: type a name, click **Start a room**, then **Copy**. Window 2: paste the code and click **Join**.
3. Check each of these:
   - [ ] Both windows connect, and **Verify** shows the same four words in both.
   - [ ] Chat works both ways.
   - [ ] **Share screen → Test pattern**: Stats shows around 60 fps.
   - [ ] **Share screen** with a real screen, then a window.
   - [ ] **Camera** turns on and shows your webcam.
   - [ ] Mic works. Both windows share one mic and speaker, so expect feedback; headphones help.
   - [ ] **Stats** names the encoder. `NVENC`, `MediaFoundation` or `VideoToolbox` means hardware; `libvpx` or `OpenH264` means software.

### B. Two devices on the same Wi-Fi (PC + Android phone)
1. On the PC, run `silent-relay.exe`. Allow it through Windows Firewall, then find the PC's IP with `ipconfig` (for example `192.168.1.23`).
2. **PC app:** set Settings → Relay server to `http://127.0.0.1:8787`, then **Start a room**.
3. **Phone:** install `app-debug.apk` and grant the camera, microphone and notification permissions. Set Settings → Relay server to `http://192.168.1.23:8787`, then join with the code.
4. Check each of these:
   - [ ] Voice both ways, with no echo on speakers.
   - [ ] The phone camera shows upright on the PC.
   - [ ] Screen share works in both directions.
   - [ ] Chat works.
   - [ ] Verify words match on both screens.
   - [ ] Stats on the phone names the encoder (`c2.*` or `MediaCodec` means hardware).
   - [ ] The call keeps going with the phone screen off or the app in the background.

### C. Over the internet (after section 4)
- [ ] A friend on another network joins.
- [ ] With "Hide my IP" on, the call still connects. This needs TURN.
- [ ] Four people at once: check CPU and battery.

## 4. Deploying the relay (5 minutes, free)

Calls between different networks need the relay online. You already have a Cloudflare account from your portfolio.

```powershell
cd signal
npm ci
npx wrangler login
npx wrangler deploy        # prints https://silent-signal.<your-account>.workers.dev
```

**Optional TURN**, so calls work through strict networks and "Hide my IP" works:
1. In the Cloudflare dashboard, go to Realtime → TURN → Create key.
2. Store the key in the Worker:
   ```powershell
   npx wrangler secret put TURN_KEY_ID
   npx wrangler secret put TURN_KEY_API_TOKEN
   ```
3. The TURN endpoint in `signal/src/index.ts` was written from memory of Cloudflare's docs. Check it against the current docs when you enable TURN.

**Bake the URL into the builds** so nobody has to type it. Ask Claude to add the URL as `SILENT_RELAY_URL` in `ci.yml` for the desktop builds, and as `-PsilentRelayUrl=` for the Android build.

## 5. Known gaps and quirks

- **No settings are saved.** The name and relay URL must be retyped on every launch, because of the "nothing stored" rule. It's the owner's call whether to remember them locally.
- **Builds aren't signed.** Windows SmartScreen warns, and macOS needs `xattr -d com.apple.quarantine silent`.
- **CPU-based video rendering.** Fine for tiles; a GPU path is needed for several 1080p60 streams at once.
- **No device pickers.** The app uses the default mic, speaker and first camera.
- **No reconnect.** A dropped network connection shows "Connection lost" instead of recovering.
- **Room squatting.** Someone with a wrong code can hold one of the 4 slots (they can't see or hear anything). See `docs/SECURITY.md`.
- **Android:** first run on a device is untested. Remote video is capped at 30 fps and 960 px for camera tiles, by design for battery.

## 6. Next steps, in order

1. **Hardware test pass** (section 3), then fix what breaks. This is the most valuable next step.
2. **Deploy the relay** (section 4) and bake its URL into the builds.
3. **Hardware encoding.** If Stats shows software encoding on a capable GPU or phone, prefer H.264 (`RtpTransceiver::set_codec_preferences`) and try `RtpSender::set_video_encoder_backend(Hardware)`.
4. **Device pickers:** mic, speaker and camera selection, plus per-friend volume.
5. **Reconnect:** an ICE restart when the network changes.
6. **Desktop GPU rendering:** Slint wgpu interop with an I420 shader.
7. **Packaging:**
   - Velopack installers with signed auto-updates
   - Windows signing (Azure Trusted Signing)
   - Apple Developer ID plus notarization
   - AppImage or Flatpak for Linux
8. **iOS.** Reuse `silent-ffi` with Swift bindings (`uniffi-bindgen generate --language swift`) and build a SwiftUI app, mirroring `android/`:
   - camera through AVFoundation
   - screen sharing through a ReplayKit Broadcast Upload Extension (a separate process with a 50 MB memory limit)
   - CallKit for background audio

   It needs a Mac (CI's macOS runners can build it) and an **Apple Developer account ($99/yr)** to install on any iPhone.
9. **Room squatting mitigation** and an **independent security review** before inviting people beyond close friends.

## 7. Decisions waiting on you

| Decision | Options | Why it matters |
|---|---|---|
| License | none (private), MIT/Apache, AGPL | Needed before making the repo public. |
| Relay URL | free `*.workers.dev` or a custom domain | Gets baked into every build. |
| Remember settings locally? | no (current) / name + relay URL only | Convenience vs. the "nothing stored" promise. |
| Code signing | Azure Trusted Signing (~$10/mo), Apple Developer ($99/yr) | Removes install warnings; Apple's is required for iOS. |
| Public or private | | CI minutes: private repos get 2,000 free minutes a month, and each macOS minute counts as 10. |

## 8. How to start the next Claude session

Open this repo in a Claude project and paste:

> Read HANDOFF.md and CLAUDE.md. I'm about to test Silent on my Windows PC and Android phone following section 3. Help me through it step by step, and fix whatever breaks.

Or, to skip testing for now:

> Read HANDOFF.md and CLAUDE.md, then deploy the relay (section 4) with me and bake the URL into the CI builds.
