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
    /// Encoder or decoder name reported by libwebrtc (e.g. "libvpx",
    /// "OpenH264", "NVENC", "VideoToolbox").
    pub implementation: String,
    /// Outbound only: the bitrate the congestion controller is aiming for.
    pub target_bitrate_bps: f64,
    /// Outbound only: "none", "cpu", "bandwidth" or "other".
    pub limited_by: String,
    pub power_efficient: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerStats {
    pub peer: PeerId,
    pub sending: Vec<VideoStats>,
    pub receiving: Vec<VideoStats>,
    pub rtt_ms: Option<f64>,
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
    for stat in report {
        match stat {
            RtcStats::OutboundRtp(s) if s.stream.kind == "video" => {
                if s.outbound.frame_width == 0 {
                    continue;
                }
                out.sending.push(VideoStats {
                    kind: kinds.get(&s.outbound.mid).copied(),
                    width: s.outbound.frame_width,
                    height: s.outbound.frame_height,
                    fps: s.outbound.frames_per_second,
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
                out.receiving.push(VideoStats {
                    kind: kinds.get(&s.inbound.mid).copied(),
                    width: s.inbound.frame_width,
                    height: s.inbound.frame_height,
                    fps: s.inbound.frames_per_second,
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
            _ => {}
        }
    }
    out
}
