//! koma — a tiling terminal emulator.
//!
//! Panes live in an n-ary split tree; each leaf owns a pty and a screen grid.
//! Everything on screen is drawn as instanced quads through one wgpu pipeline.

mod font;
mod gpu;
mod grid;
mod input;
mod mouse;
mod pane;
mod pty;
mod selection;
mod theme;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use unicode_width::UnicodeWidthChar;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use font::FontSet;
use gpu::{FrameStatus, Gpu, Inst};
use grid::{
    Cell, Color, CursorShape, FLAG_BOLD, FLAG_INVERSE, FLAG_UNDERLINE, FLAG_WIDE_SPACER, Grid,
};
use mouse::{Button as MouseBtn, Event as MouseEv, Mods as MouseMods};
use pane::{Axis, Node, PaneId, Rect};
use pty::Pty;
use selection::{Mode as SelMode, Point, Selection};
use theme::{Theme, to_linear};

/// Gap between the window edge and the pane area, in logical pixels.
const PADDING: f32 = 8.0;
const DEFAULT_FONT_PT: f32 = 13.0;
const MIN_FONT_PT: f32 = 6.0;
const MAX_FONT_PT: f32 = 48.0;

#[derive(Debug)]
enum UserEvent {
    /// A pty produced output; drain the channels and redraw.
    PtyWake,
}

struct PaneState {
    pty: Pty,
    grid: Grid,
    parser: vte::Parser,
    rect: Rect,
    exited: bool,
}

impl PaneState {
    fn new(cols: usize, rows: usize, wake: impl Fn() + Send + 'static) -> Result<Self> {
        let pty = Pty::spawn(cols as u16, rows as u16, wake)?;
        Ok(PaneState {
            pty,
            grid: Grid::new(cols, rows),
            parser: vte::Parser::new(),
            rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            exited: false,
        })
    }

    /// Feeds pty output through the VT parser, returning any reply the terminal
    /// owes the shell (cursor position reports and the like).
    fn pump(&mut self) -> (bool, Vec<u8>) {
        let bytes = self.pty.read_available();
        if bytes.is_empty() {
            return (false, Vec::new());
        }
        let mut perf = grid::Performer { grid: &mut self.grid, reply: Vec::new() };
        self.parser.advance(&mut perf, &bytes);
        let reply = std::mem::take(&mut perf.reply);
        (true, reply)
    }
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    fonts: Option<FontSet>,
    proxy: EventLoopProxy<UserEvent>,

    panes: HashMap<PaneId, PaneState>,
    tree: Node,
    focus: PaneId,
    next_id: PaneId,

