//! Per-friend video statistics, for checking frame rate, resolution and
//! whether a hardware encoder is in use, plus the quick readings taken
//! several times a second: audio levels and connection quality.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// One quick reading of a connection. Audio levels are 0..1 (linear).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Pulse {
    /// Our microphone, as sent.
    pub sent: Option<f64>,
    /// The friend on the other end, as received.
    pub received: Option<f64>,
    pub rtt_ms: Option<f64>,
    /// Audio packets from the friend so far: received and lost.
    pub audio_packets: Option<(u64, i64)>,
}

pub(crate) async fn pulse(pc: &PeerConnection) -> Pulse {
    let mut pulse = Pulse::default();
    let Ok(report) = pc.get_stats().await else {
        return pulse;
    };
    for stat in report {
        match stat {
            RtcStats::MediaSource(s) if s.source.kind == "audio" => {
                pulse.sent = Some(s.audio.audio_level);
            }
            RtcStats::InboundRtp(s) if s.stream.kind == "audio" => {
                pulse.received = Some(s.inbound.audio_level);
                pulse.audio_packets = Some((s.received.packets_received, s.received.packets_lost));
            }
            RtcStats::CandidatePair(c) if c.candidate_pair.nominated => {
                let rtt = c.candidate_pair.current_round_trip_time;
                if rtt > 0.0 {
                    pulse.rtt_ms = Some(rtt * 1000.0);
                }
            }
            _ => {}
        }
    }
    pulse
}

/// How a friend's connection is doing, judged from round trip time and the
/// share of their audio that gets lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Good,
    /// Enough loss or delay to hear: choppy or late audio, blurry video.
    Poor,
}

/// Readings are judged in windows this long.
const QUALITY_WINDOW: Duration = Duration::from_secs(2);
/// Loss is only judged with at least this many packets in a window (Opus
/// sends 50 a second, fewer in silence).
const QUALITY_MIN_PACKETS: i64 = 20;
const POOR_LOSS: f64 = 0.08;
const POOR_RTT_MS: f64 = 500.0;
/// Good windows in a row needed before Poor goes back to Good, so the mark
/// doesn't flicker.
const GOOD_WINDOWS_TO_RECOVER: u32 = 2;

/// Follows one friend's connection quality over time.
#[derive(Debug, Default)]
pub(crate) struct QualityMeter {
    /// Start of the current window and the packet counts then.
    window: Option<(Instant, u64, i64)>,
    worst_rtt_ms: f64,
    quality: Quality,
    good_windows: u32,
    /// Audio has flowed since the last reset: from then on, a window with
    /// no packets at all means the connection is down, not silence (Opus
    /// keeps sending in silence, just smaller packets).
    flowing: bool,
}

impl QualityMeter {
    /// Feeds one reading; returns the new quality when it changed.
    pub fn feed(
        &mut self,
        now: Instant,
        rtt_ms: Option<f64>,
        packets: Option<(u64, i64)>,
    ) -> Option<Quality> {
        let (received, lost) = packets.unwrap_or_default();
        let Some((start, received0, lost0)) = self.window else {
            self.window = Some((now, received, lost));
            self.worst_rtt_ms = rtt_ms.unwrap_or(0.0);
            return None;
        };
        self.worst_rtt_ms = self.worst_rtt_ms.max(rtt_ms.unwrap_or(0.0));
        if now.duration_since(start) < QUALITY_WINDOW {
            return None;
        }
        let got = received.saturating_sub(received0) as i64;
        let missed = (lost - lost0).max(0);
        let judged = got + missed >= QUALITY_MIN_PACKETS;
        let lossy = judged && missed as f64 / (got + missed) as f64 >= POOR_LOSS;
        let stalled = self.flowing && got == 0;
        self.flowing |= judged;
        let slow = self.worst_rtt_ms >= POOR_RTT_MS;
        self.window = Some((now, received, lost));
        self.worst_rtt_ms = rtt_ms.unwrap_or(0.0);

        let next = if lossy || stalled || slow {
            self.good_windows = 0;
            Quality::Poor
        } else {
            self.good_windows += 1;
            if self.quality == Quality::Poor && self.good_windows < GOOD_WINDOWS_TO_RECOVER {
                Quality::Poor
            } else {
                Quality::Good
            }
        };
        (next != self.quality).then(|| {
            self.quality = next;
            next
        })
    }

    /// Starts over, e.g. after the connection dropped and came back: packets
    /// lost during the outage say nothing about the connection now.
    pub fn reset(&mut self) {
        self.window = None;
        self.flowing = false;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a reading every 250 ms for `secs` seconds at `pps` packets a
    /// second with `loss` of them lost; returns the changes reported.
    fn run(
        meter: &mut QualityMeter,
        clock: &mut (Instant, u64, i64),
        secs: u64,
        pps: u64,
        loss: f64,
        rtt: f64,
    ) -> Vec<Quality> {
        let mut changes = Vec::new();
        for _ in 0..secs * 4 {
            clock.0 += Duration::from_millis(250);
            let sent = pps / 4;
            let lost = (sent as f64 * loss).round() as u64;
            clock.1 += sent - lost;
            clock.2 += lost as i64;
            changes.extend(meter.feed(clock.0, Some(rtt), Some((clock.1, clock.2))));
        }
        changes
    }

    #[test]
    fn good_connection_stays_good() {
        let mut meter = QualityMeter::default();
        let mut clock = (Instant::now(), 0, 0);
        assert!(run(&mut meter, &mut clock, 20, 50, 0.01, 40.0).is_empty());
    }

    #[test]
    fn loss_or_delay_is_poor_and_recovery_waits_for_two_good_windows() {
        let mut meter = QualityMeter::default();
        let mut clock = (Instant::now(), 0, 0);
        assert!(run(&mut meter, &mut clock, 4, 50, 0.0, 40.0).is_empty());
        assert_eq!(
            run(&mut meter, &mut clock, 4, 50, 0.2, 40.0),
            [Quality::Poor]
        );
        // Recovery waits for two good windows in a row: 2 s of clean audio
        // isn't enough, 4 more are.
        assert!(run(&mut meter, &mut clock, 2, 50, 0.0, 40.0).is_empty());
        assert_eq!(
            run(&mut meter, &mut clock, 4, 50, 0.0, 40.0),
            [Quality::Good]
        );
        assert_eq!(
            run(&mut meter, &mut clock, 4, 50, 0.0, 800.0),
            [Quality::Poor]
        );
    }

    #[test]
    fn audio_that_stops_coming_is_poor() {
        let mut meter = QualityMeter::default();
        let mut clock = (Instant::now(), 0, 0);
        assert!(run(&mut meter, &mut clock, 4, 50, 0.0, 40.0).is_empty());
        assert_eq!(
            run(&mut meter, &mut clock, 4, 0, 0.0, 40.0),
            [Quality::Poor]
        );
    }

    #[test]
    fn silence_with_few_packets_is_not_judged_as_loss() {
        let mut meter = QualityMeter::default();
        let mut clock = (Instant::now(), 0, 0);
        // 8 packets a second with one of four lost: too few to judge.
        assert!(run(&mut meter, &mut clock, 10, 8, 0.25, 40.0).is_empty());
    }
}
