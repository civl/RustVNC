use crate::{Rect, VncError};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest framebuffer we are willing to describe.
///
/// This used to be 8,294,400 (exactly 3840×2160), which silently rejected every
/// 5K/6K Retina Mac — a 5120×2880 Mac mini reports 14.7 M pixels and the session
/// died right after authentication with "image data cannot be decoded". 8192×4320
/// covers 6K (6016×3384) with headroom while still bounding memory: the worst
/// case 32 bpp frame is ~141 MB, and only when a server actually sends a
/// rectangle that large.
pub(crate) const MAX_PIXELS: usize = 8192 * 4320;
pub(crate) const MAX_DIMENSION: u16 = 16384;
pub(crate) const MAX_COMPRESSED: usize = 64 * 1024 * 1024;
pub(crate) const MAX_TEXT: usize = 1024 * 1024;
/// Apple pseudo-encoding bodies are tiny metadata blobs; anything larger is a
/// desynchronised stream, not a capability list.
pub(crate) const MAX_PSEUDO_PAYLOAD: usize = 64 * 1024;
pub(crate) const MAX_NAME: usize = 4096;

pub(crate) fn dimensions(width: u16, height: u16) -> Result<(), VncError> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 {
        // A zero-dimension rectangle only ever comes from a corrupted stream.
        return Err(VncError::InvalidImageData);
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION || pixels > MAX_PIXELS as u64 {
        // Distinct from corruption: the peer is asking for a framebuffer this
        // client refuses to allocate. Reported with the numbers so the user can
        // act (lower the Mac's resolution) instead of hunting a decoder bug.
        return Err(VncError::FramebufferTooLarge {
            width,
            height,
            pixels,
            max_pixels: MAX_PIXELS as u64,
            max_dimension: MAX_DIMENSION,
        });
    }
    Ok(())
}

pub(crate) fn rectangle(rect: &Rect, screen: (u16, u16)) -> Result<(), VncError> {
    dimensions(rect.width, rect.height)?;
    if u32::from(rect.x) + u32::from(rect.width) > u32::from(screen.0)
        || u32::from(rect.y) + u32::from(rect.height) > u32::from(screen.1)
    {
        return Err(VncError::InvalidImageData);
    }
    Ok(())
}

pub(crate) async fn string<S: AsyncRead + Unpin>(
    reader: &mut S,
    limit: usize,
) -> Result<String, VncError> {
    let length = reader.read_u32().await? as usize;
    if length > limit {
        return Err(VncError::General("VNC string exceeds size limit".into()));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_5k_retina_framebuffers() {
        // Real device: Mac mini M4 on a 5120x2880 display. The old 8,294,400
        // pixel cap rejected it right after authentication, which surfaced as
        // "image data cannot be decoded correctly".
        dimensions(5120, 2880).expect("5K must be accepted");
        dimensions(6016, 3384).expect("6K must be accepted");
        dimensions(8960, 2880).expect("virtual + physical side-by-side must be accepted");
        dimensions(3840, 2160).expect("4K must be accepted");
        dimensions(1, 1).expect("tiny framebuffers are fine");
    }

    #[test]
    fn rejects_oversized_with_a_distinct_error() {
        match dimensions(8192, 8192) {
            Err(VncError::FramebufferTooLarge {
                pixels,
                max_pixels,
                max_dimension,
                ..
            }) => {
                assert_eq!(pixels, 8192 * 8192);
                assert_eq!(max_pixels, MAX_PIXELS as u64);
                assert_eq!(max_dimension, MAX_DIMENSION);
            }
            other => panic!("expected FramebufferTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn rejects_zero_dimensions_as_corruption() {
        // Zero dimensions only ever come from a desynchronised stream, so they
        // stay "invalid image data" rather than the size-limit error.
        assert!(matches!(
            dimensions(0, 100),
            Err(VncError::InvalidImageData)
        ));
        assert!(matches!(
            dimensions(100, 0),
            Err(VncError::InvalidImageData)
        ));
    }
}
