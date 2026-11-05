//! Videos sent in chat, played in the chat itself: H.264 pictures with AAC
//! sound in MP4 or MOV files (what phones and most apps save). Everything is
//! decoded in memory, so nothing touches the disk. Other formats can still
//! be saved and opened elsewhere.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use openh264::decoder::{Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use silent_engine::libwebrtc::prelude::VideoRotation;
use silent_engine::render::{i420_to_rgba, RgbaImage};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::codecs::video::well_known::CODEC_ID_H264;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::TimeBase;

/// The largest frame handed to the UI: twice the chat bubble, for HiDPI.
pub const MAX_FRAME: (u32, u32) = (520, 520);

/// How far ahead of the picture the file is read, so the sound stored
/// after it is decoded in time.
const READ_AHEAD: f64 = 1.0;

/// The first picture of a video and how long it runs.
pub struct Poster {
    pub frame: RgbaImage,
    pub seconds: f64,
}

/// The first picture of `data`, or `None` when it isn't a video this
/// player can show.
pub fn poster(data: Arc<[u8]>) -> Option<Poster> {
    let mut video = Video::open(data).ok()?;
    while let Some(packet) = video.next_packet() {
        if let Packet::Video(_, bytes) = packet {
            if let Some(frame) = video.decode(&bytes) {
                return Some(Poster {
                    frame,
                    seconds: video.seconds,
                });
            }
        }
    }
    let frame = video.flush().into_iter().next()?;
    Some(Poster {
        frame,
        seconds: video.seconds,
    })
}

