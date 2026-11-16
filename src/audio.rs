use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use flexaudio_core::backend::{CaptureBackend, RawSink};
use flexaudio_core::raw_ring::{RawConsumer, raw_ring};
use flexaudio_os_macos::MacSystemBackend;
use opus::{Application, Bitrate, Channels, Encoder};
use tokio::sync::mpsc::Sender;
use tracing::{info, warn};

use crate::protocol;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const FRAME_SAMPLES: usize = 960; // 20 ms
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * CHANNELS as usize;
const RING_SAMPLES: usize = SAMPLE_RATE as usize * 2 * 2; // 2 seconds stereo

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioCapture {
    /// Start a Core Audio process tap and encode 48 kHz stereo Opus frames.
    ///
    /// This fails if the tap cannot be created (for example, missing System
    /// Audio Recording permission) or if the native format is not 48 kHz stereo.
    pub fn start(tx: Sender<Vec<u8>>) -> Result<Self> {
        let (producer, consumer) = raw_ring(RING_SAMPLES);
        let sink = RawSink::new(producer, SAMPLE_RATE, CHANNELS);
        let mut backend = MacSystemBackend::new(true, None).with_silence_probe(false);
        backend
            .start(sink)
            .map_err(|err| anyhow!("system audio tap failed: {err}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("rustvnc-audio".to_string())
            .spawn(move || {
                if let Err(err) = capture_loop(backend, consumer, tx, stop_thread) {
                    warn!("audio capture stopped: {err:#}");
                }
            })
            .context("spawn audio thread")?;

        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_loop(
    mut backend: MacSystemBackend,
    mut consumer: RawConsumer,
    tx: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
        .context("create Opus encoder")?;
    encoder.set_bitrate(Bitrate::Bits(64_000))?;
    encoder.set_vbr(true)?;
    encoder.set_inband_fec(true)?;
    encoder.set_packet_loss_perc(5)?;

    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_INTERLEAVED * 2);
    let mut out = vec![0u8; 4000];
    let mut seq: u16 = 0;
    let mut dropped = 0u64;
    let start = Instant::now();
    let mut read_buf = [0f32; 4096];

    while !stop.load(Ordering::SeqCst) {
        let read = consumer.pop_slice(&mut read_buf);
        if read > 0 {
            pending.extend_from_slice(&read_buf[..read]);
        }

        while pending.len() >= FRAME_INTERLEAVED {
            let encoded = encoder
                .encode_float(&pending[..FRAME_INTERLEAVED], &mut out)
                .context("Opus encode")?;
            let packet = protocol::encode_audio_packet(
                seq,
                start.elapsed().as_millis() as u32,
                &out[..encoded],
            );
            seq = seq.wrapping_add(1);
            pending.drain(..FRAME_INTERLEAVED);

            if tx.try_send(packet).is_err() {
                dropped = dropped.saturating_add(1);
            }
        }

        if read == 0 {
            thread::sleep(Duration::from_millis(5));
        }
    }

    if dropped > 0 {
        info!("audio packets dropped due to backpressure: {dropped}");
    }
    backend.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use opus::{Application, Channels, Decoder, Encoder};

    #[test]
    fn opus_roundtrip_produces_audio() {
        let mut encoder =
            Encoder::new(48_000, Channels::Stereo, Application::Audio).expect("encoder");
        let mut decoder = Decoder::new(48_000, Channels::Stereo).expect("decoder");

        let pcm: Vec<f32> = (0..1920)
            .map(|index| (index as f32 * 0.05).sin() * 0.1)
            .collect();
        let mut packet = vec![0u8; 4000];
        let encoded = encoder.encode_float(&pcm, &mut packet).expect("encode");
        assert!(encoded > 0);

        let mut decoded = vec![0f32; 5760];
        let samples = decoder
            .decode_float(&packet[..encoded], &mut decoded, false)
            .expect("decode");
        assert!(samples >= 960);
        assert!(
            decoded[..samples * 2]
                .iter()
                .any(|sample| sample.abs() > 1e-4)
        );
    }
}
