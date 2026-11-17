//! Apple's private pseudo-encodings (1104/1105/1107/1109/1110).
//!
//! Apple does not document these. What is known was established by capturing a
//! real session against a Mac mini M4 (macOS 27.2 _sic_, `RFB 003.889`) and is
//! reproduced verbatim in the tests below:
//!
//! ```text
//! 1107 (capabilities)  body = 00 01 | 00 04 | 10 08 fd 00 … fd 03     20 bytes
//! 1109 (keyboard)      body = 00 01 00 00 00 00 | 00 17 | "com.apple.keylayout.ABC"
//! 1110 (machine)       body = 00 02 … 00 09 | "Mac16,10\0" "unknown\0" "unknown\0"
//! ```
//!
//! Every body is length-prefixed on the wire (`u16` before the body), which is
//! what the decoder uses to skip one it cannot interpret. This module only
//! extracts the pieces whose shape is obvious; anything else is handed back as
//! raw bytes rather than guessed at, because a wrong guess would silently
//! mis-drive the UI.

/// What an Apple pseudo-encoding announcement carried.
#[derive(Debug, Clone, PartialEq)]
pub enum AppleInfo {
    /// 1104: cursor shape is carried by the rectangle, body is empty.
    Cursor,
    /// 1105: display geometry. The exact order is not documented by Apple; the
    /// decoder accepts the common `u16 width, u16 height, f64 BE scale,
    /// u32 BE display id` layout and a `u32` variant, validating every field
    /// before exposing it. `raw` is always retained for diagnostics.
    Display {
        pixel_width: u16,
        pixel_height: u16,
        scale: f64,
        display_id: u32,
        raw: Vec<u8>,
    },
    /// 1107: a capability list, four bytes per entry.
    Capabilities(Vec<[u8; 4]>),
    /// 1109: e.g. `com.apple.keylayout.ABC`.
    Keyboard(String),
    /// 1110: machine model, e.g. `Mac16,10`.
    Machine(String),
    /// Anything else, kept verbatim.
    Unknown { code: i32, raw: Vec<u8> },
}

pub const APPLE_CURSOR: i32 = 1104;
pub const APPLE_DISPLAY: i32 = 1105;
pub const APPLE_CAPABILITIES: i32 = 1107;
pub const APPLE_KEYBOARD: i32 = 1109;
pub const APPLE_MACHINE: i32 = 1110;

pub fn is_apple(code: i32) -> bool {
    matches!(
        code,
        APPLE_CURSOR | APPLE_DISPLAY | APPLE_CAPABILITIES | APPLE_KEYBOARD | APPLE_MACHINE
    )
}

pub fn parse(code: i32, body: &[u8]) -> AppleInfo {
    match code {
        APPLE_CURSOR => AppleInfo::Cursor,
        APPLE_CAPABILITIES => {
            // 2-byte version, 2-byte count, then count 4-byte entries.
            let entries = body
                .get(4..)
                .map(|rest| {
                    rest.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|chunk| [chunk[0], chunk[1], chunk[2], chunk[3]])
                        .collect()
                })
                .unwrap_or_default();
            AppleInfo::Capabilities(entries)
        }
        APPLE_KEYBOARD => match first_printable_run(body, 5) {
            Some(layout) => AppleInfo::Keyboard(layout),
            None => AppleInfo::Unknown {
                code,
                raw: body.to_vec(),
            },
        },
        APPLE_MACHINE => match first_printable_run(body, 5) {
            Some(model) => AppleInfo::Machine(model),
            None => AppleInfo::Unknown {
                code,
                raw: body.to_vec(),
            },
        },
        APPLE_DISPLAY => parse_display(body),
        _ => AppleInfo::Unknown {
            code,
            raw: body.to_vec(),
        },
    }
}

fn parse_display(body: &[u8]) -> AppleInfo {
    if let Some((pixel_width, pixel_height, scale, display_id)) = display_from_u16(body) {
        return AppleInfo::Display {
            pixel_width,
            pixel_height,
            scale,
            display_id,
            raw: body.to_vec(),
        };
    }
    if let Some((pixel_width, pixel_height, scale, display_id)) = display_from_u32(body) {
        return AppleInfo::Display {
            pixel_width,
            pixel_height,
            scale,
            display_id,
            raw: body.to_vec(),
        };
    }
    AppleInfo::Unknown {
        code: APPLE_DISPLAY,
        raw: body.to_vec(),
    }
}

fn display_from_u16(body: &[u8]) -> Option<(u16, u16, f64, u32)> {
    if body.len() < 16 {
        return None;
    }
    let pixel_width = u16::from_be_bytes([body[0], body[1]]);
    let pixel_height = u16::from_be_bytes([body[2], body[3]]);
    let scale = f64::from_be_bytes(body[4..12].try_into().ok()?);
    let display_id = u32::from_be_bytes(body[12..16].try_into().ok()?);
    valid_display(pixel_width, pixel_height, scale, display_id)
}

fn display_from_u32(body: &[u8]) -> Option<(u16, u16, f64, u32)> {
    if body.len() < 20 {
        return None;
    }
    let pixel_width = u32::from_be_bytes(body[0..4].try_into().ok()?);
    let pixel_height = u32::from_be_bytes(body[4..8].try_into().ok()?);
    let scale = f64::from_be_bytes(body[8..16].try_into().ok()?);
    let display_id = u32::from_be_bytes(body[16..20].try_into().ok()?);
    if pixel_width > u16::MAX as u32 || pixel_height > u16::MAX as u32 {
        return None;
    }
    valid_display(pixel_width as u16, pixel_height as u16, scale, display_id)
}

