//! Mouse reporting: telling the program what the pointer is doing.
//!
//! Without this a full-screen program can't see the mouse at all, which is why
//! `set -g mouse on` does nothing in tmux and scrolling has to fall back to
//! synthesising arrow keys. With it, tmux receives the wheel itself and enters
//! copy mode — the only way to scroll *its* history, which we cannot see.

/// Which events the program asked for, via DECSET.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tracking {
    #[default]
    Off,
    /// DECSET 1000 — presses and releases.
    Normal,
    /// DECSET 1002 — and motion while a button is held.
    ButtonEvent,
    /// DECSET 1003 — and motion at all times.
    AnyEvent,
}

/// How the report is spelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Encoding {
    /// The original `ESC [ M` form. Each coordinate is one byte biased by 32,
    /// so it cannot address past column 223.
    #[default]
    Legacy,
    /// DECSET 1006 — `ESC [ <` with decimal parameters, and no column limit.
    Sgr,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Press(Button),
    Release(Button),
    /// Motion, carrying whichever button is held.
    Motion(Option<Button>),
    Wheel {
        up: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// The largest coordinate the legacy encoding can express. One byte biased by
/// 32 tops out at 223 *on the wire*, and the wire counts from one — so the
/// largest we can accept, counting from zero, is 222. Off by one here and
/// column 223 encodes as 256, which truncates to NUL.
const LEGACY_MAX: usize = 222;

fn button_bits(b: Button) -> u16 {
    match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
    }
}

fn mod_bits(m: Mods) -> u16 {
    (if m.shift { 4 } else { 0 }) | (if m.alt { 8 } else { 0 }) | (if m.ctrl { 16 } else { 0 })
}

/// Whether this event is one the program asked to hear about.
fn wanted(tracking: Tracking, event: Event) -> bool {
    match (tracking, event) {
        (Tracking::Off, _) => false,
        (_, Event::Motion(held)) => match tracking {
            Tracking::AnyEvent => true,
            // 1002 reports dragging only, so motion with nothing held is not
            // a drag and stays local.
            Tracking::ButtonEvent => held.is_some(),
            _ => false,
        },
        _ => true,
    }
}

