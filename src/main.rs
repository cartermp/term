mod completion;
mod config;
mod renderer;
mod terminal;

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowAttributes};

use completion::Engine;
use config::{WINDOW_HEIGHT, WINDOW_WIDTH};
use renderer::Renderer;
use terminal::Terminal;

#[derive(Debug)]
enum AppEvent {
    PtyData { tab_id: usize, data: Vec<u8> },
    PtyExit { tab_id: usize },
}

// ── Tab drag ──────────────────────────────────────────────────────────────────

struct TabDrag {
    from_idx: usize,
    start_x: f64,
    current_x: f64,
    active: bool, // threshold exceeded → real drag, not a click
}

// ── Selection ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Selection {
    start: (usize, usize), // (row, col) in viewport coordinates
    end: (usize, usize),
}

impl Selection {
    /// Returns (r0, c0, r1, c1) with r0≤r1 (and c0≤c1 when r0==r1).
    fn normalized(self) -> (usize, usize, usize, usize) {
        let (r0, c0) = self.start;
        let (r1, c1) = self.end;
        if r0 < r1 || (r0 == r1 && c0 <= c1) {
            (r0, c0, r1, c1)
        } else {
            (r1, c1, r0, c0)
        }
    }
}

// ── Tab ───────────────────────────────────────────────────────────────────────

struct Tab {
    id: usize,
    terminal: Terminal,
    pty_master: Box<dyn MasterPty>,
    pty_writer: Box<dyn Write + Send>,
    ghost_text: Option<String>,
}

impl Tab {
    fn write(&mut self, data: &[u8]) {
        let _ = self.pty_writer.write_all(data);
        let _ = self.pty_writer.flush();
    }

