use crate::{PixelFormat, Rect, VncError, VncEvent};
use std::future::Future;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::initialized_vec;

pub struct Decoder {}

impl Decoder {
    pub fn new() -> Self {
        Self {}
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
        format.validate()?;
        if rect.width != 0 && rect.height != 0 {
            crate::limits::dimensions(rect.width, rect.height)?;
        }
        if rect.width == 0 || rect.height == 0 {
            return output_func(VncEvent::SetCursor(*rect, Vec::new())).await;
        }
        let w = rect.width;
        let h = rect.height;
        let pixels_length = w as usize * h as usize * format.bits_per_pixel as usize / 8;
        let mask_length = (w as usize).div_ceil(8) * h as usize;

        // Cursor shapes are only well defined for 32 bpp here. A 16/8 bpp
        // session can still receive one (macOS sends them unprompted once the
        // pointer moves); the body is well defined, so consume it and report an
        // empty shape instead of failing the whole session with
        // "Unknown pixel format".
        if format.bits_per_pixel != 32 || format.true_color_flag == 0 {
            let mut body = initialized_vec(pixels_length + mask_length);
            input.read_exact(&mut body).await?;
            return output_func(VncEvent::SetCursor(*rect, Vec::new())).await;
        }

        let _hotx = rect.x;
        let _hoty = rect.y;
        let _bytes = pixels_length + mask_length;

        let mut pixels = initialized_vec(pixels_length);
        input.read_exact(&mut pixels).await?;
        let mut mask = initialized_vec(mask_length);
        input.read_exact(&mut mask).await?;
        let mut image = initialized_vec(pixels_length);
        let mut pix_idx = 0;

        let pixel_mask = ((format.red_max as u32) << format.red_shift)
            | ((format.green_max as u32) << format.green_shift)
            | ((format.blue_max as u32) << format.blue_shift);

        let mut alpha_idx = match pixel_mask {
            0xff_ff_ff_00 => 3,
            0xff_ff_00_ff => 2,
            0xff_00_ff_ff => 1,
            0x00_ff_ff_ff => 0,
            _ => return Err(VncError::WrongPixelFormat),
        };
        if format.big_endian_flag == 0 {
            alpha_idx = 3 - alpha_idx;
        }
        for y in 0..h as usize {
            for x in 0..w as usize {
                let mask_idx = y * (w as usize).div_ceil(8) + (x / 8);
                let alpha = if (mask[mask_idx] << (x % 8)) & 0x80 > 0 {
                    255
                } else {
                    0
                };
                image[pix_idx] = pixels[pix_idx];
                image[pix_idx + 1] = pixels[pix_idx + 1];
                image[pix_idx + 2] = pixels[pix_idx + 2];
                image[pix_idx + 3] = pixels[pix_idx + 3];

                // use alpha from the bitmask to cover it.
                image[pix_idx + alpha_idx] = alpha;
                pix_idx += 4;
            }
        }

        output_func(VncEvent::SetCursor(*rect, image)).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn decode_with(
        format: &PixelFormat,
        rect: Rect,
        body: &[u8],
    ) -> Result<Vec<VncEvent>, VncError> {
        let mut decoder = Decoder::new();
        let mut input = body;
        let events = RefCell::new(Vec::new());
        let output = |event: VncEvent| {
            events.borrow_mut().push(event);
            async { Ok(()) }
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(decoder.decode(format, &rect, &mut input, &output))?;
        Ok(events.into_inner())
    }

    fn rgb565() -> PixelFormat {
        let mut format = PixelFormat::default();
        format.bits_per_pixel = 16;
        format.depth = 16;
        format.red_max = 31;
        format.green_max = 63;
        format.blue_max = 31;
        format.red_shift = 11;
        format.green_shift = 5;
        format.blue_shift = 0;
        format
    }

    fn rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        }
    }

    /// A 16 bpp session used to abort with `WrongPixelFormat` as soon as macOS
    /// sent a cursor, which killed the whole connection on pointer movement.
    #[test]
    fn sixteen_bpp_cursor_is_skipped_not_fatal() {
        // 4x2 pixels at 2 bytes each, plus a 1-byte-per-row mask.
        let body = vec![0u8; 4 * 2 * 2 + 2];
        let events = decode_with(&rgb565(), rect(), &body).expect("must not fail the session");
        match &events[0] {
            VncEvent::SetCursor(rect, image) => {
                assert_eq!(rect.width, 4);
                assert!(image.is_empty(), "no shape is fabricated");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn thirty_two_bpp_cursor_still_decodes() {
        let body = vec![0u8; 4 * 2 * 4 + 2];
        let events = decode_with(&PixelFormat::default(), rect(), &body).expect("decode");
        match &events[0] {
            VncEvent::SetCursor(_, image) => assert_eq!(image.len(), 4 * 2 * 4),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn empty_cursor_needs_no_body() {
        let mut empty = rect();
        empty.width = 0;
        empty.height = 0;
        let events = decode_with(&rgb565(), empty, &[]).expect("empty cursor");
        assert!(matches!(&events[0], VncEvent::SetCursor(_, image) if image.is_empty()));
    }
}
