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
    def::BufferAttr,
    sample::{Format, Spec},
    stream::Direction,
};

use crate::frame::{AudioFormat, AudioFrame, Frame};

use super::error::LinCapError;

const CHANNELS: u8 = 2;
const SAMPLE_RATE: u32 = 48_000;
const FRAMES_PER_BUFFER: usize = 960; // 20 ms
const BYTES_PER_SAMPLE: usize = 2;
const BUFFER_BYTES: usize = FRAMES_PER_BUFFER * CHANNELS as usize * BYTES_PER_SAMPLE;

fn timestamp_for_buffer(
    read_completed: SystemTime,
    capture_latency: Duration,
    buffer_duration: Duration,
) -> SystemTime {
    // pa_simple_get_latency() is the record pipeline latency at the instant the
    // read completes. The block just returned ends that far in the past, and its
    // Frame timestamp denotes the first sample in the block.
    read_completed
        .checked_sub(capture_latency.saturating_add(buffer_duration))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

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
        // PipeWire-Pulse otherwise commonly chooses a large recording fragment.
        // This bounds the client/server stream buffer without assuming anything
        // about the device or PipeWire graph latency (which we query per block).
        let buffer_attr = BufferAttr {
            maxlength: (BUFFER_BYTES * 8) as u32,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: BUFFER_BYTES as u32,
        };
        let stream = Simple::new(
            None,
            "scap",
            Direction::Record,
            Some(&source),
            "screen capture audio",
            &spec,
            None,
            Some(&buffer_attr),
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
            let mut data = vec![0; BUFFER_BYTES];
            let buffer_duration =
                Duration::from_secs_f64(FRAMES_PER_BUFFER as f64 / SAMPLE_RATE as f64);
            let mut logged_first_timestamp = false;
            while !stop.load(Ordering::Acquire) {
                if let Err(error) = stream.read(&mut data) {
                    eprintln!("Linux audio capture stopped: {error}");
                    break;
                }
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let read_completed = SystemTime::now();
                let capture_latency = match stream.get_latency() {
                    Ok(latency) => Duration::from_micros(latency.0),
                    Err(error) => {
                        eprintln!("Linux audio capture latency query failed: {error}; using block duration only");
                        Duration::ZERO
                    }
                };
                let timestamp =
                    timestamp_for_buffer(read_completed, capture_latency, buffer_duration);
                if !logged_first_timestamp {
                    eprintln!(
                        "Linux audio: PulseAudio capture latency={} ms, initial timestamp={timestamp:?}",
                        capture_latency.as_secs_f64() * 1_000.0
                    );
                    super::record_initial_timestamp(true, timestamp);
                    logged_first_timestamp = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_timestamp_accounts_for_server_latency_and_block_duration() {
        let completed = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        assert_eq!(
            timestamp_for_buffer(
                completed,
                Duration::from_millis(120),
                Duration::from_millis(20)
            ),
            SystemTime::UNIX_EPOCH + Duration::from_millis(9_860),
        );
    }
}

impl Drop for PulseAudioCapturer {
    fn drop(&mut self) {
        self.stop_capture();
    }
}
