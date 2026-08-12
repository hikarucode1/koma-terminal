//! Terminal screen state: the cell grid, scrollback, and the `vte::Perform`
//! implementation that turns parsed escape sequences into grid mutations.

use unicode_width::UnicodeWidthChar;

use crate::mouse::{Encoding as MouseEncoding, Tracking as MouseTracking};

pub const FLAG_BOLD: u8 = 1 << 0;
pub const FLAG_ITALIC: u8 = 1 << 1;
pub const FLAG_UNDERLINE: u8 = 1 << 2;
pub const FLAG_INVERSE: u8 = 1 << 3;
/// Right-hand half of a double-width character. Never drawn directly.
pub const FLAG_WIDE_SPACER: u8 = 1 << 4;

pub type Rgb = [u8; 3];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    /// Whatever the theme says the default fg/bg is.
    Default,
    /// Index into the xterm 256-colour palette.
    Indexed(u8),
    Rgb(Rgb),
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

impl Cell {
    fn blank_with(bg: Color) -> Self {
        Cell { c: ' ', fg: Color::Default, bg, flags: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

/// A character set that can be mapped into the printable range.
///
/// Only two of the VT100's sets matter in practice. `DecGraphics` is the one
/// tmux and ncurses reach for whenever the terminal isn't in a UTF-8 locale —
/// which is the normal case over ssh, where the remote often falls back to
/// `C`/`POSIX` — and without it every box-drawing character arrives as the
/// ASCII letter it shares a slot with.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Ascii,
    DecGraphics,
}

impl Charset {
    /// DEC Special Graphics occupies `_` through `~`; everything else is ASCII.
    fn map(self, c: char) -> char {
        if self == Charset::Ascii || !('_'..='~').contains(&c) {
            return c;
        }
        const GRAPHICS: [char; 32] = [
            ' ', '◆', '▒', '␉', '␌', '␍', '␊', '°', '±', '␤', '␋', '┘', '┐', '┌', '└', '┼', '⎺',
            '⎻', '─', '⎼', '⎽', '├', '┤', '┴', '┬', '│', '≤', '≥', 'π', '≠', '£', '·',
        ];
        GRAPHICS[c as usize - '_' as usize]
    }
}

/// Cursor state saved by DECSC (`ESC 7`) and CSI s, restored by DECRC / CSI u.
/// The charset belongs here as much as the position does: a program that
/// switches to line drawing, saves, and restores must come back to the set it
/// saved, not to whatever is current.
#[derive(Clone, Copy)]
struct SavedCursor {
    cx: usize,
    cy: usize,
    fg: Color,
    bg: Color,
    flags: u8,
    charsets: [Charset; 2],
    gl: usize,
}

pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// Row-major, `rows * cols` cells.
    pub cells: Vec<Cell>,
    /// Lines that have scrolled off the top. Front is oldest.
    pub scrollback: Vec<Vec<Cell>>,
    pub max_scrollback: usize,
    /// Lines dropped off the front of `scrollback` over the session. Added to
    /// a scrollback index it gives a row id that stays valid as history is
    /// trimmed, which is what selection coordinates are built on.
    pub trimmed: usize,
    /// How many lines up from the live screen the viewport is scrolled.
    pub view_offset: usize,

    pub cx: usize,
    pub cy: usize,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,

    fg: Color,
    bg: Color,
    flags: u8,
    saved: Option<SavedCursor>,

    /// The G0 and G1 character sets, and which of them SO/SI has mapped into
    /// the printable range.
    charsets: [Charset; 2],
    gl: usize,

    /// Scroll region, inclusive rows.
    top: usize,
    bot: usize,

    /// Set once the cursor has been pushed past the last column; the next
    /// printable character wraps first. (deferred wrap / "pending wrap" state)
    wrap_pending: bool,

    pub title: String,
    pub bell: bool,
    /// Text a program asked us to put on the system clipboard (OSC 52), waiting
    /// for the app to pick it up. Over ssh this is the only route a copy has:
    /// the program — tmux, say — is on the far end, and its own paste buffer
    /// lives there with it.
    pub pending_clipboard: Option<String>,
    /// Application cursor keys (DECCKM) — changes what arrow keys send.
    pub app_cursor_keys: bool,
    /// Bracketed paste (DECSET 2004). When the program asks for it, pasted
    /// text is wrapped in markers so it can tell a paste from typing.
    pub bracketed_paste: bool,
    /// Which mouse events the program wants (DECSET 1000/1002/1003).
    pub mouse_tracking: MouseTracking,
    /// How to spell them (DECSET 1006).
    pub mouse_encoding: MouseEncoding,
    /// Alternate scroll (DECSET 1007): may the wheel be turned into arrow keys
    /// on the alternate screen? On by default, which is what less and man
    /// expect; tmux turns it off once it handles the mouse itself.
    pub alternate_scroll: bool,
    /// Alternate screen buffer, saved main screen while active.
    alt: Option<(Vec<Cell>, usize, usize)>,
    pub alt_active: bool,
}

/// Moves the top `count` rows of `buf` into `scrollback`, trimming to `max` and
/// counting whatever is dropped so row ids stay valid.
fn archive(
    scrollback: &mut Vec<Vec<Cell>>,
    trimmed: &mut usize,
    max: usize,
    buf: &[Cell],
    cols: usize,
    count: usize,
) {
    for r in 0..count {
        let s = r * cols;
        scrollback.push(buf[s..s + cols].to_vec());
    }
    if scrollback.len() > max {
        let excess = scrollback.len() - max;
        scrollback.drain(0..excess);
        *trimmed += excess;
    }
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            scrollback: Vec::new(),
            max_scrollback: 10_000,
            trimmed: 0,
            view_offset: 0,
            cx: 0,
            cy: 0,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
            saved: None,
            charsets: [Charset::Ascii; 2],
            gl: 0,
            top: 0,
            bot: rows - 1,
            wrap_pending: false,
            title: String::new(),
            bell: false,
            pending_clipboard: None,
            app_cursor_keys: false,
            bracketed_paste: false,
            mouse_tracking: MouseTracking::Off,
            mouse_encoding: MouseEncoding::Legacy,
            alternate_scroll: true,
            alt: None,
            alt_active: false,
        }
    }

    #[inline]
    fn idx(&self, x: usize, y: usize) -> usize {
        y * self.cols + x
    }

    /// Direct access to the live screen. The renderer goes through `view_row`
    /// instead, so this is currently only used by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cell(&self, x: usize, y: usize) -> &Cell {
        &self.cells[self.idx(x, y)]
    }

    /// Row `y` of the current viewport, accounting for scrollback offset.
    /// Returns `None` for rows that predate the retained scrollback.
    ///
    /// Deliberately blind to the alternate screen. Scrollback outlives the
    /// switch, so scrolling back while a full-screen program is up shows what
    /// was on screen before it started — the thing Shift+wheel exists to do.
    /// Callers that move `view_offset` for some other reason are covering the
    /// program's display, and should be sure that is what they meant.
    pub fn view_row(&self, y: usize) -> Option<&[Cell]> {
        if self.view_offset == 0 {
            let s = y * self.cols;
            return self.cells.get(s..s + self.cols);
        }
        // Rows [0, view_offset) of the viewport come from scrollback.
        if y < self.view_offset {
            let back = self.view_offset - y;
            let i = self.scrollback.len().checked_sub(back)?;
            self.scrollback.get(i).map(|r| r.as_slice())
        } else {
            let sy = y - self.view_offset;
            let s = sy * self.cols;
            self.cells.get(s..s + self.cols)
        }
    }

    /// Row id for viewport row `y`, stable as the line scrolls into history.
    ///
    /// Viewport rows above the fold come from scrollback and those below from
    /// the live screen, but both land on the same expression: history grows by
    /// exactly as much as the screen scrolls, so the id of a given line never
    /// changes.
    pub fn abs_row(&self, y: usize) -> usize {
        self.trimmed + self.scrollback.len() + y - self.view_offset
    }

    /// The row with id `abs`, from history or the live screen. `None` once the
    /// line has been trimmed away, or for ids past the bottom.
    pub fn row_at(&self, abs: usize) -> Option<&[Cell]> {
        let idx = abs.checked_sub(self.trimmed)?;
        if idx < self.scrollback.len() {
            return self.scrollback.get(idx).map(|r| r.as_slice());
        }
        let sy = idx - self.scrollback.len();
        let s = sy * self.cols;
        self.cells.get(s..s + self.cols)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let (old_rows, old_cols) = (self.rows, self.cols);
        let copy_cols = old_cols.min(cols);

        // The window a buffer keeps, as `(src_start, keep, dst_start)`.
        //
        // Rows below the cursor are the unused tail of the screen, so they go
        // first; the top is only touched for whatever is still needed. Every
        // buffer is measured against *its own* cursor — the alternate screen's
        // saved copy of the main screen has a cursor of its own, usually near
        // the bottom, and judging it by the cursor of whatever is on screen
        // would cut it off from the top.
        let window = |cy: usize, pulled: usize| -> (usize, usize, usize) {
            let src_start = if rows < old_rows {
                let below = old_rows - 1 - cy.min(old_rows - 1);
                (old_rows - rows).saturating_sub(below)
            } else {
                0
            };
            let keep = (old_rows - src_start).min(rows - pulled);
            (src_start, keep, rows - keep)
        };

        // No reflow: the surviving window keeps its old wrapping.
        let reshape = |old: &[Cell], (src_start, keep, dst_start): (usize, usize, usize)| {
            let mut next = vec![Cell::default(); cols * rows];
            for r in 0..keep {
                for c in 0..copy_cols {
                    next[(dst_start + r) * cols + c] = old[(src_start + r) * old_cols + c];
                }
            }
            next
        };

        // Exactly one buffer is the main screen — the visible one, or the copy
        // the alternate screen stands in for — and only that one has a history.
        let saved = self.alt.take();
        let main_cy = saved.as_ref().map_or(self.cy, |&(_, _, cy)| cy);
        let pulled = if rows > old_rows && saved.is_none() {
            (rows - old_rows).min(self.scrollback.len())
        } else {
            0
        };
        let main_win = window(main_cy, pulled);

        // Rows falling off the top of the main screen are history, not rubbish.
        // Splitting a pane shrinks it, and without this everything above the
        // fold vanished with no way to scroll back to it — including when the
        // main screen is the one parked behind vim.
        if rows < old_rows && main_win.0 > 0 {
            let max = self.max_scrollback;
            let (scrollback, trimmed) = (&mut self.scrollback, &mut self.trimmed);
            match saved.as_ref() {
                Some((buf, _, _)) => archive(scrollback, trimmed, max, buf, old_cols, main_win.0),
                None => archive(scrollback, trimmed, max, &self.cells, old_cols, main_win.0),
            }
        }

        let visible_win = if saved.is_some() { window(self.cy, 0) } else { main_win };
        let mut next = reshape(&self.cells, visible_win);

        // Growing pulls that history back rather than opening blank space above
        // the cursor, so closing a split undoes what opening it did.
        for i in 0..pulled {
            let row = &self.scrollback[self.scrollback.len() - pulled + i];
            let dst = (main_win.2 - pulled + i) * cols;
            let n = copy_cols.min(row.len());
            next[dst..dst + n].copy_from_slice(&row[..n]);
        }
        if pulled > 0 {
            let keep_back = self.scrollback.len() - pulled;
            self.scrollback.truncate(keep_back);
        }

        if let Some((buf, sx, sy)) = saved {
            let reshaped = reshape(&buf, main_win);
            let ny = (sy + main_win.2).saturating_sub(main_win.0).min(rows - 1);
            self.alt = Some((reshaped, sx.min(cols - 1), ny));
        }

        self.cells = next;
        self.cx = self.cx.min(cols - 1);
        self.cy = (self.cy + visible_win.2).saturating_sub(visible_win.0).min(rows - 1);
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.wrap_pending = false;
        self.view_offset = 0;
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        // Only the true top of the screen feeds scrollback.
        if self.top == 0 && !self.alt_active {
            for r in 0..n {
                let s = r * self.cols;
                self.scrollback.push(self.cells[s..s + self.cols].to_vec());
            }
            // A viewport that has been scrolled back is anchored to the lines
            // it is showing, not to a distance from the bottom. Without this,
            // arriving output would slide those lines out from under the user.
            if self.view_offset > 0 {
                self.view_offset += n;
            }
            if self.scrollback.len() > self.max_scrollback {
                let excess = self.scrollback.len() - self.max_scrollback;
                self.scrollback.drain(0..excess);
                self.trimmed += excess;
            }
            // Dropping the oldest lines can put the viewport past the top.
            self.view_offset = self.view_offset.min(self.scrollback.len());
        }
        let bg = self.bg;
        for r in self.top..=self.bot {
            let src = r + n;
            if src <= self.bot {
                let (s0, s1) = (src * self.cols, src * self.cols + self.cols);
                let d0 = r * self.cols;
                self.cells.copy_within(s0..s1, d0);
            } else {
                let d0 = r * self.cols;
                self.cells[d0..d0 + self.cols].fill(Cell::blank_with(bg));
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        let bg = self.bg;
        for r in (self.top..=self.bot).rev() {
            if r >= self.top + n {
                let src = r - n;
                let (s0, s1) = (src * self.cols, src * self.cols + self.cols);
                let d0 = r * self.cols;
                self.cells.copy_within(s0..s1, d0);
            } else {
                let d0 = r * self.cols;
                self.cells[d0..d0 + self.cols].fill(Cell::blank_with(bg));
            }
        }
    }

    fn newline(&mut self) {
        if self.cy == self.bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            cx: self.cx,
            cy: self.cy,
            fg: self.fg,
            bg: self.bg,
            flags: self.flags,
            charsets: self.charsets,
            gl: self.gl,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(s) = self.saved {
            self.cx = s.cx.min(self.cols - 1);
            self.cy = s.cy.min(self.rows - 1);
            self.fg = s.fg;
            self.bg = s.bg;
            self.flags = s.flags;
            self.charsets = s.charsets;
            self.gl = s.gl;
        }
    }

    fn put(&mut self, c: char) {
        // The active character set is applied before anything else: the width
        // of a box-drawing glyph is the width that matters, not the width of
        // the ASCII letter that stood in for it on the wire.
        let c = self.charsets[self.gl].map(c);
        let w = c.width().unwrap_or(1).max(1);

        if self.wrap_pending {
            self.cx = 0;
            self.newline();
            self.wrap_pending = false;
        }
        if self.cx + w > self.cols {
            self.cx = 0;
            self.newline();
        }

        let (x, y) = (self.cx, self.cy);
        let i = self.idx(x, y);
        self.cells[i] = Cell { c, fg: self.fg, bg: self.bg, flags: self.flags };
        if w == 2 && x + 1 < self.cols {
            self.cells[i + 1] =
                Cell { c: ' ', fg: self.fg, bg: self.bg, flags: self.flags | FLAG_WIDE_SPACER };
        }

        if self.cx + w >= self.cols {
            // Stay put; wrap happens on the *next* printable character.
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cx += w;
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let bg = self.bg;
        let (cx, cy) = (self.cx, self.cy);
        match mode {
            0 => {
                let from = self.idx(cx, cy);
                self.cells[from..].fill(Cell::blank_with(bg));
            }
            1 => {
                let to = self.idx(cx, cy) + 1;
                self.cells[..to].fill(Cell::blank_with(bg));
            }
            2 | 3 => self.cells.fill(Cell::blank_with(bg)),
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let bg = self.bg;
        let (cx, cy) = (self.cx, self.cy);
        let row = cy * self.cols;
        match mode {
            0 => self.cells[row + cx..row + self.cols].fill(Cell::blank_with(bg)),
            1 => self.cells[row..row + cx + 1].fill(Cell::blank_with(bg)),
            2 => self.cells[row..row + self.cols].fill(Cell::blank_with(bg)),
            _ => {}
        }
    }

    fn set_mode(&mut self, private: bool, params: &[u16], enable: bool) {
        for &p in params {
            match (private, p) {
                (true, 1) => self.app_cursor_keys = enable,
                (true, 25) => self.cursor_visible = enable,
                (true, 2004) => self.bracketed_paste = enable,
                // Any of these turning off means "stop tracking"; xterm has no
                // notion of falling back to a lesser mode.
                (true, 1000) => {
                    self.mouse_tracking =
                        if enable { MouseTracking::Normal } else { MouseTracking::Off }
                }
                (true, 1002) => {
                    self.mouse_tracking =
                        if enable { MouseTracking::ButtonEvent } else { MouseTracking::Off }
                }
                (true, 1003) => {
                    self.mouse_tracking =
                        if enable { MouseTracking::AnyEvent } else { MouseTracking::Off }
                }
                (true, 1006) => {
                    self.mouse_encoding =
                        if enable { MouseEncoding::Sgr } else { MouseEncoding::Legacy }
                }
                (true, 1007) => self.alternate_scroll = enable,
                (true, 1049) | (true, 1047) | (true, 47) => self.set_alt_screen(enable),
                _ => {}
            }
        }
    }

    fn set_alt_screen(&mut self, on: bool) {
        if on == self.alt_active {
            return;
        }
        if on {
            self.alt = Some((self.cells.clone(), self.cx, self.cy));
            self.cells.fill(Cell::default());
            self.cx = 0;
            self.cy = 0;
            self.alt_active = true;
        } else if let Some((cells, cx, cy)) = self.alt.take() {
            // Guard against a resize while the alt screen was up.
            if cells.len() == self.cells.len() {
                self.cells = cells;
                self.cx = cx.min(self.cols - 1);
                self.cy = cy.min(self.rows - 1);
            } else {
                self.cells.fill(Cell::default());
                self.cx = 0;
                self.cy = 0;
            }
            self.alt_active = false;
        }
        self.view_offset = 0;
    }

    fn sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            self.fg = Color::Default;
            self.bg = Color::Default;
            self.flags = 0;
            return;
        }
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => {
                    self.fg = Color::Default;
                    self.bg = Color::Default;
                    self.flags = 0;
                }
                1 => self.flags |= FLAG_BOLD,
                3 => self.flags |= FLAG_ITALIC,
                4 => self.flags |= FLAG_UNDERLINE,
                7 => self.flags |= FLAG_INVERSE,
                22 => self.flags &= !FLAG_BOLD,
                23 => self.flags &= !FLAG_ITALIC,
                24 => self.flags &= !FLAG_UNDERLINE,
                27 => self.flags &= !FLAG_INVERSE,
                30..=37 => self.fg = Color::Indexed((p - 30) as u8),
                90..=97 => self.fg = Color::Indexed((p - 90 + 8) as u8),
                40..=47 => self.bg = Color::Indexed((p - 40) as u8),
                100..=107 => self.bg = Color::Indexed((p - 100 + 8) as u8),
                39 => self.fg = Color::Default,
                49 => self.bg = Color::Default,
                38 | 48 => {
                    let target_fg = p == 38;
                    // 5;n = indexed, 2;r;g;b = truecolor
                    match params.get(i + 1) {
                        Some(5) => {
                            if let Some(&n) = params.get(i + 2) {
                                let c = Color::Indexed(n as u8);
                                if target_fg { self.fg = c } else { self.bg = c }
                            }
                            i += 2;
                        }
                        Some(2) => {
                            if let (Some(&r), Some(&g), Some(&b)) =
                                (params.get(i + 2), params.get(i + 3), params.get(i + 4))
                            {
                                let c = Color::Rgb([r as u8, g as u8, b as u8]);
                                if target_fg { self.fg = c } else { self.bg = c }
                            }
                            i += 4;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Wraps a `Grid` so `vte::Parser` can drive it.
pub struct Performer<'a> {
    pub grid: &'a mut Grid,
    /// Bytes the terminal wants to send back to the pty (e.g. DA, CPR replies).
    pub reply: Vec<u8>,
}

/// The largest OSC 52 payload we will decode. Three bytes of clipboard per four
/// of base64, so this is a 6 MiB copy — far past any real one, and short of
/// letting a runaway program have the heap.
const MAX_CLIPBOARD_BASE64: usize = 8 << 20;

impl Performer<'_> {
    /// The payload of an OSC 52.
    ///
    /// `?` asks us to send the clipboard *back*, and we never answer it: that
    /// would hand whatever the user last copied — a password, most likely — to
    /// whatever is running on the far end of an ssh session, for the asking.
    /// Writes are honoured, reads are not.
    fn set_clipboard(&mut self, payload: &[u8]) {
        // An empty payload is xterm's "clear the selection". Wiping the local
        // clipboard on a remote program's say-so is the sort of thing you only
        // notice by pasting the wrong thing, so it is a no-op here.
        if payload == b"?" || payload.is_empty() {
            return;
        }
        if payload.len() > MAX_CLIPBOARD_BASE64 {
            log::warn!("ignoring an OSC 52 payload of {} bytes", payload.len());
            return;
        }
        match decode_base64(payload) {
            Some(bytes) => {
                self.grid.pending_clipboard = Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            None => log::warn!("OSC 52 payload was not base64"),
        }
    }
}

/// Base64 as OSC 52 spells it, tolerating missing padding and any line breaks a
/// sender wrapped the payload with. Anything else is rejected rather than
/// decoded past: a clipboard full of garbage is worse than a copy that failed.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    fn sextet(b: u8) -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => (b - b'A') as u32,
            b'a'..=b'z' => (b - b'a') as u32 + 26,
            b'0'..=b'9' => (b - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc = 0u32;
    // How many bits of `acc` are still waiting to make up a byte. Never 8 or
    // more on the way round the loop, so the low bits are all that matter and
    // whatever has been shifted up past them can be ignored.
    let mut bits = 0u32;
    for &b in input {
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            // Padding, and nothing that follows it carries data.
            b'=' => break,
            _ => {}
        }
        acc = (acc << 6) | sextet(b)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Four sextets are three bytes, so a group can legitimately end with 2 or 4
    // bits over. Six left means a lone trailing character that spells no byte at
    // all: the payload was cut short, not merely unpadded.
    if bits >= 6 {
        return None;
    }
    Some(out)
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        self.grid.put(c);
    }

    fn execute(&mut self, byte: u8) {
        let g = &mut self.grid;
        match byte {
            b'\n' | 0x0b | 0x0c => {
                g.wrap_pending = false;
                g.newline();
            }
            b'\r' => {
                g.wrap_pending = false;
                g.cx = 0;
            }
            0x08 => {
                g.wrap_pending = false;
                g.cx = g.cx.saturating_sub(1);
            }
            b'\t' => {
                g.wrap_pending = false;
                let next = ((g.cx / 8) + 1) * 8;
                g.cx = next.min(g.cols - 1);
            }
            0x07 => g.bell = true,
            // Shift Out / Shift In: swap which designated set is printable.
            0x0e => g.gl = 1,
            0x0f => g.gl = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _ignore: bool, c: char) {
        // Flatten to plain u16s; sub-parameters (`38:2:...`) are rare enough
        // that treating them as separate params is good enough here.
        let flat: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        let arg = |i: usize, default: u16| -> u16 {
            match flat.get(i) {
                Some(0) | None => default,
                Some(&v) => v,
            }
        };
        let private = intermediates.first() == Some(&b'?');
        let g = &mut *self.grid;
        // Sequences that neither move the cursor nor touch the screen must
        // leave a pending wrap intact — otherwise `…last column` followed by an
        // SGR change would overwrite the final cell instead of wrapping.
        if !matches!(c, 'm' | 'n' | 'c' | 'q') {
            g.wrap_pending = false;
        }

        match c {
            'A' => g.cy = g.cy.saturating_sub(arg(0, 1) as usize),
            'B' | 'e' => g.cy = (g.cy + arg(0, 1) as usize).min(g.rows - 1),
            'C' | 'a' => g.cx = (g.cx + arg(0, 1) as usize).min(g.cols - 1),
            'D' => g.cx = g.cx.saturating_sub(arg(0, 1) as usize),
            'E' => {
                g.cx = 0;
                g.cy = (g.cy + arg(0, 1) as usize).min(g.rows - 1);
            }
            'F' => {
                g.cx = 0;
                g.cy = g.cy.saturating_sub(arg(0, 1) as usize);
            }
            'G' | '`' => g.cx = (arg(0, 1) as usize - 1).min(g.cols - 1),
            'd' => g.cy = (arg(0, 1) as usize - 1).min(g.rows - 1),
            'H' | 'f' => {
                g.cy = (arg(0, 1) as usize - 1).min(g.rows - 1);
                g.cx = (arg(1, 1) as usize - 1).min(g.cols - 1);
            }
            'J' => g.erase_in_display(flat.first().copied().unwrap_or(0)),
            'K' => g.erase_in_line(flat.first().copied().unwrap_or(0)),
            'L' => {
                // Insert lines at the cursor: scroll the region below it down.
                let n = arg(0, 1) as usize;
                if g.cy >= g.top && g.cy <= g.bot {
                    let saved_top = g.top;
                    g.top = g.cy;
                    g.scroll_down(n);
                    g.top = saved_top;
                }
            }
            'M' => {
                let n = arg(0, 1) as usize;
                if g.cy >= g.top && g.cy <= g.bot {
                    let saved_top = g.top;
                    g.top = g.cy;
                    g.scroll_up(n);
                    g.top = saved_top;
                }
            }
            'P' => {
                // Delete characters: shift the rest of the line left.
                let n = (arg(0, 1) as usize).min(g.cols - g.cx);
                let row = g.cy * g.cols;
                g.cells.copy_within(row + g.cx + n..row + g.cols, row + g.cx);
                let bg = g.bg;
                g.cells[row + g.cols - n..row + g.cols].fill(Cell::blank_with(bg));
            }
            '@' => {
                // Insert blanks: shift the rest of the line right.
                let n = (arg(0, 1) as usize).min(g.cols - g.cx);
                let row = g.cy * g.cols;
                g.cells.copy_within(row + g.cx..row + g.cols - n, row + g.cx + n);
                let bg = g.bg;
                g.cells[row + g.cx..row + g.cx + n].fill(Cell::blank_with(bg));
            }
            'X' => {
                let n = (arg(0, 1) as usize).min(g.cols - g.cx);
                let row = g.cy * g.cols;
                let bg = g.bg;
                g.cells[row + g.cx..row + g.cx + n].fill(Cell::blank_with(bg));
            }
            'S' => g.scroll_up(arg(0, 1) as usize),
            'T' => g.scroll_down(arg(0, 1) as usize),
            'm' => g.sgr(&flat),
            'h' => g.set_mode(private, &flat, true),
            'l' => g.set_mode(private, &flat, false),
            'r' => {
                let top = arg(0, 1) as usize - 1;
                let bot =
                    flat.get(1).copied().filter(|&v| v != 0).unwrap_or(g.rows as u16) as usize - 1;
                if top < bot && bot < g.rows {
                    g.top = top;
                    g.bot = bot;
                    g.cx = 0;
                    g.cy = top;
                }
            }
            's' => g.save_cursor(),
            'u' => g.restore_cursor(),
            'q' if intermediates.first() == Some(&b' ') => {
                g.cursor_shape = match flat.first().copied().unwrap_or(0) {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
            }
            // XTVERSION: name ourselves. tmux asks alongside the two device
            // attributes queries, and takes silence for an answer, but naming
            // ourselves is both cheap and what it is actually asking for.
            'q' if intermediates.first() == Some(&b'>') => {
                self.reply.extend_from_slice(
                    format!("\x1bP>|koma({})\x1b\\", env!("CARGO_PKG_VERSION")).as_bytes(),
                );
            }
            'n' if !private && flat.first() == Some(&6) => {
                // Device Status Report: report cursor position. Gated on the
                // plain form: `CSI ? 6 n` is DECXCPR and wants its own reply, so
                // answering it with a CPR would be a reply the asker cannot
                // parse — see the device-attributes note below for where that
                // ends up.
                self.reply.extend_from_slice(format!("\x1b[{};{}R", g.cy + 1, g.cx + 1).as_bytes());
            }
            'c' => match intermediates.first() {
                // Primary DA: identify as a VT102.
                None => self.reply.extend_from_slice(b"\x1b[?6c"),
                // Secondary DA (`CSI > c`) asks what model and version we are,
                // and the answer has to be in its own `CSI > ... c` form.
                //
                // We used to send the primary reply to both. tmux asks both
                // questions the moment it starts, could not parse `\x1b[?6c` as
                // an answer to the second, and did what it does with input it
                // doesn't recognise: passed it to the pane as keystrokes. So
                // every tmux session opened with `^[[?6c` echoed at the prompt
                // and `6c` left on the command line.
                Some(b'>') => self.reply.extend_from_slice(b"\x1b[>0;0;0c"),
                _ => {}
            },
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        let g = &mut self.grid;
        // `ESC ( x` designates a set as G0, `ESC ) x` as G1. `0` is DEC Special
        // Graphics — tmux's pane dividers and every ncurses box outside a UTF-8
        // locale — and anything else is close enough to ASCII to treat as such.
        if let Some(slot) = match intermediates.first() {
            Some(b'(') => Some(0),
            Some(b')') => Some(1),
            _ => None,
        } {
            g.charsets[slot] = if byte == b'0' { Charset::DecGraphics } else { Charset::Ascii };
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'D' => g.newline(),
            b'E' => {
                g.cx = 0;
                g.newline();
            }
            b'M' => {
                if g.cy == g.top {
                    g.scroll_down(1);
                } else {
                    g.cy = g.cy.saturating_sub(1);
                }
            }
            b'7' => g.save_cursor(),
            b'8' => g.restore_cursor(),
            b'c' => {
                let (cols, rows) = (g.cols, g.rows);
                **g = Grid::new(cols, rows);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        match params.first() {
            // OSC 0/1/2 all set (some flavour of) the window title.
            Some(&b"0") | Some(&b"1") | Some(&b"2") => {
                if let Some(t) = params.get(1) {
                    self.grid.title = String::from_utf8_lossy(t).into_owned();
                }
            }
            // OSC 52: put this on the clipboard. The selection it names — `c`,
            // `p`, a cut buffer, or nothing at all, which is what tmux sends —
            // is ignored, because there is only one clipboard here to write to.
            Some(&b"52") => self.set_clipboard(params.get(2).copied().unwrap_or(b"")),
            _ => {}
        }
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `input` through the VT parser against a fresh grid.
    fn feed(cols: usize, rows: usize, input: &[&str]) -> Grid {
        let mut g = Grid::new(cols, rows);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            for chunk in input {
                parser.advance(&mut perf, chunk.as_bytes());
            }
        }
        g
    }

    pub(super) fn row_text(g: &Grid, y: usize) -> String {
        (0..g.cols).map(|x| g.cell(x, y).c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn prints_text_and_moves_cursor() {
        let g = feed(20, 5, &["hello"]);
        assert_eq!(row_text(&g, 0), "hello");
        assert_eq!((g.cx, g.cy), (5, 0));
    }

    #[test]
    fn cr_lf_start_a_new_line() {
        let g = feed(20, 5, &["ab\r\ncd"]);
        assert_eq!(row_text(&g, 0), "ab");
        assert_eq!(row_text(&g, 1), "cd");
        assert_eq!((g.cx, g.cy), (2, 1));
    }

    #[test]
    fn wrap_is_deferred_until_the_next_character() {
        // Filling the last column must not scroll on its own.
        let g = feed(4, 3, &["abcd"]);
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!((g.cx, g.cy), (3, 0));

        let g = feed(4, 3, &["abcde"]);
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1), "e");
        assert_eq!((g.cx, g.cy), (1, 1));
    }

    #[test]
    fn scrolling_past_the_bottom_fills_scrollback() {
        let g = feed(8, 2, &["one\r\ntwo\r\nthree"]);
        assert_eq!(row_text(&g, 0), "two");
        assert_eq!(row_text(&g, 1), "three");
        assert_eq!(g.scrollback.len(), 1);
        let first: String = g.scrollback[0].iter().map(|c| c.c).collect();
        assert_eq!(first.trim_end(), "one");
    }

    #[test]
    fn view_row_reads_back_into_scrollback() {
        let mut g = feed(8, 2, &["one\r\ntwo\r\nthree"]);
        g.view_offset = 1;
        let top: String = g.view_row(0).unwrap().iter().map(|c| c.c).collect();
        assert_eq!(top.trim_end(), "one");
        let next: String = g.view_row(1).unwrap().iter().map(|c| c.c).collect();
        assert_eq!(next.trim_end(), "two");
    }

    #[test]
    fn cup_positions_the_cursor_one_based() {
        let g = feed(20, 10, &["\x1b[3;5Hx"]);
        assert_eq!((g.cx, g.cy), (5, 2));
        assert_eq!(g.cell(4, 2).c, 'x');
    }

    #[test]
    fn sgr_sets_and_resets_attributes() {
        let g = feed(20, 3, &["\x1b[1;31mA\x1b[0mB"]);
        let a = g.cell(0, 0);
        assert_eq!(a.fg, Color::Indexed(1));
        assert_eq!(a.flags & FLAG_BOLD, FLAG_BOLD);
        let b = g.cell(1, 0);
        assert_eq!(b.fg, Color::Default);
        assert_eq!(b.flags, 0);
    }

    #[test]
    fn sgr_truecolor_and_256_colour() {
        let g = feed(20, 3, &["\x1b[38;2;10;20;30mA\x1b[48;5;99mB"]);
        assert_eq!(g.cell(0, 0).fg, Color::Rgb([10, 20, 30]));
        assert_eq!(g.cell(1, 0).bg, Color::Indexed(99));
    }

    #[test]
    fn erase_in_line_clears_to_the_right() {
        let g = feed(10, 3, &["abcdef\x1b[1;3H\x1b[0K"]);
        assert_eq!(row_text(&g, 0), "ab");
    }

    #[test]
    fn erase_in_display_clears_everything() {
        let g = feed(10, 3, &["ab\r\ncd\x1b[2J"]);
        assert_eq!(row_text(&g, 0), "");
        assert_eq!(row_text(&g, 1), "");
    }

    #[test]
    fn delete_and_insert_characters_shift_the_line() {
        let g = feed(10, 2, &["abcdef\x1b[1;2H\x1b[2P"]);
        assert_eq!(row_text(&g, 0), "adef");

        let g = feed(10, 2, &["abc\x1b[1;2H\x1b[2@"]);
        assert_eq!(row_text(&g, 0), "a  bc");
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let g = feed(10, 2, &["あa"]);
        assert_eq!(g.cell(0, 0).c, 'あ');
        assert_eq!(g.cell(1, 0).flags & FLAG_WIDE_SPACER, FLAG_WIDE_SPACER);
        assert_eq!(g.cell(2, 0).c, 'a');
    }

    #[test]
    fn wide_character_wraps_rather_than_splitting() {
        // Only one column left, so the two-cell glyph must move to the next row.
        let g = feed(3, 3, &["ab", "あ"]);
        assert_eq!(g.cell(0, 1).c, 'あ');
        assert_eq!(g.cell(1, 1).flags & FLAG_WIDE_SPACER, FLAG_WIDE_SPACER);
    }

    #[test]
    fn alt_screen_saves_and_restores_the_main_buffer() {
        let mut g = Grid::new(10, 3);
        let mut parser = vte::Parser::new();
        let mut run = |g: &mut Grid, s: &str| {
            let mut perf = Performer { grid: g, reply: Vec::new() };
            parser.advance(&mut perf, s.as_bytes());
        };
        run(&mut g, "main");
        run(&mut g, "\x1b[?1049h");
        assert!(g.alt_active);
        assert_eq!(row_text(&g, 0), "");
        run(&mut g, "alt");
        assert_eq!(row_text(&g, 0), "alt");
        run(&mut g, "\x1b[?1049l");
        assert!(!g.alt_active);
        assert_eq!(row_text(&g, 0), "main");
    }

    #[test]
    fn scroll_region_limits_scrolling() {
        // Region rows 2..3 (1-based); row 1 must stay put.
        let g = feed(6, 4, &["\x1b[2;3r\x1b[1;1Htop\x1b[2;1Ha\r\nb\r\nc"]);
        assert_eq!(row_text(&g, 0), "top");
        assert_eq!(row_text(&g, 1), "b");
        assert_eq!(row_text(&g, 2), "c");
    }

    #[test]
    fn dsr_reports_cursor_position() {
        let mut g = Grid::new(20, 5);
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\x1b[3;7H\x1b[6n");
        assert_eq!(perf.reply, b"\x1b[3;7R");
    }

    #[test]
    fn osc_sets_the_window_title() {
        let g = feed(20, 3, &["\x1b]0;my title\x07"]);
        assert_eq!(g.title, "my title");
    }

    #[test]
    fn resize_keeps_the_bottom_of_the_screen() {
        let mut g = feed(10, 4, &["a\r\nb\r\nc\r\nd"]);
        g.resize(10, 2);
        assert_eq!(g.rows, 2);
        assert_eq!(row_text(&g, 0), "c");
        assert_eq!(row_text(&g, 1), "d");
    }

    #[test]
    fn decckm_toggles_application_cursor_keys() {
        let g = feed(10, 3, &["\x1b[?1h"]);
        assert!(g.app_cursor_keys);
        let g = feed(10, 3, &["\x1b[?1h\x1b[?1l"]);
        assert!(!g.app_cursor_keys);
    }

    #[test]
    fn sgr_does_not_cancel_a_pending_wrap() {
        // Colour changes at the end of a line must not eat the wrap.
        let g = feed(4, 3, &["abcd", "\x1b[31m", "e"]);
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1), "e");
        assert_eq!(g.cell(0, 1).fg, Color::Indexed(1));
    }

    #[test]
    fn cursor_movement_does_cancel_a_pending_wrap() {
        let g = feed(4, 3, &["abcd", "\x1b[1;1H", "e"]);
        assert_eq!(row_text(&g, 0), "ebcd");
        assert_eq!(row_text(&g, 1), "");
    }

    #[test]
    fn scrollback_rows_keep_their_original_width() {
        // Renderers must clamp to the row length, not the current column count.
        let mut g = feed(4, 2, &["ab\r\ncd\r\nef"]);
        assert_eq!(g.scrollback[0].len(), 4);
        g.resize(10, 2);
        g.view_offset = 1;
        assert_eq!(g.view_row(0).unwrap().len(), 4, "old row stays 4 wide");
        assert_eq!(g.view_row(1).unwrap().len(), 10, "live rows use the new width");
    }

    #[test]
    fn output_does_not_slide_a_scrolled_back_viewport() {
        // Scroll back to "one", then let more output arrive. The user is
        // anchored to the line they are reading, not to a distance from the
        // bottom, so "one" must still be the top row.
        let mut g = feed(8, 2, &["one\r\ntwo\r\nthree"]);
        g.view_offset = 1;
        let top = |g: &Grid| -> String {
            g.view_row(0).unwrap().iter().map(|c| c.c).collect::<String>().trim_end().to_string()
        };
        assert_eq!(top(&g), "one");

        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\r\nfour\r\nfive");
        assert_eq!(top(&g), "one", "the viewport drifted to {:?}", top(&g));
        // "two" and "three" scrolled off, so the offset grew by two to keep
        // pointing at the same line.
        assert_eq!(g.view_offset, 3);
    }

    #[test]
    fn a_live_viewport_keeps_following_new_output() {
        // view_offset == 0 means "pinned to the bottom" and must stay there.
        let mut g = feed(8, 2, &["one\r\ntwo"]);
        assert_eq!(g.view_offset, 0);
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\r\nthree\r\nfour");
        assert_eq!(g.view_offset, 0);
        assert_eq!(row_text(&g, 1), "four");
    }

    #[test]
    fn trimming_scrollback_clamps_the_viewport() {
        let mut g = Grid::new(8, 2);
        g.max_scrollback = 3;
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"a\r\nb\r\nc");
        }
        g.view_offset = g.scrollback.len();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\r\nd\r\ne\r\nf\r\ng");
        }
        assert_eq!(g.scrollback.len(), 3, "trimmed to max_scrollback");
        assert!(g.view_offset <= g.scrollback.len(), "offset {} ran past the top", g.view_offset);
        assert!(g.view_row(0).is_some(), "the top row must still resolve");
    }

    #[test]
    fn the_alternate_screen_does_not_grow_scrollback() {
        // tmux and vim redraw constantly; none of it belongs in our history.
        let mut g = feed(8, 2, &["one\r\ntwo"]);
        let before = g.scrollback.len();
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\x1b[?1049h");
        parser.advance(&mut perf, b"a\r\nb\r\nc\r\nd");
        assert_eq!(g.scrollback.len(), before);
    }

    #[test]
    fn a_row_id_stays_with_its_line_as_it_scrolls_away() {
        // "one" is on screen, then scrolls into history. Its id must not move,
        // or a selection made before the output would drift.
        let mut g = feed(8, 2, &["one\r\ntwo"]);
        let id_of_one = g.abs_row(0);
        let text = |g: &Grid, abs| -> String {
            g.row_at(abs).unwrap().iter().map(|c| c.c).collect::<String>().trim_end().to_string()
        };
        assert_eq!(text(&g, id_of_one), "one");

        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\r\nthree\r\nfour");
        assert_eq!(text(&g, id_of_one), "one", "the id followed the wrong line");
    }

    #[test]
    fn row_ids_survive_history_being_trimmed() {
        let mut g = Grid::new(8, 2);
        g.max_scrollback = 2;
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"a\r\nb\r\nc\r\nd\r\ne");
        // "a" was dropped, so its id must go dead rather than being handed to
        // whatever line moved into its slot.
        assert_eq!(g.trimmed, 1);
        assert!(g.row_at(0).is_none(), "a trimmed line must not resolve");
        let text = |abs| -> Option<String> {
            g.row_at(abs).map(|r| r.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
        };
        assert_eq!(text(1).as_deref(), Some("b"), "ids did not shift with the trim");
        assert_eq!(text(2).as_deref(), Some("c"));
        assert_eq!(text(3).as_deref(), Some("d"));
        assert_eq!(text(4).as_deref(), Some("e"));
    }

    #[test]
    fn row_ids_line_up_across_the_scrollback_fold() {
        // With the viewport scrolled back, rows above the fold come from
        // history and rows below from the live screen; ids must run
        // continuously across the join.
        let mut g = feed(8, 3, &["a\r\nb\r\nc\r\nd\r\ne"]);
        g.view_offset = 2;
        let ids: Vec<usize> = (0..3).map(|y| g.abs_row(y)).collect();
        assert_eq!(ids, vec![ids[0], ids[0] + 1, ids[0] + 2], "ids must be contiguous");
        for (y, &abs) in ids.iter().enumerate() {
            let via_view: String = g.view_row(y).unwrap().iter().map(|c| c.c).collect();
            let via_id: String = g.row_at(abs).unwrap().iter().map(|c| c.c).collect();
            assert_eq!(via_view, via_id, "row {y} disagrees with id {abs}");
        }
    }

    #[test]
    fn an_id_past_the_bottom_does_not_resolve() {
        let g = feed(8, 2, &["a\r\nb"]);
        let last = g.abs_row(1);
        assert!(g.row_at(last).is_some());
        assert!(g.row_at(last + 1).is_none());
    }

    #[test]
    fn decset_2004_toggles_bracketed_paste() {
        assert!(!Grid::new(10, 3).bracketed_paste, "off until the program asks");
        assert!(feed(10, 3, &["\x1b[?2004h"]).bracketed_paste);
        assert!(!feed(10, 3, &["\x1b[?2004h\x1b[?2004l"]).bracketed_paste);
    }

    #[test]
    fn decset_selects_a_mouse_tracking_mode() {
        assert_eq!(Grid::new(10, 3).mouse_tracking, MouseTracking::Off);
        assert_eq!(feed(10, 3, &["\x1b[?1000h"]).mouse_tracking, MouseTracking::Normal);
        assert_eq!(feed(10, 3, &["\x1b[?1002h"]).mouse_tracking, MouseTracking::ButtonEvent);
        assert_eq!(feed(10, 3, &["\x1b[?1003h"]).mouse_tracking, MouseTracking::AnyEvent);
    }

    #[test]
    fn turning_a_mouse_mode_off_stops_tracking_entirely() {
        // There is no falling back to a lesser mode; tmux relies on this when
        // it hands the mouse back.
        let g = feed(10, 3, &["\x1b[?1002h\x1b[?1002l"]);
        assert_eq!(g.mouse_tracking, MouseTracking::Off);
        let g = feed(10, 3, &["\x1b[?1003h\x1b[?1000l"]);
        assert_eq!(g.mouse_tracking, MouseTracking::Off);
    }

    #[test]
    fn decset_1006_switches_to_sgr_encoding() {
        assert_eq!(Grid::new(10, 3).mouse_encoding, MouseEncoding::Legacy);
        assert_eq!(feed(10, 3, &["\x1b[?1006h"]).mouse_encoding, MouseEncoding::Sgr);
        assert_eq!(feed(10, 3, &["\x1b[?1006h\x1b[?1006l"]).mouse_encoding, MouseEncoding::Legacy);
    }

    #[test]
    fn tmux_style_startup_leaves_the_mouse_fully_configured() {
        // What tmux actually sends for `set -g mouse on`.
        let g = feed(10, 3, &["\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h"]);
        assert!(g.alt_active);
        assert_eq!(g.mouse_tracking, MouseTracking::ButtonEvent);
        assert_eq!(g.mouse_encoding, MouseEncoding::Sgr);
    }

    #[test]
    fn alternate_scroll_is_on_until_a_program_declines_it() {
        // less and man expect the wheel to become arrows; a program that
        // handles the mouse itself turns this off.
        assert!(Grid::new(10, 3).alternate_scroll);
        assert!(!feed(10, 3, &["\x1b[?1007l"]).alternate_scroll);
        assert!(feed(10, 3, &["\x1b[?1007l\x1b[?1007h"]).alternate_scroll);
    }

    #[test]
    fn shrinking_moves_the_lost_rows_into_history() {
        // Splitting a pane shrinks it. Those rows used to be discarded, so
        // everything above the fold vanished with no way to scroll back.
        let mut g = feed(20, 5, &["one\r\ntwo\r\nthree\r\nfour\r\nfive"]);
        assert_eq!(g.scrollback.len(), 0);
        g.resize(20, 2);
        assert_eq!(row_text(&g, 0), "four");
        assert_eq!(row_text(&g, 1), "five");
        assert_eq!(g.scrollback.len(), 3, "the three rows above must be recoverable");
        let back: Vec<String> = g
            .scrollback
            .iter()
            .map(|r| r.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect();
        assert_eq!(back, vec!["one", "two", "three"]);
    }

    #[test]
    fn growing_pulls_history_back_instead_of_opening_a_gap() {
        // Closing a split should undo what opening it did.
        let mut g = feed(20, 5, &["one\r\ntwo\r\nthree\r\nfour\r\nfive"]);
        g.resize(20, 2);
        g.resize(20, 5);
        let shown: Vec<String> = (0..5).map(|y| row_text(&g, y)).collect();
        assert_eq!(shown, vec!["one", "two", "three", "four", "five"]);
        assert_eq!(g.scrollback.len(), 0, "the pulled rows must leave history");
    }

    #[test]
    fn growing_past_the_history_leaves_the_gap_at_the_top() {
        let mut g = feed(20, 2, &["a\r\nb"]);
        assert_eq!(g.scrollback.len(), 0);
        g.resize(20, 4);
        // Nothing to pull, so the blanks belong above the cursor, not below.
        assert_eq!((0..4).map(|y| row_text(&g, y)).collect::<Vec<_>>(), vec!["", "", "a", "b"]);
    }

    #[test]
    fn shrinking_keeps_the_cursor_on_screen() {
        // The cursor sits on the last used row of a shell. Anchoring purely to
        // the bottom of the buffer pushed it into history, so a freshly split
        // pane showed blank rows where the prompt had been.
        let mut g = feed(20, 5, &["one\r\ntwo\r\nthree"]);
        assert_eq!((g.cx, g.cy), (5, 2));
        g.resize(20, 2);
        assert_eq!(row_text(&g, g.cy), "three", "the cursor left its line");
        assert_eq!(g.cx, 5);
        assert!(g.cy < 2);
    }

    #[test]
    fn a_resize_round_trip_keeps_the_cursor_on_its_line() {
        let mut g = feed(20, 5, &["one\r\ntwo\r\nthree"]);
        g.resize(20, 2);
        g.resize(20, 5);
        assert_eq!(row_text(&g, g.cy), "three", "the cursor drifted off its line");
        assert_eq!(g.cx, 5);
        // And nothing was lost on the way.
        let shown: Vec<String> = (0..5).map(|y| row_text(&g, y)).collect();
        assert!(
            shown.iter().any(|r| r == "one") && shown.iter().any(|r| r == "two"),
            "history did not come back: {shown:?}"
        );
    }

    #[test]
    fn a_full_screen_programs_own_rows_never_reach_history() {
        // vim and tmux redraw themselves; their frames are not our scrollback.
        // What the main screen behind them loses off the top still is, which is
        // why this checks whose rows arrived rather than merely counting them.
        let mut g = feed(20, 4, &["a\r\nb\r\nc"]);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049h");
            parser.advance(&mut perf, b"x\r\ny\r\nz");
        }
        g.resize(20, 2);
        g.resize(20, 4);
        let history: Vec<String> = g
            .scrollback
            .iter()
            .map(|r| r.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect();
        for frame in ["x", "y", "z"] {
            assert!(!history.contains(&frame.to_string()), "{frame:?} leaked into {history:?}");
        }
    }

    #[test]
    fn shrinking_respects_the_scrollback_limit() {
        let mut g = Grid::new(20, 6);
        g.max_scrollback = 2;
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
        drop(perf);
        g.resize(20, 1);
        assert_eq!(g.scrollback.len(), 2, "history grew past its cap");
        assert_eq!(g.trimmed, 3, "trimmed rows must stay counted for row ids");
    }

    #[test]
    fn shrinking_eats_the_blank_tail_before_touching_history() {
        // The common case: a pane with a few lines of output and a prompt, then
        // split. Taking from the top first sent real output to history while
        // eleven blank rows sat underneath, and the prompt jumped to the top.
        let mut g = feed(20, 24, &["one\r\ntwo\r\n$ "]);
        g.resize(20, 12);
        assert_eq!(g.scrollback.len(), 0, "nothing needed to leave the screen");
        assert_eq!(
            (0..4).map(|y| row_text(&g, y)).collect::<Vec<_>>(),
            vec!["one", "two", "$", ""]
        );
        assert_eq!(g.cy, 2, "the cursor stayed on its line");
    }

    #[test]
    fn a_full_screen_still_pushes_from_the_top() {
        // With no blank tail to consume, the old behaviour is the right one.
        let mut g = feed(20, 5, &["a\r\nb\r\nc\r\nd\r\ne"]);
        g.resize(20, 2);
        assert_eq!((0..2).map(|y| row_text(&g, y)).collect::<Vec<_>>(), vec!["d", "e"]);
        assert_eq!(g.scrollback.len(), 3);
        assert_eq!(g.cy, 1, "the cursor ends on the new bottom row");
    }

    #[test]
    fn the_cursor_survives_any_shrink() {
        // shrink_from is at most cy + 1 - rows, so the window always contains
        // the cursor whatever the mix of blank tail and pushed history.
        for rows_old in 2..12usize {
            for cy in 0..rows_old {
                for rows_new in 1..rows_old {
                    let mut g = Grid::new(8, rows_old);
                    g.cy = cy;
                    g.resize(8, rows_new);
                    assert!(
                        g.cy < rows_new,
                        "old={rows_old} cy={cy} new={rows_new} left the cursor at {}",
                        g.cy
                    );
                }
            }
        }
    }

    /// The one route a copy has out of tmux on the far end of an ssh session.
    #[test]
    fn osc_52_puts_text_on_the_clipboard() {
        // BEL-terminated, and ST-terminated, which is what tmux sends.
        let g = feed(20, 3, &["\x1b]52;c;aGVsbG8=\x07"]);
        assert_eq!(g.pending_clipboard.as_deref(), Some("hello"));
        let g = feed(20, 3, &["\x1b]52;c;aGVsbG8=\x1b\\"]);
        assert_eq!(g.pending_clipboard.as_deref(), Some("hello"));
    }

    #[test]
    fn osc_52_takes_any_selection_name() {
        // tmux names no selection at all; xterm's own clients say c, p, or a cut
        // buffer. There is one clipboard here, so all of them write to it.
        for target in ["", "c", "p", "s0", "pc"] {
            let g = feed(20, 3, &[&format!("\x1b]52;{target};aGk=\x07")]);
            assert_eq!(g.pending_clipboard.as_deref(), Some("hi"), "target {target:?}");
        }
    }

    #[test]
    fn dec_line_drawing_turns_letters_into_box_characters() {
        // Without a UTF-8 locale — the usual state of a machine reached over
        // ssh — tmux draws its pane dividers this way, and they arrived as
        // literal rows of `q`.
        let g = feed(10, 2, &["\x1b(0qqqxlk\x1b(Bqx"]);
        assert_eq!(row_text(&g, 0), "───│┌┐qx");
    }

    #[test]
    fn shift_out_selects_the_other_charset() {
        // G1 holds the graphics set; SO maps it in, SI takes it back out.
        let g = feed(10, 2, &["\x1b)0a\x0eq\x0fq"]);
        assert_eq!(row_text(&g, 0), "a─q");
    }

    #[test]
    fn the_saved_cursor_carries_its_charset() {
        // Save while drawing boxes, print ASCII, restore: `q` must be a line
        // again, not the letter.
        let g = feed(10, 2, &["\x1b(0\x1b7\x1b(Bx\x1b8q"]);
        assert_eq!(row_text(&g, 0), "─", "the restore did not bring the charset back");
    }

    #[test]
    fn a_charset_switch_does_not_cancel_a_pending_wrap() {
        let g = feed(4, 3, &["abcd", "\x1b(0", "q"]);
        assert_eq!(row_text(&g, 0), "abcd");
        assert_eq!(row_text(&g, 1), "─");
    }

    /// Runs `input` and returns whatever the terminal wants to send back.
    fn reply_to(input: &[u8]) -> Vec<u8> {
        let mut g = Grid::new(20, 5);
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, input);
        perf.reply
    }

    #[test]
    fn the_two_device_attributes_queries_get_their_own_answers() {
        // The regression. tmux asks both the moment it starts. Answering the
        // secondary query with the primary reply gave tmux something it could
        // not parse, and tmux hands unparsed input to the pane — so every
        // session began with `^[[?6c` echoed and `6c` typed at the prompt.
        assert_eq!(reply_to(b"\x1b[c"), b"\x1b[?6c");
        assert_eq!(reply_to(b"\x1b[0c"), b"\x1b[?6c");
        assert_eq!(reply_to(b"\x1b[>c"), b"\x1b[>0;0;0c");
        assert_eq!(reply_to(b"\x1b[>0c"), b"\x1b[>0;0;0c");
    }

    #[test]
    fn a_secondary_attributes_reply_is_never_a_primary_one() {
        // Stated as the property rather than the byte string: whatever we grow
        // into, the two replies must stay tellable apart by their asker.
        let secondary = reply_to(b"\x1b[>c");
        assert!(secondary.starts_with(b"\x1b[>"), "tmux looks for CSI > here");
        assert_ne!(secondary, reply_to(b"\x1b[c"));
    }

    #[test]
    fn a_cursor_report_answers_only_the_query_that_asked_for_it() {
        assert_eq!(reply_to(b"\x1b[3;7H\x1b[6n"), b"\x1b[3;7R");
        // DECXCPR wants a `CSI ? ... R`. Saying nothing is better than saying
        // something the asker will feed to the shell as keystrokes.
        assert!(reply_to(b"\x1b[3;7H\x1b[?6n").is_empty());
        // The mode query tmux sends looking for a light/dark answer.
        assert!(reply_to(b"\x1b[?996n").is_empty());
    }

    #[test]
    fn xtversion_names_the_terminal() {
        let r = reply_to(b"\x1b[>0q");
        let s = String::from_utf8_lossy(&r);
        assert!(s.starts_with("\x1bP>|koma("), "unexpected XTVERSION reply: {s:?}");
        assert!(s.ends_with("\x1b\\"), "XTVERSION must be ST-terminated: {s:?}");
    }

    #[test]
    fn setting_the_cursor_style_still_works() {
        // `CSI Ps SP q` shares its final byte with XTVERSION, so the two must
        // not be confused for each other.
        let g = feed(10, 3, &["\x1b[5 q"]);
        assert!(matches!(g.cursor_shape, CursorShape::Bar));
        assert!(reply_to(b"\x1b[5 q").is_empty());
    }

    #[test]
    fn splitting_while_a_full_screen_program_runs_keeps_the_shell_behind_it() {
        // Reported in review of #12: resize() left the saved main screen at its
        // old size, so the size guard in set_alt_screen blanked it on the way
        // out. Opening vim, splitting the pane, then quitting wiped the shell.
        let mut g = feed(20, 6, &["shell1\r\nshell2\r\nshell3"]);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049h");
            parser.advance(&mut perf, b"vim-a\r\nvim-b");
        }
        g.resize(20, 4);
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049l");
        }
        assert!(!g.alt_active);
        let shown: Vec<String> = (0..4).map(|y| row_text(&g, y)).collect();
        assert_eq!(shown, vec!["shell1", "shell2", "shell3", ""]);
    }

    #[test]
    fn the_saved_screen_survives_growing_as_well_as_shrinking() {
        let mut g = feed(20, 4, &["a\r\nb"]);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049h");
        }
        g.resize(20, 8);
        g.resize(30, 8);
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049l");
        }
        let shown: Vec<String> = (0..8).map(|y| row_text(&g, y)).collect();
        assert!(shown.contains(&"a".to_string()) && shown.contains(&"b".to_string()), "{shown:?}");
    }

    #[test]
    fn the_cursor_behind_a_full_screen_program_comes_back_in_range() {
        let mut g = feed(20, 6, &["one\r\ntwo\r\nthree"]);
        let (cx, cy) = (g.cx, g.cy);
        assert_eq!((cx, cy), (5, 2));
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049h");
        }
        g.resize(20, 3);
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049l");
        }
        assert!(g.cy < 3, "cursor at {} in a 3-row screen", g.cy);
        assert_eq!(row_text(&g, g.cy), "three", "the cursor left its line");
    }

    #[test]
    fn a_full_shell_screen_behind_vim_survives_a_split_intact() {
        // Reported in review of #13: the saved main screen was cut to a window
        // measured against *vim's* cursor, which sits at the top. The shell's
        // own cursor is at the bottom, so its screen lost the prompt and the
        // eleven rows above it — and they were not in the scrollback either.
        let mut g = Grid::new(20, 24);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            for i in 1..=23 {
                parser.advance(&mut perf, format!("l{i}\r\n").as_bytes());
            }
            parser.advance(&mut perf, b"$ ");
            parser.advance(&mut perf, b"\x1b[?1049h");
        }
        g.resize(20, 12);
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049l");
        }

        assert_eq!(row_text(&g, g.cy), "$", "the prompt must come back with the shell");
        assert_eq!(
            (0..12).map(|y| row_text(&g, y)).collect::<Vec<_>>(),
            (13..=23).map(|i| format!("l{i}")).chain(["$".to_string()]).collect::<Vec<_>>()
        );

        // And nothing went missing on the way: every line is still reachable.
        let reachable: Vec<String> = (0..g.trimmed + g.scrollback.len() + g.rows)
            .filter_map(|abs| g.row_at(abs))
            .map(|r| r.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect();
        for i in 1..=23 {
            assert!(reachable.contains(&format!("l{i}")), "l{i} was lost behind vim");
        }
    }

    #[test]
    fn osc_52_carries_multibyte_text() {
        // 日本語 as base64. Nothing about the payload is per-character, so this
        // is really a check that we decode bytes and only then read them as text.
        let g = feed(20, 3, &["\x1b]52;c;5pel5pys6Kqe\x07"]);
        assert_eq!(g.pending_clipboard.as_deref(), Some("日本語"));
    }

    /// Answering would hand the local clipboard to whatever is on the far end.
    #[test]
    fn osc_52_queries_are_never_answered() {
        let mut g = Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        let mut perf = Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\x1b]52;c;?\x07");
        assert!(perf.reply.is_empty(), "the clipboard leaked back to the program");
        assert!(g.pending_clipboard.is_none());
    }

    #[test]
    fn osc_52_leaves_the_clipboard_alone_when_it_says_nothing() {
        // xterm reads an empty payload as "clear the selection". Wiping what the
        // user copied on a remote program's say-so is worse than ignoring it.
        let g = feed(20, 3, &["\x1b]52;c;\x07"]);
        assert!(g.pending_clipboard.is_none());
        let g = feed(20, 3, &["\x1b]52;c\x07"]);
        assert!(g.pending_clipboard.is_none());
    }

    #[test]
    fn a_bad_osc_52_payload_is_dropped_rather_than_pasted_as_garbage() {
        let g = feed(20, 3, &["\x1b]52;c;not base64!!\x07"]);
        assert!(g.pending_clipboard.is_none());
    }

    #[test]
    fn base64_decodes_every_padding_case() {
        // The three group lengths: no padding, one =, two =.
        assert_eq!(decode_base64(b"YWJj").unwrap(), b"abc");
        assert_eq!(decode_base64(b"YWJjZA==").unwrap(), b"abcd");
        assert_eq!(decode_base64(b"YWJjZGU=").unwrap(), b"abcde");
        // Unpadded is accepted too: senders vary, and the length is unambiguous.
        assert_eq!(decode_base64(b"YWJjZA").unwrap(), b"abcd");
        assert_eq!(decode_base64(b"YWJjZGU").unwrap(), b"abcde");
        assert_eq!(decode_base64(b"").unwrap(), b"");
        // Both non-alphanumeric characters of the standard alphabet.
        assert_eq!(decode_base64(b"++//").unwrap(), &[0xfb, 0xef, 0xff]);
    }

    #[test]
    fn base64_tolerates_wrapped_lines_but_not_junk() {
        assert_eq!(decode_base64(b"YWJj\r\nZGVm").unwrap(), b"abcdef");
        assert_eq!(decode_base64(b" YWJj ").unwrap(), b"abc");
        // A lone trailing character spells no whole byte: cut short, not merely
        // unpadded, and decoding it would hand back a truncated copy.
        assert!(decode_base64(b"YWJjZ").is_none());
        assert!(decode_base64(b"Y").is_none());
        assert!(decode_base64(b"YWJj$").is_none());
        // URL-safe base64 is a different alphabet; rejecting beats mangling.
        assert!(decode_base64(b"a-b_").is_none());
    }

    #[test]
    fn a_split_behind_vim_matches_a_split_without_it() {
        // The invariant #12 established must not lapse just because a
        // full-screen program happens to be in front.
        let build = || {
            let mut g = Grid::new(20, 24);
            let mut parser = vte::Parser::new();
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            for i in 1..=23 {
                parser.advance(&mut perf, format!("l{i}\r\n").as_bytes());
            }
            parser.advance(&mut perf, b"$ ");
            drop(perf);
            g
        };

        let mut plain = build();
        plain.resize(20, 12);

        let mut behind = build();
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut behind, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049h");
        }
        behind.resize(20, 12);
        {
            let mut perf = Performer { grid: &mut behind, reply: Vec::new() };
            parser.advance(&mut perf, b"\x1b[?1049l");
        }

        assert_eq!(
            (0..12).map(|y| row_text(&behind, y)).collect::<Vec<_>>(),
            (0..12).map(|y| row_text(&plain, y)).collect::<Vec<_>>(),
        );
        assert_eq!(behind.scrollback.len(), plain.scrollback.len());
        assert_eq!(behind.cy, plain.cy);
    }
}