fn valid_display(
    width: u16,
    height: u16,
    scale: f64,
    display_id: u32,
) -> Option<(u16, u16, f64, u32)> {
    let plausible = (320..=32768).contains(&width)
        && (200..=32768).contains(&height)
        && scale.is_finite()
        && (0.5..=8.0).contains(&scale);
    plausible.then_some((width, height, scale, display_id))
}

/// The first run of printable ASCII of at least `min` bytes.
///
/// Apple's headers around the string differ between encodings and macOS
/// versions; the string itself does not. Matching on its shape keeps this from
/// breaking when a header field changes, and yields `None` instead of garbage
/// when the body is not what we expect.
fn first_printable_run(body: &[u8], min: usize) -> Option<String> {
    let mut start = None;
    for (index, byte) in body.iter().chain(std::iter::once(&0u8)).enumerate() {
        let printable = byte.is_ascii_graphic() || *byte == b' ';
        match (printable, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                if index - begin >= min {
                    return Some(String::from_utf8_lossy(&body[begin..index]).into_owned());
                }
                start = None;
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real Mac mini M4 session (2026-11-17).
    const CAPTURED_1107: &[u8] = &[
        0x00, 0x01, 0x00, 0x04, 0x10, 0x08, 0xfd, 0x00, 0x10, 0x08, 0xfd, 0x01, 0x10, 0x08, 0xfd,
        0x02, 0x10, 0x08, 0xfd, 0x03,
    ];
    const CAPTURED_1109: &[u8] = &[
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x17, b'c', b'o', b'm', b'.', b'a', b'p', b'p',
        b'l', b'e', b'.', b'k', b'e', b'y', b'l', b'a', b'y', b'o', b'u', b't', b'.', b'A', b'B',
        b'C',
    ];
    const CAPTURED_1110: &[u8] = &[
        0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x00, 0x08, 0x00,
        0x08, b'M', b'a', b'c', b'1', b'6', b',', b'1', b'0', 0x00, b'u', b'n', b'k', b'n', b'o',
        b'w', b'n', 0x00, b'u', b'n', b'k', b'n', b'o', b'w', b'n', 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn parses_the_captured_keyboard_layout() {
        assert_eq!(
            parse(APPLE_KEYBOARD, CAPTURED_1109),
            AppleInfo::Keyboard("com.apple.keylayout.ABC".to_string())
        );
    }

    #[test]
    fn parses_the_captured_machine_model() {
        assert_eq!(
            parse(APPLE_MACHINE, CAPTURED_1110),
            AppleInfo::Machine("Mac16,10".to_string())
        );
    }

    #[test]
    fn parses_the_captured_capability_list() {
        let AppleInfo::Capabilities(entries) = parse(APPLE_CAPABILITIES, CAPTURED_1107) else {
            panic!("expected capability list");
        };
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0], [0x10, 0x08, 0xfd, 0x00]);
        assert_eq!(entries[3], [0x10, 0x08, 0xfd, 0x03]);
    }

    #[test]
    fn garbage_bodies_do_not_produce_invented_strings() {
        // A body that is not the expected shape must come back as Unknown, never
        // as a half-decoded string.
        assert!(matches!(
            parse(APPLE_MACHINE, &[0x00, 0x02, 0x00]),
            AppleInfo::Unknown { .. }
        ));
        assert!(matches!(
            parse(APPLE_KEYBOARD, &[0xff; 3]),
            AppleInfo::Unknown { .. }
        ));
        assert!(matches!(
            parse(APPLE_DISPLAY, CAPTURED_1110),
            AppleInfo::Unknown { .. }
        ));
        assert_eq!(parse(APPLE_CURSOR, &[]), AppleInfo::Cursor);
    }

    #[test]
    fn parses_a_candidate_display_geometry_layout() {
        let mut body = Vec::new();
        body.extend_from_slice(&1920u16.to_be_bytes());
        body.extend_from_slice(&1080u16.to_be_bytes());
        body.extend_from_slice(&2.0f64.to_be_bytes());
        body.extend_from_slice(&0x1234_5678u32.to_be_bytes());
        let AppleInfo::Display {
            pixel_width,
            pixel_height,
            scale,
            display_id,
            ..
        } = parse(APPLE_DISPLAY, &body)
        else {
            panic!("expected display geometry");
        };
        assert_eq!((pixel_width, pixel_height), (1920, 1080));
        assert_eq!(scale, 2.0);
        assert_eq!(display_id, 0x1234_5678);
    }

    #[test]
    fn rejects_implausible_display_geometry() {
        let mut body = Vec::new();
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&0.0f64.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        assert!(matches!(
            parse(APPLE_DISPLAY, &body),
            AppleInfo::Unknown { .. }
        ));
    }

    #[test]
    fn recognises_only_the_known_codes() {
        for code in [1104, 1105, 1107, 1109, 1110] {
            assert!(is_apple(code));
        }
        for code in [1100, 1101, 1106, 1111, -239, 0] {
            assert!(!is_apple(code));
        }
    }
}
