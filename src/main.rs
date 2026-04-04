mod completion;
mod config;
mod platform;
mod renderer;
mod terminal;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

use completion::Engine;
use config::{WINDOW_HEIGHT, WINDOW_WIDTH};
use renderer::{PaneView, Renderer};
use terminal::Terminal;

#[derive(Debug)]
enum AppEvent {
    PtyData { pane_id: u64, data: Vec<u8> },
    PtyExit { pane_id: u64 },
    NewTab,
}

// ── Split direction ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum SplitDir {
    Vertical,
    Horizontal,
}

// ── Selection ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
struct Selection {
    // grid_row = visual_row as i64 - viewport_offset as i64.
    // Negative = rows in scrollback above the live grid; 0 = top of live grid.
    // Stable across viewport changes: scrolling shifts visual_row and viewport_offset
    // by the same amount, so grid_row for a given cell never changes.
    start: (i64, usize), // (grid_row, col)
    end: (i64, usize),
}

impl Selection {
    /// Returns (r0, c0, r1, c1) with r0≤r1 (and c0≤c1 when r0==r1), in grid_row space.
    fn normalized(self) -> (i64, usize, i64, usize) {
        let (r0, c0) = self.start;
        let (r1, c1) = self.end;
        if r0 < r1 || (r0 == r1 && c0 <= c1) {
            (r0, c0, r1, c1)
        } else {
            (r1, c1, r0, c0)
        }
    }

    /// Convert to viewport-relative row range for rendering.
    /// `viewport_offset` is the current scroll position.
    /// Returns (r0, c0, r1, c1) in visual (viewport) row coordinates.
    fn to_viewport(self, viewport_offset: usize) -> (usize, usize, usize, usize) {
        let vo = viewport_offset as i64;
        let (gr0, c0, gr1, c1) = self.normalized();
        let r0 = (gr0 + vo).max(0) as usize;
        let r1 = (gr1 + vo).max(0) as usize;
        (r0, c0, r1, c1)
    }
}

// ── Pane ──────────────────────────────────────────────────────────────────────

struct Pane {
    id: u64,
    terminal: Terminal,
    pty_master: Box<dyn MasterPty>,
    pty_writer: Box<dyn Write + Send>,
    ghost_text: Option<String>,
    engine: Engine,
    selection: Option<Selection>,
}

impl Pane {
    fn write(&mut self, data: &[u8]) {
        let _ = self.pty_writer.write_all(data);
        let _ = self.pty_writer.flush();
    }

    fn title(&self) -> &str {
        let t = self.terminal.state.title.as_str();
        if t.is_empty() || t == "term" {
            let cwd = self.terminal.state.current_dir.as_str();
            let home = std::env::var("HOME").unwrap_or_default();
            if !cwd.is_empty() {
                return cwd
                    .strip_prefix(&home)
                    .map(|s| if s.is_empty() { "~" } else { s })
                    .unwrap_or(cwd);
            }
            "zsh"
        } else {
            t
        }
    }
}

// ── URL detection ─────────────────────────────────────────────────────────────

/// Map a physical key code to its base ASCII byte (unshifted, no modifiers).
/// Used to recover the intended letter when Alt/Option produces a Unicode character (macOS).
fn physical_key_to_ascii(kc: KeyCode) -> Option<u8> {
    match kc {
        KeyCode::KeyA => Some(b'a'), KeyCode::KeyB => Some(b'b'),
        KeyCode::KeyC => Some(b'c'), KeyCode::KeyD => Some(b'd'),
        KeyCode::KeyE => Some(b'e'), KeyCode::KeyF => Some(b'f'),
        KeyCode::KeyG => Some(b'g'), KeyCode::KeyH => Some(b'h'),
        KeyCode::KeyI => Some(b'i'), KeyCode::KeyJ => Some(b'j'),
        KeyCode::KeyK => Some(b'k'), KeyCode::KeyL => Some(b'l'),
        KeyCode::KeyM => Some(b'm'), KeyCode::KeyN => Some(b'n'),
        KeyCode::KeyO => Some(b'o'), KeyCode::KeyP => Some(b'p'),
        KeyCode::KeyQ => Some(b'q'), KeyCode::KeyR => Some(b'r'),
        KeyCode::KeyS => Some(b's'), KeyCode::KeyT => Some(b't'),
        KeyCode::KeyU => Some(b'u'), KeyCode::KeyV => Some(b'v'),
        KeyCode::KeyW => Some(b'w'), KeyCode::KeyX => Some(b'x'),
        KeyCode::KeyY => Some(b'y'), KeyCode::KeyZ => Some(b'z'),
        KeyCode::Digit0 => Some(b'0'), KeyCode::Digit1 => Some(b'1'),
        KeyCode::Digit2 => Some(b'2'), KeyCode::Digit3 => Some(b'3'),
        KeyCode::Digit4 => Some(b'4'), KeyCode::Digit5 => Some(b'5'),
        KeyCode::Digit6 => Some(b'6'), KeyCode::Digit7 => Some(b'7'),
        KeyCode::Digit8 => Some(b'8'), KeyCode::Digit9 => Some(b'9'),
        KeyCode::Minus => Some(b'-'),    KeyCode::Equal => Some(b'='),
        KeyCode::BracketLeft => Some(b'['), KeyCode::BracketRight => Some(b']'),
        KeyCode::Backslash => Some(b'\\'), KeyCode::Semicolon => Some(b';'),
        KeyCode::Quote => Some(b'\''),   KeyCode::Backquote => Some(b'`'),
        KeyCode::Comma => Some(b','),    KeyCode::Period => Some(b'.'),
        KeyCode::Slash => Some(b'/'),
        _ => None,
    }
}

fn find_urls(
    state: &terminal::TerminalState,
    vis_rows: usize,
    vis_cols: usize,
) -> Vec<(usize, usize, usize, String)> {
    let mut out = Vec::new();
    for row in 0..vis_rows {
        let cells: Vec<char> =
            (0..vis_cols).map(|col| state.visual_cell(row, col).c).collect();
        let mut col = 0;
        while col < vis_cols {
            let https = col + 8 <= vis_cols
                && cells[col..col + 8] == ['h', 't', 't', 'p', 's', ':', '/', '/'];
            let http = !https
                && col + 7 <= vis_cols
                && cells[col..col + 7] == ['h', 't', 't', 'p', ':', '/', '/'];
            if https || http {
                let start = col;
                let mut end = col + if https { 8 } else { 7 };
                while end < vis_cols {
                    match cells[end] {
                        ' ' | '\0' | '"' | '\'' | '`' | '<' | '>' | '\t' => break,
                        _ => end += 1,
                    }
                }
                // strip trailing punctuation unlikely to be part of the URL
                while end > start {
                    match cells[end - 1] {
                        '.' | ',' | ')' | ';' | ':' => end -= 1,
                        _ => break,
                    }
                }
                let url: String = cells[start..end].iter().collect();
                out.push((row, start, end, url));
                col = end;
            } else {
                col += 1;
            }
        }
    }
    out
}