    mods: ModifiersState,
    cursor_pos: (f32, f32),
    scale: f32,
    font_pt: f32,
    /// Leftover sub-line scroll from the last wheel event, and the pane it
    /// belongs to.
    scroll_accum: f32,
    wheel_target: PaneId,
    /// Whether the last wheel event was read off the horizontal axis.
    wheel_across: bool,
    /// Text the IME is composing for the focused pane, if any.
    preedit: Preedit,
    /// Selection in progress or finished, and the pane it belongs to.
    selection: Option<(PaneId, Selection)>,
    /// True while the left button is down, so motion extends the selection.
    dragging: bool,
    /// Set while a press has been handed to a program instead of starting a
    /// selection. Holds the pane it went to, so the motion and release that
    /// follow reach the same program even if the pointer wanders off it.
    reporting_drag: Option<PaneId>,
    /// Last left-press, for turning repeats into word and line selection.
    last_click: Option<(Instant, PaneId, usize, usize)>,
    click_streak: u32,
    clipboard: Option<arboard::Clipboard>,
    theme: Theme,
    /// Reused frame buffer so we're not allocating a Vec per redraw.
    instances: Vec<Inst>,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        App {
            window: None,
            gpu: None,
            fonts: None,
            proxy,
            panes: HashMap::new(),
            tree: Node::leaf(0),
            focus: 0,
            next_id: 1,
            mods: ModifiersState::empty(),
            cursor_pos: (0.0, 0.0),
            scale: 1.0,
            font_pt: DEFAULT_FONT_PT,
            scroll_accum: 0.0,
            wheel_target: 0,
            wheel_across: false,
            preedit: Preedit::default(),
            selection: None,
            dragging: false,
            reporting_drag: None,
            last_click: None,
            click_streak: 0,
            clipboard: None,
            theme: Theme::default(),
            instances: Vec::new(),
        }
    }

    fn waker(&self) -> impl Fn() + Send + 'static {
        let proxy = self.proxy.clone();
        move || {
            let _ = proxy.send_event(UserEvent::PtyWake);
        }
    }

    fn cell_size(&self) -> (f32, f32) {
        self.fonts.as_ref().map(|f| (f.cell_w, f.cell_h)).unwrap_or((8.0, 16.0))
    }

    /// The whole pane area in physical pixels.
    fn content_rect(&self) -> Rect {
        let (w, h) = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height as f32))
            .unwrap_or((960.0, 640.0));
        let p = PADDING * self.scale;
        Rect { x: p, y: p, w: (w - 2.0 * p).max(1.0), h: (h - 2.0 * p).max(1.0) }
    }

    fn layout(&self) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.tree.layout(self.content_rect(), &mut out);
        out
    }

    /// Recomputes pane rects and pushes the resulting size onto each pty.
    fn relayout(&mut self) {
        let (cw, ch) = self.cell_size();
        for (id, rect) in self.layout() {
            let Some(p) = self.panes.get_mut(&id) else { continue };
            p.rect = rect;
            let cols = ((rect.w / cw).floor() as usize).max(1);
            let rows = ((rect.h / ch).floor() as usize).max(1);
            if cols != p.grid.cols || rows != p.grid.rows {
                p.grid.resize(cols, rows);
                p.pty.resize(cols as u16, rows as u16);
                // Row ids hold across a shrink now, but not across a width
                // change (columns are truncated) or a grow that outruns the
                // history, so a live selection can still end up pointing at
                // something else.
                if self.selection.is_some_and(|(sid, _)| sid == id) {
                    self.selection = None;
                    self.dragging = false;
                }
            }
        }
        // Panes just moved, so the IME's anchor is stale. Doing it here covers
        // resize, scale changes, font size and closing an unfocused pane in one
        // place, none of which go through set_focus.
        self.update_ime_area();
    }

    fn spawn_pane(&mut self, cols: usize, rows: usize) -> Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let pane = PaneState::new(cols, rows, self.waker())?;
        self.panes.insert(id, pane);
        Ok(id)
    }

    fn split_focused(&mut self, axis: Axis) {
        let (cw, ch) = self.cell_size();
        // Give the new pane a plausible starting size; relayout fixes it up.
        let base = self.panes.get(&self.focus).map(|p| p.rect).unwrap_or(self.content_rect());
        let (w, h) = match axis {
            Axis::Horizontal => (base.w / 2.0, base.h),
            Axis::Vertical => (base.w, base.h / 2.0),
        };
        let cols = ((w / cw).floor() as usize).max(1);
        let rows = ((h / ch).floor() as usize).max(1);

        match self.spawn_pane(cols, rows) {
            Ok(new_id) => {
                if self.tree.split(self.focus, axis, new_id) {
                    self.focus = new_id;
                    // relayout() refreshes the IME anchor on its way out.
                    self.relayout();
                } else {
                    // Focus wasn't in the tree; drop the pane we just made.
                    if let Some(mut p) = self.panes.remove(&new_id) {
                        p.pty.kill();
                    }
                }
            }
            Err(e) => log::error!("failed to spawn pane: {e}"),
        }
    }

    /// Removes a pane. Returns false when it was the last one (caller exits).
    fn close_pane(&mut self, id: PaneId) -> bool {
        if !self.tree.remove(id) {
            return false; // root leaf — nothing to collapse into
        }
        if let Some(mut p) = self.panes.remove(&id) {
            p.pty.kill();
        }
        self.drop_composition_of(id);
        self.relayout();
        if self.focus == id {
            let mut leaves = Vec::new();
            self.tree.leaves(&mut leaves);
            self.set_focus(leaves.first().copied().unwrap_or(0));
        }
        true
    }

    fn focus_direction(&mut self, dx: f32, dy: f32) {
        let rects = self.layout();
        if let Some(id) = pane::neighbor(&rects, self.focus, dx, dy) {
            self.set_focus(id);
        }
    }

    /// Moves focus. Any in-flight composition stays bound to the pane it
    /// started in, so it neither follows nor is silently thrown away.
    fn set_focus(&mut self, id: PaneId) {
        if self.focus == id {
            return;
        }
        self.focus = id;
        self.update_ime_area();
    }

    fn set_font_pt(&mut self, pt: f32) {
        let pt = pt.clamp(MIN_FONT_PT, MAX_FONT_PT);
        if (pt - self.font_pt).abs() < 0.01 {
            return;
        }
        self.font_pt = pt;
        if let (Some(f), Some(g)) = (self.fonts.as_mut(), self.gpu.as_mut()) {
            f.set_size(pt * self.scale);
            // The atlas was reset, so its texture contents are stale.
            g.rebuild_bind_group();
        }
        self.relayout();
    }

    /// App-level shortcuts. Returns true if the key was consumed.
    fn handle_shortcut(&mut self, key: &Key, event_loop: &ActiveEventLoop) -> bool {
        let Some(shift) = leader_shift(self.mods) else {
            // Scrollback needs no leader beyond Shift.
            if self.mods.shift_key() {
                if let Key::Named(n) = key {
                    let page = self.page_lines();
                    match n {
                        NamedKey::PageUp => {
                            self.scroll_focused(page);
                            return true;
                        }
                        NamedKey::PageDown => {
                            self.scroll_focused(-page);
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            return false;
        };

        let alt = self.mods.alt_key();

        if let Key::Named(n) = key {
            // Scrollback. Mac keyboards have no PageUp, so Cmd+Shift+Arrow is
            // the binding that actually gets used there. Linux keeps
            // Ctrl+Shift+Arrow for resizing and scrolls with Shift+PageUp.
            if shift && !alt {
                match n {
                    NamedKey::ArrowUp => {
                        self.scroll_focused(1);
                        return true;
                    }
                    NamedKey::ArrowDown => {
                        self.scroll_focused(-1);
                        return true;
                    }
                    _ => {}
                }
            }
            match n {
                NamedKey::PageUp => {
                    let page = self.page_lines();
                    self.scroll_focused(page);
                    return true;
                }
                NamedKey::PageDown => {
                    let page = self.page_lines();
                    self.scroll_focused(-page);
                    return true;
                }
                NamedKey::Home => {
                    self.scroll_to_edge(true);
                    return true;
                }
                NamedKey::End => {
                    self.scroll_to_edge(false);
                    return true;
                }
                _ => {}
            }
            match n {
                NamedKey::ArrowLeft if alt => {
                    self.focus_direction(-1.0, 0.0);
                    return true;
                }
                NamedKey::ArrowRight if alt => {
                    self.focus_direction(1.0, 0.0);
                    return true;
                }
                NamedKey::ArrowUp if alt => {
                    self.focus_direction(0.0, -1.0);
                    return true;
                }
                NamedKey::ArrowDown if alt => {
                    self.focus_direction(0.0, 1.0);
                    return true;
                }
                NamedKey::ArrowLeft => {
                    self.tree.resize(self.focus, Axis::Horizontal, -0.02);
                    self.relayout();
                    return true;
                }
                NamedKey::ArrowRight => {
                    self.tree.resize(self.focus, Axis::Horizontal, 0.02);
                    self.relayout();
                    return true;
                }
                NamedKey::ArrowUp => {
                    self.tree.resize(self.focus, Axis::Vertical, -0.02);
                    self.relayout();
                    return true;
                }
                NamedKey::ArrowDown => {
                    self.tree.resize(self.focus, Axis::Vertical, 0.02);
                    self.relayout();
                    return true;
                }
                _ => return false,
            }
        }

        let Key::Character(s) = key else { return false };
        // Lowercase so Cmd+Shift+D and Cmd+D land in the same arm.
        match s.to_lowercase().as_str() {
            // Cmd+D / Cmd+Shift+D on macOS. Under the Ctrl+Shift leader the
            // shifted variant is unreachable, so E is the second split key —
            // it works under either leader.
            "d" => {
                self.split_focused(if shift { Axis::Vertical } else { Axis::Horizontal });
                true
            }
            "e" => {
                self.split_focused(Axis::Vertical);
                true
            }
            "c" => {
                // Always consume it. Falling through would reach input::encode,
                // and under the Ctrl+Shift leader that means Ctrl+C — pressing
                // the copy key with nothing selected would kill the running job.
                self.copy_selection();
                true
            }
            "v" => {
                self.paste_clipboard();
                true
            }
            "w" => {
                let id = self.focus;
                if !self.close_pane(id) {
                    event_loop.exit();
                }
                true
            }
            "[" => {
                self.cycle_focus(-1);
                true
            }
            "]" => {
                self.cycle_focus(1);
                true
            }
            "+" | "=" => {
                self.set_font_pt(self.font_pt + 1.0);
                true
            }
            "-" => {
                self.set_font_pt(self.font_pt - 1.0);
                true
            }
            "0" => {
                self.set_font_pt(DEFAULT_FONT_PT);
                true
            }
            _ => false,
        }
    }

    fn cycle_focus(&mut self, step: isize) {
        let mut leaves = Vec::new();
        self.tree.leaves(&mut leaves);
        if leaves.is_empty() {
            return;
        }
        let cur = leaves.iter().position(|&id| id == self.focus).unwrap_or(0) as isize;
        let n = leaves.len() as isize;
        self.set_focus(leaves[(cur + step).rem_euclid(n) as usize]);
    }

    /// Scrolls a pane's viewport through its scrollback. Positive `lines` moves
    /// back through history.
    ///
    /// Not a no-op on the alternate screen, whatever it looks like. `view_row`
    /// does not ask which screen is showing, so this covers a running
    /// full-screen program with older history.
    ///
    /// Which history, exactly: the lines that had already scrolled off *before*
    /// the program started. The screenful it replaced is not among them — that
    /// went into the saved buffer, which nothing can scroll to. Measured, not
    /// assumed: a 3-row pane holding `SCREEN-A/B/C` over a history of
    /// `old1/old2` shows `old2` and `old1` on the way back, never `SCREEN-C`.
    ///
    /// It comes back by scrolling the other way, by typing or pasting, by a
    /// resize, or when the program leaves the alternate screen. Not on its own:
    /// `scroll_up` adds to `view_offset` rather than clearing it, to keep a
    /// scrolled-back view on the lines it is showing.
    ///
    /// The keyboard's scrollback keys come through here and do all of this on
    /// the alternate screen, deliberately. The *wheel* does not, unless Shift
    /// is held — see `wheel_action`, which is a different question from this
    /// one and answers it for itself.
    fn scroll_pane(&mut self, id: PaneId, lines: isize) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        if lines == 0 {
            return;
        }
        p.grid.scroll_view(lines);
    }

    /// Scrolling from the wheel or trackpad. On the alternate screen this turns
    /// into arrow keys for the application — xterm calls it alternate scroll.
    ///
    /// Deliberately wheel-only. Routing the keyboard's scrollback keys through
    /// here too would make `Shift+PageUp` move an application's *cursor*
    /// instead of its view, and a page's worth of arrows is not a page anyway.
    ///
    /// Reached only when no mouse report went out — which does not mean none
    /// was wanted. `wheel_action` has the table.
    ///
    /// `id` is the pane under the pointer, or the focused one when the pointer
    /// is over the padding or the gap between panes and belongs to no pane at
    /// all. That targeting is older than this routing and unchanged by it.
    fn wheel_scroll(&mut self, id: PaneId, lines: isize, shift: bool) {
        if lines == 0 {
            return;
        }
        let Some(p) = self.panes.get_mut(&id) else { return };
        let holds_mouse = p.grid.mouse_tracking != mouse::Tracking::Off;
        let action = wheel_action(shift, holds_mouse, p.grid.alt_active, p.grid.alternate_scroll);
        let bytes = apply_wheel(action, &mut p.grid, lines);
        if !bytes.is_empty() {
            p.pty.write(&bytes);
        }
    }

    fn scroll_focused(&mut self, lines: isize) {
        self.scroll_pane(self.focus, lines);
    }

    /// Jumps to the oldest retained line, or back to the live screen.
    fn scroll_to_edge(&mut self, top: bool) {
        let Some(p) = self.panes.get_mut(&self.focus) else { return };
        p.grid.view_offset = if top { p.grid.scrollback.len() } else { 0 };
    }

    fn page_lines(&self) -> isize {
        self.panes.get(&self.focus).map(|p| p.grid.rows.saturating_sub(1)).unwrap_or(10).max(1)
            as isize
    }

    /// The pane a composition belongs to: its owner, or the focused pane when
    /// nothing is being composed.
    fn composing_pane(&self) -> PaneId {
        self.preedit.owner.unwrap_or(self.focus)
    }

    /// Grid position where composing text starts: the owning pane's cursor.
    fn composition_origin(&self) -> Option<(Rect, usize, usize, usize, usize)> {
        let p = self.panes.get(&self.composing_pane())?;
        let (cw, ch) = self.cell_size();
        let cols = ((p.rect.w / cw).floor().max(1.0) as usize).min(p.grid.cols);
        let rows = ((p.rect.h / ch).floor().max(1.0) as usize).min(p.grid.rows);
        Some((p.rect, p.grid.cx.min(cols.saturating_sub(1)), p.grid.cy, cols, rows))
    }

    /// Tells the OS where to park the candidate window, so it tracks the caret
    /// instead of sitting in a corner.
    fn update_ime_area(&self) {
        let (Some(window), Some((rect, cx, cy, cols, rows))) =
            (self.window.as_ref(), self.composition_origin())
        else {
            return;
        };
        let (cw, ch) = self.cell_size();
        // Anchor to the end of the composing text, which is where the caret is.
        // Clipped to the pane's real row count, the same as the renderer — an
        // unclipped layout could hand the OS a coordinate outside the window.
        let cells = layout_preedit(&self.preedit.text, self.preedit.target, (cx, cy), cols, rows);
        let (col, row) = preedit_caret(&cells, (cx, cy), cols);
        let row = row.min(rows.saturating_sub(1));
        window.set_ime_cursor_area(
            PhysicalPosition::new(rect.x + col as f32 * cw, rect.y + row as f32 * ch),
            PhysicalSize::new(cw, ch),
        );
    }

    fn handle_ime(&mut self, ime: Ime) {
        match ime {
            Ime::Enabled => self.update_ime_area(),
            Ime::Preedit(text, target) => {
                let focus = self.focus;
                self.preedit.update(text, target, focus);
                // Composing is input, so leave the scrollback the way typing
                // does. Otherwise the preedit would be drawn at the live
                // cursor's coordinates, on top of history.
                //
                // Only for a real composition: an empty preedit also arrives
                // when the IME is dismissed or the input source changes, and
                // yanking the view back then would look unprovoked.
                if self.preedit.is_active() {
                    let id = self.composing_pane();
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.grid.view_offset = 0;
                    }
                }
                self.update_ime_area();
                self.request_redraw();
            }
            Ime::Commit(text) => {
                // Composition finished: only now does the shell hear about it,
                // and it goes to the pane it was composed in.
                let owner = self.preedit.commit(self.focus);
                if let Some(p) = self.panes.get_mut(&owner) {
                    p.grid.view_offset = 0;
                    p.pty.write(text.as_bytes());
                }
                self.update_ime_area();
                self.request_redraw();
            }
            Ime::Disabled => {
                self.preedit.clear();
                self.request_redraw();
            }
        }
    }

    /// Abandons a composition whose pane is going away — otherwise it would
    /// render nowhere and commit into a dead pty.
    fn drop_composition_of(&mut self, id: PaneId) {
        if self.preedit.owner == Some(id) {
            self.preedit.clear();
            self.request_redraw();
        }
    }

    /// Turns a window position into a cell in some pane, as `(pane, abs row,
    /// column)`. Columns and rows are clamped to the pane, so dragging off the
    /// edge extends to it rather than stopping.
    fn cell_at(&self, px: f32, py: f32) -> Option<(PaneId, usize, usize)> {
        let (cw, ch) = self.cell_size();
        let (id, rect) = self.layout().into_iter().find(|(_, r)| r.contains(px, py))?;
        let p = self.panes.get(&id)?;
        let rows = ((rect.h / ch).floor().max(1.0) as usize).min(p.grid.rows);
        let cols = ((rect.w / cw).floor().max(1.0) as usize).min(p.grid.cols);
        let y = (((py - rect.y) / ch).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let x = (((px - rect.x) / cw).floor().max(0.0) as usize).min(cols);
        Some((id, p.grid.abs_row(y), x))
    }

    /// As `cell_at`, but for a drag that has wandered outside every pane: it
    /// stays with the pane the selection started in.
    fn drag_cell(&self, px: f32, py: f32, id: PaneId) -> Option<(usize, usize)> {
        let (cw, ch) = self.cell_size();
        let p = self.panes.get(&id)?;
        let rect = p.rect;
        let rows = ((rect.h / ch).floor().max(1.0) as usize).min(p.grid.rows);
        let cols = ((rect.w / cw).floor().max(1.0) as usize).min(p.grid.cols);
        let y = (((py - rect.y) / ch).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let x = (((px - rect.x) / cw).floor().max(0.0) as usize).min(cols);
        Some((p.grid.abs_row(y), x))
    }

    /// The row as one entry per **grid column**, so mouse columns index it
    /// directly. The spacer half of a double-width character is `None`: it must
    /// keep its column (or every column after a CJK glyph would be off by one)
    /// while contributing nothing to the text.
    fn row_cells(&self, id: PaneId, abs: usize) -> Option<Vec<Option<char>>> {
        let p = self.panes.get(&id)?;
        Some(
            p.grid
                .row_at(abs)?
                .iter()
                .map(|c| (c.flags & FLAG_WIDE_SPACER == 0).then_some(c.c))
                .collect(),
        )
    }

    /// Expands a click to the span the current mode selects: the cell itself,
    /// the word around it, or the whole line.
    fn span_for(&self, id: PaneId, abs: usize, col: usize, mode: SelMode) -> (Point, Point) {
        match mode {
            SelMode::Char => (Point::new(abs, col), Point::new(abs, col)),
            SelMode::Word => {
                let row = self.row_cells(id, abs).unwrap_or_default();
                let (s, e) = selection::word_at(&row, col);
                (Point::new(abs, s), Point::new(abs, e))
            }
            SelMode::Line => {
                let cols = self.panes.get(&id).map(|p| p.grid.cols).unwrap_or(0);
                (Point::new(abs, 0), Point::new(abs, cols))
            }
        }
    }

    /// Screen position of the pointer inside a pane, as `(pane, col, row)`.
    /// Rows are viewport rows: what the program drew is what the user clicked.
    fn report_cell(&self, px: f32, py: f32) -> Option<(PaneId, usize, usize)> {
        let (cw, ch) = self.cell_size();
        let (id, rect) = self.layout().into_iter().find(|(_, r)| r.contains(px, py))?;
        let p = self.panes.get(&id)?;
        let cols = ((rect.w / cw).floor().max(1.0) as usize).min(p.grid.cols);
        let rows = ((rect.h / ch).floor().max(1.0) as usize).min(p.grid.rows);
        let x = (((px - rect.x) / cw).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let y = (((py - rect.y) / ch).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        Some((id, x, y))
    }

    /// Screen position within a specific pane, clamped to it — the reporting
    /// counterpart of `drag_cell`.
    fn pane_local_cell(&self, px: f32, py: f32, id: PaneId) -> Option<(usize, usize)> {
        let (cw, ch) = self.cell_size();
        let p = self.panes.get(&id)?;
        let rect = p.rect;
        let cols = ((rect.w / cw).floor().max(1.0) as usize).min(p.grid.cols);
        let rows = ((rect.h / ch).floor().max(1.0) as usize).min(p.grid.rows);
        let x = (((px - rect.x) / cw).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let y = (((py - rect.y) / ch).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        Some((x, y))
    }

    fn mouse_mods(&self) -> MouseMods {
        MouseMods {
            shift: self.mods.shift_key(),
            alt: self.mods.alt_key(),
            ctrl: self.mods.control_key(),
        }
    }

    /// Hands a mouse event to the program if it asked for one.
    ///
    /// Shift always keeps the event local. That is the convention every
    /// terminal follows, and it is the only way to select text or reach our own
    /// scrollback once a program has taken the mouse.
    fn report_mouse(&mut self, px: f32, py: f32, event: MouseEv) -> bool {
        self.report_mouse_to(None, px, py, event)
    }

    /// As `report_mouse`, but pinned to `owner` when a drag is already in
    /// flight: the program that saw the press must see the release, even if the
    /// pointer has since left its pane or the window entirely.
    fn report_mouse_to(&mut self, owner: Option<PaneId>, px: f32, py: f32, event: MouseEv) -> bool {
        if self.mods.shift_key() {
            return false;
        }
        let resolved = match owner {
            Some(id) => self.drag_cell(px, py, id).map(|(_, _)| id).and_then(|id| {
                let (col, row) = self.pane_local_cell(px, py, id)?;
                Some((id, col, row))
            }),
            None => self.report_cell(px, py),
        };
        let Some((id, col, row)) = resolved else { return false };
        let mods = self.mouse_mods();
        let Some(p) = self.panes.get_mut(&id) else { return false };
        let Some(bytes) =
            mouse::encode(p.grid.mouse_tracking, p.grid.mouse_encoding, event, col, row, mods)
        else {
            return false;
        };
        p.pty.write(&bytes);
        true
    }

    fn begin_selection(&mut self, px: f32, py: f32) {
        let Some((id, abs, col)) = self.cell_at(px, py) else {
            self.selection = None;
            return;
        };
        // Repeat clicks in the same cell cycle char -> word -> line.
        let now = Instant::now();
        let repeat = self.last_click.is_some_and(|(t, lid, lrow, lcol)| {
            now.duration_since(t) < Duration::from_millis(400)
                && (lid, lrow, lcol) == (id, abs, col)
        });
        self.click_streak = if repeat { self.click_streak + 1 } else { 1 };
        self.last_click = Some((now, id, abs, col));

        let mode = match self.click_streak % 3 {
            1 => SelMode::Char,
            2 => SelMode::Word,
            _ => SelMode::Line,
        };
        let span = self.span_for(id, abs, col, mode);
        self.selection = Some((id, Selection::new(span, mode)));
        self.dragging = true;
    }

    fn extend_selection(&mut self, px: f32, py: f32) {
        let Some((id, sel)) = self.selection else { return };
        let Some((abs, col)) = self.drag_cell(px, py, id) else { return };
        let span = self.span_for(id, abs, col, sel.mode);
        if let Some((_, s)) = self.selection.as_mut() {
            s.extend_to(span);
        }
    }

    fn selected_text(&self) -> Option<String> {
        let (id, sel) = self.selection?;
        if sel.is_empty() {
            return None;
        }
        let cols = self.panes.get(&id)?.grid.cols;
        let text = selection::extract(sel.range(), cols, |abs| self.row_cells(id, abs));
        (!text.is_empty()).then_some(text)
    }

    fn paste_clipboard(&mut self) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    log::error!("no clipboard available: {e}");
                    return;
                }
            }
        }
        let text = match self.clipboard.as_mut().map(|c| c.get_text()) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                log::error!("could not read the clipboard: {e}");
                return;
            }
            None => return,
        };
        if text.is_empty() {
            return;
        }
        let Some(p) = self.panes.get_mut(&self.focus) else { return };
        let bytes = paste_bytes(&text, p.grid.bracketed_paste);
        // Pasting is input, so it jumps back to the live screen like typing.
        p.grid.view_offset = 0;
        p.pty.write(&bytes);
        self.request_redraw();
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selected_text() else { return };
        self.set_clipboard(text);
    }

    /// Puts text on the system clipboard, opening it on first use. Shared by the
    /// copy key and by OSC 52, which is a program asking for the same thing.
    fn set_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    log::error!("no clipboard available: {e}");
                    return;
                }
            }
        }
        if let Some(c) = self.clipboard.as_mut() {
            if let Err(e) = c.set_text(text) {
                log::error!("could not write to the clipboard: {e}");
            }
        }
    }

    fn pane_at(&self, px: f32, py: f32) -> Option<PaneId> {
        self.layout().into_iter().find(|(_, r)| r.contains(px, py)).map(|(id, _)| id)
    }

    /// Drains every pty, updating grids. Returns true if anything changed.
    fn pump_all(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let mut dirty = false;
        let mut finished: Vec<PaneId> = Vec::new();
        // Collected rather than written as we go, because the clipboard is the
        // app's and we are holding every pane borrowed. Last writer wins, which
        // is what the clipboard means anyway.
        let mut clipboard: Option<String> = None;

        for (&id, p) in self.panes.iter_mut() {
            let (changed, reply) = p.pump();
            dirty |= changed;
            if !reply.is_empty() {
                p.pty.write(&reply);
            }
            if !p.exited && p.pty.is_dead() {
                // Give the final chunk of output a chance to land before the
                // pane goes away.
                let (changed2, _) = p.pump();
                dirty |= changed2;
                p.exited = true;
                finished.push(id);
            }
            // After the final pump, so an OSC 52 in a program's last breath —
            // copy and quit is one keystroke in tmux — still lands.
            if let Some(text) = p.grid.pending_clipboard.take() {
                clipboard = Some(text);
            }
        }

        // Before closing panes: that path can exit the loop, and a copy made on
        // the way out is still a copy the user asked for.
        if let Some(text) = clipboard {
            self.set_clipboard(text);
        }

        for id in finished {
            dirty = true;
            if !self.close_pane(id) {
                event_loop.exit();
                return dirty;
            }
        }
        dirty
    }

    fn redraw(&mut self) {
        let (Some(gpu), Some(fonts)) = (self.gpu.as_mut(), self.fonts.as_mut()) else {
            return;
        };
        let (cw, ch) = (fonts.cell_w, fonts.cell_h);
        let ascent = fonts.ascent;
        let scale = self.scale;
        let theme = &self.theme;
        let focus = self.focus;

        let content = {
            let p = PADDING * scale;
            Rect {
                x: p,
                y: p,
                w: (gpu.config.width as f32 - 2.0 * p).max(1.0),
                h: (gpu.config.height as f32 - 2.0 * p).max(1.0),
            }
        };

        let mut rects = Vec::new();
        self.tree.layout(content, &mut rects);

        let inst = &mut self.instances;
        inst.clear();

        let multi = rects.len() > 1;
        for (id, rect) in &rects {
            let Some(pane) = self.panes.get(id) else { continue };
            let focused = *id == focus;
            let g = &pane.grid;

            let pane_bg = if focused { theme.bg } else { theme.bg_unfocused };
            let sel_here = self.selection.and_then(|(sid, s)| (sid == *id).then_some(s));
            inst.push(Inst::solid(rect.x, rect.y, rect.w, rect.h, to_linear(pane_bg, 1.0)));

            let rows = g.rows.min((rect.h / ch).floor().max(0.0) as usize);
            let max_cols = ((rect.w / cw).floor().max(0.0) as usize).min(g.cols);

            for y in 0..rows {
                let Some(row) = g.view_row(y) else { continue };
                let py = rect.y + y as f32 * ch;
                // Scrollback rows were captured at the width the grid had then,
                // so they can be narrower than the grid is now.
                let max_cols = max_cols.min(row.len());

                // Cell backgrounds first, so no glyph gets painted over.
                let abs = g.abs_row(y);
                for x in 0..max_cols {
                    let cell = &row[x];
                    if cell.flags & FLAG_WIDE_SPACER != 0 {
                        continue;
                    }
                    let (_, bg) = resolve_pair(cell, theme);
                    let selected = sel_here.is_some_and(|s| s.contains(abs, x, g.cols));
                    if selected {
                        let w = if is_wide(row, x) { cw * 2.0 } else { cw };
                        inst.push(Inst::solid(
                            rect.x + x as f32 * cw,
                            py,
                            w,
                            ch,
                            to_linear(theme.selection_bg, 1.0),
                        ));
                    } else if bg != pane_bg {
                        let w = if is_wide(row, x) { cw * 2.0 } else { cw };
                        inst.push(Inst::solid(
                            rect.x + x as f32 * cw,
                            py,
                            w,
                            ch,
                            to_linear(bg, 1.0),
                        ));
                    }
                }

                for x in 0..max_cols {
                    let cell = &row[x];
                    if cell.flags & FLAG_WIDE_SPACER != 0 {
                        continue;
                    }
                    let px = rect.x + x as f32 * cw;
                    let (fg, _) = resolve_pair(cell, theme);
                    if cell.c != ' ' {
                        let bold = cell.flags & FLAG_BOLD != 0;
                        if let Some(gi) = fonts.glyph(cell.c, bold) {
                            inst.push(Inst::glyph(
                                glyph_rect(&gi, px, py, ascent),
                                gi.uv,
                                to_linear(fg, 1.0),
                            ));
                        }
                    }
                    if cell.flags & FLAG_UNDERLINE != 0 {
                        inst.push(Inst::solid(
                            px,
                            py + ascent + 2.0,
                            cw,
                            scale.max(1.0),
                            to_linear(fg, 1.0),
                        ));
                    }
                }
            }

            // While the IME is composing, the caret belongs to the composition,
            // so the terminal's own cursor stands down.
            let composing = self.preedit.is_active() && self.preedit.owner == Some(*id);

            // Cursor, live screen only.
            let show_cursor = !composing
                && g.cursor_visible
                && g.view_offset == 0
                && g.cy < rows
                && g.cx < max_cols;
            if show_cursor {
                let px = rect.x + g.cx as f32 * cw;
                let py = rect.y + g.cy as f32 * ch;
                let thickness = (2.0 * scale).max(1.0);
                let col = to_linear(theme.cursor, if focused { 1.0 } else { 0.4 });
                let r = match g.cursor_shape {
                    CursorShape::Block => [px, py, cw, ch],
                    CursorShape::Bar => [px, py, thickness, ch],
                    CursorShape::Underline => [px, py + ch - thickness, cw, thickness],
                };
                inst.push(Inst::solid(r[0], r[1], r[2], r[3], col));

                // Repaint the glyph under a focused block cursor in the pane
                // background colour so it stays readable.
                if focused && g.cursor_shape == CursorShape::Block {
                    if let Some(cell) = g.view_row(g.cy).and_then(|r| r.get(g.cx)) {
                        if cell.c != ' ' {
                            let bold = cell.flags & FLAG_BOLD != 0;
                            if let Some(gi) = fonts.glyph(cell.c, bold) {
                                inst.push(Inst::glyph(
                                    glyph_rect(&gi, px, py, ascent),
                                    gi.uv,
                                    to_linear(theme.bg, 1.0),
                                ));
                            }
                        }
                    }
                }
            }

            // Composing text, drawn over the grid. It is not in the pty yet, so
            // it exists only here until the IME commits.
            if composing {
                let origin = (g.cx.min(max_cols.saturating_sub(1)), g.cy);
                let cells =
                    layout_preedit(&self.preedit.text, self.preedit.target, origin, max_cols, rows);
                let thin = scale.max(1.0);
                let thick = (2.0 * scale).max(2.0);

                for pc in &cells {
                    let px = rect.x + pc.col as f32 * cw;
                    let py = rect.y + pc.row as f32 * ch;
                    let span = cw * pc.width as f32;

                    inst.push(Inst::solid(px, py, span, ch, to_linear(theme.preedit_bg, 1.0)));
                    if let Some(gi) = fonts.glyph(pc.c, false) {
                        inst.push(Inst::glyph(
                            glyph_rect(&gi, px, py, ascent),
                            gi.uv,
                            to_linear(theme.fg, 1.0),
                        ));
                    }
                    // The active segment gets a heavier rule, the rest a thin
                    // one — the usual way IMEs show what is being converted.
                    let (h, col) = if pc.active {
                        (thick, theme.cursor)
                    } else {
                        (thin, theme.preedit_underline)
                    };
                    inst.push(Inst::solid(px, py + ch - h, span, h, to_linear(col, 1.0)));
                }

                let (col, row) = preedit_caret(&cells, origin, max_cols);
                if row < rows {
                    inst.push(Inst::solid(
                        rect.x + col as f32 * cw,
                        rect.y + row as f32 * ch,
                        thin,
                        ch,
                        to_linear(theme.cursor, 1.0),
                    ));
                }
            }

            // Accent bar above the focused pane, so it reads at a glance.
            if focused && multi {
                inst.push(Inst::solid(
                    rect.x,
                    rect.y - 2.0 * scale,
                    rect.w,
                    2.0 * scale,
                    to_linear(theme.divider_focus, 1.0),
                ));
            }
        }

        let mut dividers = Vec::new();
        self.tree.dividers(content, &mut dividers);
        for d in dividers {
            inst.push(Inst::solid(d.x, d.y, d.w, d.h, to_linear(theme.divider, 1.0)));
        }

        gpu.upload_atlas(&mut fonts.atlas);
        let clear = to_linear(theme.bg, 1.0);
        if gpu.render(inst, clear) == FrameStatus::NeedsReconfigure {
            let (w, h) = (gpu.config.width, gpu.config.height);
            gpu.resize(w, h);
            if let Some(win) = &self.window {
                win.request_redraw();
            }
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn update_title(&self) {
        let Some(w) = &self.window else { return };
        let t = self
            .panes
            .get(&self.focus)
            .map(|p| p.grid.title.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("koma");
        w.set_title(t);
    }
}

/// Quad for a glyph drawn with its pen at `(px, py)`, the cell's top-left.
/// Rounded to whole pixels so stems stay crisp at the atlas's native scale.
fn glyph_rect(gi: &font::GlyphInfo, px: f32, py: f32, ascent: f32) -> [f32; 4] {
    [(px + gi.left).round(), (py + ascent - gi.top).round(), gi.w, gi.h]
}

/// Removes every occurrence of `marker`, including any that removal itself
/// splices together.
///
/// A single pass is not enough: `ESC[20` + `ESC[201~` + `1~` becomes `ESC[201~`
/// once the inner marker is taken out. Nothing reaches this with an ESC intact
/// today, but the guarantee belongs with the marker rather than depending on a
/// filter three branches away — which is the whole reason this is a function
/// with its own tests.
fn strip_end_markers(mut s: String, marker: &str) -> String {
    while s.contains(marker) {
        s = s.replace(marker, "");
    }
    s
}

/// Encodes clipboard text for the pty.
///
/// With bracketed paste on (DECSET 2004) the text is wrapped in markers, so the
/// program can tell a paste from typing and treat it as literal input — that is
/// what stops a multi-line paste from running each line as it arrives.
///
/// Without it there is no such protection, and the newlines *are* Enter. The
/// least surprising thing left is to send what was on the clipboard, which is
/// what every other terminal does.
///
/// Either way the text is sanitised first:
///
/// - CR and CRLF become LF, so a clipboard from Windows or a browser doesn't
///   submit twice per line.
/// - The end marker is stripped from the payload. Pasting text that contains
///   it would otherwise end bracketed mode early and let the remainder run as
///   commands — the standard injection against this feature.
/// - C0 and C1 controls other than tab and newline are dropped, so a stray ESC
///   — or a bare CSI at U+009B — can't start an escape sequence.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    const START: &str = "\x1b[200~";
    const END: &str = "\x1b[201~";

    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // Swallow the LF of a CRLF pair rather than emitting two.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                clean.push('\n');
            }
            '\n' | '\t' => clean.push(c),
            // C0, DEL, and C1 — the last of which includes a standalone CSI at
            // U+009B, so dropping ESC alone would not be consistent.
            c if (c as u32) < 0x20 || c as u32 == 0x7f || (0x80..=0x9f).contains(&(c as u32)) => {}
            c => clean.push(c),
        }
    }
    let clean = strip_end_markers(clean, END);

    if bracketed { format!("{START}{clean}{END}").into_bytes() } else { clean.into_bytes() }
}

