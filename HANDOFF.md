# Silent: handoff

The state of the project, how to pick it up, and what to do next. First written at the end of the first build session (October 2026), updated after the second (reconnecting, hardware video, desktop UI, installers, iOS skeleton). For Kyle and the next Claude session. Read [CLAUDE.md](CLAUDE.md) for the architecture and rules.

## 1. Where things stand

| Area | Built | Verified | Not verified yet |
|---|---|---|---|
| Crypto and protocol (`silent-core`, protocol v2) | ✅ | 24 unit tests | Independent security review |
| Rust relay (`silent-relay`) | ✅ | 6 protocol tests, incl. the join budget | |
| Cloudflare Worker relay (`signal/`) | ✅ | Same 6 tests against `wrangler dev`, plus all 7 call tests through it | Deployed? Check section 4 |
| Call engine (`silent-engine`) | ✅ | Real WebRTC calls in-process: 1:1 with chat and 1080p60 screen share, wrong-code isolation, 4-person mesh plus 5th turned away, rotated phone frames, **relay dropping mid-call and coming back** (call carries on, friends re-pair without a "left"), **ICE restart**, **H.264 negotiation** | Real networks, NAT, TURN, a real network switch |
| Desktop app (Slint, red and black) | ✅ | Two instances on a virtual display: join, test-pattern screen share, **auto-spotlight of a friend's screen, full screen (F11, Esc), Ctrl+D mute, Devices sheet, scrolling Settings** | Real webcam, mic, speakers, device switching, GPU encoding |
| Installers | ✅ | CI builds a Windows zip (exe with icon), macOS dmg (.app with permission texts, ad hoc signed), Linux .deb and tarball; the .deb was inspected locally | Installing them on real machines |
| Android app | ✅ | APK builds locally and in CI | **Never run on a phone** |
| iOS app | 🟡 skeleton | SwiftUI app, builds for the simulator in CI (see `ios/README.md`) | Never run; no screen sharing yet |

**Repo:** https://github.com/rrblack/silent (private), branch `main`.
**Builds:** https://github.com/rrblack/silent/actions. Open the newest run; the downloads are under Artifacts:
- `silent-Windows`: `Silent-0.1.0-windows.zip` with `Silent.exe` and `silent-relay.exe`.
- `silent-macOS`: `Silent-0.1.0-macOS.dmg`. Drag Silent to Applications.
- `silent-Linux`: a `.deb` (`sudo apt install ./silent_0.1.0_amd64.deb`) and a tarball with `silent` and `silent-relay`.
- `silent-android`: `app-debug.apk`.
- `silent-ios-simulator` (iOS workflow): the simulator build, only useful on a Mac.

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
1. Download `silent-Windows`, unzip it, and open `Silent.exe` twice.
   - Windows SmartScreen will warn about an unknown publisher: click **More info**, then **Run anyway**.
2. Window 1: type a name, click **Start a room**, then **Copy**. Window 2: paste the code and click **Join**.
3. Check each of these:
   - [ ] Both windows connect, and **Verify** shows the same four words in both.
   - [ ] Chat works both ways.
   - [ ] **Share screen → Test pattern**: Stats shows around 60 fps.
   - [ ] **Share screen** with a real screen, then a window.
   - [ ] **Camera** turns on and shows your webcam.
   - [ ] Mic works. Both windows share one mic and speaker, so expect feedback; headphones help.
   - [ ] **Stats** names the codec and encoder. On a PC with a GPU it should say `H264 via NVENC` (NVIDIA), `MediaFoundation` (AMD/Intel) or `VideoToolbox` (Mac). `VP8 via libvpx` or `OpenH264` means software. To compare, start the app with `SILENT_VIDEO_CODEC=vp8` or `h264`.
   - [ ] Click a tile: it fills the stage. A friend's screen share takes the stage by itself. Double-click or **F11** for full screen, **Esc** to come back.
   - [ ] **Ctrl+D** mutes (the tile label turns red on both sides), **Ctrl+E** toggles the camera.
   - [ ] **Devices**: pick another microphone or speaker mid-call; the other window should hear the change at once.

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
   - [ ] The call keeps going with the phone screen off or the app in the background, and after pressing Back.
   - [ ] The call notification's **Mute** and **Leave** buttons work.
   - [ ] The camera flip button switches between front and back.
   - [ ] **Reconnect:** turn the phone's Wi-Fi off mid-call (so it moves to mobile data). Expect "Reconnecting…" for a few seconds, then the call resumes. Needs the relay from section 4.
   - [ ] **Relay restart:** stop `silent-relay.exe` mid-call. The call keeps going; the app says the relay is offline. Start it again: within ~15 s the notice goes away and nobody shows as having left.

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
3. Give the Worker a way to read TURN usage, so it can stop handing out TURN credentials before the free 1,000 GB a month runs out. In the Cloudflare dashboard, create an API token (My Profile → API Tokens → Create Custom Token) with **Account → Account Analytics → Read**, and copy your Account ID from the account home page:
   ```powershell
   npx wrangler secret put CF_ACCOUNT_ID
   npx wrangler secret put CF_ANALYTICS_TOKEN
   ```
   Without these, the Worker hands out no TURN credentials. The cap is `TURN_CAP_GB` (900) in `wrangler.toml`, measured over the last 31 days. Credentials already handed out last up to 4 hours, so the 100 GB margin covers calls that are still running when the cap is reached.

