//! Opus decoding and playback for the agent's audio stream.
//!
//! The network side decodes 20 ms Opus frames into interleaved stereo `f32` and
//! pushes them into a lock-free ring. The device callback only ever touches the
//! ring and two atomics, so a slow disk or a stalled Tokio task can never block
//! the audio thread.
//!
//! There is no sample-rate conversion: the agent always sends 48 kHz stereo, so
//! the output device is opened at exactly that. Devices that cannot do 48 kHz
//! stereo f32 report an actionable error instead of playing chipmunk audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 20 ms of 48 kHz stereo, i.e. one Opus frame.
pub const FRAME_SAMPLES: usize = 960 * 2;
/// Start playing once this much audio is buffered (~60 ms), which absorbs the
/// first jitter spikes without adding a permanent delay.
const PREBUFFER_SAMPLES: u32 = (FRAME_SAMPLES * 3) as u32;
const RING_SAMPLES: usize = 48_000 * 2; // one second

pub struct Player {
    _stream: cpal::Stream,
    producer: rtrb::Producer<f32>,
    /// Set once the prebuffer is full; the callback emits silence until then.
    playing: Arc<AtomicBool>,
    buffered: Arc<AtomicU32>,
}

impl Player {
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("没有可用的音频输出设备 (no default audio output)"))?;

        let mut supported = device
            .supported_output_configs()
            .context("enumerate audio output formats")?;
        let chosen = supported.find(|range| {
            range.channels() == 2
                && range.sample_format() == cpal::SampleFormat::F32
                && range.min_sample_rate().0 <= 48_000
                && range.max_sample_rate().0 >= 48_000
        });
        let Some(chosen) = chosen else {
            bail!(
                "音频设备不支持 48 kHz 立体声 f32，无法播放 Agent 音频 (output device lacks 48 kHz stereo f32)"
            );
        };
        let config = chosen.with_sample_rate(cpal::SampleRate(48_000)).config();

        let (producer, mut consumer) = rtrb::RingBuffer::<f32>::new(RING_SAMPLES);
        let playing = Arc::new(AtomicBool::new(false));
        let buffered = Arc::new(AtomicU32::new(0));
        let callback_playing = Arc::clone(&playing);

        let stream = device
            .build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let live = callback_playing.load(Ordering::Relaxed);
                    for frame in output.chunks_mut(2) {
                        let filled = live
                            && consumer.slots() >= 2
                            && match (consumer.pop(), consumer.pop()) {
                                (Ok(left), Ok(right)) => {
                                    frame[0] = left;
                                    if frame.len() > 1 {
                                        frame[1] = right;
                                    }
                                    true
                                }
                                _ => false,
                            };
                        if !filled {
                            for sample in frame.iter_mut() {
                                *sample = 0.0;
                            }
                        }
                    }
                },
                move |err| tracing::warn!("audio output error: {err}"),
                None,
            )
            .context("open audio output stream")?;
        stream.play().context("start audio output")?;

        Ok(Self {
            _stream: stream,
            producer,
            playing,
            buffered,
        })
    }

    /// Queue decoded interleaved samples. Drops the frame when the ring is full
    /// rather than blocking the network task.
    pub fn push(&mut self, samples: &[f32]) {
        if self.producer.slots() < samples.len() {
            return;
        }
        for sample in samples {
            if self.producer.push(*sample).is_err() {
                return;
            }
        }
        let buffered = self
            .buffered
            .fetch_add(samples.len() as u32, Ordering::Relaxed)
            + samples.len() as u32;
        if !self.playing.load(Ordering::Relaxed) && buffered >= PREBUFFER_SAMPLES {
            self.playing.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }
}

/// Decodes the agent's Opus stream (48 kHz stereo, 20 ms frames).
pub struct Decoder {
    decoder: opus::Decoder,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: opus::Decoder::new(48_000, opus::Channels::Stereo)
                .context("create Opus decoder")?,
        })
    }

    /// Decode one packet into `out` as interleaved stereo samples.
    pub fn decode_into(&mut self, payload: &[u8], out: &mut Vec<f32>) -> Result<()> {
        // 120 ms is the longest Opus frame; size for the worst case.
        out.resize(5760 * 2, 0.0);
        let frames = self
            .decoder
            .decode_float(payload, out, false)
            .context("decode Opus packet")?;
        out.truncate(frames * 2);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_emits_stereo_frames_for_silence() {
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
                .expect("encoder");
        let silence = vec![0i16; 960 * 2];
        let mut packet = vec![0u8; 4000];
        let len = encoder.encode(&silence, &mut packet).expect("encode");
        packet.truncate(len);

        let mut decoder = Decoder::new().expect("decoder");
        let mut samples = Vec::new();
        decoder.decode_into(&packet, &mut samples).expect("decode");
        assert_eq!(samples.len(), FRAME_SAMPLES);
        assert!(samples.iter().all(|sample| sample.abs() < 1e-3));
    }

    #[test]
    fn missing_or_corrupt_packets_never_panic() {
        let mut decoder = Decoder::new().expect("decoder");
        let mut samples = Vec::new();
        // An empty payload is how Opus is told "this packet was lost": it
        // synthesises concealment audio instead of failing, which is exactly the
        // behaviour a datagram transport needs.
        decoder
            .decode_into(&[], &mut samples)
            .expect("packet loss concealment");
        // With no previous frame the decoder has no size to conceal from, so the
        // count is allowed to be anything — what matters is that it stays sane.
        assert!(samples.iter().all(|sample| sample.is_finite()));
        // Arbitrary bytes must not take the audio thread down either.
        let garbage = vec![0xff; 3];
        let _ = decoder.decode_into(&garbage, &mut samples);
    }
}