// ── ns_view helper ────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn ns_view_ptr(window: &winit::window::Window) -> Option<*mut std::ffi::c_void> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr() as *mut std::ffi::c_void),
        _ => None,
    }
}

// ── WgpuShared ────────────────────────────────────────────────────────────────

struct WgpuShared {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface_format: wgpu::TextureFormat,
}

// ── TerminalWindow ────────────────────────────────────────────────────────────

struct TerminalWindow {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    renderer: Renderer,

    panes: Vec<Pane>,
    active_pane: usize,
    split: Option<SplitDir>,

    modifiers: ModifiersState,
    cursor_pos: (f64, f64),

    // Selection drag state (mirrors old App fields, now per-window)
    sel_anchor: Option<(i64, usize)>,
    selecting: bool,
    sel_scroll: i32,

    // Cursor blink
    cursor_visible: bool,
    last_blink: Instant,
}

impl TerminalWindow {
    fn pane_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let sz = self.window.inner_size();
        let w = sz.width  as f32;
        let h = sz.height as f32;
        match (self.panes.len(), &self.split) {
            (2, Some(SplitDir::Vertical)) => {
                let half = (w / 2.).floor();
                vec![(0., 0., half - 1., h), (half + 1., 0., w - half - 1., h)]
            }
            (2, Some(SplitDir::Horizontal)) => {
                let half = (h / 2.).floor();
                vec![(0., 0., w, half - 1.), (0., half + 1., w, h - half - 1.)]
            }
            _ => vec![(0., 0., w, h)],
        }
    }

    fn divider_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let sz = self.window.inner_size();
        let w = sz.width  as f32;
        let h = sz.height as f32;
        match (self.panes.len(), &self.split) {
            (2, Some(SplitDir::Vertical))   => {
                let half = (w / 2.).floor();
                vec![(half - 1., 0., 2., h)]
            }
            (2, Some(SplitDir::Horizontal)) => {
                let half = (h / 2.).floor();
                vec![(0., half - 1., w, 2.)]
            }
            _ => vec![],
        }
    }

    fn active(&self) -> &Pane {
        &self.panes[self.active_pane]
    }

    fn active_mut(&mut self) -> &mut Pane {
        &mut self.panes[self.active_pane]
    }

    fn pty_write(&mut self, data: &[u8]) {
        self.panes[self.active_pane].write(data);
    }

    fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.last_blink = Instant::now();
    }

    fn update_ghost(&mut self) {
        let buf = self.active().terminal.state.input_buffer.clone();
        let cur = self.active().terminal.state.input_cursor;
        let ghost = self.panes[self.active_pane]
            .engine
            .ghost(&buf, cur >= buf.len())
            .map(|s| s.to_string());
        self.panes[self.active_pane].ghost_text = ghost;
    }

    fn sync_title(&self) {
        let title = self.active().title();
        self.window.set_title(title);
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = ns_view_ptr(&self.window) {
            platform::set_tab_title(ns_view, title);
        }
    }

    fn accept_ghost(&mut self) {
        let g = self.panes[self.active_pane].ghost_text.take();
        if let Some(g) = g {
            self.panes[self.active_pane].write(g.as_bytes());
        }
    }

    fn selection_text(&self) -> String {
        let pane = &self.panes[self.active_pane];
        let sel = match pane.selection {
            Some(s) => s,
            None => return String::new(),
        };
        let state = &pane.terminal.state;
        let (r0, c0, r1, c1) = sel.to_viewport(state.viewport_offset);
        let mut result = String::new();
        for row in r0..=r1 {
            let col_start = if row == r0 { c0 } else { 0 };
            let col_end = if row == r1 { c1 + 1 } else { state.cols };
            let mut line = String::new();
            for col in col_start..col_end.min(state.cols) {
                let cell = state.visual_cell(row, col);
                if cell.c == '\0' {
                    line.push(' ');
                } else {
                    line.push(cell.c);
                    for &combining in cell.combining_chars() {
                        line.push(combining);
                    }
                }
            }
            if row > r0 {
                result.push('\n');
            }
            result.push_str(line.trim_end());
        }
        result
    }

    /// Convert pixel coordinate to (pane_idx, row, col). Returns None if out of bounds.
    fn pixel_to_pane_cell(&self, mx: f64, my: f64) -> Option<(usize, usize, usize)> {
        let rects = self.pane_rects();
        let cw = self.renderer.cell_width as f64;
        let ch = self.renderer.cell_height as f64;
        for (i, &(ox, oy, pw, ph)) in rects.iter().enumerate() {
            if mx >= ox as f64 && mx < (ox + pw) as f64
                && my >= oy as f64 && my < (oy + ph) as f64
            {
                let col = ((mx - ox as f64) / cw) as usize;
                let row = ((my - oy as f64) / ch) as usize;
                let cols = (pw as usize / self.renderer.cell_width).max(1);
                let rows = (ph as usize / self.renderer.cell_height).max(1);
                return Some((i, row.min(rows.saturating_sub(1)), col.min(cols.saturating_sub(1))));
            }
        }
        None
    }

    fn pane_at_pixel(&self, mx: f64, my: f64) -> Option<usize> {
        self.pixel_to_pane_cell(mx, my).map(|(i, _, _)| i)
    }

    /// Convert pixel to grid-relative (pane_idx, grid_row, col).
    fn pixel_to_grid_cell(&self, mx: f64, my: f64) -> Option<(usize, i64, usize)> {
        let (pi, vrow, col) = self.pixel_to_pane_cell(mx, my)?;
        let vo = self.panes[pi].terminal.state.viewport_offset as i64;
        Some((pi, vrow as i64 - vo, col))
    }

    fn update_cursor_icon(&self) {
        let icon = if self.modifiers.super_key() {
            let is_url = self.pixel_to_pane_cell(self.cursor_pos.0, self.cursor_pos.1)
                .map(|(pi, row, col)| {
                    let pane = &self.panes[pi];
                    let rects = self.pane_rects();
                    let (_, _, pw, ph) = rects.get(pi).copied().unwrap_or((0., 0., 0., 0.));
                    let vis_cols = (pw as usize / self.renderer.cell_width).max(1);
                    let vis_rows = (ph as usize / self.renderer.cell_height).max(1);
                    find_urls(&pane.terminal.state, vis_rows, vis_cols)
                        .iter()
                        .any(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
                })
                .unwrap_or(false);
            if is_url { CursorIcon::Pointer } else { CursorIcon::Default }
        } else {
            CursorIcon::Default
        };
        self.window.set_cursor(icon);
    }

    fn do_paste(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if let Some(png) = platform::clipboard_png() {
                let path = std::env::temp_dir().join(format!(
                    "term_paste_{}.png",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ));
                if std::fs::write(&path, &png).is_ok() {
                    let path_str = path.to_string_lossy().to_string();
                    let bracketed = self.active().terminal.state.bracketed_paste;
                    if bracketed { self.pty_write(b"\x1b[200~"); }
                    self.pty_write(path_str.as_bytes());
                    if bracketed { self.pty_write(b"\x1b[201~"); }
                }
                return;
            }
            if let Some(text) = platform::clipboard_text() {
                if !text.is_empty() {
                    let bracketed = self.active().terminal.state.bracketed_paste;
                    if bracketed { self.pty_write(b"\x1b[200~"); }
                    self.pty_write(text.as_bytes());
                    if bracketed { self.pty_write(b"\x1b[201~"); }
                }
                return;
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let text = paste_from_clipboard();
            if !text.is_empty() {
                let bracketed = self.active().terminal.state.bracketed_paste;
                if bracketed { self.pty_write(b"\x1b[200~"); }
                self.pty_write(text.as_bytes());
                if bracketed { self.pty_write(b"\x1b[201~"); }
            }
        }
    }

    fn redraw(&mut self) {
        let window = self.window.clone();
        let sz = window.inner_size();
        let (w, h) = (sz.width, sz.height);
        if w == 0 || h == 0 { return; }

        let rects    = self.pane_rects();
        let dividers = self.divider_rects();
        let cw = self.renderer.cell_width;
        let ch = self.renderer.cell_height;
        let mods_super = self.modifiers.super_key();

        // Build URL underlines per pane
        let url_ulines: Vec<Vec<(usize, usize, usize)>> = (0..self.panes.len())
            .map(|i| {
                if i < rects.len() && mods_super {
                    let (_, _, pw, ph) = rects[i];
                    let vis_rows = (ph as usize / ch).max(1);
                    let vis_cols = (pw as usize / cw).max(1);
                    find_urls(&self.panes[i].terminal.state, vis_rows, vis_cols)
                        .into_iter()
                        .map(|(r, c0, c1, _)| (r, c0, c1))
                        .collect()
                } else {
                    vec![]
                }
            })
            .collect();

        // Build PaneView slice
        let mut pane_views: Vec<PaneView<'_>> = Vec::new();
        for (i, pane) in self.panes.iter().enumerate() {
            if i >= rects.len() { break; }
            let (ox, oy, pw, ph) = rects[i];
            let sel = pane.selection.map(|s| s.to_viewport(pane.terminal.state.viewport_offset));
            pane_views.push(PaneView {
                x: ox, y: oy,
                width: pw, height: ph,
                state: &pane.terminal.state,
                show_cursor: (i == self.active_pane) && self.cursor_visible,
                ghost: pane.ghost_text.as_deref(),
                selection: sel,
                url_underlines: &url_ulines[i],
            });
        }

        let output = match self.surface.get_current_texture() {
            Ok(o) => o,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.renderer.device, &self.surface_config);
                return;
            }
            Err(e) => { eprintln!("surface error: {e}"); return; }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.render(&view, w, h, &pane_views, &dividers);
        output.present();
    }

    fn on_resize(&mut self, width: u32, height: u32) {
        self.surface_config.width  = width.max(1);
        self.surface_config.height = height.max(1);
        self.surface.configure(&self.renderer.device, &self.surface_config);

        let rects = self.pane_rects();
        let cw = self.renderer.cell_width;
        let ch = self.renderer.cell_height;
        for (i, pane) in self.panes.iter_mut().enumerate() {
            let (_, _, pw, ph) = if i < rects.len() { rects[i] } else { (0., 0., width as f32, height as f32) };
            let cols = (pw as usize / cw).max(1);
            let rows = (ph as usize / ch).max(1);
            pane.terminal.resize(cols, rows);
            let _ = pane.pty_master.resize(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: width as u16,
                pixel_height: height as u16,
            });
        }
    }

    /// Returns true if the key event was consumed at the window level (tab navigation etc.)
    /// Returns false if the key should be forwarded to PTY.
    fn handle_key(
        &mut self,
        event: winit::event::KeyEvent,
        // Actions that need App-level handling are returned as strings
    ) -> KeyAction {
        if event.state != ElementState::Pressed {
            return KeyAction::None;
        }
        self.reset_blink();

        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let sup = self.modifiers.super_key();
        let shift = self.modifiers.shift_key();

        // Clear selection on any keypress (except Cmd+C and bare modifiers)
        let is_cmd_c = sup
            && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "c");
        let is_modifier_only = matches!(
            &event.logical_key,
            Key::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::Super
                    | NamedKey::Hyper
                    | NamedKey::Meta
                    | NamedKey::CapsLock
            )
        );
        if !is_cmd_c && !is_modifier_only {
            if self.panes[self.active_pane].selection.is_some() {
                self.panes[self.active_pane].selection = None;
                self.window.request_redraw();
            }
        }

        // ── Cmd chords ────────────────────────────────────────────────────────
        if sup {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    if alt {
                        // Cmd+Opt+Left: previous pane
                        if self.panes.len() > 1 {
                            self.active_pane = self.active_pane.saturating_sub(1);
                        }
                        return KeyAction::None;
                    }
                    self.pty_write(b"\x01");
                    return KeyAction::None;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    if alt {
                        // Cmd+Opt+Right: next pane
                        if self.panes.len() > 1 {
                            self.active_pane = (self.active_pane + 1).min(self.panes.len() - 1);
                        }
                        return KeyAction::None;
                    }
                    self.pty_write(b"\x05");
                    return KeyAction::None;
                }
                Key::Named(NamedKey::ArrowUp) => {
                    let n = self.active().terminal.state.rows as i32;
                    self.active_mut().terminal.state.scroll_viewport(n);
                    return KeyAction::None;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let n = self.active().terminal.state.rows as i32;
                    self.active_mut().terminal.state.scroll_viewport(-n);
                    return KeyAction::None;
                }
                Key::Named(NamedKey::Home) => {
                    self.active_mut().terminal.state.scroll_viewport(i32::MAX);
                    return KeyAction::None;
                }
                Key::Named(NamedKey::End) => {
                    self.active_mut().terminal.state.snap_to_bottom();
                    return KeyAction::None;
                }
                Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) => {
                    self.pty_write(b"\x15");
                    return KeyAction::None;
                }
                Key::Character(c) => match c.as_str() {
                    "c" => {
                        if self.panes[self.active_pane].selection.is_some() {
                            let raw = self.selection_text();
                            if !raw.is_empty() {
                                copy_to_clipboard(&strip_tcat_gutter(&raw));
                            }
                            self.panes[self.active_pane].selection = None;
                            self.window.request_redraw();
                        }
                        return KeyAction::None;
                    }
                    "v" => {
                        self.do_paste();
                        return KeyAction::None;
                    }
                    "t" => {
                        return KeyAction::OpenTab;
                    }
                    "w" => {
                        return KeyAction::ClosePaneOrWindow;
                    }
                    "[" => {
                        return KeyAction::PrevTab;
                    }
                    "]" => {
                        return KeyAction::NextTab;
                    }
                    "d" => {
                        if shift {
                            return KeyAction::SplitHorizontal;
                        } else {
                            return KeyAction::SplitVertical;
                        }
                    }
                    "k" => {
                        self.pty_write(b"\x0b");
                        return KeyAction::None;
                    }
                    "a" => {
                        self.pty_write(b"\x01");
                        return KeyAction::None;
                    }
                    "e" => {
                        self.pty_write(b"\x05");
                        return KeyAction::None;
                    }
                    s => {
                        if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10))
                            && d >= 1
                        {
                            // Cmd+1..9: no-op (native tab bar handles it)
                            let _ = d;
                        }
                        return KeyAction::None;
                    }
                },
                _ => return KeyAction::None,
            }
        }

        // ── Alt+named keys ────────────────────────────────────────────────────
        if alt {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    self.pty_write(b"\x1bb");
                    return KeyAction::None;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.pty_write(b"\x1bf");
                    return KeyAction::None;
                }
                Key::Named(NamedKey::Backspace) => {
                    self.pty_write(b"\x1b\x7f");
                    return KeyAction::None;
                }
                Key::Named(NamedKey::Delete) => {
                    self.pty_write(b"\x1bd");
                    return KeyAction::None;
                }
                _ => {}
            }
        }

        // Shift+Enter → kitty Shift+Enter
        if shift && !ctrl && !alt && matches!(&event.logical_key, Key::Named(NamedKey::Enter)) {
            self.active_mut().ghost_text = None;
            self.pty_write(b"\x1b[13;2u");
            return KeyAction::None;
        }
        // Shift+Tab → reverse tab
        if shift && !ctrl && !alt && matches!(&event.logical_key, Key::Named(NamedKey::Tab)) {
            self.pty_write(b"\x1b[Z");
            return KeyAction::None;
        }

        // ── Printable text ────────────────────────────────────────────────────
        if !ctrl && !alt
            && let Some(text) = &event.text {
                self.active_mut().ghost_text = None;
                let bytes = text.as_str().as_bytes().to_vec();
                self.active_mut().write(&bytes);
                return KeyAction::None;
            }

        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\r");
            }
            Key::Named(NamedKey::Backspace) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\x7f");
            }
            Key::Named(NamedKey::Tab) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\t");
            }
            Key::Named(NamedKey::Escape) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\x1b");
            }
            Key::Named(NamedKey::ArrowRight) => {
                if self.active().ghost_text.is_some() {
                    self.accept_ghost();
                } else {
                    self.pty_write(b"\x1b[C");
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\x1b[A");
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\x1b[B");
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.active_mut().ghost_text = None;
                self.pty_write(b"\x1b[D");
            }
            Key::Named(NamedKey::Home) => self.pty_write(b"\x1b[H"),
            Key::Named(NamedKey::End) => self.pty_write(b"\x1b[F"),
            Key::Named(NamedKey::PageUp) => self.pty_write(b"\x1b[5~"),
            Key::Named(NamedKey::PageDown) => self.pty_write(b"\x1b[6~"),
            Key::Named(NamedKey::Delete) => self.pty_write(b"\x1b[3~"),
            Key::Character(c) if ctrl => {
                if let Some(ch) = c.chars().next() {
                    let byte = match ch {
                        '_' => 0x1f_u8,
                        ch => (ch.to_ascii_lowercase() as u8)
                            .wrapping_sub(b'a')
                            .wrapping_add(1),
                    };
                    if byte > 0 && byte < 32 {
                        self.active_mut().ghost_text = None;
                        self.pty_write(&[byte]);
                    }
                }
            }
            Key::Character(_) if alt => {
                if let PhysicalKey::Code(kc) = event.physical_key {
                    if let Some(ascii) = physical_key_to_ascii(kc) {
                        self.pty_write(&[0x1b, ascii]);
                    }
                }
            }
            _ => {}
        }

        KeyAction::None
    }
}