/// A playing video. Dropping it stops the video and its sound.
pub struct Player {
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl Player {
    /// Plays `data` on its own thread, the sound on the output device
    /// called `speaker` if there is one. `on_frame` gets each picture with
    /// how far through the video it is (0 to 1); `on_end` runs when the
    /// video finishes, unless the player was dropped first.
    pub fn start(
        data: Arc<[u8]>,
        speaker: Option<String>,
        on_frame: impl FnMut(RgbaImage, f32) + Send + 'static,
        on_end: impl FnOnce() + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let (s, p) = (stop.clone(), paused.clone());
        let _ = std::thread::Builder::new()
            .name("video".into())
            .spawn(move || match play(data, speaker, &s, &p, on_frame) {
                Ok(()) if !s.load(Ordering::Relaxed) => on_end(),
                Ok(()) => {}
                Err(e) => {
                    tracing::debug!("video stopped: {e:#}");
                    on_end();
                }
            });
        Self { stop, paused }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn play(
    data: Arc<[u8]>,
    speaker: Option<String>,
    stop: &AtomicBool,
    paused: &Arc<AtomicBool>,
    mut on_frame: impl FnMut(RgbaImage, f32),
) -> anyhow::Result<()> {
    let mut video = Video::open(data)?;
    let mut sound = match video.audio.as_ref() {
        Some(_) => Sound::start(speaker, paused.clone())
            .map_err(|e| tracing::debug!("video plays without sound: {e:#}"))
            .ok(),
        None => None,
    };
    let seconds = video.seconds.max(0.001);
    let mut clock = Clock::default();
    let mut queue: VecDeque<(f64, Box<[u8]>)> = VecDeque::new();
    let mut times: VecDeque<f64> = VecDeque::new();
    let mut read_to = 0.0f64;
    let mut eof = false;
    let mut show = |at: f64, frame: RgbaImage, clock: &mut Clock| {
        if !clock.wait_until(at, stop, paused) {
            return false;
        }
        on_frame(frame, (at / seconds).clamp(0.0, 1.0) as f32);
        true
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        while !eof && (queue.is_empty() || read_to < clock.now(paused) + READ_AHEAD) {
            match video.next_packet() {
                Some(Packet::Video(at, bytes)) => {
                    read_to = read_to.max(at);
                    queue.push_back((at, bytes));
                }
                Some(Packet::Audio(at, samples, rate, channels)) => {
                    read_to = read_to.max(at);
                    if let Some(sound) = sound.as_mut() {
                        sound.push(&samples, rate, channels);
                    }
                }
                None => eof = true,
            }
        }
        let Some((at, bytes)) = queue.pop_front() else {
            break;
        };
        times.push_back(at);
        if let Some(frame) = video.decode(&bytes) {
            let at = times.pop_front().unwrap_or(at);
            if !show(at, frame, &mut clock) {
                return Ok(());
            }
        }
    }
    for frame in video.flush() {
        let Some(at) = times.pop_front() else { break };
        if !show(at, frame, &mut clock) {
            return Ok(());
        }
    }
    // Let the last of the sound play out.
    if let Some(sound) = sound {
        while !sound.drained() && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

/// Media time, paused with the video.
#[derive(Default)]
struct Clock {
    played: Duration,
    since: Option<Instant>,
}

impl Clock {
    /// Seconds of video played so far.
    fn now(&mut self, paused: &AtomicBool) -> f64 {
        let paused = paused.load(Ordering::Relaxed);
        match (self.since, paused) {
            (Some(at), true) => {
                self.played += at.elapsed();
                self.since = None;
            }
            (None, false) => self.since = Some(Instant::now()),
            _ => {}
        }
        (self.played + self.since.map_or(Duration::ZERO, |at| at.elapsed())).as_secs_f64()
    }

    /// Sleeps until `at` seconds of video have played; false if stopped.
    fn wait_until(&mut self, at: f64, stop: &AtomicBool, paused: &AtomicBool) -> bool {
        loop {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let left = at - self.now(paused);
            if left <= 0.0 {
                return true;
            }
            std::thread::sleep(Duration::from_secs_f64(left.min(0.02)));
        }
    }
}

enum Packet {
    /// Presentation time in seconds and one H.264 access unit, as stored.
    Video(f64, Box<[u8]>),
    /// Start time in seconds and interleaved samples at a rate and channel
    /// count.
    Audio(f64, Vec<f32>, u32, usize),
}

struct Video {
    reader: Box<dyn FormatReader>,
    track: u32,
    time_base: Option<TimeBase>,
    /// SPS and PPS with start codes, fed before the first picture.
    header: Option<Vec<u8>>,
    length_size: usize,
    decoder: Decoder,
    rotation: VideoRotation,
    seconds: f64,
    audio: Option<Audio>,
    annex_b: Vec<u8>,
}

struct Audio {
    track: u32,
    time_base: Option<TimeBase>,
    decoder: Box<dyn AudioDecoder>,
}

impl Video {
    fn open(data: Arc<[u8]>) -> anyhow::Result<Self> {
        let rotation = rotation(&data);
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        let reader = symphonia::default::get_probe().probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;
        let (track, time_base, duration, avcc) = reader
            .tracks()
            .iter()
            .find_map(|t| match &t.codec_params {
                Some(CodecParameters::Video(v)) if v.codec == CODEC_ID_H264 => {
                    let avcc = v.extra_data.first()?.data.to_vec();
                    Some((t.id, t.time_base, t.duration, avcc))
                }
                _ => None,
            })
            .context("no H.264 picture")?;
        let (header, length_size) = parse_avcc(&avcc).context("bad H.264 setup")?;
        let audio = reader.tracks().iter().find_map(|t| {
            if t.track_type() != Some(TrackType::Audio) {
                return None;
            }
            let Some(CodecParameters::Audio(params)) = &t.codec_params else {
                return None;
            };
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())
                .map_err(|e| tracing::debug!("video sound not supported: {e}"))
                .ok()?;
            Some(Audio {
                track: t.id,
                time_base: t.time_base,
                decoder,
            })
        });
        let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), config)?;
        let seconds = duration.map_or(0.0, |d| to_seconds(time_base, d.get() as i64));
        Ok(Self {
            reader,
            track,
            time_base,
            header: Some(header),
            length_size,
            decoder,
            rotation,
            seconds,
            audio,
            annex_b: Vec::new(),
        })
    }

    /// The next picture or sound in the file; `None` at its end.
    fn next_packet(&mut self) -> Option<Packet> {
        loop {
            let packet = self.reader.next_packet().ok()??;
            if packet.track_id == self.track {
                let at = to_seconds(self.time_base, packet.pts.get());
                return Some(Packet::Video(at, packet.data));
            }
            let Some(audio) = self.audio.as_mut().filter(|a| a.track == packet.track_id) else {
                continue;
            };
            let at = to_seconds(audio.time_base, packet.pts.get());
            let Ok(decoded) = audio.decoder.decode(&packet) else {
                continue;
            };
            let rate = decoded.spec().rate();
            let channels = decoded.spec().channels().count();
            let mut samples = Vec::new();
            decoded.copy_to_vec_interleaved(&mut samples);
            return Some(Packet::Audio(at, samples, rate, channels));
        }
    }

    /// Decodes one stored access unit; a picture once one is ready.
    fn decode(&mut self, sample: &[u8]) -> Option<RgbaImage> {
        self.annex_b.clear();
        if let Some(header) = self.header.take() {
            self.annex_b.extend_from_slice(&header);
        }
        to_annex_b(sample, self.length_size, &mut self.annex_b);
        let rotation = self.rotation;
        match self.decoder.decode(&self.annex_b) {
            Ok(Some(yuv)) => to_rgba(&yuv, rotation),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!("video picture skipped: {e}");
                None
            }
        }
    }

    /// The pictures still held in the decoder.
    fn flush(&mut self) -> Vec<RgbaImage> {
        let rotation = self.rotation;
        self.decoder
            .flush_remaining()
            .map(|frames| frames.iter().filter_map(|f| to_rgba(f, rotation)).collect())
            .unwrap_or_default()
    }
}

fn to_rgba(yuv: &impl YUVSource, rotation: VideoRotation) -> Option<RgbaImage> {
    let (w, h) = yuv.dimensions();
    let (sy, su, sv) = yuv.strides();
    i420_to_rgba(
        [yuv.y(), yuv.u(), yuv.v()],
        [sy, su, sv],
        (w as u32, h as u32),
        rotation,
        MAX_FRAME,
    )
}

fn to_seconds(time_base: Option<TimeBase>, ts: i64) -> f64 {
    let tb = time_base.unwrap_or(TimeBase::try_new(1, 1).expect("1/1 is a valid time base"));
    ts as f64 * f64::from(tb.numer.get()) / f64::from(tb.denom.get())
}

/// The SPS and PPS of an `avcC` box as start-code prefixed NAL units, and
/// the size of the length before each NAL unit in the samples.
fn parse_avcc(avcc: &[u8]) -> Option<(Vec<u8>, usize)> {
    let length_size = usize::from(*avcc.get(4)? & 3) + 1;
    let mut header = Vec::new();
    let mut at = 5;
    for mask in [0x1f, 0xff] {
        let count = *avcc.get(at)? & mask;
        at += 1;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
            at += 2;
            header.extend_from_slice(&[0, 0, 0, 1]);
            header.extend_from_slice(avcc.get(at..at + len)?);
            at += len;
        }
    }
    Some((header, length_size))
}

