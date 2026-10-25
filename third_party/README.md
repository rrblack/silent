# Third-party code

## webrtc-sys

[webrtc-sys](https://crates.io/crates/webrtc-sys) 0.3.49 from LiveKit
(Apache-2.0, see `webrtc-sys/NOTICE.md`), patched in through
`[patch.crates-io]` in the workspace `Cargo.toml`. Four changes.

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

`FairTaskQueueFactory` in `src/global_task_queue.cpp`, passed to
`webrtc::CreateEnvironment` (`src/peer_connection_factory.cpp`) and behind
`GetGlobalTaskQueueFactory()`: libwebrtc's task queues (video encoders,
decoders, render queues) run at real-time priority where the platform allows
it (`TIME_CRITICAL` for high-priority queues on Windows; `SCHED_FIFO` for
every queue on Linux when permitted, e.g. as root). With three or more
people sending video and the CPU saturated, those threads starved libwebrtc's
normal-priority worker thread, captured frames queued up for it without
bound, and memory grew by 5–15 MB a second (found with a soak test; jemalloc
stats showed the live allocations were each process's own outgoing frames).
The factory creates every queue at normal priority (low stays low) and, on
Linux, moves its thread back to `SCHED_OTHER`. With it the same soak stays
flat.

Max-bundle, in `to_native_rtc_configuration` (`src/peer_connection.cpp`):
webrtc-sys has no bundle policy field, so libwebrtc used its default,
balanced, and gathered ICE candidates for each of the five m-lines until the
answer arrived. Each candidate is one signaling message, so the offering
side of a four-person join sent well over 200 in half a second and the relay
cut it off for flooding (`RateLimited`; `four_friends_full_mesh_and_fifth_is_turned_away`
failed about one run in thirty). Every Silent peer bundles anyway, so the
patch sets `kBundlePolicyMaxBundle` and gathers for the one transport in use:
about five times fewer candidates.

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