/// Encodes one event at 0-based grid position `(col, row)`.
///
/// `None` when the program isn't listening for it, or when the position is past
/// what the legacy encoding can express — in which case xterm sends nothing
/// rather than a wrong coordinate, and so do we.
pub fn encode(
    tracking: Tracking,
    encoding: Encoding,
    event: Event,
    col: usize,
    row: usize,
    mods: Mods,
) -> Option<Vec<u8>> {
    if !wanted(tracking, event) {
        return None;
    }

    let (mut code, release) = match event {
        Event::Press(b) => (button_bits(b), false),
        Event::Release(b) => (button_bits(b), true),
        Event::Motion(held) => (held.map_or(3, button_bits) + 32, false),
        Event::Wheel { up } => (if up { 64 } else { 65 }, false),
    };
    code |= mod_bits(mods);

    match encoding {
        Encoding::Sgr => {
            let final_byte = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, final_byte).into_bytes())
        }
        Encoding::Legacy => {
            if col > LEGACY_MAX || row > LEGACY_MAX {
                return None;
            }
            // Legacy has no way to say which button came up, so every release
            // is reported as the same "something was released" code.
            let code = if release { 3 | mod_bits(mods) } else { code };
            Some(vec![
                0x1b,
                b'[',
                b'M',
                (code + 32) as u8,
                (col + 1 + 32) as u8,
                (row + 1 + 32) as u8,
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr(event: Event, col: usize, row: usize) -> String {
        String::from_utf8(
            encode(Tracking::AnyEvent, Encoding::Sgr, event, col, row, Mods::default()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn nothing_is_reported_while_tracking_is_off() {
        for e in [
            Event::Press(Button::Left),
            Event::Release(Button::Left),
            Event::Motion(None),
            Event::Wheel { up: true },
        ] {
            assert!(
                encode(Tracking::Off, Encoding::Sgr, e, 0, 0, Mods::default()).is_none(),
                "{e:?} leaked while off"
            );
        }
    }

    #[test]
    fn sgr_reports_are_one_based() {
        // The wire protocol counts from 1; our grid counts from 0.
        assert_eq!(sgr(Event::Press(Button::Left), 0, 0), "\x1b[<0;1;1M");
        assert_eq!(sgr(Event::Press(Button::Left), 9, 4), "\x1b[<0;10;5M");
    }

    #[test]
    fn sgr_distinguishes_release_by_its_final_byte() {
        // This is the whole point of 1006: legacy can't say which button.
        assert_eq!(sgr(Event::Press(Button::Right), 2, 3), "\x1b[<2;3;4M");
        assert_eq!(sgr(Event::Release(Button::Right), 2, 3), "\x1b[<2;3;4m");
    }

    #[test]
    fn the_wheel_uses_its_own_button_codes() {
        assert_eq!(sgr(Event::Wheel { up: true }, 0, 0), "\x1b[<64;1;1M");
        assert_eq!(sgr(Event::Wheel { up: false }, 0, 0), "\x1b[<65;1;1M");
    }

    #[test]
    fn motion_sets_the_drag_bit_and_carries_the_button() {
        assert_eq!(sgr(Event::Motion(Some(Button::Left)), 0, 0), "\x1b[<32;1;1M");
        assert_eq!(sgr(Event::Motion(Some(Button::Middle)), 0, 0), "\x1b[<33;1;1M");
        // Nothing held is reported as button 3 plus the motion bit.
        assert_eq!(sgr(Event::Motion(None), 0, 0), "\x1b[<35;1;1M");
    }

    #[test]
    fn modifiers_add_their_bits() {
        // Ctrl and Alt are the ones a program can actually see: koma keeps
        // Shift for itself, so a report never carries that bit in practice.
        let m = Mods { shift: false, alt: true, ctrl: true };
        let out = encode(Tracking::Normal, Encoding::Sgr, Event::Press(Button::Left), 0, 0, m);
        // 0 (left) + 8 (alt) + 16 (ctrl)
        assert_eq!(String::from_utf8(out.unwrap()).unwrap(), "\x1b[<24;1;1M");

        // The encoder is general even so, since the bit is part of the format.
        let shifted = Mods { shift: true, alt: false, ctrl: false };
        let out =
            encode(Tracking::Normal, Encoding::Sgr, Event::Press(Button::Left), 0, 0, shifted);
        assert_eq!(String::from_utf8(out.unwrap()).unwrap(), "\x1b[<4;1;1M");
    }

    #[test]
    fn plain_tracking_ignores_motion_but_not_buttons() {
        let m = Mods::default();
        assert!(
            encode(Tracking::Normal, Encoding::Sgr, Event::Motion(Some(Button::Left)), 0, 0, m)
                .is_none()
        );
        assert!(
            encode(Tracking::Normal, Encoding::Sgr, Event::Press(Button::Left), 0, 0, m).is_some()
        );
    }

    #[test]
    fn button_event_tracking_reports_drags_only() {
        let m = Mods::default();
        let held = Event::Motion(Some(Button::Left));
        let hover = Event::Motion(None);
        assert!(encode(Tracking::ButtonEvent, Encoding::Sgr, held, 0, 0, m).is_some());
        assert!(
            encode(Tracking::ButtonEvent, Encoding::Sgr, hover, 0, 0, m).is_none(),
            "a bare hover is not a drag"
        );
        assert!(encode(Tracking::AnyEvent, Encoding::Sgr, hover, 0, 0, m).is_some());
    }

    #[test]
    fn legacy_biases_every_field_by_32() {
        let out = encode(
            Tracking::Normal,
            Encoding::Legacy,
            Event::Press(Button::Left),
            0,
            0,
            Mods::default(),
        )
        .unwrap();
        assert_eq!(out, vec![0x1b, b'[', b'M', 32, 33, 33]);
    }

    #[test]
    fn legacy_cannot_name_the_released_button() {
        let m = Mods::default();
        let left =
            encode(Tracking::Normal, Encoding::Legacy, Event::Release(Button::Left), 1, 1, m)
                .unwrap();
        let right =
            encode(Tracking::Normal, Encoding::Legacy, Event::Release(Button::Right), 1, 1, m)
                .unwrap();
        assert_eq!(left, right, "legacy releases are indistinguishable by design");
        assert_eq!(left[3], 3 + 32);
    }

    #[test]
    fn legacy_stays_silent_rather_than_reporting_a_wrong_column() {
        let m = Mods::default();
        let press = Event::Press(Button::Left);
        let at = |col, row| encode(Tracking::Normal, Encoding::Legacy, press, col, row, m);

        // 222 counting from zero is 223 on the wire: the last byte that fits.
        assert_eq!(at(222, 0).unwrap()[4], 255);
        // 223 used to encode as 256 and truncate to NUL, which is worse than a
        // wrong number — a NUL can terminate the sequence at the far end.
        assert!(at(223, 0).is_none(), "the column past the ceiling must send nothing");
        assert!(at(224, 0).is_none());
        // Rows have the same ceiling and the same failure.
        assert_eq!(at(0, 222).unwrap()[5], 255);
        assert!(at(0, 223).is_none());

        // No byte of any accepted report may be NUL.
        for col in 0..=222 {
            for row in [0usize, 100, 222] {
                let out = at(col, row).unwrap();
                assert!(!out.contains(&0), "col={col} row={row} produced {out:?}");
            }
        }

        // SGR has no such limit, which is why programs ask for it.
        assert!(encode(Tracking::Normal, Encoding::Sgr, press, 5000, 0, m).is_some());
    }
}
