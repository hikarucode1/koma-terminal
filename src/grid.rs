//! Terminal screen state: the cell grid, scrollback, and the `vte::Perform`
//! implementation that turns parsed escape sequences into grid mutations.

use unicode_width::UnicodeWidthChar;

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
    saved: Option<(usize, usize, Color, Color, u8)>,

    /// Scroll region, inclusive rows.
    top: usize,
    bot: usize,

    /// Set once the cursor has been pushed past the last column; the next
    /// printable character wraps first. (deferred wrap / "pending wrap" state)
    wrap_pending: bool,

    pub title: String,
    pub bell: bool,
    /// Application cursor keys (DECCKM) — changes what arrow keys send.
    pub app_cursor_keys: bool,
    /// Alternate screen buffer, saved main screen while active.
    alt: Option<(Vec<Cell>, usize, usize)>,
    pub alt_active: bool,
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
            top: 0,
            bot: rows - 1,
            wrap_pending: false,
            title: String::new(),
            bell: false,
            app_cursor_keys: false,
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
        // No reflow: copy the bottom-anchored window of old content across.
        let mut next = vec![Cell::default(); cols * rows];
        let copy_rows = self.rows.min(rows);
        let copy_cols = self.cols.min(cols);
        let src_start = self.rows - copy_rows;
        let dst_start = rows - copy_rows;
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                next[(dst_start + r) * cols + c] = self.cells[(src_start + r) * self.cols + c];
            }
        }
        self.cells = next;
        self.cx = self.cx.min(cols - 1);
        self.cy = (self.cy + dst_start).saturating_sub(src_start).min(rows - 1);
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

    fn put(&mut self, c: char) {
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
            's' => g.saved = Some((g.cx, g.cy, g.fg, g.bg, g.flags)),
            'u' => {
                if let Some((cx, cy, fg, bg, fl)) = g.saved {
                    g.cx = cx.min(g.cols - 1);
                    g.cy = cy.min(g.rows - 1);
                    g.fg = fg;
                    g.bg = bg;
                    g.flags = fl;
                }
            }
            'q' if intermediates.first() == Some(&b' ') => {
                g.cursor_shape = match flat.first().copied().unwrap_or(0) {
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => CursorShape::Block,
                };
            }
            'n' if flat.first() == Some(&6) => {
                // Device Status Report: report cursor position.
                self.reply.extend_from_slice(format!("\x1b[{};{}R", g.cy + 1, g.cx + 1).as_bytes());
            }
            'c' => self.reply.extend_from_slice(b"\x1b[?6c"), // identify as a VT102
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        let g = &mut self.grid;
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
            b'7' => g.saved = Some((g.cx, g.cy, g.fg, g.bg, g.flags)),
            b'8' => {
                if let Some((cx, cy, fg, bg, fl)) = g.saved {
                    g.cx = cx.min(g.cols - 1);
                    g.cy = cy.min(g.rows - 1);
                    g.fg = fg;
                    g.bg = bg;
                    g.flags = fl;
                }
            }
            b'c' => {
                let (cols, rows) = (g.cols, g.rows);
                **g = Grid::new(cols, rows);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/1/2 all set (some flavour of) the window title.
        match params.first() {
            Some(&b"0") | Some(&b"1") | Some(&b"2") => {
                if let Some(t) = params.get(1) {
                    self.grid.title = String::from_utf8_lossy(t).into_owned();
                }
            }
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

    fn row_text(g: &Grid, y: usize) -> String {
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
}
