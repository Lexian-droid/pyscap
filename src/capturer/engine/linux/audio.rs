use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use psimple::Simple;
use pulse::{
    sample::{Format, Spec},
    stream::Direction,
};

use crate::frame::{AudioFormat, AudioFrame, Frame};

use super::error::LinCapError;

const CHANNELS: u8 = 2;
const SAMPLE_RATE: u32 = 48_000;
const FRAMES_PER_BUFFER: usize = 960; // 20 ms
const BYTES_PER_SAMPLE: usize = 2;

pub struct PulseAudioCapturer {
    stream: Option<Simple>,
    tx: Sender<Frame>,
    stop: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
}

impl PulseAudioCapturer {
    pub fn new(tx: Sender<Frame>) -> Result<Self, LinCapError> {
        let spec = Spec {
            format: Format::S16NE,
            channels: CHANNELS,
            rate: SAMPLE_RATE,
        };
        let source =
            std::env::var("SCAP_AUDIO_SOURCE").unwrap_or_else(|_| "@DEFAULT_MONITOR@".to_string());
        let stream = Simple::new(
            None,
            "scap",
            Direction::Record,
            Some(&source),
            "screen capture audio",
            &spec,
            None,
            None,
        )
        .map_err(|error| {
            LinCapError::new(format!(
                "Linux audio capture could not open PulseAudio source {source:?}: {error}"
            ))
        })?;
        Ok(Self {
            stream: Some(stream),
            tx,
            stop: Arc::new(AtomicBool::new(false)),
            join_handle: None,
        })
    }

    pub fn start_capture(&mut self) {
        if self.join_handle.is_some() {
            return;
        }
        let Some(stream) = self.stream.take() else {
            return;
        };
        self.stop.store(false, Ordering::Release);
        let stop = self.stop.clone();
        let tx = self.tx.clone();
        self.join_handle = Some(std::thread::spawn(move || {
            let mut data = vec![0; FRAMES_PER_BUFFER * CHANNELS as usize * BYTES_PER_SAMPLE];
            while !stop.load(Ordering::Acquire) {
                if let Err(error) = stream.read(&mut data) {
                    eprintln!("Linux audio capture stopped: {error}");
                    break;
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let duration =
                    Duration::from_secs_f64(FRAMES_PER_BUFFER as f64 / SAMPLE_RATE as f64);
                let timestamp = SystemTime::now()
                    .checked_sub(duration)
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let frame = AudioFrame::new(
                    AudioFormat::I16,
                    CHANNELS as u16,
                    false,
                    data.clone(),
                    FRAMES_PER_BUFFER,
                    SAMPLE_RATE,
                    timestamp,
                );
                if tx.send(Frame::Audio(frame)).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn stop_capture(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PulseAudioCapturer {
    fn drop(&mut self) {
        self.stop_capture();
    }
}