    fn title(&self) -> &str {
        let t = self.terminal.state.title.as_str();
        if t.is_empty() || t == "term" {
            // Fall back to shortened CWD
            let cwd = self.terminal.state.current_dir.as_str();
            let home = std::env::var("HOME").unwrap_or_default();
            if !cwd.is_empty() {
                // We return a &str into state, so store nothing — just use cwd directly
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

/// Scan the visible rows of `state` for http(s):// URLs.
/// Returns `(row, col_start, col_end_exclusive, url_string)` for each span found.
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

// ── App ───────────────────────────────────────────────────────────────────────

struct App {
    // Window / rendering
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    renderer: Option<Renderer>,

    // Tabs
    tabs: Vec<Tab>,
    active_tab: usize,
    next_id: usize,
    proxy: EventLoopProxy<AppEvent>,

    // Input
    modifiers: ModifiersState,
    cursor_pos: (f64, f64),

    // Tab drag
    tab_drag: Option<TabDrag>,

    // Selection
    sel_anchor: Option<(usize, usize)>,
    selection: Option<Selection>,
    selecting: bool,

    // Cursor blink
    cursor_visible: bool,
    last_blink: Instant,

    // Ghost text engine (shared across tabs)
    engine: Engine,
}

impl App {
    fn new(first_tab: Tab, proxy: EventLoopProxy<AppEvent>) -> Self {
        Self {
            window: None,
            context: None,
            surface: None,
            renderer: None,
            tabs: vec![first_tab],
            active_tab: 0,
            next_id: 1,
            proxy,
            modifiers: ModifiersState::empty(),
            cursor_pos: (-1.0, -1.0),
            tab_drag: None,
            sel_anchor: None,
            selection: None,
            selecting: false,
            cursor_visible: true,
            last_blink: Instant::now(),
            engine: Engine::new(),
        }
    }

    fn active(&self) -> &Tab {
        &self.tabs[self.active_tab]
    }
    fn active_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active_tab]
    }

    fn pty_write(&mut self, data: &[u8]) {
        self.active_mut().write(data);
    }

    fn reset_blink(&mut self) {
        self.cursor_visible = true;
        self.last_blink = Instant::now();
    }

    fn term_size(&self) -> (usize, usize) {
        if let (Some(w), Some(r)) = (&self.window, &self.renderer) {
            let sz = w.inner_size();
            let cols = (sz.width as usize / r.cell_width).max(1);
            let rows =
                ((sz.height as usize).saturating_sub(r.tab_bar_height) / r.cell_height).max(1);
            (cols, rows)
        } else {
            (80, 24)
        }
    }

    // ── Tab lifecycle ─────────────────────────────────────────────────────────

    fn open_tab(&mut self) {
        let (cols, rows) = self.term_size();
        let tab_id = self.next_id;
        self.next_id += 1;

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
                return;
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

        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy.send_event(AppEvent::PtyExit { tab_id });
                        break;
                    }
                    Ok(n) => {
                        let _ = proxy.send_event(AppEvent::PtyData {
                            tab_id,
                            data: buf[..n].to_vec(),
                        });
                    }
                }
            }
        });

        self.tabs.push(Tab {
            id: tab_id,
            terminal: Terminal::new(cols, rows),
            pty_master: pair.master,
            pty_writer: writer,
            ghost_text: None,
        });
        self.active_tab = self.tabs.len() - 1;
    }

    fn close_active_tab(&mut self, event_loop: &ActiveEventLoop) {
        if self.tabs.len() <= 1 {
            event_loop.exit();
            return;
        }
        self.tabs.remove(self.active_tab);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
    }

    fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            self.selection = None;
        }
    }

    /// Which tab slot does pixel x land in (clamped to 0..tabs.len()-1).
    fn drag_target_idx(&self, mx: f64) -> usize {
        let r = match self.renderer.as_ref() {
            Some(r) => r,
            None => return 0,
        };
        let bw = self
            .window
            .as_ref()
            .map(|w| w.inner_size().width as usize)
            .unwrap_or(0);
        let tabs_w = bw.saturating_sub(r.tab_bar_height);
        let n = self.tabs.len().max(1);
        let tab_w = tabs_w / n;
        let mx = mx.clamp(0.0, tabs_w.saturating_sub(1) as f64) as usize;
        (mx / tab_w).min(n - 1)
    }

    fn reorder_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active_tab = if self.active_tab == from {
            to
        } else if from < self.active_tab && to >= self.active_tab {
            self.active_tab - 1
        } else if from > self.active_tab && to <= self.active_tab {
            self.active_tab + 1
        } else {
            self.active_tab
        };
    }

    /// Convert a pixel coordinate to a terminal (row, col), or None if in the tab bar.
    fn pixel_to_cell(&self, mx: f64, my: f64) -> Option<(usize, usize)> {
        let r = self.renderer.as_ref()?;
        let tby = r.tab_bar_height;
        if mx < 0.0 || my < tby as f64 {
            return None;
        }
        let (cols, rows) = self.term_size();
        let row = ((my as usize).saturating_sub(tby)) / r.cell_height;
        let col = mx as usize / r.cell_width;
        Some((row.min(rows.saturating_sub(1)), col.min(cols.saturating_sub(1))))
    }

    /// Set the cursor icon based on whether Cmd is held and a URL is under the pointer.
    fn update_cursor_icon(&self) {
        let icon = if self.modifiers.super_key() {
            let is_url = self
                .pixel_to_cell(self.cursor_pos.0, self.cursor_pos.1)
                .map(|(row, col)| {
                    let (vis_cols, vis_rows) = self.term_size();
                    find_urls(&self.active().terminal.state, vis_rows, vis_cols)
                        .iter()
                        .any(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
                })
                .unwrap_or(false);
            if is_url { CursorIcon::Pointer } else { CursorIcon::Default }
        } else {
            CursorIcon::Default
        };
        if let Some(w) = &self.window {
            w.set_cursor(icon);
        }
    }

    /// Extract the text covered by the current selection.
    fn selection_text(&self) -> String {
        let sel = match self.selection {
            Some(s) => s,
            None => return String::new(),
        };
        let state = &self.active().terminal.state;
        let (r0, c0, r1, c1) = sel.normalized();
        let mut result = String::new();
        for row in r0..=r1 {
            let col_start = if row == r0 { c0 } else { 0 };
            let col_end = if row == r1 { c1 + 1 } else { state.cols };
            let mut line = String::new();
            for col in col_start..col_end.min(state.cols) {
                let cell = state.visual_cell(row, col);
                line.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
            if row > r0 {
                result.push('\n');
            }
            // Trim trailing spaces on wrapped lines, keep them on the last line
            if row < r1 {
                result.push_str(line.trim_end());
            } else {
                result.push_str(line.trim_end());
            }
        }
        result
    }

    fn prev_tab(&mut self) {
        self.active_tab = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
    }

    fn next_tab(&mut self) {
        self.active_tab = (self.active_tab + 1) % self.tabs.len();
    }

    // ── Ghost text ────────────────────────────────────────────────────────────

    fn update_ghost(&mut self) {
        let buf = self.active().terminal.state.input_buffer.clone();
        let cur = self.active().terminal.state.input_cursor;
        let ghost = self
            .engine
            .ghost(&buf, cur >= buf.len())
            .map(|s| s.to_string());
        self.active_mut().ghost_text = ghost;
    }

    fn sync_window_title(&self) {
        if let Some(w) = &self.window {
            w.set_title(self.active().title());
        }
    }

    fn accept_ghost(&mut self) {
        let g = self.active_mut().ghost_text.take();
        if let Some(g) = g {
            self.active_mut().write(g.as_bytes());
        }
    }

    // ── Key handling ──────────────────────────────────────────────────────────

    fn handle_key(&mut self, event: winit::event::KeyEvent, event_loop: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        self.reset_blink();

        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let sup = self.modifiers.super_key();

        // Clear selection on any keypress, except Cmd+C (which copies it) and
        // bare modifier key presses (Cmd/Shift/Alt/Ctrl alone must not clear the
        // selection before the chord is complete).
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
        if !is_cmd_c && !is_modifier_only && self.selection.is_some() {
            self.selection = None;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }

        // ── Cmd chords ────────────────────────────────────────────────────────
        if sup {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    self.pty_write(b"\x01");
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.pty_write(b"\x05");
                }
                Key::Named(NamedKey::ArrowUp) => {
                    let n = self.active().terminal.state.rows as i32;
                    self.active_mut().terminal.state.scroll_viewport(n);
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let n = self.active().terminal.state.rows as i32;
                    self.active_mut().terminal.state.scroll_viewport(-n);
                }
                Key::Named(NamedKey::Home) => {
                    self.active_mut().terminal.state.scroll_viewport(i32::MAX);
                }
                Key::Named(NamedKey::End) => {
                    self.active_mut().terminal.state.snap_to_bottom();
                }
                Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) => {
                    self.pty_write(b"\x15");
                }
                Key::Character(c) => match c.as_str() {
                    "c" => {
                        // Copy selection if active; otherwise do nothing.
                        if self.selection.is_some() {
                            let raw = self.selection_text();
                            if !raw.is_empty() {
                                copy_to_clipboard(&strip_tcat_gutter(&raw));
                            }
                            self.selection = None;
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                    "v" => {
                        let text = paste_from_clipboard();
                        if !text.is_empty() {
                            self.pty_write(text.as_bytes());
                        }
                    }
                    "t" => {
                        self.open_tab();
                        self.sync_window_title();
                    }
                    "w" => {
                        self.close_active_tab(event_loop);
                        self.sync_window_title();
                    }
                    "[" => {
                        self.prev_tab();
                        self.sync_window_title();
                    }
                    "]" => {
                        self.next_tab();
                        self.sync_window_title();
                    }
                    "k" => {
                        self.pty_write(b"\x0b");
                    }
                    "a" => {
                        self.pty_write(b"\x01");
                    }
                    "e" => {
                        self.pty_write(b"\x05");
                    }
                    s => {
                        if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10))
                            && d >= 1 {
                                self.switch_tab(d as usize - 1);
                                self.sync_window_title();
                            }
                    }
                },
                _ => {}
            }
            return;
        }

        // ── Alt+named keys ────────────────────────────────────────────────────
        if alt {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    self.pty_write(b"\x1bb");
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.pty_write(b"\x1bf");
                    return;
                }
                Key::Named(NamedKey::Backspace) => {
                    self.pty_write(b"\x1b\x7f");
                    return;
                }
                Key::Named(NamedKey::Delete) => {
                    self.pty_write(b"\x1bd");
                    return;
                }
                _ => {}
            }
        }

        // ── Printable text ────────────────────────────────────────────────────
        if !ctrl && !alt
            && let Some(text) = &event.text {
                self.active_mut().ghost_text = None;
                let bytes = text.as_str().as_bytes().to_vec();
                self.active_mut().write(&bytes);
                return;
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
                    let byte = (ch.to_ascii_lowercase() as u8)
                        .wrapping_sub(b'a')
                        .wrapping_add(1);
                    if byte > 0 && byte < 32 {
                        self.active_mut().ghost_text = None;
                        self.pty_write(&[byte]);
                    }
                }
            }
            Key::Character(c) if alt => {
                let mut v = vec![0x1b_u8];
                v.extend_from_slice(c.as_str().as_bytes());
                self.pty_write(&v);
            }
            _ => {}
        }
    }

    // ── Rendering ─────────────────────────────────────────────────────────────

    fn redraw(&mut self) {
        let window = match &self.window {
            Some(w) => w.clone(),
            None => return,
        };
        let size = window.inner_size();
        let (w, h) = (size.width as usize, size.height as usize);
        if w == 0 || h == 0 {
            return;
        }

        // Compute hover up-front: only needs cursor_pos + renderer.tab_bar_height + window width
        let hover = {
            let (mx, my) = self.cursor_pos;
            match &self.renderer {
                Some(r) if my >= 0.0 && my < r.tab_bar_height as f64 => {
                    let tabs_w = w.saturating_sub(r.tab_bar_height);
                    let n = self.tabs.len().max(1);
                    let tab_w = tabs_w / n;
                    if mx as usize >= tabs_w {
                        Some(self.tabs.len())
                    } else {
                        let idx = (mx as usize) / tab_w;
                        if idx < self.tabs.len() {
                            Some(idx)
                        } else {
                            None
                        }
                    }
                }
                _ => None,
            }
        };

        let ai = self.active_tab;
        let tab_titles: Vec<String> = self.tabs.iter().map(|t| t.title().to_string()).collect();
        // Capture borrows into self.tabs before taking &mut self.renderer / &mut self.surface.
        // SAFETY: renderer and surface are disjoint from tabs in memory.
        let state_ptr: *const _ = &self.tabs[ai].terminal.state;
        let ghost_owned: Option<String> = self.tabs[ai].ghost_text.clone();
        let show_cur = self.cursor_visible;
        let sel = self.selection.map(|s| s.normalized());
        // Compute URL underlines (only when Cmd is held so they don't render every frame)
        let url_underlines: Vec<(usize, usize, usize)> = if self.modifiers.super_key() {
            let (vis_cols, vis_rows) = self.term_size();
            find_urls(&self.tabs[ai].terminal.state, vis_rows, vis_cols)
                .into_iter()
                .map(|(r, c0, c1, _)| (r, c0, c1))
                .collect()
        } else {
            Vec::new()
        };
        // Extract drag state as owned values before any &mut borrows.
        let drag_vals = self.tab_drag.as_ref().map(|d| (d.active, d.from_idx, d.current_x));
        let drag_preview = drag_vals.and_then(|(active, from, cx)| {
            if active {
                Some((from, self.drag_target_idx(cx), cx))
            } else {
                None
            }
        });

        let surface = match &mut self.surface {
            Some(s) => s,
            None => return,
        };
        let renderer = match &mut self.renderer {
            Some(r) => r,
            None => return,
        };

        surface
            .resize(
                NonZeroU32::new(w as u32).unwrap(),
                NonZeroU32::new(h as u32).unwrap(),
            )
            .unwrap();

        let state = unsafe { &*state_ptr };
        let mut buf = surface.buffer_mut().unwrap();
        renderer.render(
            buf.as_mut(),
            w,
            h,
            state,
            show_cur,
            ghost_owned.as_deref(),
            &tab_titles,
            ai,
            hover,
            sel,
            drag_preview,
            &url_underlines,
        );
        buf.present().unwrap();
    }

    fn on_resize(&mut self, width: u32, height: u32) {
        let (Some(renderer), _) = (&self.renderer, ()) else {
            return;
        };
        let cw = renderer.cell_width;
        let ch = renderer.cell_height;
        let tby = renderer.tab_bar_height;
        let cols = (width as usize / cw).max(1);
        let rows = ((height as usize).saturating_sub(tby) / ch).max(1);
        let pty_size = PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: width as u16,
            pixel_height: height as u16,
        };
        for tab in &mut self.tabs {
            tab.terminal.resize(cols, rows);
            let _ = tab.pty_master.resize(pty_size);
        }
    }
}

