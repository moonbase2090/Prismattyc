//! Keyboard encoding for the windowed host (legacy xterm subset).
//!
//! winit delivers keys as [`Key::Named`], [`Key::Character`], and optional
//! `text` / physical [`KeyCode`]. Terminal hosts must handle **all three** —
//! Space was dropped because it arrives as `NamedKey::Space`, not `" "`.
//!
//! Nested `prism` still owns the full Kitty CSI-u path; this module matches
//! legacy encodings for interactive shell use.

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

/// Encode a winit key event for the child PTY.
///
/// `None` = ignore (pure modifiers, media keys, unknown).
pub fn encode_key_event(
    logical: &Key,
    physical: PhysicalKey,
    text: Option<&str>,
    modifiers: ModifiersState,
) -> Option<Vec<u8>> {
    let shift = modifiers.shift_key();
    let alt = modifiers.alt_key();
    let ctrl = modifiers.control_key();
    let super_key = modifiers.super_key();
    let mod_param = xterm_mod_param(shift, alt, ctrl, super_key);

    // 1) Named keys (Space, Enter, arrows, F-keys, …) — do this before `text`
    // so Enter is always CR, not platform-dependent.
    if let Key::Named(named) = logical {
        if is_modifier_only(*named) {
            return None;
        }
        if let Some(bytes) = encode_named(*named, mod_param, shift, ctrl) {
            return Some(bytes);
        }
        // Unmapped named keys (media, browser, …): do not fall through to
        // garbage text if any.
        if !is_terminal_fallback_named(*named) {
            return None;
        }
    }

    // 2) Logical character (letters, digits, punctuation).
    if let Key::Character(s) = logical {
        return encode_character_str(s.as_str(), ctrl, alt, mod_param);
    }

    // 3) Composed / IME / some compositors only fill `text`.
    if let Some(t) = text {
        if !t.is_empty() {
            return encode_text_fallback(t, ctrl, alt, mod_param);
        }
    }

    // 4) Physical key code when logical is Unidentified (rare, but real).
    if let PhysicalKey::Code(code) = physical {
        return encode_keycode(code, mod_param, shift, ctrl, alt);
    }

    None
}

/// Back-compat helper used by unit tests.
#[cfg(test)]
pub fn encode_key(key: &Key, modifiers: ModifiersState) -> Option<Vec<u8>> {
    encode_key_event(
        key,
        PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
        None,
        modifiers,
    )
}

fn encode_text_fallback(t: &str, ctrl: bool, alt: bool, mod_param: u8) -> Option<Vec<u8>> {
    match t {
        "\r" | "\n" => Some(vec![b'\r']),
        "\t" => Some(vec![b'\t']),
        "\u{1b}" => Some(vec![0x1b]),
        "\u{7f}" | "\u{08}" => Some(vec![0x7f]),
        _ => encode_character_str(t, ctrl, alt, mod_param),
    }
}

fn encode_character_str(s: &str, ctrl: bool, alt: bool, _mod_param: u8) -> Option<Vec<u8>> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        // Multi-codepoint cluster (emoji, etc.): send UTF-8 as-is.
        return Some(s.as_bytes().to_vec());
    }
    encode_char(c, ctrl, alt)
}

fn encode_char(c: char, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    if ctrl {
        if let Some(b) = ctrl_byte(c) {
            return Some(vec![b]);
        }
    }

    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }
    let mut buf = [0u8; 4];
    let enc = c.encode_utf8(&mut buf);
    out.extend_from_slice(enc.as_bytes());
    Some(out)
}

