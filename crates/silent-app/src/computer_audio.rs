//! The computer's own sound for screen shares. On Windows (10 version 2004
//! or later) it is a process loopback that captures everything playing
//! except Silent itself, so friends never hear their own voices back.
//! Nothing is recorded: samples go straight into the call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use silent_engine::ScreenAudioInput;

/// Whether this platform can share computer audio.
pub const SUPPORTED: bool = cfg!(windows);

/// Captures into the call until dropped.
pub struct ComputerAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ComputerAudio {
    /// Starts capturing into `input`, or says why it can't.
    pub fn start(input: ScreenAudioInput) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("silent-computer-audio".into())
            .spawn(move || capture(input, stopping, ready_tx))
            .map_err(|e| e.to_string())?;
        // Windows opens the loopback asynchronously; don't hang the window
        // if it never answers (the thread then stops by itself).
        let ready = ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or_else(|_| Err("Windows didn't answer".into()));
        match ready {
            Ok(()) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Err(reason) => {
                stop.store(true, Ordering::Relaxed);
                Err(format!("Couldn't share computer audio: {reason}."))
            }
        }
    }
}

impl Drop for ComputerAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(not(windows))]
fn capture(
    _input: ScreenAudioInput,
    _stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let _ = ready.send(Err("it is only available on Windows so far".into()));
}

#[cfg(windows)]
fn capture(
    input: ScreenAudioInput,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    use silent_engine::{SCREEN_AUDIO_CHANNELS, SCREEN_AUDIO_RATE};
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

    let _ = wasapi::initialize_mta();
    let open = || -> Result<_, wasapi::WasapiError> {
        // 16-bit stereo at the call's rate; Windows converts whatever plays.
        let format = WaveFormat::new(
            16,
            16,
            &SampleType::Int,
            SCREEN_AUDIO_RATE as usize,
            SCREEN_AUDIO_CHANNELS as usize,
            None,
        );
        // `false`: everything except this process and its children.
        let mut client = AudioClient::new_application_loopback_client(std::process::id(), false)?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        Ok((client, capture, event))
    };
    let (client, capture, event) = match open() {
        Ok(opened) => opened,
        Err(e) => {
            let _ = ready.send(Err(format!(
                "{e} (it needs Windows 10 version 2004 or later)"
            )));
            return;
        }
    };
    if stop.load(Ordering::Relaxed) {
        // Opened only after `start` gave up waiting.
        return;
    }
    let _ = ready.send(Ok(()));

    let bytes_per_frame = 2 * SCREEN_AUDIO_CHANNELS as usize;
    let mut buffer = vec![0u8; SCREEN_AUDIO_RATE as usize * bytes_per_frame];
    while !stop.load(Ordering::Relaxed) {
        // No event while nothing plays; look at `stop` again.
        if event.wait_for_event(200).is_err() {
            continue;
        }
        loop {
            match capture.get_next_packet_size() {
                Ok(Some(frames)) if frames > 0 => {}
                Ok(_) => break,
                Err(e) => {
                    tracing::warn!("computer audio stopped: {e}");
                    return;
                }
            }
            let (frames, info) = match capture.read_from_device(&mut buffer) {
                Ok(read) => read,
                Err(e) => {
                    tracing::warn!("computer audio stopped: {e}");
                    return;
                }
            };
            if frames == 0 {
                break;
            }
            let len = frames as usize * bytes_per_frame;
            let samples = if info.flags.silent {
                vec![0i16; len / 2]
            } else {
                buffer[..len]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect()
            };
            input.push(samples);
        }
    }
    let _ = client.stop_stream();
}