// ── KeyAction ─────────────────────────────────────────────────────────────────

enum KeyAction {
    None,
    OpenTab,
    ClosePaneOrWindow,
    PrevTab,
    NextTab,
    SplitVertical,
    SplitHorizontal,
}

// ── App ───────────────────────────────────────────────────────────────────────

struct App {
    wgpu: Option<WgpuShared>,
    windows: HashMap<WindowId, TerminalWindow>,
    pane_to_window: HashMap<u64, WindowId>,
    next_pane_id: u64,
    proxy: EventLoopProxy<AppEvent>,
    tabbing_id: String,
}

impl App {
    fn new(proxy: EventLoopProxy<AppEvent>) -> Self {
        Self {
            wgpu: None,
            windows: HashMap::new(),
            pane_to_window: HashMap::new(),
            next_pane_id: 0,
            proxy,
            tabbing_id: format!("term-{}", std::process::id()),
        }
    }

    fn alloc_pane_id(&mut self) -> u64 {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    fn create_pane(
        &mut self,
        window_id: WindowId,
        proxy: &EventLoopProxy<AppEvent>,
        cols: usize,
        rows: usize,
    ) -> Option<Pane> {
        let pane_id = self.alloc_pane_id();
        self.pane_to_window.insert(pane_id, window_id);

        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("openpty: {e}");
                return None;
            }
        };

