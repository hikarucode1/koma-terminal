//! Colours. Everything is stored as sRGB bytes and converted to linear on the
//! way to the GPU, because the surface format is sRGB.

use crate::grid::Color;

pub struct Theme {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor: [u8; 3],
    pub divider: [u8; 3],
    pub divider_focus: [u8; 3],
    /// Background of panes that don't have focus, so the active one stands out.
    pub bg_unfocused: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            fg: [0xd8, 0xdE, 0xe9],
            bg: [0x1b, 0x1f, 0x27],
            cursor: [0x7a, 0xa2, 0xf7],
            divider: [0x2a, 0x2f, 0x3a],
            divider_focus: [0x7a, 0xa2, 0xf7],
            bg_unfocused: [0x16, 0x19, 0x20],
            palette: [
                [0x1b, 0x1f, 0x27], // 0 black
                [0xf7, 0x76, 0x8e], // 1 red
                [0x9e, 0xce, 0x6a], // 2 green
                [0xe0, 0xaf, 0x68], // 3 yellow
                [0x7a, 0xa2, 0xf7], // 4 blue
                [0xbb, 0x9a, 0xf7], // 5 magenta
                [0x7d, 0xcf, 0xff], // 6 cyan
                [0xa9, 0xb1, 0xd6], // 7 white
                [0x41, 0x48, 0x68], // 8 bright black
                [0xff, 0x7a, 0x93], // 9
                [0xb9, 0xf2, 0x7c], // 10
                [0xff, 0x9e, 0x64], // 11
                [0x7d, 0xa6, 0xff], // 12
                [0xbb, 0x9a, 0xf7], // 13
                [0x0d, 0xb9, 0xd7], // 14
                [0xc0, 0xca, 0xf5], // 15
            ],
        }
    }
}

impl Theme {
    /// Resolves a grid colour to sRGB bytes. `is_fg` picks the right default.
    pub fn resolve(&self, c: Color, is_fg: bool) -> [u8; 3] {
        match c {
            Color::Default => {
                if is_fg {
                    self.fg
                } else {
                    self.bg
                }
            }
            Color::Rgb(rgb) => rgb,
            Color::Indexed(i) => self.indexed(i),
        }
    }

    /// The xterm 256-colour palette: 16 themed, a 6×6×6 cube, then 24 greys.
    pub fn indexed(&self, i: u8) -> [u8; 3] {
        match i {
            0..=15 => self.palette[i as usize],
            16..=231 => {
                let i = i - 16;
                let steps = [0u8, 95, 135, 175, 215, 255];
                [
                    steps[(i / 36) as usize],
                    steps[((i % 36) / 6) as usize],
                    steps[(i % 6) as usize],
                ]
            }
            232..=255 => {
                let v = 8 + (i - 232) * 10;
                [v, v, v]
            }
        }
    }
}

/// sRGB byte -> linear float, matching what an sRGB render target expects.
fn srgb_channel_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn to_linear(rgb: [u8; 3], alpha: f32) -> [f32; 4] {
    [
        srgb_channel_to_linear(rgb[0]),
        srgb_channel_to_linear(rgb[1]),
        srgb_channel_to_linear(rgb[2]),
        alpha,
    ]
}
