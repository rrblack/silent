# Silent

Encrypted voice and video calls, 60 fps screen sharing and chat for up to four friends.
No accounts, nothing stored. Share a code like `4071-amber-falcon-river` and you're in a temporary room.

- **Peer to peer.** Every friend gets a direct WebRTC connection (TURN only as a fallback, or on request to hide your IP).
- **End to end.** Codes are never sent anywhere. Peers prove they know the code with a PAKE (SPAKE2), seal all signaling with the resulting keys, and use that to authenticate the DTLS media connection. The relay only ever sees a room number and ciphertext. Safety words let friends confirm nobody is in the middle.
- **Native.** Rust and libwebrtc, with a Slint UI. Windows, macOS and Linux.

See [docs/SECURITY.md](docs/SECURITY.md) for the protocol and threat model, and [HANDOFF.md](HANDOFF.md) for the current state and the testing plan.

## Layout

| Path | What |
|---|---|
| `crates/silent-core` | Invite codes, SPAKE2 pair handshake, sealed envelopes, safety words, wire types. Pure, no I/O. |
| `crates/silent-engine` | Rooms: relay client, per-peer WebRTC connections, audio/camera/screen tracks, chat, stats. |
| `crates/silent-app` | The desktop app (`silent` binary), Slint UI in `ui/app.slint`. |
| `crates/silent-ffi` | UniFFI bindings of the engine for the mobile apps (Kotlin now, Swift later). |
| `crates/uniffi-bindgen` | Generates those bindings from a built library. |
| `android/` | Android app: Kotlin + Jetpack Compose, CameraX, MediaProjection. |
| `crates/silent-relay` | Signaling relay in Rust, for local development and self-hosting. |
| `signal/` | The same relay as a Cloudflare Worker with Durable Objects, for production. |

## Building

You need Rust (stable), plus:

- **Windows:** Visual Studio 2022 Build Tools with "Desktop development with C++".
- **macOS:** Xcode Command Line Tools.
- **Linux:** clang 21 or newer (the prebuilt libwebrtc requires it) and the usual desktop dev packages. The CI workflow lists them. Export `CC=clang-21 CXX=clang++-21` before building.

```sh
cargo build --release -p silent-app        # the app: target/release/silent
cargo test --workspace                     # unit, relay and in-process call tests
```

## Running locally

```sh
cargo run --release -p silent-app          # start two of these to call yourself
```

The first window hosts a local relay on 127.0.0.1:8787 if nothing else is serving it. For calls between machines, run `SILENT_RELAY_ADDR=0.0.0.0:8787 cargo run -p silent-relay` on one of them, or deploy the Worker.


| Environment variable | Effect |
|---|---|
| `SILENT_RELAY_URL` | Relay to use (also shown under Settings). Set at build time to change the default. |
| `SILENT_TEST_PATTERN=1` | Camera button sends a moving test pattern instead of the webcam. |
| `SILENT_NO_AUDIO=1` | Don't open audio devices; send silence. |
| `SILENT_LOG=debug` | Verbose logs to stderr (nothing is written to disk). |

## Android

Needs the Android SDK (platform 35), NDK r28, `rustup target add aarch64-linux-android` and `cargo install cargo-ndk`.

```sh
export ANDROID_NDK_HOME=$ANDROID_HOME/ndk/28.2.13676358
android/build-native.sh                    # Rust engine, Kotlin bindings, libwebrtc.jar
cd android && ./gradlew assembleDebug      # app/build/outputs/apk/debug/app-debug.apk
```

Pass `-PsilentRelayUrl=https://...` to Gradle to bake in a default relay. CI builds the APK on every push (artifact `silent-android`).

## Relay (Cloudflare Worker)

```sh
cd signal
npm ci
npx wrangler dev                           # local, same protocol as silent-relay
npx wrangler deploy                        # production
npx wrangler secret put TURN_KEY_ID        # optional: Cloudflare Realtime TURN
npx wrangler secret put TURN_KEY_API_TOKEN
```