/// The buttons a program can be told about. Anything else stays local.
fn mouse_button(b: MouseButton) -> Option<MouseBtn> {
    match b {
        MouseButton::Left => Some(MouseBtn::Left),
        MouseButton::Middle => Some(MouseBtn::Middle),
        MouseButton::Right => Some(MouseBtn::Right),
        _ => None,
    }
}

/// One gesture must not be able to inject an unbounded burst of wheel reports.
const WHEEL_REPORT_CAP: usize = 32;

/// Vertical lines to scroll, from a wheel event's two axes.
///
/// macOS swaps the scroll axes while Shift is held, and winit passes NSEvent's
/// deltas through untouched — so the very gesture meant to bypass the
/// application arrives on x. Reading only y made Shift+wheel do nothing at all.
/// There is no horizontal scrolling here, so under Shift take whichever axis
/// actually moved.
///
/// Not gated on the platform, though only macOS swaps: keeping it a plain
/// function of its arguments is what makes it testable, and the cost elsewhere
/// is that a Shift+horizontal swipe scrolls vertically instead of doing
/// nothing at all.
fn wheel_lines(across: f32, down: f32, shift: bool) -> f32 {
    if shift && across.abs() > down.abs() { across } else { down }
}

/// Where a wheel event should go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WheelAction {
    /// Move our own viewport through the scrollback.
    Viewport,
    /// Hand it to the program as arrow keys (xterm's alternate scroll).
    Application,
    /// Nowhere. Everything else would be worse than leaving it alone.
    Nothing,
}