        let mut cmd = CommandBuilder::new("zsh");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        setup_shell_env(&mut cmd);

        let _ = pair.slave.spawn_command(cmd);
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let writer = pair.master.take_writer().expect("take writer");

        let proxy_clone = proxy.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy_clone.send_event(AppEvent::PtyExit { pane_id });
                        break;
                    }
                    Ok(n) => {
                        let _ = proxy_clone.send_event(AppEvent::PtyData {
                            pane_id,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        Some(Pane {
            id: pane_id,
            terminal: Terminal::new(cols, rows),
            pty_master: pair.master,
            pty_writer: writer,
            ghost_text: None,
            engine: Engine::new(),
            selection: None,
        })
    }

    fn open_tab(&mut self, event_loop: &ActiveEventLoop) {
        let wgpu = match &self.wgpu {
            Some(w) => w,
            None => return,
        };

        // Find an existing window to group with
        let existing_ns_view: Option<*mut std::ffi::c_void> = {
            #[cfg(target_os = "macos")]
            {
                self.windows.values().next().and_then(|tw| ns_view_ptr(&tw.window))
            }
            #[cfg(not(target_os = "macos"))]
            { None }
        };

        let mut attrs = WindowAttributes::default()
            .with_title("term")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));

        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs = attrs.with_tabbing_identifier(&self.tabbing_id);
        }

        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let window_id = window.id();

        // Set NSWindowTabbingModePreferred so tab bar always shows
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = ns_view_ptr(&window) {
            use objc2::{msg_send, runtime::AnyObject};
            unsafe {
                let view = ns_view as *mut AnyObject;
                let win: *mut AnyObject = msg_send![view, window];
                if !win.is_null() {
                    let _: () = msg_send![win, setTabbingMode: 1i64];
                }
            }
        }

        // Add as native tab to existing window group
        #[cfg(target_os = "macos")]
        if let (Some(existing), Some(ns_view)) = (existing_ns_view, ns_view_ptr(&window)) {
            platform::add_window_as_tab(existing, ns_view);
        }

        // Create wgpu surface
        let surface: wgpu::Surface<'static> = wgpu.instance
            .create_surface(window.clone())
            .expect("create surface");

        let caps = surface.get_capabilities(&wgpu.adapter);
        let surface_format = wgpu.surface_format;
        let size = window.inner_size();
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&wgpu.device, &surface_config);

        let scale = window.scale_factor();
        let renderer = Renderer::new(wgpu.device.clone(), wgpu.queue.clone(), surface_format, scale);

        let (cols, rows) = {
            let cw = renderer.cell_width;
            let ch = renderer.cell_height;
            (
                (size.width as usize / cw).max(1),
                (size.height as usize / ch).max(1),
            )
        };

        let proxy = self.proxy.clone();
        let first_pane = match self.create_pane(window_id, &proxy, cols, rows) {
            Some(p) => p,
            None => return,
        };

        let tw = TerminalWindow {
            window,
            surface,
            surface_config,
            renderer,
            panes: vec![first_pane],
            active_pane: 0,
            split: None,
            modifiers: ModifiersState::empty(),
            cursor_pos: (-1., -1.),
            sel_anchor: None,
            selecting: false,
            sel_scroll: 0,
            cursor_visible: true,
            last_blink: Instant::now(),
        };
        tw.sync_title();
        self.windows.insert(window_id, tw);

        // Wire the "+" button after the window is fully set up.
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = self.windows.get(&window_id).and_then(|tw| ns_view_ptr(&tw.window)) {
            platform::setup_add_tab_button(ns_view);
        }
    }

    fn add_split(&mut self, window_id: WindowId, dir: SplitDir) {
        let tw = match self.windows.get_mut(&window_id) {
            Some(w) => w,
            None => return,
        };
        if tw.panes.len() >= 2 {
            return; // already split
        }
        let idx = tw.panes.len(); // will be 1
        tw.split = Some(dir);

        // Temporarily compute size; we need renderer + window size
        let sz = tw.window.inner_size();
        let cw = tw.renderer.cell_width;
        let ch = tw.renderer.cell_height;
        let (cols, rows) = match dir {
            SplitDir::Vertical => {
                let half = (sz.width as f32 / 2.).floor() as usize;
                let cols = (half / cw).max(1);
                let rows = (sz.height as usize / ch).max(1);
                (cols, rows)
            }
            SplitDir::Horizontal => {
                let half = (sz.height as f32 / 2.).floor() as usize;
                let cols = (sz.width as usize / cw).max(1);
                let rows = (half / ch).max(1);
                (cols, rows)
            }
        };

        let proxy = self.proxy.clone();
        let new_pane = match self.create_pane(window_id, &proxy, cols, rows) {
            Some(p) => p,
            None => return,
        };
        let tw = self.windows.get_mut(&window_id).unwrap();
        tw.panes.push(new_pane);
        tw.active_pane = idx;

        // Also resize the first pane to its new half
        let rects = tw.pane_rects();
        for (i, pane) in tw.panes.iter_mut().enumerate() {
            if let Some(&(_, _, pw, ph)) = rects.get(i) {
                let cols = (pw as usize / cw).max(1);
                let rows = (ph as usize / ch).max(1);
                pane.terminal.resize(cols, rows);
                let _ = pane.pty_master.resize(PtySize {
                    rows: rows as u16,
                    cols: cols as u16,
                    pixel_width: sz.width as u16,
                    pixel_height: sz.height as u16,
                });
            }
        }
    }

    fn close_pane_or_window(&mut self, window_id: WindowId, event_loop: &ActiveEventLoop) {
        let tw = match self.windows.get_mut(&window_id) {
            Some(w) => w,
            None => return,
        };
        if tw.panes.len() <= 1 {
            // Close the window
            self.close_window(window_id, event_loop);
        } else {
            // Remove the active pane
            let ai = tw.active_pane;
            let removed_id = tw.panes[ai].id;
            tw.panes.remove(ai);
            tw.active_pane = tw.active_pane.min(tw.panes.len() - 1);
            tw.split = None;
            self.pane_to_window.remove(&removed_id);

            // Resize the remaining pane to fill the window
            let tw = self.windows.get_mut(&window_id).unwrap();
            let sz = tw.window.inner_size();
            let cw = tw.renderer.cell_width;
            let ch = tw.renderer.cell_height;
            let cols = (sz.width as usize / cw).max(1);
            let rows = (sz.height as usize / ch).max(1);
            if let Some(pane) = tw.panes.get_mut(0) {
                pane.terminal.resize(cols, rows);
                let _ = pane.pty_master.resize(PtySize {
                    rows: rows as u16,
                    cols: cols as u16,
                    pixel_width: sz.width as u16,
                    pixel_height: sz.height as u16,
                });
            }
        }
    }

    fn close_window(&mut self, window_id: WindowId, event_loop: &ActiveEventLoop) {
        if let Some(tw) = self.windows.remove(&window_id) {
            for pane in &tw.panes {
                self.pane_to_window.remove(&pane.id);
            }
        }
        if self.windows.is_empty() {
            event_loop.exit();
        }
    }
}

