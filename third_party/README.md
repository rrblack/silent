# Third-party code

## webrtc-sys

[webrtc-sys](https://crates.io/crates/webrtc-sys) 0.3.49 from LiveKit
(Apache-2.0, see `webrtc-sys/NOTICE.md`), patched in through
`[patch.crates-io]` in the workspace `Cargo.toml`. Two changes.

The log level, in `src/webrtc.cpp`:

```diff
-  webrtc::LogMessage::AddLogToStream(this, webrtc::LoggingSeverity::LS_VERBOSE);
+  webrtc::LogMessage::AddLogToStream(this, webrtc::LoggingSeverity::LS_WARNING);
```

The upstream log sink asks libwebrtc for verbose logs, so every log line in
libwebrtc is formatted and handed to Rust, and dcSCTP hex-dumps every data
channel packet. That kept file transfers under 1 MB/s and cost CPU in every
call. As of webrtc-sys 0.3.51 upstream still hardcodes verbose.

`AudioTrack::set_volume` (`src/audio_track.rs`, `src/audio_track.cpp`,
`include/livekit/audio_track.h`): sets the playback volume of a remote track
through `AudioSourceInterface::SetVolume`, for the per-friend volume.

## libwebrtc

[libwebrtc](https://crates.io/crates/libwebrtc) 0.3.52 from LiveKit
(Apache-2.0), the Rust wrapper over webrtc-sys, patched in the same way. One
change: `RtcAudioTrack::set_volume` (`src/audio_track.rs`,
`src/native/audio_track.rs`), which calls the webrtc-sys method above.

## Upgrading

Copy the new crates here and reapply the changes, or drop the patches once
upstream makes the log level configurable and exposes track volume.
`friend_volume_changes_what_you_hear` in the engine's call tests fails if the
volume patch is lost.