/// Where a wheel tick goes when no mouse report was sent.
///
/// Written as the whole table rather than a condition at a time, because every
/// previous attempt at this got one cell wrong and left a wheel dead or a
/// program fed input it never asked for:
///
/// | Shift | screen | program holds mouse | 1007 | goes to     |
/// |-------|--------|---------------------|------|-------------|
/// | yes   | any    | any                 | any  | our view    |
/// | no    | main   | any                 | any  | our view    |
/// | no    | alt    | yes                 | any  | nowhere     |
/// | no    | alt    | no                  | on   | arrow keys  |
/// | no    | alt    | no                  | off  | nowhere     |
///
/// **Shift** is the escape hatch every terminal has, and the only way to reach
/// our scrollback while a program owns the pointer.
///
/// **The main screen** has no program's display to protect and no arrows worth
/// synthesising, so the gesture is ours whatever else is true.
///
/// **A program holding the mouse** gets nothing. Reaching here means no report
/// went out, and the reasons are all forms of "we could not say where the
/// pointer was" — over the window's padding, over the gap between panes, or
/// past what the legacy encoding can address. Arrow keys would be input it
/// never asked for: cursor movement inside a full-screen application, or, once
/// through tmux, command history at the shell underneath. Our own viewport is
/// no better, because moving it on the alternate screen covers the running
/// program with older history — see `scroll_pane`.
///
/// **Otherwise** it is plain alternate scroll, the case xterm invented it for,
/// down to a program that declines it with `1007l` getting nothing rather than
/// a viewport it did not ask us to move.
fn wheel_action(shift: bool, holds_mouse: bool, alt: bool, alternate_scroll: bool) -> WheelAction {
    if shift || !alt {
        return WheelAction::Viewport;
    }
    if holds_mouse {
        return WheelAction::Nothing;
    }
    if alternate_scroll { WheelAction::Application } else { WheelAction::Nothing }
}