#[cfg(test)]
mod split_scenario {
    use super::tests::row_text;
    use super::*;

    /// What splitting a pane does to a screenful of shell output: the pane
    /// halves in height, then the split is closed again.
    #[test]
    fn splitting_a_pane_and_closing_it_keeps_every_line() {
        let mut g = Grid::new(40, 24);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            for i in 1..=20 {
                parser.advance(&mut perf, format!("line{i}\r\n").as_bytes());
            }
            parser.advance(&mut perf, b"$ ");
        }

        // Split: the pane is now half as tall.
        g.resize(40, 12);
        assert_eq!(row_text(&g, g.cy), "$", "the prompt must stay in view");

        // Everything above the fold is reachable by scrolling.
        let reachable: Vec<String> = (0..g.trimmed + g.scrollback.len() + g.rows)
            .filter_map(|abs| g.row_at(abs))
            .map(|r| r.iter().map(|c| c.c).collect::<String>().trim_end().to_string())
            .collect();
        for i in 1..=20 {
            assert!(reachable.contains(&format!("line{i}")), "line{i} was lost by the split");
        }

        // Close the split: the pane grows back.
        g.resize(40, 24);
        assert_eq!(row_text(&g, g.cy), "$");
        let shown: Vec<String> = (0..24).map(|y| row_text(&g, y)).collect();
        for i in 1..=20 {
            assert!(shown.contains(&format!("line{i}")), "line{i} did not come back: {shown:?}");
        }
    }

    /// The same, but narrower as well — splitting left/right changes columns.
    #[test]
    fn a_narrower_pane_keeps_its_rows_even_though_they_are_clipped() {
        let mut g = Grid::new(40, 6);
        let mut parser = vte::Parser::new();
        {
            let mut perf = Performer { grid: &mut g, reply: Vec::new() };
            parser.advance(&mut perf, b"a-long-line-of-output-here\r\nsecond\r\n$ ");
        }
        g.resize(20, 6);
        // No reflow, so the tail is clipped — but the line is still there.
        assert!(row_text(&g, 0).starts_with("a-long-line-of-outpu"));
        assert_eq!(row_text(&g, 1), "second");
        assert_eq!(row_text(&g, g.cy), "$");
    }
}