/// ASCII control mapping for Ctrl+key (xterm/bash common set).
fn ctrl_byte(c: char) -> Option<u8> {
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        return Some((lower as u8) - b'a' + 1);
    }
    Some(match c {
        '@' | ' ' | '2' => 0x00,       // NUL (Ctrl+Space / Ctrl+2 / Ctrl+@)
        '[' | '3' => 0x1b,             // ESC
        '\\' | '4' => 0x1c,            // FS
        ']' | '5' => 0x1d,             // GS
        '^' | '6' | '~' => 0x1e,       // RS
        '_' | '7' | '?' | '/' => 0x1f, // US / sometimes DEL path
        '8' => 0x7f,                   // DEL
        // Digits 0/1 and other punctuation: no standard single-byte Ctrl.
        _ => return None,
    })
}

fn encode_named(named: NamedKey, mod_param: u8, shift: bool, ctrl: bool) -> Option<Vec<u8>> {
    match named {
        NamedKey::Enter => Some(vec![b'\r']),
        NamedKey::Tab if shift => Some(b"\x1b[Z".to_vec()),
        NamedKey::Tab => Some(vec![b'\t']),
        NamedKey::Space if ctrl => Some(vec![0x00]),
        NamedKey::Space => Some(vec![b' ']),
        NamedKey::Backspace => Some(vec![0x7f]),
        NamedKey::Escape | NamedKey::Cancel => Some(vec![0x1b]),

        NamedKey::ArrowUp => Some(csi_mod("1", b'A', mod_param)),
        NamedKey::ArrowDown => Some(csi_mod("1", b'B', mod_param)),
        NamedKey::ArrowRight => Some(csi_mod("1", b'C', mod_param)),
        NamedKey::ArrowLeft => Some(csi_mod("1", b'D', mod_param)),
        NamedKey::Home => Some(csi_mod("1", b'H', mod_param)),
        NamedKey::End => Some(csi_mod("1", b'F', mod_param)),
        NamedKey::PageUp => Some(csi_mod("5", b'~', mod_param)),
        NamedKey::PageDown => Some(csi_mod("6", b'~', mod_param)),
        NamedKey::Delete => Some(csi_mod("3", b'~', mod_param)),
        NamedKey::Insert => Some(csi_mod("2", b'~', mod_param)),

        // Editing cluster (when compositors expose them as named).
        NamedKey::Clear => Some(b"\x1b[3;5~".to_vec()), // uncommon; best-effort
        NamedKey::Copy | NamedKey::Cut | NamedKey::Paste | NamedKey::Undo | NamedKey::Redo => None,

        _ => encode_function_key(named, mod_param),
    }
}

fn encode_function_key(named: NamedKey, mod_param: u8) -> Option<Vec<u8>> {
    match named {
        NamedKey::F1 => Some(plain_or_mod_f(b"\x1bOP", "1", b'P', mod_param)),
        NamedKey::F2 => Some(plain_or_mod_f(b"\x1bOQ", "1", b'Q', mod_param)),
        NamedKey::F3 => Some(plain_or_mod_f(b"\x1bOR", "1", b'R', mod_param)),
        NamedKey::F4 => Some(plain_or_mod_f(b"\x1bOS", "1", b'S', mod_param)),
        NamedKey::F5 => Some(csi_mod("15", b'~', mod_param)),
        NamedKey::F6 => Some(csi_mod("17", b'~', mod_param)),
        NamedKey::F7 => Some(csi_mod("18", b'~', mod_param)),
        NamedKey::F8 => Some(csi_mod("19", b'~', mod_param)),
        NamedKey::F9 => Some(csi_mod("20", b'~', mod_param)),
        NamedKey::F10 => Some(csi_mod("21", b'~', mod_param)),
        NamedKey::F11 => Some(csi_mod("23", b'~', mod_param)),
        NamedKey::F12 => Some(csi_mod("24", b'~', mod_param)),
        // xterm F13–F20 (common enough for some keyboards / bindings).
        NamedKey::F13 => Some(csi_mod("25", b'~', mod_param)),
        NamedKey::F14 => Some(csi_mod("26", b'~', mod_param)),
        NamedKey::F15 => Some(csi_mod("28", b'~', mod_param)),
        NamedKey::F16 => Some(csi_mod("29", b'~', mod_param)),
        NamedKey::F17 => Some(csi_mod("31", b'~', mod_param)),
        NamedKey::F18 => Some(csi_mod("32", b'~', mod_param)),
        NamedKey::F19 => Some(csi_mod("33", b'~', mod_param)),
        NamedKey::F20 => Some(csi_mod("34", b'~', mod_param)),

        _ => None,
    }
}

