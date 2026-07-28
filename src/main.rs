//! koma — a tiling terminal emulator.
//!
//! Panes live in an n-ary split tree; each leaf owns a pty and a screen grid.
//! Everything on screen is drawn as instanced quads through one wgpu pipeline.

mod font;
mod gpu;
mod grid;
mod input;
mod pane;
mod pty;
mod theme;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use font::FontSet;
use gpu::{FrameStatus, Gpu, Inst};
use grid::{
    Cell, Color, CursorShape, FLAG_BOLD, FLAG_INVERSE, FLAG_UNDERLINE, FLAG_WIDE_SPACER, Grid,
};
use pane::{Axis, Node, PaneId, Rect};
use pty::Pty;
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
        // New output cancels scrollback, matching every other terminal.
        self.grid.view_offset = 0;
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
            }
        }
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
        if self.focus == id {
            let mut leaves = Vec::new();
            self.tree.leaves(&mut leaves);
            self.focus = leaves.first().copied().unwrap_or(0);
        }
        self.relayout();
        true
    }

    fn focus_direction(&mut self, dx: f32, dy: f32) {
        let rects = self.layout();
        if let Some(id) = pane::neighbor(&rects, self.focus, dx, dy) {
            self.focus = id;
        }
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
        // Cmd on macOS; Ctrl+Shift elsewhere. Both are accepted everywhere so
        // the same muscle memory works when running this on Linux too.
        let cmd = self.mods.super_key();
        let ctrl_shift = self.mods.control_key() && self.mods.shift_key();

        if !cmd && !ctrl_shift {
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
        }

        let alt = self.mods.alt_key();
        // With the leader held, Shift picks the other split direction.
        let shift = self.mods.shift_key();

        if let Key::Named(n) = key {
            // Scrollback. Mac keyboards have no PageUp, so Cmd+Shift+Arrow is
            // the binding that actually gets used there.
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
            "d" => {
                self.split_focused(if shift { Axis::Vertical } else { Axis::Horizontal });
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
        self.focus = leaves[(cur + step).rem_euclid(n) as usize];
    }

    /// Scrolls a pane's viewport. Positive `lines` moves back through history.
    fn scroll_pane(&mut self, id: PaneId, lines: isize) {
        let Some(p) = self.panes.get_mut(&id) else { return };
        if lines == 0 {
            return;
        }
        if p.grid.alt_active {
            // Full-screen apps (vim, less, man) keep no scrollback of ours, so
            // send arrow keys instead — xterm calls this alternate scroll.
            let seq: &[u8] = match (lines > 0, p.grid.app_cursor_keys) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut out = Vec::new();
            for _ in 0..lines.unsigned_abs().min(32) {
                out.extend_from_slice(seq);
            }
            p.pty.write(&out);
            return;
        }
        let max = p.grid.scrollback.len() as isize;
        let next = p.grid.view_offset as isize + lines;
        p.grid.view_offset = next.clamp(0, max) as usize;
    }

    fn scroll_focused(&mut self, lines: isize) {
        self.scroll_pane(self.focus, lines);
    }

    /// Jumps to the oldest retained line, or back to the live screen.
    fn scroll_to_edge(&mut self, top: bool) {
        let Some(p) = self.panes.get_mut(&self.focus) else { return };
        if p.grid.alt_active {
            return;
        }
        p.grid.view_offset = if top { p.grid.scrollback.len() } else { 0 };
    }

    fn page_lines(&self) -> isize {
        self.panes
            .get(&self.focus)
            .map(|p| p.grid.rows.saturating_sub(1))
            .unwrap_or(10)
            .max(1) as isize
    }

    fn pane_at(&self, px: f32, py: f32) -> Option<PaneId> {
        self.layout().into_iter().find(|(_, r)| r.contains(px, py)).map(|(id, _)| id)
    }

    /// Drains every pty, updating grids. Returns true if anything changed.
    fn pump_all(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let mut dirty = false;
        let mut finished: Vec<PaneId> = Vec::new();

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
                for x in 0..max_cols {
                    let cell = &row[x];
                    if cell.flags & FLAG_WIDE_SPACER != 0 {
                        continue;
                    }
                    let (_, bg) = resolve_pair(cell, theme);
                    if bg != pane_bg {
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
                                [(px + gi.left).round(), (py + ascent - gi.top).round(), gi.w, gi.h],
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

            // Cursor, live screen only.
            if g.cursor_visible && g.view_offset == 0 && g.cy < rows && g.cx < max_cols {
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
                                    [
                                        (px + gi.left).round(),
                                        (py + ascent - gi.top).round(),
                                        gi.w,
                                        gi.h,
                                    ],
                                    gi.uv,
                                    to_linear(theme.bg, 1.0),
                                ));
                            }
                        }
                    }
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

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                if self.handle_shortcut(&event.logical_key, event_loop) {
                    self.request_redraw();
                    return;
                }
                let app_cursor = self
                    .panes
                    .get(&self.focus)
                    .map(|p| p.grid.app_cursor_keys)
                    .unwrap_or(false);
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
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed && button == MouseButton::Left {
                    if let Some(id) = self.pane_at(self.cursor_pos.0, self.cursor_pos.1) {
                        if id != self.focus {
                            self.focus = id;
                            self.update_title();
                            self.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (_, ch) = self.cell_size();
                // Positive means "reveal content above", i.e. go back in history.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 3.0,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / ch,
                };
                // Scroll whatever is under the pointer, not just the focused pane.
                let target =
                    self.pane_at(self.cursor_pos.0, self.cursor_pos.1).unwrap_or(self.focus);
                if target != self.wheel_target {
                    self.wheel_target = target;
                    self.scroll_accum = 0.0;
                }
                // A trackpad reports a few pixels per event — well under one
                // row — so the remainder has to carry over between events or
                // every scroll rounds away to nothing.
                let whole = take_whole_lines(&mut self.scroll_accum, lines);
                if whole != 0 {
                    self.scroll_pane(target, whole);
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
}