/// Rewrites length-prefixed NAL units (as MP4 stores them) with start
/// codes (as the decoder reads them).
fn to_annex_b(mut sample: &[u8], length_size: usize, out: &mut Vec<u8>) {
    while sample.len() > length_size {
        let len = sample[..length_size]
            .iter()
            .fold(0usize, |n, b| n << 8 | usize::from(*b));
        sample = &sample[length_size..];
        let len = len.min(sample.len());
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[..len]);
        sample = &sample[len..];
    }
}

/// How the picture must be turned to stand upright: phones store portrait
/// video sideways with a turn in the track header's matrix.
fn rotation(file: &[u8]) -> VideoRotation {
    let video_track = children(file)
        .filter(|(kind, _)| kind == b"moov")
        .flat_map(|(_, moov)| children(moov))
        .filter(|(kind, _)| kind == b"trak")
        .find(|(_, trak)| {
            child(trak, b"mdia")
                .and_then(|mdia| child(mdia, b"hdlr"))
                .is_some_and(|hdlr| hdlr.get(8..12) == Some(b"vide"))
        });
    let Some(tkhd) = video_track.and_then(|(_, trak)| child(trak, b"tkhd")) else {
        return VideoRotation::VideoRotation0;
    };
    // Version 1 headers have 64-bit times: 12 more bytes before the matrix.
    let matrix = if tkhd.first() == Some(&1) { 52 } else { 40 };
    let at = |i: usize| {
        tkhd.get(matrix + i * 4..matrix + i * 4 + 4)
            .map_or(0, |b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    match (at(0).signum(), at(1).signum()) {
        (0, 1) => VideoRotation::VideoRotation90,
        (-1, 0) => VideoRotation::VideoRotation180,
        (0, -1) => VideoRotation::VideoRotation270,
        _ => VideoRotation::VideoRotation0,
    }
}

/// The boxes directly inside an MP4 box's payload, as (type, payload).
fn children(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (start, size) = match size {
            0 => (8, data.len()),
            1 => {
                let big = u64::from_be_bytes(data.get(8..16)?.try_into().ok()?);
                (16, usize::try_from(big).ok()?)
            }
            n => (8, n),
        };
        let payload = data.get(start..size)?;
        data = &data[size..];
        Some((kind, payload))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).find(|(k, _)| k == kind).map(|(_, p)| p)
}

/// The video's sound, resampled to the output device.
struct Sound {
    queue: Arc<Mutex<VecDeque<f32>>>,
    rate: u32,
    channels: usize,
    /// Source frames not yet resampled, and the position between them.
    input: Vec<f32>,
    position: f64,
    _stream: cpal::Stream,
}

impl Sound {
    fn start(speaker: Option<String>, paused: Arc<AtomicBool>) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let named = speaker.and_then(|name| {
            host.output_devices()
                .ok()?
                .find(|d| d.name().is_ok_and(|n| n == name))
        });
        let device = named
            .or_else(|| host.default_output_device())
            .context("no output device")?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, queue.clone(), paused),
            SampleFormat::I16 => build::<i16>(&device, &config, queue.clone(), paused),
            SampleFormat::U16 => build::<u16>(&device, &config, queue.clone(), paused),
            SampleFormat::I32 => build::<i32>(&device, &config, queue.clone(), paused),
            other => anyhow::bail!("unsupported sample format {other}"),
        }?;
        stream.play()?;
        Ok(Self {
            queue,
            rate: config.sample_rate.0,
            channels: usize::from(config.channels),
            input: Vec::new(),
            position: 0.0,
            _stream: stream,
        })
    }

    /// Queues interleaved `samples` at `rate` with `channels`.
    fn push(&mut self, samples: &[f32], rate: u32, channels: usize) {
        if channels == 0 || rate == 0 {
            return;
        }
        let out = resample(
            &mut self.input,
            &mut self.position,
            samples,
            (rate, channels),
            (self.rate, self.channels),
        );
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(out);
    }

    fn drained(&self) -> bool {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

/// Linear resampling from `from` (rate, channels) to `to`, carrying the
/// leftover frames and position in `input` and `position` between calls.
/// Mono goes to every output channel; extra input channels are dropped.
fn resample(
    input: &mut Vec<f32>,
    position: &mut f64,
    samples: &[f32],
    from: (u32, usize),
    to: (u32, usize),
) -> Vec<f32> {
    let (ch_in, ch_out) = (from.1, to.1);
    input.extend_from_slice(samples);
    let frames = input.len() / ch_in;
    let step = f64::from(from.0) / f64::from(to.0);
    let mut out = Vec::new();
    while *position + 1.0 < frames as f64 {
        let i = *position as usize;
        let t = (*position - i as f64) as f32;
        for c in 0..ch_out {
            let c = c.min(ch_in - 1);
            let (a, b) = (input[i * ch_in + c], input[(i + 1) * ch_in + c]);
            out.push(a + (b - a) * t);
        }
        *position += step;
    }
    let used = (*position as usize).min(frames);
    input.drain(..used * ch_in);
    *position -= used as f64;
    out
}

fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    queue: Arc<Mutex<VecDeque<f32>>>,
    paused: Arc<AtomicBool>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    Ok(device.build_output_stream(
        config,
        move |out: &mut [T], _| {
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            let paused = paused.load(Ordering::Relaxed);
            for sample in out.iter_mut() {
                let value = if paused {
                    0.0
                } else {
                    queue.pop_front().unwrap_or(0.0)
                };
                *sample = T::from_sample(value);
            }
        },
        |e| tracing::debug!("video sound: {e}"),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_becomes_start_codes() {
        // Version 1, profile 100, 4-byte lengths, one SPS, one PPS.
        let avcc = [1, 100, 0, 30, 0xff, 0xe1, 0, 2, 0x67, 0xaa, 1, 0, 1, 0x68];
        let (header, length_size) = parse_avcc(&avcc).unwrap();
        assert_eq!(length_size, 4);
        assert_eq!(header, [0, 0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x68]);
        assert!(parse_avcc(&avcc[..9]).is_none());

        let mut out = Vec::new();
        to_annex_b(&[0, 0, 0, 2, 0x65, 1, 0, 0, 0, 1, 0x41], 4, &mut out);
        assert_eq!(out, [0, 0, 0, 1, 0x65, 1, 0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn resampling_keeps_time_and_spreads_mono() {
        let (mut input, mut position) = (Vec::new(), 0.0);
        let mut out = Vec::new();
        for _ in 0..10 {
            let chunk: Vec<f32> = (0..441).map(|i| i as f32 / 441.0).collect();
            out.extend(resample(
                &mut input,
                &mut position,
                &chunk,
                (44_100, 1),
                (48_000, 2),
            ));
        }
        // 0.1 s of mono at 44.1 kHz is about 4800 stereo frames at 48 kHz.
        let frames = out.len() / 2;
        assert!((4790..=4800).contains(&frames), "{frames}");
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
    }

    const PORTRAIT: &[u8] = include_bytes!("../testdata/portrait.mp4");

    #[test]
    fn posters_stand_upright() {
        let poster = poster(PORTRAIT.into()).unwrap();
        // Stored 64x48 with a quarter turn, like a phone held upright.
        assert_eq!((poster.frame.width, poster.frame.height), (48, 64));
        assert!((poster.seconds - 0.5).abs() < 0.05, "{}", poster.seconds);
        assert!(super::poster(b"not a video".as_slice().into()).is_none());
    }

    #[test]
    fn plays_every_picture_in_time() {
        let (tx, rx) = std::sync::mpsc::channel();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let seen = frames.clone();
        let started = Instant::now();
        let _player = Player::start(
            PORTRAIT.into(),
            None,
            move |_, progress| seen.lock().unwrap().push(progress),
            move || tx.send(()).unwrap(),
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(350));
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 5);
        assert!(frames.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn rotation_comes_from_the_video_track_header() {
        fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(payload);
            b
        }
        let mut tkhd = vec![0u8; 84];
        tkhd[44..48].copy_from_slice(&0x10000i32.to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");
        let trak = [
            mp4_box(b"tkhd", &tkhd),
            mp4_box(b"mdia", &mp4_box(b"hdlr", &hdlr)),
        ]
        .concat();
        let file = [
            mp4_box(b"ftyp", b"isom"),
            mp4_box(b"moov", &mp4_box(b"trak", &trak)),
        ]
        .concat();
        assert_eq!(rotation(&file), VideoRotation::VideoRotation90);
        assert_eq!(rotation(b"not a video"), VideoRotation::VideoRotation0);
    }
}