/// Carries out what `wheel_action` decided, returning whatever the program
/// should be sent. Separate from `wheel_scroll` so it can be tested without a
/// window: doing nothing is a behaviour here, and sending the tick to the
/// viewport instead only looks like doing nothing.
fn apply_wheel(action: WheelAction, g: &mut grid::Grid, lines: isize) -> Vec<u8> {
    match action {
        WheelAction::Nothing => Vec::new(),
        WheelAction::Viewport => {
            g.scroll_view(lines);
            Vec::new()
        }
        WheelAction::Application => alternate_scroll_bytes(lines, g.app_cursor_keys),
    }
}

/// Arrow keys an application should see for `lines` of wheel scrolling on the
/// alternate screen. Positive scrolls back through the application's own view,
/// which means Up.
///
/// Capped, because one gesture must not be able to inject an unbounded burst of
/// keystrokes. A single wheel event never legitimately exceeds this.
fn alternate_scroll_bytes(lines: isize, app_cursor_keys: bool) -> Vec<u8> {
    const MAX_KEYS: usize = 32;
    let seq: &[u8] = match (lines > 0, app_cursor_keys) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    let count = lines.unsigned_abs().min(MAX_KEYS);
    let mut out = Vec::with_capacity(count * seq.len());
    for _ in 0..count {
        out.extend_from_slice(seq);
    }
    out
}

/// Resolves modifiers into "is a shortcut leader held, and is Shift an *extra*
/// modifier on top of it".
///
/// The leader is Cmd on macOS and Ctrl+Shift elsewhere, and both are accepted
/// on both platforms. That makes Shift ambiguous: under Ctrl+Shift it is part
/// of the leader, so treating it as a modifier would make every Ctrl+Shift
/// binding behave like its shifted variant.
///
/// Returns `None` when no leader is held.
fn leader_shift(m: ModifiersState) -> Option<bool> {
    let cmd = m.super_key();
    let ctrl_shift = m.control_key() && m.shift_key();
    if !cmd && !ctrl_shift {
        return None;
    }
    Some(cmd && m.shift_key())
}

/// Text the IME is still composing. It belongs to the terminal, not the shell:
/// nothing is written to the pty until the IME commits it.
#[derive(Default)]
struct Preedit {
    text: String,
    /// Byte range within `text` that the IME marks as the active segment
    /// (変換対象). `None` means the IME wants no cursor shown.
    target: Option<(usize, usize)>,
    /// Pane the composition belongs to. The OS keeps its marked text when
    /// focus moves between our panes — it has no idea they exist — so the
    /// composition has to remember where it started, or a later commit would
    /// land in whichever pane happens to be focused.
    ///
    /// Deliberately outlives an empty `text`: winit sends an empty preedit
    /// immediately before `Ime::Commit`, and clearing the owner there would
    /// orphan the commit. So a cancelled composition also leaves a stale owner
    /// behind — harmless, because only `commit` reads it and the next
    /// composition rebinds it first.
    owner: Option<PaneId>,
}

impl Preedit {
    fn is_active(&self) -> bool {
        !self.text.is_empty()
    }

    /// Applies an `Ime::Preedit` update. A composition that is *starting* binds
    /// to `focus`; one already in flight keeps its owner — which matters
    /// because winit sends an empty preedit immediately before `Ime::Commit`,
    /// and that must not orphan it.
    fn update(&mut self, text: String, target: Option<(usize, usize)>, focus: PaneId) {
        if !self.is_active() && !text.is_empty() {
            self.owner = Some(focus);
        }
        self.text = text;
        self.target = target;
    }

    /// Consumes the composition, returning the pane the text belongs to.
    fn commit(&mut self, fallback: PaneId) -> PaneId {
        let owner = self.owner.take().unwrap_or(fallback);
        self.text.clear();
        self.target = None;
        owner
    }

    fn clear(&mut self) {
        self.text.clear();
        self.target = None;
        self.owner = None;
    }
}

/// One composed character placed on the grid.
#[derive(Debug, PartialEq)]
struct PreeditCell {
    col: usize,
    row: usize,
    c: char,
    /// Columns occupied — 2 for full-width characters.
    width: usize,
    /// Inside the IME's active segment, which is drawn more prominently.
    active: bool,
}

/// Places composing text on the grid starting at `start`, wrapping within
/// `cols` and stopping at `rows`. Full-width characters take two columns and
/// wrap as a unit rather than being split across the edge.
fn layout_preedit(
    text: &str,
    target: Option<(usize, usize)>,
    start: (usize, usize),
    cols: usize,
    rows: usize,
) -> Vec<PreeditCell> {
    let mut out = Vec::new();
    if cols == 0 || rows == 0 {
        return out;
    }
    let (mut col, mut row) = start;
    col = col.min(cols.saturating_sub(1));

    for (byte, c) in text.char_indices() {
        if c == '\n' || c == '\r' {
            continue;
        }
        let width = UnicodeWidthChar::width(c).unwrap_or(1).max(1);
        if col + width > cols {
            col = 0;
            row += 1;
        }
        if row >= rows {
            break;
        }
        let active = target.is_some_and(|(t0, t1)| byte >= t0 && byte < t1);
        out.push(PreeditCell { col, row, c, width, active });
        col += width;
    }
    out
}

/// Where the caret sits after `cells`, for drawing and for telling the IME
/// where to put its candidate window.
fn preedit_caret(cells: &[PreeditCell], start: (usize, usize), cols: usize) -> (usize, usize) {
    match cells.last() {
        None => start,
        Some(last) => {
            let col = last.col + last.width;
            if col >= cols { (0, last.row + 1) } else { (col, last.row) }
        }
    }
}

/// Adds a sub-line scroll delta to `accum` and takes out whole lines, leaving
/// the remainder behind. A trackpad sends a few pixels per event — far less
/// than one row — so without carrying the remainder every scroll would round
/// to zero and nothing would move.
fn take_whole_lines(accum: &mut f32, delta: f32) -> isize {
    if !delta.is_finite() {
        return 0;
    }
    *accum += delta;
    let whole = accum.trunc();
    *accum -= whole;
    whole as isize
}

/// True when cell `x` is the left half of a double-width character.
fn is_wide(row: &[Cell], x: usize) -> bool {
    row.get(x + 1).is_some_and(|n| n.flags & FLAG_WIDE_SPACER != 0)
}