/// Named keys that may still have useful `text` (none today — reserved).
fn is_terminal_fallback_named(_named: NamedKey) -> bool {
    false
}

fn is_modifier_only(named: NamedKey) -> bool {
    matches!(
        named,
        NamedKey::Shift
            | NamedKey::Control
            | NamedKey::Alt
            | NamedKey::AltGraph
            | NamedKey::Super
            | NamedKey::Meta
            | NamedKey::Hyper
            | NamedKey::CapsLock
            | NamedKey::NumLock
            | NamedKey::ScrollLock
            | NamedKey::Fn
            | NamedKey::FnLock
            | NamedKey::Symbol
            | NamedKey::SymbolLock
    )
}

fn encode_keycode(
    code: KeyCode,
    mod_param: u8,
    shift: bool,
    ctrl: bool,
    alt: bool,
) -> Option<Vec<u8>> {
    if let Some(named) = physical_named_key(code) {
        return encode_named(named, mod_param, shift, ctrl);
    }
    let ch = physical_character(code, shift)?;
    encode_char(ch, ctrl, alt)
}

fn physical_named_key(code: KeyCode) -> Option<NamedKey> {
    Some(match code {
        KeyCode::Space => NamedKey::Space,
        KeyCode::Enter | KeyCode::NumpadEnter => NamedKey::Enter,
        KeyCode::Tab => NamedKey::Tab,
        KeyCode::Backspace => NamedKey::Backspace,
        KeyCode::Escape => NamedKey::Escape,
        KeyCode::ArrowUp => NamedKey::ArrowUp,
        KeyCode::ArrowDown => NamedKey::ArrowDown,
        KeyCode::ArrowRight => NamedKey::ArrowRight,
        KeyCode::ArrowLeft => NamedKey::ArrowLeft,
        KeyCode::Home => NamedKey::Home,
        KeyCode::End => NamedKey::End,
        KeyCode::PageUp => NamedKey::PageUp,
        KeyCode::PageDown => NamedKey::PageDown,
        KeyCode::Delete => NamedKey::Delete,
        KeyCode::Insert => NamedKey::Insert,
        KeyCode::F1 => NamedKey::F1,
        KeyCode::F2 => NamedKey::F2,
        KeyCode::F3 => NamedKey::F3,
        KeyCode::F4 => NamedKey::F4,
        KeyCode::F5 => NamedKey::F5,
        KeyCode::F6 => NamedKey::F6,
        KeyCode::F7 => NamedKey::F7,
        KeyCode::F8 => NamedKey::F8,
        KeyCode::F9 => NamedKey::F9,
        KeyCode::F10 => NamedKey::F10,
        KeyCode::F11 => NamedKey::F11,
        KeyCode::F12 => NamedKey::F12,
        _ => return None,
    })
}

fn physical_character(code: KeyCode, shift: bool) -> Option<char> {
    if let Some(letter) = physical_letter(code) {
        return Some(if shift {
            letter.to_ascii_uppercase()
        } else {
            letter
        });
    }
    if let Some(number) = physical_number_or_operator(code) {
        return Some(number);
    }
    let (plain, shifted) = physical_punctuation(code)?;
    Some(if shift { shifted } else { plain })
}

fn physical_letter(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        _ => return None,
    })
}