**Bake the URL into the builds** so nobody has to type it: on GitHub, open the repo's Settings → Secrets and variables → Actions → Variables, and add `SILENT_RELAY_URL` with the Worker's URL. The desktop, Android and iOS builds all pick it up on their next run.

## 5. Known gaps and quirks

- **No settings are saved.** The name, relay URL and device choices must be redone on every launch, because of the "nothing stored" rule. It's the owner's call whether to remember them locally.
- **Builds aren't signed.** Windows SmartScreen warns. On macOS the dmg's app is only ad hoc signed: right-click → Open the first time, or `xattr -dr com.apple.quarantine /Applications/Silent.app`.
- **CPU-based video rendering.** Fine for tiles; a GPU path is needed for several 1080p60 streams at once.
- **Hardware encoding is untested.** The app asks for H.264 and the hardware encoder when libwebrtc reports one, with VP8 in software as the fallback. Nobody has looked at Stats on a real GPU yet.
- **Rejoining after a relay outage starts every pair over.** New keys mean new safety words; compare them again if it matters.
- **Room squatting and join spam.** Someone with a wrong code can hold one of the 4 slots (they can't see or hear anything). Both relays limit join attempts per room, which also means a join spammer can keep dropped friends from rejoining for a minute. See `docs/SECURITY.md`.
- **Android:** first run on a device is untested. Remote video is capped at 30 fps and 960 px for camera tiles, by design for battery.
- **iOS:** a skeleton that only builds for the simulator. No screen sharing, no CallKit.

## 6. Next steps, in order

1. **Hardware test pass** (section 3), then fix what breaks. This is still the most valuable next step: everything since the first session was verified only in the cloud container.
2. **Deploy the relay** (section 4) if it isn't yet, and set `SILENT_RELAY_URL` as an Actions variable.
3. **Check hardware encoding** from the Stats sheet on a real GPU and a real phone. If it still says software, look at `media::CodecChoice` and `VideoEncoderBackend::list_available()`.
4. **Per-friend volume** and a speaking indicator (audio levels are in the inbound audio stats).
5. **Desktop GPU rendering:** Slint wgpu interop with an I420 shader.
6. **Signing and updates:**
   - Windows signing (Azure Trusted Signing)
   - Apple Developer ID plus notarization for the dmg
   - auto-updates (Velopack or similar), once builds are signed
7. **iOS:** run it on a Mac and an iPhone, then add the ReplayKit Broadcast Upload Extension for screen sharing and CallKit. Needs an **Apple Developer account ($99/yr)**.
8. **Room squatting mitigation** and an **independent security review** before inviting people beyond close friends.

## 7. Decisions waiting on you

| Decision | Options | Why it matters |
|---|---|---|
| License | none (private), MIT/Apache, AGPL | Needed before making the repo public. |
| Relay URL | free `*.workers.dev` or a custom domain | Gets baked into every build. |
| Remember settings locally? | no (current) / name, relay URL and device choices only | Convenience vs. the "nothing stored" promise. Not built, on purpose, until you decide. |
| Code signing | Azure Trusted Signing (~$10/mo), Apple Developer ($99/yr) | Removes install warnings; Apple's is required for iOS. |
| Public or private | | CI minutes: private repos get 2,000 free minutes a month, and each macOS minute counts as 10. Every push to `main` builds the macOS app (~5 min, so ~50 minutes of the budget); the iOS workflow only runs when `ios/` or `crates/silent-ffi` change. Public repos build for free. |

## 8. How to start the next Claude session

Open this repo in a Claude project and paste:

> Read HANDOFF.md and CLAUDE.md. I'm about to test Silent on my Windows PC and Android phone following section 3. Help me through it step by step, and fix whatever breaks.

Or, to skip testing for now:

> Read HANDOFF.md and CLAUDE.md, then deploy the relay (section 4) with me and bake the URL into the CI builds.
