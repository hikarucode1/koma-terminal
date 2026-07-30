//! Translates winit key events into the bytes a pty expects (xterm encoding).

use winit::event::KeyEvent;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// xterm's modifier parameter: 1 + bitfield(shift=1, alt=2, ctrl=4, super=8).
fn modifier_param(m: ModifiersState) -> u8 {
    let mut v = 0;
    if m.shift_key() {
        v |= 1;
    }
    if m.alt_key() {
        v |= 2;
    }
    if m.control_key() {
        v |= 4;
    }
    if m.super_key() {
        v |= 8;
    }
    v + 1
}

/// `\x1b[A` normally, `\x1bOA` in application-cursor mode, and
/// `\x1b[1;<mod>A` when any modifier is held.
fn cursor_seq(final_byte: char, m: ModifiersState, app_cursor: bool) -> Vec<u8> {
    let p = modifier_param(m);
    if p > 1 {
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    } else if app_cursor {
        format!("\x1bO{final_byte}").into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

fn tilde_seq(n: u8, m: ModifiersState) -> Vec<u8> {
    let p = modifier_param(m);
    if p > 1 { format!("\x1b[{n};{p}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
}

/// Maps a character to its control code, e.g. `c` -> 0x03.
fn control_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

pub fn encode(ev: &KeyEvent, m: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    // Cmd is reserved for app shortcuts on every platform here.
    if m.super_key() {
        return None;
    }

    if let Key::Named(named) = &ev.logical_key {
        let bytes = match named {
            NamedKey::Enter => vec![b'\r'],
            NamedKey::Backspace => {
                if m.alt_key() {
                    vec![0x1b, 0x7f]
                } else if m.control_key() {
                    vec![0x08]
                } else {
                    vec![0x7f]
                }
            }
            NamedKey::Tab => {
                if m.shift_key() {
                    b"\x1b[Z".to_vec()
                } else {
                    vec![b'\t']
                }
            }
            NamedKey::Escape => vec![0x1b],
            NamedKey::ArrowUp => cursor_seq('A', m, app_cursor),
            NamedKey::ArrowDown => cursor_seq('B', m, app_cursor),
            NamedKey::ArrowRight => cursor_seq('C', m, app_cursor),
            NamedKey::ArrowLeft => cursor_seq('D', m, app_cursor),
            NamedKey::Home => cursor_seq('H', m, app_cursor),
            NamedKey::End => cursor_seq('F', m, app_cursor),
            NamedKey::Insert => tilde_seq(2, m),
            NamedKey::Delete => tilde_seq(3, m),
            NamedKey::PageUp => tilde_seq(5, m),
            NamedKey::PageDown => tilde_seq(6, m),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => tilde_seq(15, m),
            NamedKey::F6 => tilde_seq(17, m),
            NamedKey::F7 => tilde_seq(18, m),
            NamedKey::F8 => tilde_seq(19, m),
            NamedKey::F9 => tilde_seq(20, m),
            NamedKey::F10 => tilde_seq(21, m),
            NamedKey::F11 => tilde_seq(23, m),
            NamedKey::F12 => tilde_seq(24, m),
            NamedKey::Space => {
                if m.control_key() {
                    vec![0x00]
                } else {
                    vec![b' ']
                }
            }
            _ => return None,
        };
        return Some(bytes);
    }

    // Printable input. `ev.text` already has the layout and dead keys applied.
    let text = ev.text.as_ref().map(|s| s.as_str()).unwrap_or("");

    if m.control_key() {
        // Control codes come from the *unmodified* key, since `text` for
        // Ctrl+A is often already \x01 (or empty) depending on platform.
        let base = match &ev.logical_key {
            Key::Character(s) => s.chars().next(),
            _ => text.chars().next(),
        };
        if let Some(b) = base.and_then(control_byte) {
            return Some(if m.alt_key() { vec![0x1b, b] } else { vec![b] });
        }
    }

    if text.is_empty() {
        return None;
    }
    if m.alt_key() {
        // Alt/Option sends ESC-prefixed input (xterm's metaSendsEscape).
        let mut out = vec![0x1b];
        out.extend_from_slice(text.as_bytes());
        return Some(out);
    }
    Some(text.as_bytes().to_vec())
}