// ── winit ApplicationHandler ──────────────────────────────────────────────────

impl ApplicationHandler<AppEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = WindowAttributes::default()
            .with_title("term")
            .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
        let window = Arc::new(event_loop.create_window(attrs).unwrap());

        let scale = window.scale_factor();
        let renderer = Renderer::new(scale);
        let size = window.inner_size();
        let (cols, rows) = {
            let cw = renderer.cell_width;
            let ch = renderer.cell_height;
            let tby = renderer.tab_bar_height;
            (
                (size.width as usize / cw).max(1),
                ((size.height as usize).saturating_sub(tby) / ch).max(1),
            )
        };
        let pty_size = PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: size.width as u16,
            pixel_height: size.height as u16,
        };
        for tab in &mut self.tabs {
            tab.terminal.resize(cols, rows);
            let _ = tab.pty_master.resize(pty_size);
        }

        let context = softbuffer::Context::new(window.clone()).unwrap();
        let surface = softbuffer::Surface::new(&context, window.clone()).unwrap();
        self.context = Some(context);
        self.surface = Some(surface);
        self.renderer = Some(renderer);
        self.window = Some(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: winit::window::WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::RedrawRequested => self.redraw(),

            WindowEvent::Resized(s) => {
                self.on_resize(s.width, s.height);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.renderer = Some(Renderer::new(scale_factor));
            }

            WindowEvent::ModifiersChanged(state) => {
                self.modifiers = state.state();
                self.update_cursor_icon();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_key(event, event_loop);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                self.update_cursor_icon();
                let in_bar = self
                    .renderer
                    .as_ref()
                    .map(|r| position.y < r.tab_bar_height as f64)
                    .unwrap_or(false);

                // Update tab drag
                if let Some(ref mut drag) = self.tab_drag {
                    drag.current_x = position.x;
                    if !drag.active && (position.x - drag.start_x).abs() > 8.0 {
                        drag.active = true;
                    }
                }
                let drag_active = self.tab_drag.as_ref().map(|d| d.active).unwrap_or(false);

                if self.selecting {
                    if let (Some(anchor), Some(cell)) =
                        (self.sel_anchor, self.pixel_to_cell(position.x, position.y))
                    {
                        self.selection = if cell != anchor {
                            Some(Selection { start: anchor, end: cell })
                        } else {
                            None
                        };
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                } else if drag_active || in_bar {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor_pos = (-1.0, -1.0);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                let (mx, my) = self.cursor_pos;
                let in_bar = self
                    .renderer
                    .as_ref()
                    .map(|r| my >= 0.0 && my < r.tab_bar_height as f64)
                    .unwrap_or(false);
                if in_bar {
                    let bw = self
                        .window
                        .as_ref()
                        .map(|w| w.inner_size().width as usize)
                        .unwrap_or(0);
                    let tab_bar_h =
                        self.renderer.as_ref().map(|r| r.tab_bar_height).unwrap_or(0);
                    let tabs_w = bw.saturating_sub(tab_bar_h);
                    let n = self.tabs.len().max(1);
                    let tab_w = tabs_w / n;
                    if mx as usize >= tabs_w {
                        self.open_tab();
                        self.sync_window_title();
                    } else {
                        let idx = (mx as usize) / tab_w;
                        if idx < self.tabs.len() {
                            self.switch_tab(idx);
                            self.sync_window_title();
                            // Arm drag detection — activates once the mouse moves far enough
                            self.tab_drag = Some(TabDrag {
                                from_idx: idx,
                                start_x: mx,
                                current_x: mx,
                                active: false,
                            });
                        }
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                } else {
                    // Terminal area: Cmd+click opens URL if one is under the cursor
                    let sup = self.modifiers.super_key();
                    let opened = if sup {
                        let clicked = self.pixel_to_cell(mx, my);
                        if let Some((row, col)) = clicked {
                            let (vis_cols, vis_rows) = self.term_size();
                            let url = find_urls(&self.active().terminal.state, vis_rows, vis_cols)
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
                        self.tab_drag = None;
                        self.selection = None;
                        self.sel_anchor = self.pixel_to_cell(mx, my);
                        self.selecting = true;
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: winit::event::MouseButton::Left,
                ..
            } => {
                // Finalize tab drag
                if let Some(drag) = self.tab_drag.take() {
                    if drag.active {
                        let to = self.drag_target_idx(drag.current_x);
                        self.reorder_tab(drag.from_idx, to);
                    }
                }
                self.selecting = false;
                if self.selection.is_none() {
                    self.sel_anchor = None;
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as i32,
                    MouseScrollDelta::PixelDelta(pos) => {
                        let ch = self
                            .renderer
                            .as_ref()
                            .map(|r| r.cell_height as f64)
                            .unwrap_or(20.0);
                        (pos.y / ch * 1.5) as i32
                    }
                };
                self.active_mut().terminal.state.scroll_viewport(lines);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        const PERIOD: Duration = Duration::from_millis(530);
        let now = Instant::now();
        if now.duration_since(self.last_blink) >= PERIOD {
            self.cursor_visible = !self.cursor_visible;
            self.last_blink = now;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.last_blink + PERIOD));
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::PtyData { tab_id, data } => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.terminal.state.snap_to_bottom();
                    tab.terminal.process(&data);
                    let responses: Vec<Vec<u8>> =
                        tab.terminal.state.pending_responses.drain(..).collect();
                    for r in responses {
                        let _ = tab.pty_writer.write_all(&r);
                        let _ = tab.pty_writer.flush();
                    }
                }
                // Update ghost text only for the active tab
                if self.tabs.get(self.active_tab).map(|t| t.id) == Some(tab_id) {
                    self.update_ghost();
                    self.sync_window_title();
                }
                self.reset_blink();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::PtyExit { tab_id } => {
                if let Some(pos) = self.tabs.iter().position(|t| t.id == tab_id) {
                    if self.tabs.len() == 1 {
                        event_loop.exit();
                    } else {
                        self.tabs.remove(pos);
                        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
            }
        }
    }
}

// ── Clipboard ─────────────────────────────────────────────────────────────────

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

/// Strip tcat's line-number gutter from copied text.
/// tcat renders each line as "  {N} │ {content}".  We find the U+2502
/// separator flanked by spaces, verify the prefix is digits+spaces, and
/// return only the content.
fn strip_tcat_gutter(text: &str) -> String {
    const SEP: char = '\u{2502}'; // │
    text.lines()
        .map(|line| {
            // Find │ with a space on each side
            let chars: Vec<char> = line.chars().collect();
            for i in 1..chars.len().saturating_sub(1) {
                if chars[i] != SEP || chars[i - 1] != ' ' || chars[i + 1] != ' ' {
                    continue;
                }
                // Prefix (0..i-1) must be spaces/digits with at least one digit
                let prefix = &chars[..i - 1];
                if prefix.iter().all(|&c| c == ' ' || c == '\0' || c.is_ascii_digit())
                    && prefix.iter().any(|c| c.is_ascii_digit())
                {
                    // Content starts at i+2 (after │ and trailing space)
                    return chars[i + 2..].iter().collect::<String>();
                }
            }
            line.to_string()
        })
        .collect::<Vec<String>>()
        .join("\n")
}

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
"#;

    let zshrc = format!(
        "ZDOTDIR='{home}'\n\
         [ -f '{home}/.zprofile' ] && source '{home}/.zprofile'\n\
         [ -f '{home}/.zshrc' ] && source '{home}/.zshrc'\n\
         {cat_fn}{zle_hooks}"
    );
    let _ = std::fs::write(zdotdir.join(".zshrc"), &zshrc);

    cmd.env("ZDOTDIR", &zdotdir);
    cmd.env("TERM_PROGRAM", "ghostty");
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    // Create first tab
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty failed");

    let mut cmd = CommandBuilder::new("zsh");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    setup_shell_env(&mut cmd);

    let _child = pair.slave.spawn_command(cmd).expect("spawn zsh failed");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let writer = pair.master.take_writer().expect("take writer");

    let proxy_reader = proxy.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = proxy_reader.send_event(AppEvent::PtyExit { tab_id: 0 });
                    break;
                }
                Ok(n) => {
                    let _ = proxy_reader.send_event(AppEvent::PtyData {
                        tab_id: 0,
                        data: buf[..n].to_vec(),
                    });
                }
            }
        }
    });

    let first_tab = Tab {
        id: 0,
        terminal: Terminal::new(80, 24),
        pty_master: pair.master,
        pty_writer: writer,
        ghost_text: None,
    };

    let mut app = App::new(first_tab, proxy);
    app.next_id = 1;
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::find_urls;
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
}
