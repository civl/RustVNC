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
/// Apple pseudo-encoding bodies are tiny metadata blobs (the three observed on
/// a real Mac — 1107/1109/1110 — are 20, 31 and 45 bytes), so anything in the
/// kilobytes is a desynchronised stream, not a capability list.
///
/// The wire length is a `u16`, i.e. at most 65535; this cap must therefore stay
/// below that or the check below it can never fire.
pub(crate) const MAX_PSEUDO_PAYLOAD: usize = 16 * 1024;
/// Total Apple pseudo-encoding body bytes accepted in one `FramebufferUpdate`.
///
/// A batch advertises its rectangle count as a `u16`, and each Apple rectangle
/// carries its own `u16` length prefix, so per-message caps alone still allow
/// 65535 × 16 KiB of metadata from a single message. Real updates carry at most
/// a handful of bodies; 1 MiB leaves two orders of magnitude of headroom.
pub(crate) const MAX_PSEUDO_BUDGET: usize = 1024 * 1024;
pub(crate) const MAX_NAME: usize = 4096;

/// Largest compressed payload a rectangle may announce, given the pixel bytes
/// it has to expand to.
///
/// Every compressed encoding here (`zlib`, `ZRLE`, `TRLE`) is a zlib stream
/// whose worst-case expansion is a few bytes per 64 KiB block, far below the
/// 12.5 % + 64 B headroom allowed here, so a legitimate stream always fits.
/// Without this bound a 1×1 rectangle could announce `MAX_COMPRESSED`
/// (64 MiB) and force that allocation before a single payload byte arrives.
pub(crate) fn max_compressed_for(expected: usize) -> usize {
    expected
        .saturating_add(expected / 8)
        .saturating_add(64)
        .min(MAX_COMPRESSED)
}

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
    fn pseudo_payload_cap_can_actually_fire() {
        // Read through `black_box` so the assertion is checked at run time
        // rather than being folded into a lint-visible constant.
        let payload = std::hint::black_box(MAX_PSEUDO_PAYLOAD);
        let budget = std::hint::black_box(MAX_PSEUDO_BUDGET);
        // The length comes off the wire as a u16, so a cap at or above 65536
        // would make the decoder's check unreachable — the bug this constant
        // used to have.
        assert!(payload < usize::from(u16::MAX), "the cap must be reachable");
        assert!(payload >= 64, "must fit every observed Apple body");
        assert!(budget >= payload, "a batch may always carry one body");
    }

    #[test]
    fn compressed_bound_follows_the_expected_pixels() {
        // A 1x1 rectangle at 32 bpp may announce a few dozen bytes, not 64 MiB.
        assert_eq!(max_compressed_for(4), 68);
        // Larger rectangles get proportional slack, never more than the global cap.
        assert_eq!(max_compressed_for(1 << 20), (1 << 20) + (1 << 17) + 64);
        assert_eq!(max_compressed_for(usize::MAX), MAX_COMPRESSED);
        assert_eq!(max_compressed_for(0), 64, "never a zero allowance");
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