// ── winit ApplicationHandler ──────────────────────────────────────────────────

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Init wgpu
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let mut attrs = WindowAttributes::default()
            .with_title("term")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));

        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowAttributesExtMacOS;
            let tid = self.tabbing_id.clone();
            attrs = attrs.with_tabbing_identifier(&tid);
        }

        let window = Arc::new(event_loop.create_window(attrs).unwrap());
        let window_id = window.id();

        let surface: wgpu::Surface<'static> = instance
            .create_surface(window.clone())
            .expect("create wgpu surface");

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            },
        ))
        .expect("no suitable GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("term"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
            },
            None,
        ))
        .expect("request_device");

        let device = Arc::new(device);
        let queue  = Arc::new(queue);

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or_else(|| caps.formats.first().copied().unwrap_or(wgpu::TextureFormat::Bgra8Unorm));
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // Set NSWindowTabbingModePreferred
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = ns_view_ptr(&window) {
            use objc2::{msg_send, runtime::AnyObject};
            unsafe {
                let view = ns_view as *mut AnyObject;
                let win: *mut AnyObject = msg_send![view, window];
                if !win.is_null() {
                    let _: () = msg_send![win, setTabbingMode: 1i64];
                }
            }
        }

        // Register the new-tab callback (only fires once since OnceLock).
        #[cfg(target_os = "macos")]
        {
            let proxy = self.proxy.clone();
            platform::set_new_tab_callback(move || {
                let _ = proxy.send_event(AppEvent::NewTab);
            });
        }

        let scale = window.scale_factor();
        let renderer = Renderer::new(device.clone(), queue.clone(), surface_format, scale);

        let (cols, rows) = {
            let cw = renderer.cell_width;
            let ch = renderer.cell_height;
            (
                (size.width as usize / cw).max(1),
                (size.height as usize / ch).max(1),
            )
        };

        self.wgpu = Some(WgpuShared {
            instance,
            adapter,
            device,
            queue,
            surface_format,
        });

        let proxy = self.proxy.clone();
        let first_pane = match self.create_pane(window_id, &proxy, cols, rows) {
            Some(p) => p,
            None => return,
        };

        let tw = TerminalWindow {
            window,
            surface,
            surface_config,
            renderer,
            panes: vec![first_pane],
            active_pane: 0,
            split: None,
            modifiers: ModifiersState::empty(),
            cursor_pos: (-1., -1.),
            sel_anchor: None,
            selecting: false,
            sel_scroll: 0,
            cursor_visible: true,
            last_blink: Instant::now(),
        };
        tw.sync_title();
        self.windows.insert(window_id, tw);

        // Wire the "+" button after the window is fully set up.
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = self.windows.get(&window_id).and_then(|tw| ns_view_ptr(&tw.window)) {
            platform::setup_add_tab_button(ns_view);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                self.close_window(window_id, event_loop);
            }

            WindowEvent::RedrawRequested => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.redraw();
                }
            }

            WindowEvent::Resized(s) => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.on_resize(s.width, s.height);
                    tw.window.request_redraw();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(queue) = self.wgpu.as_ref().map(|w| w.queue.clone()) {
                    if let Some(tw) = self.windows.get_mut(&window_id) {
                        let fmt = tw.renderer.surface_format;
                        let device = tw.renderer.device.clone();
                        tw.renderer = Renderer::new(device, queue, fmt, scale_factor);
                    }
                }
            }

            WindowEvent::ModifiersChanged(state) => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.modifiers = state.state();
                    tw.update_cursor_icon();
                    tw.window.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let action = if let Some(tw) = self.windows.get_mut(&window_id) {
                    let a = tw.handle_key(event);
                    tw.window.request_redraw();
                    a
                } else {
                    KeyAction::None
                };

                match action {
                    KeyAction::None => {}
                    KeyAction::OpenTab => {
                        self.open_tab(event_loop);
                    }
                    KeyAction::ClosePaneOrWindow => {
                        self.close_pane_or_window(window_id, event_loop);
                        if let Some(tw) = self.windows.get(&window_id) {
                            tw.sync_title();
                            tw.window.request_redraw();
                        }
                    }
                    KeyAction::PrevTab => {
                        #[cfg(target_os = "macos")]
                        if let Some(tw) = self.windows.get(&window_id) {
                            if let Some(ns_view) = ns_view_ptr(&tw.window) {
                                platform::select_prev_tab(ns_view);
                            }
                        }
                    }
                    KeyAction::NextTab => {
                        #[cfg(target_os = "macos")]
                        if let Some(tw) = self.windows.get(&window_id) {
                            if let Some(ns_view) = ns_view_ptr(&tw.window) {
                                platform::select_next_tab(ns_view);
                            }
                        }
                    }
                    KeyAction::SplitVertical => {
                        self.add_split(window_id, SplitDir::Vertical);
                        if let Some(tw) = self.windows.get(&window_id) {
                            tw.window.request_redraw();
                        }
                    }
                    KeyAction::SplitHorizontal => {
                        self.add_split(window_id, SplitDir::Horizontal);
                        if let Some(tw) = self.windows.get(&window_id) {
                            tw.window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.cursor_pos = (position.x, position.y);
                    tw.update_cursor_icon();

                    if tw.selecting {
                        if let Some(anchor) = tw.sel_anchor {
                            let cell = tw.pixel_to_grid_cell(position.x, position.y)
                                .or_else(|| {
                                    // Clamp to nearest edge
                                    let rects = tw.pane_rects();
                                    let ai = tw.active_pane;
                                    let (ox, oy, pw, ph) = rects.get(ai).copied().unwrap_or((0., 0., tw.window.inner_size().width as f32, tw.window.inner_size().height as f32));
                                    let cw = tw.renderer.cell_width as f64;
                                    let vis_cols = (pw as usize / tw.renderer.cell_width).max(1);
                                    let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                                    let vo = tw.panes[ai].terminal.state.viewport_offset as i64;
                                    let col = ((position.x - ox as f64).max(0.) / cw as f64) as usize;
                                    let col = col.min(vis_cols.saturating_sub(1));
                                    if position.y < oy as f64 {
                                        Some((ai, -vo, col))
                                    } else {
                                        Some((ai, vis_rows as i64 - 1 - vo, col))
                                    }
                                });
                            if let Some((_pi, grid_row, col)) = cell {
                                let c = (grid_row, col);
                                tw.panes[tw.active_pane].selection = if c != anchor {
                                    Some(Selection { start: anchor, end: c })
                                } else {
                                    None
                                };
                            }

                            // Auto-scroll detection
                            let ai = tw.active_pane;
                            let rects = tw.pane_rects();
                            let (_ox, oy, _pw, ph) = rects.get(ai).copied().unwrap_or((0., 0., 0., 0.));
                            let ch = tw.renderer.cell_height as f64;
                            let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                            let term_bottom = oy as f64 + vis_rows as f64 * ch;
                            tw.sel_scroll = if position.y < oy as f64 {
                                1
                            } else if position.y >= term_bottom {
                                -1
                            } else {
                                0
                            };
                            tw.window.request_redraw();
                        }
                    } else {
                        tw.window.request_redraw();
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.cursor_pos = (-1., -1.);
                    tw.window.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    let (mx, my) = tw.cursor_pos;
                    let sup = tw.modifiers.super_key();

                    // Cmd+click: open URL
                    let opened = if sup {
                        if let Some((pi, row, col)) = tw.pixel_to_pane_cell(mx, my) {
                            let rects = tw.pane_rects();
                            let (_, _, pw, ph) = rects.get(pi).copied().unwrap_or((0., 0., 0., 0.));
                            let vis_cols = (pw as usize / tw.renderer.cell_width).max(1);
                            let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                            let url = find_urls(&tw.panes[pi].terminal.state, vis_rows, vis_cols)
                                .into_iter()
                                .find(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
                                .map(|(_, _, _, u)| u);
                            if let Some(u) = url {
                                let _ = std::process::Command::new("open").arg(&u).spawn();
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !opened {
                        // Activate pane under cursor
                        if let Some(pi) = tw.pane_at_pixel(mx, my) {
                            tw.active_pane = pi;
                        }
                        // Start selection
                        tw.panes[tw.active_pane].selection = None;
                        if let Some((_pi, grid_row, col)) = tw.pixel_to_grid_cell(mx, my) {
                            tw.sel_anchor = Some((grid_row, col));
                        } else {
                            tw.sel_anchor = None;
                        }
                        tw.selecting = true;
                    }
                    tw.window.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    tw.selecting = false;
                    tw.sel_scroll = 0;
                    if tw.panes[tw.active_pane].selection.is_none() {
                        tw.sel_anchor = None;
                    }
                    tw.window.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 4.5) as i32,
                        MouseScrollDelta::PixelDelta(pos) => {
                            let ch = tw.renderer.cell_height as f64;
                            (pos.y / ch * 2.25) as i32
                        }
                    };
                    tw.active_mut().terminal.state.scroll_viewport(lines);
                    tw.window.request_redraw();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        const BLINK_PERIOD: Duration = Duration::from_millis(530);
        const SCROLL_PERIOD: Duration = Duration::from_millis(50);
        let now = Instant::now();

        let mut earliest_deadline = now + BLINK_PERIOD;

        for tw in self.windows.values_mut() {
            // Cursor blink
            if now.duration_since(tw.last_blink) >= BLINK_PERIOD {
                tw.cursor_visible = !tw.cursor_visible;
                tw.last_blink = now;
                tw.window.request_redraw();
            }

            // Auto-scroll while selection drag extends beyond terminal area
            if tw.selecting && tw.sel_scroll != 0 {
                let dir = tw.sel_scroll;
                tw.panes[tw.active_pane].terminal.state.scroll_viewport(dir);
                if let Some(anchor) = tw.sel_anchor {
                    let rects = tw.pane_rects();
                    let ai = tw.active_pane;
                    let (_, _, pw, ph) = rects.get(ai).copied().unwrap_or((0., 0., 0., 0.));
                    let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                    let vo = tw.panes[ai].terminal.state.viewport_offset as i64;
                    let (mx, _) = tw.cursor_pos;
                    let cw = tw.renderer.cell_width as f64;
                    let vis_cols = (pw as usize / tw.renderer.cell_width).max(1);
                    let col = ((mx.max(0.0)) / cw) as usize;
                    let col = col.min(vis_cols.saturating_sub(1));
                    let edge_row = if dir > 0 { -vo } else { vis_rows as i64 - 1 - vo };
                    let c = (edge_row, col);
                    tw.panes[ai].selection = if c != anchor {
                        Some(Selection { start: anchor, end: c })
                    } else {
                        None
                    };
                }
                tw.window.request_redraw();
                earliest_deadline = earliest_deadline.min(now + SCROLL_PERIOD);
            }

            let next_blink = tw.last_blink + BLINK_PERIOD;
            earliest_deadline = earliest_deadline.min(next_blink);
        }

        event_loop.set_control_flow(ControlFlow::WaitUntil(earliest_deadline));
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::PtyData { pane_id, data } => {
                let window_id = match self.pane_to_window.get(&pane_id).copied() {
                    Some(w) => w,
                    None => return,
                };
                let tw = match self.windows.get_mut(&window_id) {
                    Some(w) => w,
                    None => return,
                };
                if let Some(pane) = tw.panes.iter_mut().find(|p| p.id == pane_id) {
                    if !pane.terminal.state.is_scrolled_back() {
                        pane.terminal.state.snap_to_bottom();
                    }
                    pane.terminal.process(&data);
                    let responses: Vec<Vec<u8>> =
                        pane.terminal.state.pending_responses.drain(..).collect();
                    for r in responses {
                        let _ = pane.pty_writer.write_all(&r);
                        let _ = pane.pty_writer.flush();
                    }
                    if pane.terminal.state.osc_52_query {
                        pane.terminal.state.osc_52_query = false;
                        let payload = osc52_clipboard_payload();
                        let response = format!("\x1b]52;c;{payload}\x07");
                        let _ = pane.pty_writer.write_all(response.as_bytes());
                        let _ = pane.pty_writer.flush();
                    }
                }
                // Update ghost text for active pane
                if tw.panes.get(tw.active_pane).map(|p| p.id) == Some(pane_id) {
                    tw.update_ghost();
                    tw.sync_title();
                }
                tw.reset_blink();
                tw.window.request_redraw();
            }
            AppEvent::PtyExit { pane_id } => {
                let window_id = match self.pane_to_window.get(&pane_id).copied() {
                    Some(w) => w,
                    None => return,
                };
                self.pane_to_window.remove(&pane_id);
                let should_close_window = {
                    let tw = match self.windows.get_mut(&window_id) {
                        Some(w) => w,
                        None => return,
                    };
                    if let Some(pos) = tw.panes.iter().position(|p| p.id == pane_id) {
                        if tw.panes.len() == 1 {
                            true
                        } else {
                            tw.panes.remove(pos);
                            tw.active_pane = tw.active_pane.min(tw.panes.len() - 1);
                            tw.split = None;
                            tw.window.request_redraw();
                            false
                        }
                    } else {
                        false
                    }
                };
                if should_close_window {
                    self.close_window(window_id, event_loop);
                }
            }
            AppEvent::NewTab => {
                self.open_tab(event_loop);
            }
        }
    }
}

// ── Clipboard ─────────────────────────────────────────────────────────────────

fn osc52_clipboard_payload() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Some(png) = platform::clipboard_png() {
            return base64_encode(&png);
        }
        if let Some(text) = platform::clipboard_text() {
            if !text.is_empty() {
                return base64_encode(text.as_bytes());
            }
        }
        return String::new();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let text = paste_from_clipboard();
        if text.is_empty() {
            return String::new();
        }
        base64_encode(text.as_bytes())
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[((b0 & 3) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 15) << 2 | b2 >> 6) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .env("LANG", "en_US.UTF-8")
        .env("LC_ALL", "en_US.UTF-8")
        .spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// Strip tcat's line-number gutter from copied text.
fn strip_tcat_gutter(text: &str) -> String {
    const SEP: char = '\u{2502}'; // │
    text.lines()
        .map(|line| {
            let chars: Vec<char> = line.chars().collect();
            for i in 1..chars.len().saturating_sub(1) {
                if chars[i] != SEP || chars[i - 1] != ' ' || chars[i + 1] != ' ' {
                    continue;
                }
                let prefix = &chars[..i - 1];
                if prefix.iter().all(|&c| c == ' ' || c == '\0' || c.is_ascii_digit())
                    && prefix.iter().any(|c| c.is_ascii_digit())
                {
                    return chars[i + 2..].iter().collect::<String>();
                }
            }
            line.to_string()
        })
        .collect::<Vec<String>>()
        .join("\n")
}

#[cfg(not(target_os = "macos"))]
fn paste_from_clipboard() -> String {
    use std::process::Command;
    Command::new("pbpaste")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ── Shell environment setup ───────────────────────────────────────────────────

fn setup_shell_env(cmd: &mut CommandBuilder) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let tcat = exe_dir
        .as_ref()
        .map(|d| d.join("tcat"))
        .filter(|p| p.exists());
    let tdiff = exe_dir
        .as_ref()
        .map(|d| d.join("tdiff"))
        .filter(|p| p.exists());
    let tjson = exe_dir
        .as_ref()
        .map(|d| d.join("tjson"))
        .filter(|p| p.exists());

    let zdotdir = std::env::temp_dir().join(format!("term_zsh_{}", std::process::id()));
    if std::fs::create_dir_all(&zdotdir).is_err() {
        return;
    }

    let _ = std::fs::write(
        zdotdir.join(".zshenv"),
        format!("[ -f '{home}/.zshenv' ] && source '{home}/.zshenv'\n"),
    );

    let cat_fn = match &tcat {
        Some(p) => format!(
            "_TCAT='{}'\nfunction cat() {{\n  if [ $# -ge 1 ] && [ -f \"$1\" ]; then\n    \"$_TCAT\" \"$@\"\n  else\n    command cat \"$@\"\n  fi\n}}\n",
            p.display()
        ),
        None => String::new(),
    };
    let diff_fn = match &tdiff {
        Some(p) => format!(
            "export GIT_PAGER='{}'\nexport GIT_COLOR_UI=never\n",
            p.display()
        ),
        None => String::new(),
    };
    let json_fn = match &tjson {
        Some(p) => format!(
            "_TJSON='{}'\nfunction json() {{ \"$_TJSON\" \"$@\"; }}\n",
            p.display()
        ),
        None => String::new(),
    };

    let zle_hooks = r#"
_term_buf_report() { printf '\033]9001;%s\034%d\033\\' "$BUFFER" "$CURSOR"; }
_term_preexec_clear() { printf '\033]9001;\034\033\\'; }
_term_chpwd() { printf '\033]7;file://%s%s\033\\' "$HOST" "$PWD"; }
# Dynamic tab titles: CWD at prompt, command name while running
_term_title_precmd() {
    local dir="${PWD/#$HOME/~}"
    printf '\033]0;%s\033\\' "$dir"
}
_term_title_preexec() {
    # Show just the first word (command name) to keep it compact
    printf '\033]0;%s\033\\' "${1%% *}"
}
if autoload -Uz add-zle-hook-widget 2>/dev/null && (( ${+functions[add-zle-hook-widget]} )); then
    add-zle-hook-widget zle-line-pre-redraw _term_buf_report
fi
preexec_functions+=( _term_preexec_clear _term_title_preexec )
precmd_functions+=( _term_title_precmd )
chpwd_functions+=( _term_chpwd )
_term_chpwd
_term_title_precmd
# Case-insensitive tab completion (e.g. c<TAB> completes Cargo.toml)
autoload -Uz compinit && compinit -C
zstyle ':completion:*' matcher-list 'm:{a-zA-Z}={A-Za-z}'
"#;

    let zshrc = format!(
        "ZDOTDIR='{home}'\n\
         [ -f '{home}/.zprofile' ] && source '{home}/.zprofile'\n\
         [ -f '{home}/.zshrc' ] && source '{home}/.zshrc'\n\
         {cat_fn}{diff_fn}{json_fn}{zle_hooks}"
    );
    let _ = std::fs::write(zdotdir.join(".zshrc"), &zshrc);

    cmd.env("ZDOTDIR", &zdotdir);
    cmd.env("TERM_PROGRAM", "ghostty");
    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }
    if std::env::var("LC_ALL").is_err() {
        cmd.env("LC_ALL", "en_US.UTF-8");
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::{base64_encode, find_urls};
    use crate::terminal::TerminalState;

    /// Write `text` into row 0 of a freshly-created state.
    fn make_state(text: &str) -> TerminalState {
        let cols = text.chars().count().max(1);
        let mut s = TerminalState::new(cols, 1);
        for (i, c) in text.chars().enumerate() {
            s.grid[0][i].c = c;
        }
        s
    }

    /// Convenience: collect just the URL strings from a single-row state.
    fn urls(text: &str) -> Vec<String> {
        let s = make_state(text);
        let cols = s.cols;
        find_urls(&s, 1, cols).into_iter().map(|(_, _, _, u)| u).collect()
    }

    // ── happy-path detection ──────────────────────────────────────────────────

    #[test]
    fn detects_https() {
        assert_eq!(urls("https://example.com"), vec!["https://example.com"]);
    }

    #[test]
    fn detects_http() {
        assert_eq!(urls("http://example.com"), vec!["http://example.com"]);
    }

    #[test]
    fn url_embedded_in_prose() {
        assert_eq!(
            urls("see https://example.com for details"),
            vec!["https://example.com"]
        );
    }

    // ── column span accuracy ──────────────────────────────────────────────────

    #[test]
    fn col_span_is_accurate() {
        let text = "go to https://x.com now";
        let s = make_state(text);
        let cols = s.cols;
        let spans = find_urls(&s, 1, cols);
        assert_eq!(spans.len(), 1);
        let (row, c0, c1, url) = &spans[0];
        assert_eq!(*row, 0);
        assert_eq!(*c0, 6); // "go to " is 6 chars
        assert_eq!(*c1, 6 + url.len());
        assert_eq!(url, "https://x.com");
    }

    // ── trailing punctuation stripping ────────────────────────────────────────

    #[test]
    fn strips_trailing_dot() {
        assert_eq!(urls("https://example.com."), vec!["https://example.com"]);
    }

    #[test]
    fn strips_trailing_comma() {
        assert_eq!(urls("https://example.com,"), vec!["https://example.com"]);
    }

    #[test]
    fn strips_trailing_paren() {
        assert_eq!(urls("(https://example.com)"), vec!["https://example.com"]);
    }

    #[test]
    fn strips_trailing_semicolon() {
        assert_eq!(urls("https://example.com;"), vec!["https://example.com"]);
    }

    #[test]
    fn strips_trailing_colon() {
        assert_eq!(urls("https://example.com:"), vec!["https://example.com"]);
    }

    // ── termination at delimiters ─────────────────────────────────────────────

    #[test]
    fn stops_at_space() {
        assert_eq!(urls("https://a.com https://b.com"), vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn stops_at_double_quote() {
        assert_eq!(urls("\"https://a.com\""), vec!["https://a.com"]);
    }

    #[test]
    fn stops_at_angle_bracket() {
        assert_eq!(urls("<https://a.com>"), vec!["https://a.com"]);
    }

    // ── multi-row scanning ────────────────────────────────────────────────────

    #[test]
    fn finds_url_on_second_row() {
        let cols = 20;
        let mut s = TerminalState::new(cols, 2);
        for (i, c) in "plain text".chars().enumerate() {
            s.grid[0][i].c = c;
        }
        for (i, c) in "https://row2.io".chars().enumerate() {
            s.grid[1][i].c = c;
        }
        let spans = find_urls(&s, 2, cols);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 1); // row 1
        assert_eq!(spans[0].3, "https://row2.io");
    }

    // ── non-matches ───────────────────────────────────────────────────────────

    #[test]
    fn no_urls_in_plain_text() {
        assert!(urls("just some text").is_empty());
    }

    #[test]
    fn partial_scheme_not_matched() {
        assert!(urls("http:/only-one-slash").is_empty());
    }

    #[test]
    fn empty_row_not_matched() {
        assert!(urls(" ").is_empty());
    }

    // ── path and query preservation ───────────────────────────────────────────

    #[test]
    fn preserves_path_and_query() {
        let u = "https://example.com/path?q=1&r=2#frag";
        assert_eq!(urls(u), vec![u]);
    }

    // ── base64_encode ─────────────────────────────────────────────────────────

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_well_known_vectors() {
        // RFC 4648 test vectors
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_hello() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn base64_all_zeros() {
        assert_eq!(base64_encode(&[0u8; 3]), "AAAA");
    }

    #[test]
    fn base64_output_length_always_multiple_of_four() {
        for len in 0..=9 {
            let data = vec![0xffu8; len];
            let encoded = base64_encode(&data);
            assert_eq!(encoded.len() % 4, 0, "length {len} → encoded len {}", encoded.len());
        }
    }

    #[test]
    fn base64_padding_one_equals() {
        // 2-byte input → 3 output chars + 1 '='
        let enc = base64_encode(b"ab");
        assert!(enc.ends_with('=') && !enc.ends_with("=="), "got: {enc}");
    }

    #[test]
    fn base64_padding_two_equals() {
        // 1-byte input → 2 output chars + "=="
        let enc = base64_encode(b"a");
        assert!(enc.ends_with("=="), "got: {enc}");
    }
}