fn physical_number_or_operator(code: KeyCode) -> Option<char> {
    Some(match code {
        KeyCode::Digit0 | KeyCode::Numpad0 => '0',
        KeyCode::Digit1 | KeyCode::Numpad1 => '1',
        KeyCode::Digit2 | KeyCode::Numpad2 => '2',
        KeyCode::Digit3 | KeyCode::Numpad3 => '3',
        KeyCode::Digit4 | KeyCode::Numpad4 => '4',
        KeyCode::Digit5 | KeyCode::Numpad5 => '5',
        KeyCode::Digit6 | KeyCode::Numpad6 => '6',
        KeyCode::Digit7 | KeyCode::Numpad7 => '7',
        KeyCode::Digit8 | KeyCode::Numpad8 => '8',
        KeyCode::Digit9 | KeyCode::Numpad9 => '9',
        KeyCode::NumpadAdd => '+',
        KeyCode::NumpadSubtract => '-',
        KeyCode::NumpadMultiply => '*',
        KeyCode::NumpadDivide => '/',
        KeyCode::NumpadDecimal => '.',
        _ => return None,
    })
}

fn physical_punctuation(code: KeyCode) -> Option<(char, char)> {
    Some(match code {
        KeyCode::Minus => ('-', '_'),
        KeyCode::Equal => ('=', '+'),
        KeyCode::BracketLeft => ('[', '{'),
        KeyCode::BracketRight => (']', '}'),
        KeyCode::Backslash => ('\\', '|'),
        KeyCode::Semicolon => (';', ':'),
        KeyCode::Quote => ('\'', '"'),
        KeyCode::Backquote => ('`', '~'),
        KeyCode::Comma => (',', '<'),
        KeyCode::Period => ('.', '>'),
        KeyCode::Slash => ('/', '?'),
        _ => return None,
    })
}

fn plain_or_mod_f(plain: &[u8], intermediate: &str, final_byte: u8, mod_param: u8) -> Vec<u8> {
    if mod_param == 1 {
        plain.to_vec()
    } else {
        csi_mod(intermediate, final_byte, mod_param)
    }
}

fn csi_mod(intermediate: &str, final_byte: u8, mod_param: u8) -> Vec<u8> {
    if mod_param == 1 {
        if intermediate == "1" {
            return vec![0x1b, b'[', final_byte];
        }
        let mut v = format!("\x1b[{intermediate}").into_bytes();
        v.push(final_byte);
        return v;
    }
    if intermediate == "1" && matches!(final_byte, b'A' | b'B' | b'C' | b'D' | b'H' | b'F') {
        return format!("\x1b[1;{mod_param}{final}", final = final_byte as char).into_bytes();
    }
    format!(
        "\x1b[{intermediate};{mod_param}{final}",
        final = final_byte as char
    )
    .into_bytes()
}

/// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl (+ 8 super when present).
fn xterm_mod_param(shift: bool, alt: bool, ctrl: bool, super_key: bool) -> u8 {
    let mut p = 1u8;
    if shift {
        p += 1;
    }
    if alt {
        p += 2;
    }
    if ctrl {
        p += 4;
    }
    if super_key {
        p += 8;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::Key;

    fn named(n: NamedKey) -> Key {
        Key::Named(n)
    }

    fn mods_ctrl() -> ModifiersState {
        let mut m = ModifiersState::empty();
        m.set(ModifiersState::CONTROL, true);
        m
    }

    fn mods_shift() -> ModifiersState {
        let mut m = ModifiersState::empty();
        m.set(ModifiersState::SHIFT, true);
        m
    }

    fn mods_alt() -> ModifiersState {
        let mut m = ModifiersState::empty();
        m.set(ModifiersState::ALT, true);
        m
    }

    #[test]
    fn ctrl_c_is_etx() {
        let key = Key::Character("c".into());
        assert_eq!(encode_key(&key, mods_ctrl()), Some(vec![0x03]));
    }

    #[test]
    fn plain_arrow_up_is_csi_a() {
        assert_eq!(
            encode_key(&named(NamedKey::ArrowUp), ModifiersState::empty()),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn enter_is_cr() {
        assert_eq!(
            encode_key(&named(NamedKey::Enter), ModifiersState::empty()),
            Some(vec![b'\r'])
        );
    }

    #[test]
    fn named_space_is_ascii_space() {
        assert_eq!(
            encode_key(&named(NamedKey::Space), ModifiersState::empty()),
            Some(vec![b' '])
        );
    }

    #[test]
    fn character_space_is_ascii_space() {
        assert_eq!(
            encode_key(&Key::Character(" ".into()), ModifiersState::empty()),
            Some(vec![b' '])
        );
    }

    #[test]
    fn ctrl_space_is_nul() {
        assert_eq!(
            encode_key(&named(NamedKey::Space), mods_ctrl()),
            Some(vec![0x00])
        );
    }

    #[test]
    fn shift_tab_is_csi_z() {
        assert_eq!(
            encode_key(&named(NamedKey::Tab), mods_shift()),
            Some(b"\x1b[Z".to_vec())
        );
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            encode_key(&named(NamedKey::Backspace), ModifiersState::empty()),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn escape_is_esc() {
        assert_eq!(
            encode_key(&named(NamedKey::Escape), ModifiersState::empty()),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn modifiers_alone_are_ignored() {
        for n in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::CapsLock,
            NamedKey::NumLock,
        ] {
            assert_eq!(
                encode_key(&named(n), ModifiersState::empty()),
                None,
                "{n:?} should be ignored"
            );
        }
    }

    #[test]
    fn media_keys_ignored() {
        assert_eq!(
            encode_key(&named(NamedKey::AudioVolumeUp), ModifiersState::empty()),
            None
        );
        assert_eq!(
            encode_key(&named(NamedKey::BrowserBack), ModifiersState::empty()),
            None
        );
    }

    #[test]
    fn encode_named_table_covers_terminal_named_keys() {
        let cases = [
            (NamedKey::Enter, b"\r".to_vec()),
            (NamedKey::Tab, b"\t".to_vec()),
            (NamedKey::Space, b" ".to_vec()),
            (NamedKey::Backspace, vec![0x7f]),
            (NamedKey::Escape, vec![0x1b]),
            (NamedKey::Cancel, vec![0x1b]),
            (NamedKey::ArrowUp, b"\x1b[A".to_vec()),
            (NamedKey::ArrowDown, b"\x1b[B".to_vec()),
            (NamedKey::ArrowRight, b"\x1b[C".to_vec()),
            (NamedKey::ArrowLeft, b"\x1b[D".to_vec()),
            (NamedKey::Home, b"\x1b[H".to_vec()),
            (NamedKey::End, b"\x1b[F".to_vec()),
            (NamedKey::PageUp, b"\x1b[5~".to_vec()),
            (NamedKey::PageDown, b"\x1b[6~".to_vec()),
            (NamedKey::Delete, b"\x1b[3~".to_vec()),
            (NamedKey::Insert, b"\x1b[2~".to_vec()),
            (NamedKey::F1, b"\x1bOP".to_vec()),
            (NamedKey::F2, b"\x1bOQ".to_vec()),
            (NamedKey::F3, b"\x1bOR".to_vec()),
            (NamedKey::F4, b"\x1bOS".to_vec()),
            (NamedKey::F5, b"\x1b[15~".to_vec()),
            (NamedKey::F6, b"\x1b[17~".to_vec()),
            (NamedKey::F7, b"\x1b[18~".to_vec()),
            (NamedKey::F8, b"\x1b[19~".to_vec()),
            (NamedKey::F9, b"\x1b[20~".to_vec()),
            (NamedKey::F10, b"\x1b[21~".to_vec()),
            (NamedKey::F11, b"\x1b[23~".to_vec()),
            (NamedKey::F12, b"\x1b[24~".to_vec()),
            (NamedKey::F13, b"\x1b[25~".to_vec()),
            (NamedKey::F14, b"\x1b[26~".to_vec()),
            (NamedKey::F15, b"\x1b[28~".to_vec()),
            (NamedKey::F16, b"\x1b[29~".to_vec()),
            (NamedKey::F17, b"\x1b[31~".to_vec()),
            (NamedKey::F18, b"\x1b[32~".to_vec()),
            (NamedKey::F19, b"\x1b[33~".to_vec()),
            (NamedKey::F20, b"\x1b[34~".to_vec()),
            (NamedKey::Clear, b"\x1b[3;5~".to_vec()),
        ];
        for (named, expected) in cases {
            assert_eq!(
                encode_named(named, 1, false, false),
                Some(expected),
                "plain {named:?}"
            );
        }

        for (named, expected) in [
            (NamedKey::ArrowUp, b"\x1b[1;5A".to_vec()),
            (NamedKey::F1, b"\x1b[1;5P".to_vec()),
            (NamedKey::F5, b"\x1b[15;5~".to_vec()),
        ] {
            assert_eq!(
                encode_named(named, 5, false, true),
                Some(expected),
                "Ctrl+{named:?}"
            );
        }
        assert_eq!(
            encode_named(NamedKey::Tab, 2, true, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_named(NamedKey::Space, 5, false, true),
            Some(vec![0x00])
        );
        assert_eq!(encode_named(NamedKey::AudioVolumeUp, 1, false, false), None);
    }

    #[test]
    fn encode_keycode_table_covers_physical_fallbacks() {
        let special = [
            (KeyCode::Space, b" ".to_vec()),
            (KeyCode::Enter, b"\r".to_vec()),
            (KeyCode::Tab, b"\t".to_vec()),
            (KeyCode::Backspace, vec![0x7f]),
            (KeyCode::Escape, vec![0x1b]),
            (KeyCode::ArrowUp, b"\x1b[A".to_vec()),
            (KeyCode::ArrowDown, b"\x1b[B".to_vec()),
            (KeyCode::ArrowRight, b"\x1b[C".to_vec()),
            (KeyCode::ArrowLeft, b"\x1b[D".to_vec()),
            (KeyCode::Home, b"\x1b[H".to_vec()),
            (KeyCode::End, b"\x1b[F".to_vec()),
            (KeyCode::PageUp, b"\x1b[5~".to_vec()),
            (KeyCode::PageDown, b"\x1b[6~".to_vec()),
            (KeyCode::Delete, b"\x1b[3~".to_vec()),
            (KeyCode::Insert, b"\x1b[2~".to_vec()),
            (KeyCode::F1, b"\x1bOP".to_vec()),
            (KeyCode::F2, b"\x1bOQ".to_vec()),
            (KeyCode::F3, b"\x1bOR".to_vec()),
            (KeyCode::F4, b"\x1bOS".to_vec()),
            (KeyCode::F5, b"\x1b[15~".to_vec()),
            (KeyCode::F6, b"\x1b[17~".to_vec()),
            (KeyCode::F7, b"\x1b[18~".to_vec()),
            (KeyCode::F8, b"\x1b[19~".to_vec()),
            (KeyCode::F9, b"\x1b[20~".to_vec()),
            (KeyCode::F10, b"\x1b[21~".to_vec()),
            (KeyCode::F11, b"\x1b[23~".to_vec()),
            (KeyCode::F12, b"\x1b[24~".to_vec()),
            (KeyCode::NumpadEnter, b"\r".to_vec()),
            (KeyCode::NumpadAdd, b"+".to_vec()),
            (KeyCode::NumpadSubtract, b"-".to_vec()),
            (KeyCode::NumpadMultiply, b"*".to_vec()),
            (KeyCode::NumpadDivide, b"/".to_vec()),
            (KeyCode::NumpadDecimal, b".".to_vec()),
        ];
        for (code, expected) in special {
            assert_eq!(
                encode_keycode(code, 1, false, false, false),
                Some(expected),
                "plain {code:?}"
            );
        }

        for (code, digit) in [
            (KeyCode::Numpad0, '0'),
            (KeyCode::Numpad1, '1'),
            (KeyCode::Numpad2, '2'),
            (KeyCode::Numpad3, '3'),
            (KeyCode::Numpad4, '4'),
            (KeyCode::Numpad5, '5'),
            (KeyCode::Numpad6, '6'),
            (KeyCode::Numpad7, '7'),
            (KeyCode::Numpad8, '8'),
            (KeyCode::Numpad9, '9'),
            (KeyCode::Digit0, '0'),
            (KeyCode::Digit1, '1'),
            (KeyCode::Digit2, '2'),
            (KeyCode::Digit3, '3'),
            (KeyCode::Digit4, '4'),
            (KeyCode::Digit5, '5'),
            (KeyCode::Digit6, '6'),
            (KeyCode::Digit7, '7'),
            (KeyCode::Digit8, '8'),
            (KeyCode::Digit9, '9'),
        ] {
            assert_eq!(
                encode_keycode(code, 1, false, false, false),
                Some(vec![digit as u8]),
                "plain {code:?}"
            );
        }

        for (code, lower, upper) in [
            (KeyCode::KeyA, 'a', 'A'),
            (KeyCode::KeyB, 'b', 'B'),
            (KeyCode::KeyC, 'c', 'C'),
            (KeyCode::KeyD, 'd', 'D'),
            (KeyCode::KeyE, 'e', 'E'),
            (KeyCode::KeyF, 'f', 'F'),
            (KeyCode::KeyG, 'g', 'G'),
            (KeyCode::KeyH, 'h', 'H'),
            (KeyCode::KeyI, 'i', 'I'),
            (KeyCode::KeyJ, 'j', 'J'),
            (KeyCode::KeyK, 'k', 'K'),
            (KeyCode::KeyL, 'l', 'L'),
            (KeyCode::KeyM, 'm', 'M'),
            (KeyCode::KeyN, 'n', 'N'),
            (KeyCode::KeyO, 'o', 'O'),
            (KeyCode::KeyP, 'p', 'P'),
            (KeyCode::KeyQ, 'q', 'Q'),
            (KeyCode::KeyR, 'r', 'R'),
            (KeyCode::KeyS, 's', 'S'),
            (KeyCode::KeyT, 't', 'T'),
            (KeyCode::KeyU, 'u', 'U'),
            (KeyCode::KeyV, 'v', 'V'),
            (KeyCode::KeyW, 'w', 'W'),
            (KeyCode::KeyX, 'x', 'X'),
            (KeyCode::KeyY, 'y', 'Y'),
            (KeyCode::KeyZ, 'z', 'Z'),
        ] {
            assert_eq!(
                encode_keycode(code, 1, false, false, false),
                Some(vec![lower as u8]),
                "plain {code:?}"
            );
            assert_eq!(
                encode_keycode(code, 2, true, false, false),
                Some(vec![upper as u8]),
                "Shift+{code:?}"
            );
        }

        for (code, plain, shifted) in [
            (KeyCode::Minus, '-', '_'),
            (KeyCode::Equal, '=', '+'),
            (KeyCode::BracketLeft, '[', '{'),
            (KeyCode::BracketRight, ']', '}'),
            (KeyCode::Backslash, '\\', '|'),
            (KeyCode::Semicolon, ';', ':'),
            (KeyCode::Quote, '\'', '"'),
            (KeyCode::Backquote, '`', '~'),
            (KeyCode::Comma, ',', '<'),
            (KeyCode::Period, '.', '>'),
            (KeyCode::Slash, '/', '?'),
        ] {
            assert_eq!(
                encode_keycode(code, 1, false, false, false),
                Some(vec![plain as u8]),
                "plain {code:?}"
            );
            assert_eq!(
                encode_keycode(code, 2, true, false, false),
                Some(vec![shifted as u8]),
                "Shift+{code:?}"
            );
        }

        assert_eq!(
            encode_keycode(KeyCode::Tab, 2, true, false, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode_keycode(KeyCode::Space, 5, false, true, false),
            Some(vec![0x00])
        );
        assert_eq!(
            encode_keycode(KeyCode::ArrowLeft, 5, false, true, false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            encode_keycode(KeyCode::F5, 5, false, true, false),
            Some(b"\x1b[15;5~".to_vec())
        );
        assert_eq!(
            encode_keycode(KeyCode::KeyB, 7, false, true, true),
            Some(vec![0x02])
        );
    }

    #[test]
    fn all_terminal_named_keys_encode_some() {
        // Every named key we claim for shell/TUI use must produce bytes.
        let keys = [
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::Space,
            NamedKey::Backspace,
            NamedKey::Escape,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
        ];
        for n in keys {
            let out = encode_key(&named(n), ModifiersState::empty());
            assert!(out.is_some(), "{n:?} must encode");
            assert!(!out.unwrap().is_empty(), "{n:?} must be non-empty");
        }
    }

    #[test]
    fn f1_plain_is_ss3() {
        assert_eq!(
            encode_key(&named(NamedKey::F1), ModifiersState::empty()),
            Some(b"\x1bOP".to_vec())
        );
    }

    #[test]
    fn f5_plain_is_csi_15() {
        assert_eq!(
            encode_key(&named(NamedKey::F5), ModifiersState::empty()),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn ctrl_left_is_modified_csi() {
        assert_eq!(
            encode_key(&named(NamedKey::ArrowLeft), mods_ctrl()),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn alt_b_is_esc_b() {
        assert_eq!(
            encode_key(&Key::Character("b".into()), mods_alt()),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn printable_punctuation_characters() {
        for (s, b) in [
            ("!", b'!'),
            ("\"", b'"'),
            ("#", b'#'),
            ("$", b'$'),
            ("%", b'%'),
            ("&", b'&'),
            ("'", b'\''),
            ("(", b'('),
            (")", b')'),
            ("*", b'*'),
            ("+", b'+'),
            (",", b','),
            ("-", b'-'),
            (".", b'.'),
            ("/", b'/'),
            (":", b':'),
            (";", b';'),
            ("<", b'<'),
            ("=", b'='),
            (">", b'>'),
            ("?", b'?'),
            ("@", b'@'),
            ("[", b'['),
            ("\\", b'\\'),
            ("]", b']'),
            ("^", b'^'),
            ("_", b'_'),
            ("`", b'`'),
            ("{", b'{'),
            ("|", b'|'),
            ("}", b'}'),
            ("~", b'~'),
        ] {
            assert_eq!(
                encode_key(&Key::Character(s.into()), ModifiersState::empty()),
                Some(vec![b]),
                "char {s:?}"
            );
        }
    }

    #[test]
    fn digits_and_letters() {
        for c in '0'..='9' {
            let s = c.to_string();
            assert_eq!(
                encode_key(&Key::Character(s.as_str().into()), ModifiersState::empty()),
                Some(vec![c as u8])
            );
        }
        for c in 'a'..='z' {
            let s = c.to_string();
            assert_eq!(
                encode_key(&Key::Character(s.as_str().into()), ModifiersState::empty()),
                Some(vec![c as u8])
            );
        }
    }

    #[test]
    fn text_fallback_space() {
        let bytes = encode_key_event(
            &Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
            Some(" "),
            ModifiersState::empty(),
        );
        assert_eq!(bytes, Some(vec![b' ']));
    }

    #[test]
    fn physical_space_fallback() {
        let bytes = encode_key_event(
            &Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            PhysicalKey::Code(KeyCode::Space),
            None,
            ModifiersState::empty(),
        );
        assert_eq!(bytes, Some(vec![b' ']));
    }

    #[test]
    fn physical_enter_fallback() {
        let bytes = encode_key_event(
            &Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            PhysicalKey::Code(KeyCode::Enter),
            None,
            ModifiersState::empty(),
        );
        assert_eq!(bytes, Some(vec![b'\r']));
    }
}
