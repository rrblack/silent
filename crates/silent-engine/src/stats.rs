//! Per-friend video statistics, for checking frame rate, resolution and
//! whether a hardware encoder is in use.

use std::collections::HashMap;

use libwebrtc::peer_connection::PeerConnection;
use libwebrtc::stats::RtcStats;
use silent_core::PeerId;

use crate::media::VideoKind;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoStats {
    pub kind: Option<VideoKind>,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// "VP8", "H264", "VP9" or "AV1".
    pub codec: String,
    /// Encoder or decoder name reported by libwebrtc (e.g. "libvpx",
    /// "OpenH264", "NVENC", "VideoToolbox").
    pub implementation: String,
    /// Outbound only: the bitrate the congestion controller is aiming for.
    pub target_bitrate_bps: f64,
    /// Outbound only: "none", "cpu", "bandwidth" or "other".
    pub limited_by: String,
    pub power_efficient: bool,
}

impl VideoStats {
    /// One line for a stats screen, e.g.
    /// "screen 1920x1080 @ 60 fps, H264 via NVENC, 7.8 Mbps".
    pub fn summary(&self) -> String {
        let what = match self.kind {
            Some(VideoKind::Camera) => "camera",
            Some(VideoKind::Screen) => "screen",
            None => "video",
        };
        let mut line = format!(
            "{what} {}x{} @ {:.0} fps",
            self.width, self.height, self.fps
        );
        match (self.codec.is_empty(), self.implementation.is_empty()) {
            (false, false) => {
                line.push_str(&format!(", {} via {}", self.codec, self.implementation))
            }
            (false, true) => line.push_str(&format!(", {}", self.codec)),
            (true, false) => line.push_str(&format!(", {}", self.implementation)),
            (true, true) => {}
        }
        if self.target_bitrate_bps > 0.0 {
            line.push_str(&format!(", {:.1} Mbps", self.target_bitrate_bps / 1e6));
        }
        if !self.limited_by.is_empty() && self.limited_by != "none" {
            line.push_str(&format!(", limited by {}", self.limited_by));
        }
        line
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerStats {
    pub peer: PeerId,
    pub sending: Vec<VideoStats>,
    pub receiving: Vec<VideoStats>,
    pub rtt_ms: Option<f64>,
}

/// Audio levels on one connection, 0..1 (linear), when known.
pub(crate) struct AudioLevels {
    /// Our microphone, as sent.
    pub sent: Option<f64>,
    /// The friend on the other end, as received.
    pub received: Option<f64>,
}

pub(crate) async fn audio_levels(pc: &PeerConnection) -> AudioLevels {
    let mut levels = AudioLevels {
        sent: None,
        received: None,
    };
    let Ok(report) = pc.get_stats().await else {
        return levels;
    };
    for stat in report {
        match stat {
            RtcStats::MediaSource(s) if s.source.kind == "audio" => {
                levels.sent = Some(s.audio.audio_level);
            }
            RtcStats::InboundRtp(s) if s.stream.kind == "audio" => {
                levels.received = Some(s.inbound.audio_level);
            }
            _ => {}
        }
    }
    levels
}

pub(crate) async fn collect(peer: PeerId, pc: &PeerConnection) -> PeerStats {
    let kinds: HashMap<String, VideoKind> = pc
        .transceivers()
        .into_iter()
        .filter_map(|t| {
            let mid = t.mid()?;
            let id = t.sender().track()?.id();
            Some((mid, VideoKind::from_stream_id(&id)?))
        })
        .collect();

    let mut out = PeerStats {
        peer,
        ..Default::default()
    };
    let Ok(report) = pc.get_stats().await else {
        return out;
    };
    let mut codec_ids = Vec::new();
    let mut codecs = HashMap::new();
    for stat in report {
        match stat {
            RtcStats::OutboundRtp(s) if s.stream.kind == "video" => {
                if s.outbound.frame_width == 0 {
                    continue;
                }
                codec_ids.push((true, out.sending.len(), s.stream.codec_id.clone()));
                out.sending.push(VideoStats {
                    kind: kinds.get(&s.outbound.mid).copied(),
                    width: s.outbound.frame_width,
                    height: s.outbound.frame_height,
                    fps: s.outbound.frames_per_second,
                    codec: String::new(),
                    implementation: s.outbound.encoder_implementation.clone(),
                    target_bitrate_bps: s.outbound.target_bitrate,
                    limited_by: format!("{:?}", s.outbound.quality_limitation_reason)
                        .to_lowercase(),
                    power_efficient: s.outbound.power_efficient_encoder,
                });
            }
            RtcStats::InboundRtp(s) if s.stream.kind == "video" => {
                if s.inbound.frame_width == 0 {
                    continue;
                }
                codec_ids.push((false, out.receiving.len(), s.stream.codec_id.clone()));
                out.receiving.push(VideoStats {
                    kind: kinds.get(&s.inbound.mid).copied(),
                    width: s.inbound.frame_width,
                    height: s.inbound.frame_height,
                    fps: s.inbound.frames_per_second,
                    codec: String::new(),
                    implementation: s.inbound.decoder_implementation.clone(),
                    target_bitrate_bps: 0.0,
                    limited_by: String::new(),
                    power_efficient: false,
                });
            }
            RtcStats::CandidatePair(c) if c.candidate_pair.nominated => {
                let rtt = c.candidate_pair.current_round_trip_time;
                if rtt > 0.0 {
                    out.rtt_ms = Some(rtt * 1000.0);
                }
            }
            RtcStats::Codec(c) => {
                let name = c.codec.mime_type.trim_start_matches("video/").to_string();
                codecs.insert(c.rtc.id, name);
            }
            _ => {}
        }
    }
    for (sending, i, id) in codec_ids {
        let list = if sending {
            &mut out.sending
        } else {
            &mut out.receiving
        };
        if let Some(name) = codecs.get(&id) {
            list[i].codec = name.clone();
        }
    }
    out
}