/// Applies bold-brightening and inverse video, returning `(fg, bg)` sRGB bytes.
fn resolve_pair(cell: &Cell, theme: &Theme) -> ([u8; 3], [u8; 3]) {
    let mut fg_c = cell.fg;
    // Bold over a basic palette colour picks the bright variant, like xterm.
    if cell.flags & FLAG_BOLD != 0 {
        if let Color::Indexed(i @ 0..=7) = fg_c {
            fg_c = Color::Indexed(i + 8);
        }
    }
    let mut fg = theme.resolve(fg_c, true);
    let mut bg = theme.resolve(cell.bg, false);
    if cell.flags & FLAG_INVERSE != 0 {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("koma")
            .with_inner_size(LogicalSize::new(960.0, 640.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                log::error!("could not create window: {e}");
                event_loop.exit();
                return;
            }
        };
        self.scale = window.scale_factor() as f32;
        // Without this the OS never routes composition to us and Japanese,
        // Chinese and Korean input simply cannot be typed. winit leaves IME
        // off by default.
        window.set_ime_allowed(true);

        let fonts = match FontSet::new(self.font_pt * self.scale) {
            Ok(f) => f,
            Err(e) => {
                log::error!("font setup failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let gpu = match Gpu::new(window.clone(), fonts.atlas.size) {
            Ok(g) => g,
            Err(e) => {
                log::error!("GPU setup failed: {e}");
                event_loop.exit();
                return;
            }
        };

        self.fonts = Some(fonts);
        self.gpu = Some(gpu);
        self.window = Some(window);

        let (cw, ch) = self.cell_size();
        let area = self.content_rect();
        let cols = ((area.w / cw).floor() as usize).max(1);
        let rows = ((area.h / ch).floor() as usize).max(1);
        match self.spawn_pane(cols, rows) {
            Ok(id) => {
                self.tree = Node::leaf(id);
                self.focus = id;
                self.relayout();
            }
            Err(e) => {
                log::error!("could not start shell: {e}");
                event_loop.exit();
            }
        }
        self.request_redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _ev: UserEvent) {
        if self.pump_all(event_loop) {
            self.update_title();
            self.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(g) = self.gpu.as_mut() {
                    g.resize(size.width, size.height);
                }
                self.relayout();
                self.request_redraw();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                if let (Some(f), Some(g)) = (self.fonts.as_mut(), self.gpu.as_mut()) {
                    f.set_size(self.font_pt * self.scale);
                    g.rebuild_bind_group();
                }
                self.relayout();
                self.request_redraw();
            }

            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),

            WindowEvent::Ime(ime) => self.handle_ime(ime),

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                if self.handle_shortcut(&event.logical_key, event_loop) {
                    self.request_redraw();
                    return;
                }
                let app_cursor =
                    self.panes.get(&self.focus).map(|p| p.grid.app_cursor_keys).unwrap_or(false);
                if let Some(bytes) = input::encode(&event, self.mods, app_cursor) {
                    if let Some(p) = self.panes.get_mut(&self.focus) {
                        // Typing jumps back to the live screen.
                        p.grid.view_offset = 0;
                        p.pty.write(&bytes);
                    }
                    self.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x as f32, position.y as f32);
                let (px, py) = self.cursor_pos;
                if self.dragging {
                    self.extend_selection(px, py);
                    self.request_redraw();
                    return;
                }
                // Check this before report_cell, which walks the pane tree and
                // allocates: tracking is off for every ordinary shell, and the
                // pointer moves constantly.
                let owner = self.reporting_drag;
                let listening = self
                    .panes
                    .get(&owner.unwrap_or(self.focus))
                    .is_some_and(|p| p.grid.mouse_tracking != mouse::Tracking::Off);
                if !listening {
                    return;
                }
                let held = owner.map(|_| MouseBtn::Left);
                self.report_mouse_to(owner, px, py, MouseEv::Motion(held));
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let (px, py) = self.cursor_pos;
                // Middle and right only mean anything to a program that asked
                // for the mouse; locally they have no binding yet.
                if button != MouseButton::Left {
                    let Some(b) = mouse_button(button) else { return };
                    let ev = match state {
                        ElementState::Pressed => MouseEv::Press(b),
                        ElementState::Released => MouseEv::Release(b),
                    };
                    self.report_mouse(px, py, ev);
                    return;
                }
                match state {
                    ElementState::Pressed => {
                        // Focus follows the click either way — the program owns
                        // the pointer, not which pane we type into.
                        if let Some(id) = self.pane_at(px, py) {
                            if id != self.focus {
                                self.set_focus(id);
                                self.update_title();
                            }
                        }
                        if self.report_mouse(px, py, MouseEv::Press(MouseBtn::Left)) {
                            self.reporting_drag = self.report_cell(px, py).map(|(id, _, _)| id);
                            // The program owns the pointer now; a stale
                            // highlight would just sit there.
                            self.selection = None;
                            self.request_redraw();
                            return;
                        }
                        self.begin_selection(px, py);
                        self.request_redraw();
                    }
                    ElementState::Released => {
                        if let Some(owner) = self.reporting_drag.take() {
                            self.report_mouse_to(
                                Some(owner),
                                px,
                                py,
                                MouseEv::Release(MouseBtn::Left),
                            );
                            return;
                        }
                        self.dragging = false;
                        // A click that never moved leaves nothing highlighted.
                        if self.selection.is_some_and(|(_, s)| s.is_empty()) {
                            self.selection = None;
                            self.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (_, ch) = self.cell_size();
                // Positive means "reveal content above", i.e. go back in history.
                let (across, down) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 3.0, y * 3.0),
                    // `ch`, not `cw`: under Shift the x axis is carrying
                    // vertical movement, so the vertical cell size converts it.
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / ch, p.y as f32 / ch),
                };
                let shift = self.mods.shift_key();
                let lines = wheel_lines(across, down, shift);
                // Scroll whatever is under the pointer, not just the focused pane.
                let target =
                    self.pane_at(self.cursor_pos.0, self.cursor_pos.1).unwrap_or(self.focus);
                // Leftover fractions belong to the pane and the axis they came
                // from; carrying them across either would apply the remainder
                // of one gesture to a different one.
                let across_axis = lines == across && across != down;
                if target != self.wheel_target || across_axis != self.wheel_across {
                    self.wheel_target = target;
                    self.wheel_across = across_axis;
                    self.scroll_accum = 0.0;
                }
                // A trackpad reports a few pixels per event — well under one
                // row — so the remainder has to carry over between events or
                // every scroll rounds away to nothing.
                let whole = take_whole_lines(&mut self.scroll_accum, lines);
                // Logged after the accumulator, so "the axis was read but
                // nothing moved" can be told from "the fraction hasn't added up
                // to a line yet" without another round trip to real hardware.
                let back = self.panes.get(&target).map(|p| p.grid.scrollback.len()).unwrap_or(0);
                log::debug!(
                    "wheel: across={across:.2} down={down:.2} shift={shift} \
                     -> {lines:.2} lines, whole={whole}, scrollback={back}"
                );
                if whole != 0 {
                    // A program that took the mouse gets the wheel too — that
                    // is how `set -g mouse on` reaches tmux's copy mode, which
                    // is the only way to scroll history we never see.
                    //
                    // `sent` is what actually prevents double-sending: once a
                    // report goes out we don't also synthesise arrows. DECSET
                    // 1007 is a second, weaker gate — whether a program bothers
                    // to send it depends on its terminfo.
                    let up = whole > 0;
                    let mut sent = 0;
                    for _ in 0..whole.unsigned_abs().min(WHEEL_REPORT_CAP) {
                        if self.report_mouse(
                            self.cursor_pos.0,
                            self.cursor_pos.1,
                            MouseEv::Wheel { up },
                        ) {
                            sent += 1;
                        } else {
                            break;
                        }
                    }
                    if sent == 0 {
                        self.wheel_scroll(target, whole, shift);
                    }
                    self.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => self.redraw(),

            _ => {}
        }
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_trackpad_deltas_accumulate_into_a_scroll() {
        // A gentle two-finger swipe: ~4px per event against a ~30px row. Each
        // one alone is a fraction of a line, so they must add up.
        let mut accum = 0.0;
        let per_event = 4.0 / 30.0;
        let mut moved = 0;
        for _ in 0..8 {
            moved += take_whole_lines(&mut accum, per_event);
        }
        assert!(moved > 0, "eight small deltas scrolled {moved} lines");
    }

    #[test]
    fn a_single_small_delta_scrolls_nothing_yet() {
        let mut accum = 0.0;
        assert_eq!(take_whole_lines(&mut accum, 0.3), 0);
        assert!(accum > 0.0, "the remainder must be kept, not discarded");
    }

    #[test]
    fn no_scroll_is_lost_to_rounding() {
        // Whatever comes in must eventually come out; nothing evaporates.
        let mut accum = 0.0;
        let total: f32 = 100.0;
        let mut moved = 0;
        for _ in 0..1000 {
            moved += take_whole_lines(&mut accum, total / 1000.0);
        }
        assert!((moved as f32 - total).abs() <= 1.0, "moved {moved} of {total}");
    }

    #[test]
    fn direction_is_preserved_across_the_fraction_boundary() {
        let mut accum = 0.0;
        assert_eq!(take_whole_lines(&mut accum, -0.6), 0);
        assert_eq!(take_whole_lines(&mut accum, -0.6), -1);
    }

    #[test]
    fn reversing_direction_cancels_the_pending_remainder() {
        let mut accum = 0.0;
        take_whole_lines(&mut accum, 0.7);
        assert_eq!(take_whole_lines(&mut accum, -0.7), 0);
        assert!(accum.abs() < 0.01, "accum drifted to {accum}");
    }

    #[test]
    fn a_mouse_wheel_click_scrolls_immediately() {
        // LineDelta devices send whole lines; they must not be delayed.
        let mut accum = 0.0;
        assert_eq!(take_whole_lines(&mut accum, 3.0), 3);
    }

    #[test]
    fn non_finite_deltas_are_ignored() {
        let mut accum = 0.5;
        assert_eq!(take_whole_lines(&mut accum, f32::NAN), 0);
        assert_eq!(accum, 0.5, "a bad delta must not poison the accumulator");
    }

    const CMD: ModifiersState = ModifiersState::SUPER;
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;

    #[test]
    fn cmd_alone_is_a_leader_without_extra_shift() {
        assert_eq!(leader_shift(CMD), Some(false));
    }

    #[test]
    fn cmd_plus_shift_reports_shift_as_extra() {
        assert_eq!(leader_shift(CMD | SHIFT), Some(true));
    }

    #[test]
    fn ctrl_shift_is_a_leader_and_its_shift_is_not_extra() {
        // Regression: reported in review. Counting the leader's own Shift as a
        // modifier made Ctrl+Shift+Up scroll instead of resize on Linux, and
        // made every split Vertical because the "shifted" branch always won.
        assert_eq!(leader_shift(CTRL | SHIFT), Some(false));
        assert_eq!(leader_shift(CTRL | SHIFT | ALT), Some(false));
    }

    #[test]
    fn partial_modifiers_are_not_leaders() {
        assert_eq!(leader_shift(ModifiersState::empty()), None);
        assert_eq!(leader_shift(SHIFT), None, "Shift alone belongs to the shell");
        assert_eq!(leader_shift(CTRL), None, "bare Ctrl must reach the shell as ^C etc.");
        assert_eq!(leader_shift(ALT), None);
    }

    #[test]
    fn cmd_wins_when_both_leaders_are_held() {
        // Cmd+Shift+Ctrl should still count Shift as extra, since Cmd leads.
        assert_eq!(leader_shift(CMD | CTRL | SHIFT), Some(true));
    }

    #[test]
    fn alternate_scroll_sends_up_for_history_and_down_for_forward() {
        assert_eq!(alternate_scroll_bytes(1, false), b"\x1b[A".to_vec());
        assert_eq!(alternate_scroll_bytes(-1, false), b"\x1b[B".to_vec());
    }

    #[test]
    fn alternate_scroll_uses_application_cursor_keys_when_set() {
        assert_eq!(alternate_scroll_bytes(1, true), b"\x1bOA".to_vec());
        assert_eq!(alternate_scroll_bytes(-1, true), b"\x1bOB".to_vec());
    }

    #[test]
    fn alternate_scroll_repeats_once_per_line() {
        assert_eq!(alternate_scroll_bytes(3, false), b"\x1b[A\x1b[A\x1b[A".to_vec());
    }

    #[test]
    fn alternate_scroll_sends_nothing_for_no_movement() {
        assert!(alternate_scroll_bytes(0, false).is_empty());
    }

    #[test]
    fn alternate_scroll_is_capped() {
        // One gesture must not be able to inject an unbounded keystroke burst.
        let out = alternate_scroll_bytes(10_000, false);
        assert_eq!(out.len(), 32 * 3);
        let out = alternate_scroll_bytes(isize::MIN, false);
        assert_eq!(out.len(), 32 * 3, "the extreme negative must not overflow");
    }

    /// `(col, row, char)` triples, for readable assertions.
    fn placed(cells: &[PreeditCell]) -> Vec<(usize, usize, char)> {
        cells.iter().map(|c| (c.col, c.row, c.c)).collect()
    }

    #[test]
    fn composing_text_starts_at_the_caret() {
        let cells = layout_preedit("abc", None, (5, 2), 80, 24);
        assert_eq!(placed(&cells), vec![(5, 2, 'a'), (6, 2, 'b'), (7, 2, 'c')]);
    }

    #[test]
    fn kana_takes_two_columns_each() {
        let cells = layout_preedit("にほん", None, (0, 0), 80, 24);
        assert_eq!(placed(&cells), vec![(0, 0, 'に'), (2, 0, 'ほ'), (4, 0, 'ん')]);
        assert!(cells.iter().all(|c| c.width == 2));
    }

    #[test]
    fn composing_text_wraps_at_the_pane_edge() {
        let cells = layout_preedit("abcd", None, (2, 0), 4, 24);
        assert_eq!(placed(&cells), vec![(2, 0, 'a'), (3, 0, 'b'), (0, 1, 'c'), (1, 1, 'd')]);
    }

    #[test]
    fn a_wide_char_wraps_whole_rather_than_splitting() {
        // One column left, but 'あ' needs two — it must move to the next row
        // intact instead of straddling the edge.
        let cells = layout_preedit("あ", None, (3, 0), 4, 24);
        assert_eq!(placed(&cells), vec![(0, 1, 'あ')]);
    }

    #[test]
    fn composing_text_stops_at_the_bottom_of_the_pane() {
        // Two rows only: everything past them is dropped, not drawn off-pane.
        let cells = layout_preedit("abcdef", None, (0, 0), 2, 2);
        assert!(cells.iter().all(|c| c.row < 2), "{:?}", placed(&cells));
        assert_eq!(cells.len(), 4);
    }

    #[test]
    fn the_active_segment_is_marked_by_byte_range() {
        // "にほんご": each kana is 3 bytes. Mark the second one.
        let text = "にほんご";
        let cells = layout_preedit(text, Some((3, 6)), (0, 0), 80, 24);
        let active: Vec<char> = cells.iter().filter(|c| c.active).map(|c| c.c).collect();
        assert_eq!(active, vec!['ほ']);
    }

    #[test]
    fn no_segment_is_active_without_a_range() {
        let cells = layout_preedit("にほん", None, (0, 0), 80, 24);
        assert!(cells.iter().all(|c| !c.active));
    }

    #[test]
    fn an_empty_target_range_marks_nothing() {
        // IMEs send a zero-width range to mean "caret here", not "select this".
        let cells = layout_preedit("abc", Some((1, 1)), (0, 0), 80, 24);
        assert!(cells.iter().all(|c| !c.active));
    }

    #[test]
    fn the_caret_follows_the_composed_text() {
        let cells = layout_preedit("にほん", None, (1, 0), 80, 24);
        assert_eq!(preedit_caret(&cells, (1, 0), 80), (7, 0), "1 + three wide chars");
    }

    #[test]
    fn the_caret_wraps_when_composing_fills_the_row() {
        let cells = layout_preedit("abcd", None, (0, 0), 4, 24);
        assert_eq!(preedit_caret(&cells, (0, 0), 4), (0, 1));
    }

    #[test]
    fn an_empty_composition_leaves_the_caret_alone() {
        let cells = layout_preedit("", None, (7, 3), 80, 24);
        assert!(cells.is_empty());
        assert_eq!(preedit_caret(&cells, (7, 3), 80), (7, 3));
    }

    #[test]
    fn newlines_in_composing_text_are_ignored() {
        // Composition is a single run; a stray newline must not break layout.
        let cells = layout_preedit("a\nb", None, (0, 0), 80, 24);
        assert_eq!(placed(&cells), vec![(0, 0, 'a'), (1, 0, 'b')]);
    }

    #[test]
    fn a_degenerate_pane_produces_no_cells() {
        assert!(layout_preedit("abc", None, (0, 0), 0, 24).is_empty());
        assert!(layout_preedit("abc", None, (0, 0), 80, 0).is_empty());
    }

    #[test]
    fn preedit_tracks_active_state() {
        let mut p = Preedit::default();
        assert!(!p.is_active());
        p.text = "に".into();
        p.target = Some((0, 3));
        assert!(p.is_active());
        p.clear();
        assert!(!p.is_active() && p.target.is_none());
    }

    #[test]
    fn a_composition_binds_to_the_pane_it_started_in() {
        let mut p = Preedit::default();
        p.update("に".into(), None, 7);
        assert_eq!(p.owner, Some(7));
    }

    #[test]
    fn a_composition_in_flight_keeps_its_owner_when_focus_moves() {
        let mut p = Preedit::default();
        p.update("に".into(), None, 7);
        // Focus moved to pane 9 mid-composition; the text still belongs to 7.
        p.update("にほ".into(), None, 9);
        assert_eq!(p.owner, Some(7));
    }

    #[test]
    fn the_empty_preedit_before_a_commit_does_not_orphan_it() {
        // winit sends Preedit("") immediately before Commit. If that cleared
        // the owner, the commit would fall back to whatever is focused now —
        // exactly the misdelivery this is meant to prevent.
        let mut p = Preedit::default();
        p.update("にほん".into(), None, 7);
        p.update(String::new(), None, 9);
        assert_eq!(p.owner, Some(7));
        assert_eq!(p.commit(9), 7, "commit must go to the composing pane");
    }

    #[test]
    fn commit_consumes_the_composition() {
        let mut p = Preedit::default();
        p.update("に".into(), Some((0, 3)), 2);
        assert_eq!(p.commit(2), 2);
        assert!(!p.is_active());
        assert_eq!(p.owner, None);
        assert_eq!(p.target, None);
    }

    #[test]
    fn a_fresh_composition_after_a_commit_binds_to_the_new_focus() {
        let mut p = Preedit::default();
        p.update("に".into(), None, 7);
        p.commit(7);
        p.update("あ".into(), None, 9);
        assert_eq!(p.owner, Some(9));
    }

    #[test]
    fn a_composition_started_after_a_cancel_rebinds() {
        // Escape clears the text without a commit; the stale owner must not
        // capture the next composition.
        let mut p = Preedit::default();
        p.update("に".into(), None, 7);
        p.update(String::new(), None, 7);
        p.update("あ".into(), None, 9);
        assert_eq!(p.owner, Some(9));
    }

    #[test]
    fn commit_falls_back_when_nothing_was_composing() {
        let mut p = Preedit::default();
        assert_eq!(p.commit(4), 4);
    }

    #[test]
    fn clear_drops_the_owner_too() {
        let mut p = Preedit::default();
        p.update("に".into(), Some((0, 3)), 3);
        p.clear();
        assert!(!p.is_active());
        assert_eq!(p.owner, None);
    }

    #[test]
    fn the_caret_can_land_one_row_past_a_full_pane() {
        // Why update_ime_area clamps the anchor row: the cells are clipped to
        // the pane, but the caret that follows them is not, so a composition
        // that fills the pane puts it on a row that does not exist.
        let cells = layout_preedit("abcd", None, (0, 0), 2, 2);
        assert!(cells.iter().all(|c| c.row < 2), "cells are clipped");
        let (_, row) = preedit_caret(&cells, (0, 0), 2);
        assert_eq!(row, 2, "one past the last row");
    }

    #[test]
    fn clipping_the_layout_bounds_the_anchor() {
        // Passing the pane's real row count (rather than usize::MAX) is what
        // keeps the anchor within one row of the pane for any length of text.
        let rows = 3;
        let cells = layout_preedit(&"あ".repeat(200), None, (0, 0), 4, rows);
        assert!(cells.iter().all(|c| c.row < rows));
        let (_, row) = preedit_caret(&cells, (0, 0), 4);
        assert!(row <= rows, "anchor row {row} should be at most {rows}");
    }

    /// Every combination of the four inputs, spelled out.
    ///
    /// Four attempts at this routing were reverted, each because a cell nobody
    /// had written down turned out to be wrong. Enumerating them costs sixteen
    /// lines and makes "which case did we not think about" a question the test
    /// answers rather than the next review.
    #[test]
    fn the_whole_wheel_table() {
        use WheelAction::{Application, Nothing, Viewport};
        // shift, holds_mouse, alt, alternate_scroll  ->  where the tick goes
        let table = [
            // Shift is the escape hatch and wins over everything.
            (true, false, false, false, Viewport),
            (true, false, false, true, Viewport),
            (true, false, true, false, Viewport),
            (true, false, true, true, Viewport),
            (true, true, false, false, Viewport),
            (true, true, false, true, Viewport),
            (true, true, true, false, Viewport),
            (true, true, true, true, Viewport),
            // The main screen is ours: no program's display to cover, and no
            // arrows worth synthesising.
            (false, false, false, false, Viewport),
            (false, false, false, true, Viewport),
            (false, true, false, false, Viewport),
            (false, true, false, true, Viewport),
            // A program holding the mouse gets nothing: we could not say where
            // the pointer was, arrows are input it never asked for, and moving
            // our viewport would cover it.
            (false, true, true, false, Nothing),
            (false, true, true, true, Nothing),
            // Plain alternate scroll, and a program that declines it.
            (false, false, true, true, Application),
            (false, false, true, false, Nothing),
        ];
        for (shift, holds, alt, scroll, want) in table {
            assert_eq!(
                wheel_action(shift, holds, alt, scroll),
                want,
                "shift={shift} holds_mouse={holds} alt={alt} 1007={scroll}"
            );
        }
    }

    #[test]
    fn a_program_holding_the_mouse_is_never_sent_arrows() {
        // The reported bug, named. With `set -g mouse on`, the pointer in the
        // 2px gap between panes or the window's padding names no cell, so no
        // report goes out. Arrow keys then went through tmux to the shell's
        // line editor and read as command history.
        assert_eq!(wheel_action(false, true, true, true), WheelAction::Nothing);
    }

    #[test]
    fn a_pager_keeps_its_wheel_everywhere_in_the_window() {
        // less and man say nothing about the mouse. A tick over the padding
        // cannot be reported either, but arrows carry no position, so there is
        // nothing to be wrong about and they still scroll.
        assert_eq!(wheel_action(false, false, true, true), WheelAction::Application);
    }

    #[test]
    fn the_wheel_never_moves_the_viewport_under_a_program_unshifted() {
        // Moving it there covers the program with older history that neither
        // its redraws nor ours take back. Shift is the deliberate way in, and
        // must work in both directions so what it does it can undo.
        for holds in [false, true] {
            for scroll in [false, true] {
                assert_ne!(
                    wheel_action(false, holds, true, scroll),
                    WheelAction::Viewport,
                    "holds_mouse={holds} 1007={scroll}"
                );
                assert_eq!(wheel_action(true, holds, true, scroll), WheelAction::Viewport);
            }
        }
    }

    const END: &str = "\x1b[201~";

    fn pasted(text: &str, bracketed: bool) -> String {
        String::from_utf8(paste_bytes(text, bracketed)).unwrap()
    }

    #[test]
    fn bracketed_paste_wraps_the_text_in_markers() {
        assert_eq!(pasted("ls -la", true), "\x1b[200~ls -la\x1b[201~");
    }

    #[test]
    fn without_bracketed_paste_the_text_goes_as_is() {
        assert_eq!(pasted("ls -la", false), "ls -la");
    }

    #[test]
    fn multi_line_text_keeps_its_newlines() {
        // Bracketed mode is what stops these running; the bytes themselves are
        // unchanged either way.
        assert_eq!(pasted("one\ntwo", true), "\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(pasted("one\ntwo", false), "one\ntwo");
    }

    #[test]
    fn crlf_becomes_one_newline() {
        // A clipboard from a browser or from Windows would otherwise submit
        // twice for every line.
        assert_eq!(pasted("a\r\nb\r\nc", false), "a\nb\nc");
    }

    #[test]
    fn a_lone_carriage_return_also_becomes_a_newline() {
        assert_eq!(pasted("a\rb", false), "a\nb");
    }

    #[test]
    fn an_embedded_end_marker_cannot_escape_the_brackets() {
        // The standard injection: text containing the end marker would close
        // bracketed mode early and let the rest arrive as live keystrokes.
        let hostile = "safe\x1b[201~rm -rf /\n";
        let out = pasted(hostile, true);
        assert_eq!(out.matches("\x1b[201~").count(), 1, "only the real terminator: {out:?}");
        assert!(out.ends_with("\x1b[201~"));
        assert!(!out.contains("\x1b[201~rm"), "the payload still breaks out: {out:?}");
    }

    #[test]
    fn escape_sequences_in_the_clipboard_are_defused() {
        // Without this a paste could set the title, switch screens, or worse.
        let out = pasted("before\x1b]0;pwned\x07after", false);
        assert!(!out.contains('\x1b'), "ESC survived: {out:?}");
        assert!(!out.contains('\x07'), "BEL survived: {out:?}");
        assert_eq!(out, "before]0;pwnedafter");
    }

    #[test]
    fn tabs_survive_but_other_controls_do_not() {
        // Tab is real input — pasting indented code must keep it.
        assert_eq!(pasted("a\tb", false), "a\tb");
        assert_eq!(pasted("a\x00\x08\x7fb", false), "ab");
    }

    #[test]
    fn multibyte_text_is_untouched() {
        assert_eq!(pasted("日本語のテキスト", true), "\x1b[200~日本語のテキスト\x1b[201~");
    }

    #[test]
    fn empty_text_still_produces_well_formed_markers() {
        assert_eq!(pasted("", true), "\x1b[200~\x1b[201~");
        assert_eq!(pasted("", false), "");
    }

    #[test]
    fn removing_the_end_marker_cannot_splice_a_new_one() {
        // Removing the inner marker joins "ESC[20" to "1~" and makes another,
        // so one pass leaves the payload able to close bracketed mode early.
        let hostile = "\x1b[20\x1b[201~1~rm -rf /".to_string();
        assert!(
            hostile.replace(END, "").contains(END),
            "the premise: a single pass must be insufficient here"
        );
        assert!(!strip_end_markers(hostile, END).contains(END));
    }

    #[test]
    fn stripping_markers_leaves_ordinary_text_alone() {
        assert_eq!(strip_end_markers("ls -la".into(), END), "ls -la");
        assert_eq!(strip_end_markers(String::new(), END), "");
    }

    #[test]
    fn an_end_marker_never_survives_a_paste() {
        // The property, through the real encoder: whatever the clipboard held,
        // exactly one terminator comes out and it is ours.
        for hostile in
            ["safe\x1b[201~rm -rf /\n", "\x1b[20\x1b[201~1~rm -rf /", "\x1b[201~\x1b[201~\x1b[201~"]
        {
            let out = pasted(hostile, true);
            assert_eq!(out.matches(END).count(), 1, "{hostile:?} produced {out:?}");
            assert!(out.ends_with(END));
        }
    }

    #[test]
    fn c1_controls_are_dropped_too() {
        // U+009B is CSI on its own; dropping ESC but not this would be an odd
        // place to stop.
        let out = pasted("a\u{9b}0;pwned\u{7}b", false);
        assert_eq!(out, "a0;pwnedb");
    }

    #[test]
    fn a_plain_wheel_uses_the_vertical_axis() {
        assert_eq!(wheel_lines(0.0, 3.0, false), 3.0);
        assert_eq!(wheel_lines(0.0, -3.0, false), -3.0);
    }

    #[test]
    fn a_horizontal_swipe_alone_does_not_scroll() {
        // Without Shift the x axis is genuinely horizontal and not ours.
        assert_eq!(wheel_lines(5.0, 0.0, false), 0.0);
    }

    #[test]
    fn shift_reads_the_axis_macos_swapped_it_onto() {
        // Reported from real use: Shift+wheel did nothing, because macOS moves
        // the gesture to x and only y was being read.
        assert_eq!(wheel_lines(4.0, 0.0, true), 4.0);
        assert_eq!(wheel_lines(-4.0, 0.0, true), -4.0);
    }

    #[test]
    fn shift_still_works_where_the_axes_are_not_swapped() {
        // Linux and Windows deliver Shift+wheel on y as usual.
        assert_eq!(wheel_lines(0.0, 4.0, true), 4.0);
    }

    #[test]
    fn the_larger_axis_wins_when_a_gesture_is_diagonal() {
        // A trackpad rarely reports a clean zero on the other axis.
        assert_eq!(wheel_lines(6.0, 0.4, true), 6.0);
        assert_eq!(wheel_lines(0.4, 6.0, true), 6.0);
    }

    /// An alt-screen grid with history behind it: a program started at a prompt
    /// that had already scrolled.
    fn alt_grid_with_history() -> grid::Grid {
        let mut g = grid::Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        let mut run = |g: &mut grid::Grid, b: &[u8]| {
            let mut perf = grid::Performer { grid: g, reply: Vec::new() };
            parser.advance(&mut perf, b);
        };
        run(&mut g, b"one\r\ntwo\r\nthree\r\n$ ");
        run(&mut g, b"\x1b[?1049h");
        run(&mut g, b"FULL-SCREEN PROGRAM");
        assert!(g.alt_active && !g.scrollback.is_empty());
        g
    }

    /// What each decision actually does, which the table above cannot say.
    ///
    /// Two of the reverted attempts decided correctly and wired it wrongly:
    /// "nothing" was implemented as a fall-through to the viewport, which on
    /// the alternate screen is not nothing at all.
    #[test]
    fn each_wheel_action_does_what_it_says() {
        let mut g = alt_grid_with_history();
        assert!(apply_wheel(WheelAction::Nothing, &mut g, 1).is_empty());
        assert_eq!(g.view_offset, 0, "nothing must not move the viewport either");

        let mut g = alt_grid_with_history();
        assert!(apply_wheel(WheelAction::Viewport, &mut g, 1).is_empty());
        assert_eq!(g.view_offset, 1);
        apply_wheel(WheelAction::Viewport, &mut g, -1);
        assert_eq!(g.view_offset, 0, "and it has to come back the other way");

        let mut g = alt_grid_with_history();
        assert_eq!(apply_wheel(WheelAction::Application, &mut g, 2), b"\x1b[A\x1b[A".to_vec());
        assert_eq!(g.view_offset, 0, "the program scrolls itself; our view stays put");
    }

    /// The reported bug end to end, one decision away from the pty.
    #[test]
    fn a_wheel_tick_over_the_gap_writes_nothing_into_tmux() {
        // tmux with `set -g mouse on`, and a pointer the reporting path could
        // not place. Nothing may reach the program and nothing may move.
        let mut g = alt_grid_with_history();
        let mut parser = vte::Parser::new();
        let mut perf = grid::Performer { grid: &mut g, reply: Vec::new() };
        parser.advance(&mut perf, b"\x1b[?1002h\x1b[?1006h");
        assert_ne!(g.mouse_tracking, mouse::Tracking::Off);

        let action = wheel_action(false, true, g.alt_active, g.alternate_scroll);
        assert_eq!(apply_wheel(action, &mut g, 3), Vec::<u8>::new());
        assert_eq!(g.view_offset, 0);
    }
}
