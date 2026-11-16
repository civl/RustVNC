use egui::Key;

/// X11 keysyms used by RFB KeyEvent.
pub const KEY_RETURN: u32 = 0xff0d;
pub const KEY_TAB: u32 = 0xff09;
pub const KEY_BACKSPACE: u32 = 0xff08;
pub const KEY_ESCAPE: u32 = 0xff1b;
pub const KEY_DELETE: u32 = 0xffff;
pub const KEY_SPACE: u32 = 0x20;

pub fn key_to_keysym(key: Key, shift: bool) -> Option<u32> {
    let sym = match key {
        Key::A => ascii(b'a', shift),
        Key::B => ascii(b'b', shift),
        Key::C => ascii(b'c', shift),
        Key::D => ascii(b'd', shift),
        Key::E => ascii(b'e', shift),
        Key::F => ascii(b'f', shift),
        Key::G => ascii(b'g', shift),
        Key::H => ascii(b'h', shift),
        Key::I => ascii(b'i', shift),
        Key::J => ascii(b'j', shift),
        Key::K => ascii(b'k', shift),
        Key::L => ascii(b'l', shift),
        Key::M => ascii(b'm', shift),
        Key::N => ascii(b'n', shift),
        Key::O => ascii(b'o', shift),
        Key::P => ascii(b'p', shift),
        Key::Q => ascii(b'q', shift),
        Key::R => ascii(b'r', shift),
        Key::S => ascii(b's', shift),
        Key::T => ascii(b't', shift),
        Key::U => ascii(b'u', shift),
        Key::V => ascii(b'v', shift),
        Key::W => ascii(b'w', shift),
        Key::X => ascii(b'x', shift),
        Key::Y => ascii(b'y', shift),
        Key::Z => ascii(b'z', shift),
        Key::Num0 => b'0' as u32,
        Key::Num1 => b'1' as u32,
        Key::Num2 => b'2' as u32,
        Key::Num3 => b'3' as u32,
        Key::Num4 => b'4' as u32,
        Key::Num5 => b'5' as u32,
        Key::Num6 => b'6' as u32,
        Key::Num7 => b'7' as u32,
        Key::Num8 => b'8' as u32,
        Key::Num9 => b'9' as u32,
        Key::Space => KEY_SPACE,
        Key::Enter => KEY_RETURN,
        Key::Tab => KEY_TAB,
        Key::Backspace => KEY_BACKSPACE,
        Key::Escape => KEY_ESCAPE,
        Key::Delete => KEY_DELETE,
        Key::Insert => 0xff63,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::ArrowLeft => 0xff51,
        Key::ArrowUp => 0xff52,
        Key::ArrowRight => 0xff53,
        Key::ArrowDown => 0xff54,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        Key::Minus => punctuation(b'-', b'_', shift),
        Key::Equals => punctuation(b'=', b'+', shift),
        Key::OpenBracket => punctuation(b'[', b'{', shift),
        Key::CloseBracket => punctuation(b']', b'}', shift),
        Key::Backslash => punctuation(b'\\', b'|', shift),
        Key::Semicolon => punctuation(b';', b':', shift),
        Key::Quote => punctuation(b'\'', b'"', shift),
        Key::Comma => punctuation(b',', b'<', shift),
        Key::Period => punctuation(b'.', b'>', shift),
        Key::Slash => punctuation(b'/', b'?', shift),
        Key::Backtick => punctuation(b'`', b'~', shift),
        _ => return None,
    };
    Some(sym)
}

/// Convert committed text (including CJK) into RFB keysyms.
///
/// Printable ASCII uses the ASCII keysym; everything else uses the Unicode
/// keysym form 0x01000000 + codepoint.
pub fn text_to_keysyms(text: &str) -> Vec<u32> {
    let mut keysyms = Vec::new();
    for ch in text.chars() {
        match ch {
            '\n' => keysyms.push(KEY_RETURN),
            '\r' => {}
            '\t' => keysyms.push(KEY_TAB),
            ' ' => keysyms.push(KEY_SPACE),
            ch if ch.is_ascii_graphic() => keysyms.push(ch as u32),
            ch => keysyms.push(0x0100_0000 | ch as u32),
        }
    }
    keysyms
}

fn ascii(lower: u8, shift: bool) -> u32 {
    if shift {
        lower.to_ascii_uppercase() as u32
    } else {
        lower as u32
    }
}

fn punctuation(normal: u8, shifted: u8, shift: bool) -> u32 {
    if shift { shifted as u32 } else { normal as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_and_shift() {
        assert_eq!(key_to_keysym(Key::A, false), Some(0x61));
        assert_eq!(key_to_keysym(Key::A, true), Some(0x41));
        assert_eq!(key_to_keysym(Key::Enter, false), Some(KEY_RETURN));
    }

    #[test]
    fn unicode_text_uses_unicode_keysyms() {
        let keysyms = text_to_keysyms("A中\n");
        assert_eq!(keysyms[0], 0x41);
        assert_eq!(keysyms[1], 0x0100_0000 | ('中' as u32));
        assert_eq!(keysyms[2], KEY_RETURN);
    }
}
