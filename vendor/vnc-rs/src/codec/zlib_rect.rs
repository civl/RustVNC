//! Decoder for the `zlib` rectangle encoding (RFB encoding 6).
//!
//! The zlib stream is shared by every rectangle of the connection, so the
//! `Decompress` state must live in the decoder instance for the whole session.
//! Each rectangle carries a u32 length followed by that many compressed bytes,
//! which decompress to exactly `width * height * bytes_per_pixel` pixels.

use std::future::Future;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{PixelFormat, Rect, VncError, VncEvent};

use super::initialized_vec;

pub struct Decoder {
    decompress: flate2::Decompress,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            decompress: flate2::Decompress::new(true),
        }
    }

    pub async fn decode<S, F, Fut>(
        &mut self,
        format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        output_func: &F,
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
        F: Fn(VncEvent) -> Fut,
        Fut: Future<Output = Result<(), VncError>>,
    {
        crate::limits::dimensions(rect.width, rect.height)?;
        format.validate()?;

        let bytes_per_pixel = format.bits_per_pixel / 8;
        let expected = bytes_per_pixel as usize * rect.width as usize * rect.height as usize;

        let compressed_len = input.read_u32().await? as usize;
        if compressed_len == 0 || compressed_len > crate::limits::MAX_COMPRESSED {
            return Err(VncError::InvalidImageData);
        }
        let mut compressed = initialized_vec(compressed_len);
        input.read_exact(&mut compressed).await?;

        let mut pixels = initialized_vec(expected);
        let mut written = 0usize;
        let mut cursor = 0usize;
        while written < expected {
            let in_before = self.decompress.total_in();
            let out_before = self.decompress.total_out();
            let status = self
                .decompress
                .decompress(
                    &compressed[cursor..],
                    &mut pixels[written..],
                    flate2::FlushDecompress::Sync,
                )
                .map_err(|_| VncError::InvalidImageData)?;
            let consumed = (self.decompress.total_in() - in_before) as usize;
            let produced = (self.decompress.total_out() - out_before) as usize;
            cursor += consumed;
            written += produced;

            if written >= expected {
                break;
            }
            if consumed == 0 && produced == 0 {
                // No progress: a truncated or padded stream. Fail instead of
                // spinning or emitting a half-decoded rectangle.
                return Err(VncError::InvalidImageData);
            }
            if matches!(status, flate2::Status::StreamEnd) {
                return Err(VncError::InvalidImageData);
            }
        }

        output_func(VncEvent::RawImage(*rect, pixels)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        }
    }

    #[test]
    fn decodes_two_rectangles_from_one_shared_stream() {
        let format = PixelFormat::bgra();
        let bpp = format.bits_per_pixel as usize / 8;
        let first: Vec<u8> = (0..4 * bpp as u8).collect();
        let second: Vec<u8> = (100..100 + 4 * bpp as u8).collect();

        // One zlib stream, two independently movable chunks.
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&first).unwrap();
        encoder.write_all(&second).unwrap();
        let stream = encoder.finish().unwrap();
        let split = stream.len() / 2;
        let chunks = [&stream[..split], &stream[split..]];

        let mut decoder = Decoder::new();
        for (chunk, expected) in chunks.iter().zip([&first, &second]) {
            let mut input: &[u8] = chunk;
            let mut payload = Vec::new();
            payload.extend_from_slice(&(input.len() as u32).to_be_bytes());
            payload.extend_from_slice(input);
            input = &payload;
            let events = std::cell::RefCell::new(Vec::new());
            let output = |event: VncEvent| {
                events.borrow_mut().push(event);
                async { Ok(()) }
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime
                .block_on(decoder.decode(&format, &rect(), &mut input, &output))
                .expect("decode");
            let events = events.into_inner();
            match &events[0] {
                VncEvent::RawImage(_, data) => assert_eq!(data.as_slice(), expected.as_slice()),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }
}
