mod command_suggest;
mod completion;
mod config;
mod platform;
mod renderer;
mod session;
mod terminal;
mod updater;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Window, WindowAttributes, WindowId};

use completion::Engine;
use config::{BackgroundAppearance, DEFAULT_BACKGROUND_APPEARANCE, WINDOW_HEIGHT, WINDOW_WIDTH};
use renderer::{PaneView, Renderer, RendererShared};
use terminal::Terminal;
use updater::UpdateCheck;

#[derive(Debug)]
enum AppEvent {
    PtyReady { pane_id: u64 },
    PtyExit { pane_id: u64 },
    NewTab,
    CheckForUpdates,
    UpdateCheckFinished(Result<UpdateCheck, String>),
    ShowBackgroundAppearancePanel,
    BackgroundAppearanceChanged(BackgroundAppearance),
}

const PTY_OUTPUT_MAX_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct PtyOutputState {
    bytes: Vec<u8>,
    wake_pending: bool,
    closed: bool,
}

#[derive(Default)]
struct PtyOutput {
    state: Mutex<PtyOutputState>,
    space_available: Condvar,
}

impl PtyOutput {
    /// Append bytes from the blocking PTY reader. The bounded buffer applies
    /// backpressure instead of allowing user events to grow without limit.
    /// Returns true when the reader must wake the event loop.
    fn push(&self, data: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        while !state.closed && state.bytes.len() + data.len() > PTY_OUTPUT_MAX_BYTES {
            state = self.space_available.wait(state).unwrap();
        }
        if state.closed {
            return false;
        }
        state.bytes.extend_from_slice(data);
        if state.wake_pending {
            false
        } else {
            state.wake_pending = true;
            true
        }
    }

    /// Swap all pending bytes into a reusable main-thread scratch buffer.
    fn drain_into(&self, scratch: &mut Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        scratch.clear();
        std::mem::swap(scratch, &mut state.bytes);
        state.wake_pending = false;
        self.space_available.notify_all();
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.bytes.clear();
        state.wake_pending = false;
        self.space_available.notify_all();
    }
}

// ── Split direction ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum SplitDir {
    Vertical,
    Horizontal,
}

fn terminal_window_attributes(saved: Option<&session::SavedWindow>) -> WindowAttributes {
    let mut attrs = WindowAttributes::default()
        .with_title("term")
        .with_transparent(true)
        .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    if let Some(saved) = saved {
        attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(
            saved.inner_w.max(1),
            saved.inner_h.max(1),
        ));
        if let (Some(x), Some(y)) = (saved.outer_x, saved.outer_y) {
            attrs = attrs.with_position(winit::dpi::PhysicalPosition::new(x, y));
        }
    }
    attrs
}

// ── Selection ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    child_killer: Box<dyn ChildKiller + Send + Sync>,
    pty_output: Arc<PtyOutput>,
    pty_scratch: Vec<u8>,
    ghost_text: Option<String>,
    engine: Arc<Engine>,
    selection: Option<Selection>,
    /// Cached URL scan result. Keyed on terminal generation, viewport offset,
    /// and visible dimensions so scrolling cannot reuse links from another
    /// section of scrollback.
    url_cache: Vec<(usize, usize, usize, String)>,
    url_cache_gen: u64,
    url_cache_viewport_offset: usize,
    url_cache_dims: (usize, usize),
}

impl Pane {
    /// Return URL spans for the current view, using the cached result when
    /// the terminal generation and viewport dimensions haven't changed.
    fn urls_cached(
        &mut self,
        vis_rows: usize,
        vis_cols: usize,
    ) -> &[(usize, usize, usize, String)] {
        urls_with_cache(
            &mut self.url_cache,
            &mut self.url_cache_gen,
            &mut self.url_cache_viewport_offset,
            &mut self.url_cache_dims,
            &self.terminal.state,
            vis_rows,
            vis_cols,
        )
    }
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

impl Drop for Pane {
    fn drop(&mut self) {
        self.pty_output.close();
        #[cfg(unix)]
        if let Some(process_group) = self.pty_master.process_group_leader() {
            // The foreground job may be in a different process group from the
            // shell. Hang it up before terminating the shell itself.
            unsafe {
                libc::kill(-process_group, libc::SIGHUP);
            }
        }
        let _ = self.child_killer.kill();
    }
}

// ── URL detection ─────────────────────────────────────────────────────────────

/// Map a physical key code to its base ASCII byte (unshifted, no modifiers).
/// Used to recover the intended letter when Alt/Option produces a Unicode character (macOS).
fn physical_key_to_ascii(kc: KeyCode) -> Option<u8> {
    match kc {
        KeyCode::KeyA => Some(b'a'),
        KeyCode::KeyB => Some(b'b'),
        KeyCode::KeyC => Some(b'c'),
        KeyCode::KeyD => Some(b'd'),
        KeyCode::KeyE => Some(b'e'),
        KeyCode::KeyF => Some(b'f'),
        KeyCode::KeyG => Some(b'g'),
        KeyCode::KeyH => Some(b'h'),
        KeyCode::KeyI => Some(b'i'),
        KeyCode::KeyJ => Some(b'j'),
        KeyCode::KeyK => Some(b'k'),
        KeyCode::KeyL => Some(b'l'),
        KeyCode::KeyM => Some(b'm'),
        KeyCode::KeyN => Some(b'n'),
        KeyCode::KeyO => Some(b'o'),
        KeyCode::KeyP => Some(b'p'),
        KeyCode::KeyQ => Some(b'q'),
        KeyCode::KeyR => Some(b'r'),
        KeyCode::KeyS => Some(b's'),
        KeyCode::KeyT => Some(b't'),
        KeyCode::KeyU => Some(b'u'),
        KeyCode::KeyV => Some(b'v'),
        KeyCode::KeyW => Some(b'w'),
        KeyCode::KeyX => Some(b'x'),
        KeyCode::KeyY => Some(b'y'),
        KeyCode::KeyZ => Some(b'z'),
        KeyCode::Digit0 => Some(b'0'),
        KeyCode::Digit1 => Some(b'1'),
        KeyCode::Digit2 => Some(b'2'),
        KeyCode::Digit3 => Some(b'3'),
        KeyCode::Digit4 => Some(b'4'),
        KeyCode::Digit5 => Some(b'5'),
        KeyCode::Digit6 => Some(b'6'),
        KeyCode::Digit7 => Some(b'7'),
        KeyCode::Digit8 => Some(b'8'),
        KeyCode::Digit9 => Some(b'9'),
        KeyCode::Minus => Some(b'-'),
        KeyCode::Equal => Some(b'='),
        KeyCode::BracketLeft => Some(b'['),
        KeyCode::BracketRight => Some(b']'),
        KeyCode::Backslash => Some(b'\\'),
        KeyCode::Semicolon => Some(b';'),
        KeyCode::Quote => Some(b'\''),
        KeyCode::Backquote => Some(b'`'),
        KeyCode::Comma => Some(b','),
        KeyCode::Period => Some(b'.'),
        KeyCode::Slash => Some(b'/'),
        _ => None,
    }
}

/// Core URL-cache logic extracted as a free function for testability.
///
/// Checks whether the cache key matches the current terminal generation,
/// viewport position, and size. On a miss the cache is repopulated via
/// `find_urls`; on a hit it is returned as-is.
fn urls_with_cache<'a>(
    cache: &'a mut Vec<(usize, usize, usize, String)>,
    cache_gen: &mut u64,
    cache_viewport_offset: &mut usize,
    cache_dims: &mut (usize, usize),
    state: &terminal::TerminalState,
    vis_rows: usize,
    vis_cols: usize,
) -> &'a [(usize, usize, usize, String)] {
    let cur_gen = state.generation;
    let cur_viewport_offset = state.viewport_offset;
    if *cache_gen != cur_gen
        || *cache_viewport_offset != cur_viewport_offset
        || *cache_dims != (vis_rows, vis_cols)
    {
        *cache = find_urls(state, vis_rows, vis_cols);
        *cache_gen = cur_gen;
        *cache_viewport_offset = cur_viewport_offset;
        *cache_dims = (vis_rows, vis_cols);
    }
    cache
}

fn find_urls(
    state: &terminal::TerminalState,
    vis_rows: usize,
    vis_cols: usize,
) -> Vec<(usize, usize, usize, String)> {
    let mut out = Vec::new();
    for row in 0..vis_rows {
        let cells: Vec<char> = (0..vis_cols)
            .map(|col| state.visual_cell(row, col).c)
            .collect();
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

fn should_open_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn wrap_bracketed_paste(payload: &[u8], bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return payload.to_vec();
    }
    let mut out = Vec::with_capacity(payload.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[derive(Debug, PartialEq, Eq)]
enum MouseWheelAction {
    None,
    LocalViewport(i32),
    PtyWrites(Vec<Vec<u8>>),
}

fn mouse_wheel_lines(delta: &MouseScrollDelta, cell_height: usize, scroll_frac: &mut f64) -> i32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => {
            *scroll_frac = 0.0;
            (y * 4.5) as i32
        }
        MouseScrollDelta::PixelDelta(pos) => {
            let ch = cell_height.max(1) as f64;
            *scroll_frac += pos.y / ch * 3.0;
            let whole = scroll_frac.trunc() as i32;
            *scroll_frac -= whole as f64;
            whole
        }
    }
}

fn mouse_wheel_action(
    state: &terminal::TerminalState,
    lines: i32,
    cell: Option<(usize, usize)>,
) -> MouseWheelAction {
    if lines == 0 {
        return MouseWheelAction::None;
    }

    if state.mouse_tracking {
        let (row, col) = match cell {
            Some(cell) => cell,
            None => return MouseWheelAction::None,
        };
        let count = lines.unsigned_abs() as usize;
        let button = if lines > 0 { 64u32 } else { 65u32 };
        let col1 = col + 1;
        let row1 = row + 1;
        let seq = if state.mouse_sgr {
            format!("\x1b[<{button};{col1};{row1}M").into_bytes()
        } else {
            let cb = (32 + button).min(255) as u8;
            let cx = (32 + col1).min(255) as u8;
            let cy = (32 + row1).min(255) as u8;
            vec![0x1b, b'[', b'M', cb, cx, cy]
        };
        return MouseWheelAction::PtyWrites(std::iter::repeat_n(seq, count).collect());
    }

    if state.is_alt_screen() {
        let seq = if lines > 0 {
            b"\x1bOA".to_vec()
        } else {
            b"\x1bOB".to_vec()
        };
        return MouseWheelAction::PtyWrites(
            std::iter::repeat_n(seq, lines.unsigned_abs() as usize).collect(),
        );
    }

    MouseWheelAction::LocalViewport(lines)
}

fn update_cursor_blink(
    cursor_visible: &mut bool,
    last_blink: &mut Instant,
    now: Instant,
    period: Duration,
) -> bool {
    if now.duration_since(*last_blink) < period {
        return false;
    }
    *cursor_visible = !*cursor_visible;
    *last_blink = now;
    true
}

fn osc52_response(payload: &str) -> Vec<u8> {
    format!("\x1b]52;c;{payload}\x07").into_bytes()
}

fn drain_terminal_host_responses(
    state: &mut terminal::TerminalState,
    clipboard_payload: &str,
) -> Vec<Vec<u8>> {
    let mut responses: Vec<Vec<u8>> = state.pending_responses.drain(..).collect();
    if state.osc_52_query {
        state.osc_52_query = false;
        responses.push(osc52_response(clipboard_payload));
    }
    responses
}

// ── ns_view helper ────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn ns_view_ptr(window: &winit::window::Window) -> Option<*mut std::ffi::c_void> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr()),
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

fn choose_surface_alpha_mode(modes: &[wgpu::CompositeAlphaMode]) -> wgpu::CompositeAlphaMode {
    [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::PostMultiplied,
        wgpu::CompositeAlphaMode::Auto,
    ]
    .into_iter()
    .find(|mode| modes.contains(mode))
    .or_else(|| modes.first().copied())
    .unwrap_or(wgpu::CompositeAlphaMode::Auto)
}

fn should_bootstrap_resize(current: (usize, usize), desired: (usize, usize)) -> bool {
    (desired.0 > current.0 || desired.1 > current.1) && desired.0 > 1 && desired.1 > 1
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

    // Accumulated sub-line scroll from smooth touchpad events
    scroll_frac: f64,

    // Cursor blink
    cursor_visible: bool,
    last_blink: Instant,
}

impl TerminalWindow {
    fn pane_rects(&self) -> Vec<(f32, f32, f32, f32)> {
        let sz = self.window.inner_size();
        let w = sz.width as f32;
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
        let w = sz.width as f32;
        let h = sz.height as f32;
        match (self.panes.len(), &self.split) {
            (2, Some(SplitDir::Vertical)) => {
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
        let idx = self.active_pane.min(self.panes.len().saturating_sub(1));
        &self.panes[idx]
    }

    fn active_mut(&mut self) -> &mut Pane {
        let idx = self.active_pane.min(self.panes.len().saturating_sub(1));
        &mut self.panes[idx]
    }

    fn set_active_pane(&mut self, index: usize) {
        let index = index.min(self.panes.len().saturating_sub(1));
        if index == self.active_pane {
            return;
        }
        self.active_pane = index;
        self.update_ghost();
        self.sync_title();
        self.update_cursor_icon();
        self.window.request_redraw();
    }

    fn pty_write(&mut self, data: &[u8]) {
        let idx = self.active_pane.min(self.panes.len().saturating_sub(1));
        self.panes[idx].write(data);
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
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = ns_view_ptr(&self.window) {
            let (idx, count) = platform::tab_index_and_count(ns_view);
            let labeled = if count > 1 {
                format!("{idx} · {title}")
            } else {
                title.to_string()
            };
            self.window.set_title(&labeled);
            platform::set_tab_title(ns_view, &labeled);
            return;
        }
        self.window.set_title(title);
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
        selection_text_for_range(state, (r0, c0, r1, c1))
    }

    /// Convert pixel coordinate to (pane_idx, row, col). Returns None if out of bounds.
    fn pixel_to_pane_cell(&self, mx: f64, my: f64) -> Option<(usize, usize, usize)> {
        let rects = self.pane_rects();
        let cw = self.renderer.cell_width as f64;
        let ch = self.renderer.cell_height as f64;
        for (i, &(ox, oy, pw, ph)) in rects.iter().enumerate() {
            if mx >= ox as f64 && mx < (ox + pw) as f64 && my >= oy as f64 && my < (oy + ph) as f64
            {
                let col = ((mx - ox as f64) / cw) as usize;
                let row = ((my - oy as f64) / ch) as usize;
                let cols = (pw as usize / self.renderer.cell_width).max(1);
                let rows = (ph as usize / self.renderer.cell_height).max(1);
                return Some((
                    i,
                    row.min(rows.saturating_sub(1)),
                    col.min(cols.saturating_sub(1)),
                ));
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

    fn update_cursor_icon(&mut self) {
        let icon = if self.modifiers.super_key() {
            let is_clickable = self
                .pixel_to_pane_cell(self.cursor_pos.0, self.cursor_pos.1)
                .map(|(pi, row, col)| {
                    // OSC 8 hyperlink first — apps (e.g. copilot-cli, gh)
                    // opt into these explicitly, so treat the whole cell run
                    // as clickable regardless of visible text.
                    let osc8 = self.panes[pi].terminal.state.visual_cell(row, col).link_id != 0;
                    if osc8 {
                        return true;
                    }
                    // Fall back to the auto-detected http/https URL scanner.
                    let rects = self.pane_rects();
                    let (_, _, pw, ph) = rects.get(pi).copied().unwrap_or((0., 0., 0., 0.));
                    let vis_cols = (pw as usize / self.renderer.cell_width).max(1);
                    let vis_rows = (ph as usize / self.renderer.cell_height).max(1);
                    self.panes[pi]
                        .urls_cached(vis_rows, vis_cols)
                        .iter()
                        .any(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
                })
                .unwrap_or(false);
            if is_clickable {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            }
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
                    let payload = wrap_bracketed_paste(path_str.as_bytes(), bracketed);
                    self.pty_write(&payload);
                }
                return;
            }
            if let Some(text) = platform::clipboard_text()
                && !text.is_empty()
            {
                let bracketed = self.active().terminal.state.bracketed_paste;
                let payload = wrap_bracketed_paste(text.as_bytes(), bracketed);
                self.pty_write(&payload);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let text = paste_from_clipboard();
            if !text.is_empty() {
                let bracketed = self.active().terminal.state.bracketed_paste;
                let payload = wrap_bracketed_paste(text.as_bytes(), bracketed);
                self.pty_write(&payload);
            }
        }
    }

    fn maybe_bootstrap_resize(&mut self) {
        let size = self.window.inner_size();
        if size.width <= self.surface_config.width && size.height <= self.surface_config.height {
            return;
        }

        let rects = self.pane_rects();
        let cw = self.renderer.cell_width.max(1);
        let ch = self.renderer.cell_height.max(1);
        let should_resize = self.panes.iter().enumerate().any(|(i, pane)| {
            let (_, _, pw, ph) =
                rects
                    .get(i)
                    .copied()
                    .unwrap_or((0.0, 0.0, size.width as f32, size.height as f32));
            let desired = ((pw as usize / cw).max(1), (ph as usize / ch).max(1));
            let current = (pane.terminal.state.cols, pane.terminal.state.rows);
            should_bootstrap_resize(current, desired)
        });

        if should_resize {
            self.on_resize(size.width, size.height);
        }
    }

    fn redraw(&mut self) {
        self.maybe_bootstrap_resize();
        let window = self.window.clone();
        let sz = window.inner_size();
        let (w, h) = (sz.width, sz.height);
        if w == 0 || h == 0 {
            return;
        }

        let rects = self.pane_rects();
        let dividers = self.divider_rects();
        let cw = self.renderer.cell_width;
        let ch = self.renderer.cell_height;
        let mods_super = self.modifiers.super_key();

        // Build URL underlines per pane (uses cached scan when content unchanged)
        let url_ulines: Vec<Vec<(usize, usize, usize)>> = {
            let mut result = Vec::with_capacity(self.panes.len());
            for i in 0..self.panes.len() {
                if i < rects.len() && mods_super {
                    let (_, _, pw, ph) = rects[i];
                    let vis_rows = (ph as usize / ch).max(1);
                    let vis_cols = (pw as usize / cw).max(1);
                    let spans = self.panes[i]
                        .urls_cached(vis_rows, vis_cols)
                        .iter()
                        .map(|(r, c0, c1, _)| (*r, *c0, *c1))
                        .collect();
                    result.push(spans);
                } else {
                    result.push(vec![]);
                }
            }
            result
        };

        // Build PaneView slice
        let mut pane_views: Vec<PaneView<'_>> = Vec::new();
        for (i, pane) in self.panes.iter().enumerate() {
            if i >= rects.len() {
                break;
            }
            let (ox, oy, pw, ph) = rects[i];
            let sel = pane
                .selection
                .map(|s| s.to_viewport(pane.terminal.state.viewport_offset));
            pane_views.push(PaneView {
                x: ox,
                y: oy,
                width: pw,
                height: ph,
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
                self.surface
                    .configure(&self.renderer.device, &self.surface_config);
                return;
            }
            Err(e) => {
                eprintln!("surface error: {e}");
                return;
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.render(&view, w, h, &pane_views, &dividers);
        output.present();
    }

    fn on_resize(&mut self, width: u32, height: u32) {
        self.surface_config.width = width.max(1);
        self.surface_config.height = height.max(1);
        self.surface
            .configure(&self.renderer.device, &self.surface_config);

        let rects = self.pane_rects();
        let cw = self.renderer.cell_width;
        let ch = self.renderer.cell_height;
        for (i, pane) in self.panes.iter_mut().enumerate() {
            let (_, _, pw, ph) = if i < rects.len() {
                rects[i]
            } else {
                (0., 0., width as f32, height as f32)
            };
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
        let is_cmd_c = sup && matches!(&event.logical_key, Key::Character(c) if c.as_str() == "c");
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
        if !is_cmd_c && !is_modifier_only && self.panes[self.active_pane].selection.is_some() {
            self.panes[self.active_pane].selection = None;
            self.window.request_redraw();
        }

        // ── Cmd chords ────────────────────────────────────────────────────────
        if sup {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => {
                    if alt {
                        // Cmd+Opt+Left: previous pane
                        if self.panes.len() > 1 {
                            self.set_active_pane(self.active_pane.saturating_sub(1));
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
                            self.set_active_pane(self.active_pane + 1);
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
                            return KeyAction::SelectTab(d as usize);
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
        if !ctrl
            && !alt
            && let Some(text) = &event.text
        {
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
                    let app = self.active().terminal.state.cursor_keys_app_mode;
                    self.pty_write(if app { b"\x1bOC" } else { b"\x1b[C" });
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.active_mut().ghost_text = None;
                let app = self.active().terminal.state.cursor_keys_app_mode;
                self.pty_write(if app { b"\x1bOA" } else { b"\x1b[A" });
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.active_mut().ghost_text = None;
                let app = self.active().terminal.state.cursor_keys_app_mode;
                self.pty_write(if app { b"\x1bOB" } else { b"\x1b[B" });
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.active_mut().ghost_text = None;
                let app = self.active().terminal.state.cursor_keys_app_mode;
                self.pty_write(if app { b"\x1bOD" } else { b"\x1b[D" });
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
                if let PhysicalKey::Code(kc) = event.physical_key
                    && let Some(ascii) = physical_key_to_ascii(kc)
                {
                    self.pty_write(&[0x1b, ascii]);
                }
            }
            _ => {}
        }

        KeyAction::None
    }
}

fn selection_text_for_range(
    state: &terminal::TerminalState,
    (r0, c0, r1, c1): (usize, usize, usize, usize),
) -> String {
    let mut result = String::new();
    for row in r0..=r1 {
        let col_start = if row == r0 { c0 } else { 0 };
        let col_end = if row == r1 { c1 + 1 } else { state.cols };
        let mut line = String::new();
        for col in col_start..col_end.min(state.cols) {
            let cell = state.visual_cell(row, col);
            if cell.c != '\0' {
                line.push(cell.c);
                for &combining in cell.combining_chars() {
                    line.push(combining);
                }
            }
        }
        if row > r0 && !state.visual_row_wrapped(row - 1) {
            result.push('\n');
        }
        if row < r1 && state.visual_row_wrapped(row) {
            result.push_str(&line);
        } else {
            result.push_str(line.trim_end());
        }
    }
    result
}

// ── KeyAction ─────────────────────────────────────────────────────────────────

enum KeyAction {
    None,
    OpenTab,
    ClosePaneOrWindow,
    PrevTab,
    NextTab,
    SelectTab(usize),
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
    background: BackgroundAppearance,
    checking_for_updates: bool,
    /// Ghost-text completion engine, shared across every pane. Loaded once at
    /// startup — new commands in the current session won't be reflected until
    /// `term` is restarted (zsh only flushes history on shell exit anyway).
    engine: Arc<Engine>,
    /// Heavy GPU/font resources, shared across every `Renderer` at the same
    /// DPI scale. Keyed by `scale_factor.to_bits()` — most users only have
    /// one entry; mixed-DPI setups grow to one entry per unique scale.
    renderer_shareds: HashMap<u64, Arc<RendererShared>>,
    /// Session restore payload — set from `--restore-session` at process
    /// start, consumed once by `resumed()` to recreate the saved tabs.
    pending_restore: Option<session::SavedSession>,
    /// Most recently focused native window, used to preserve the active tab
    /// across update-driven session restores.
    focused_window: Option<WindowId>,
    /// Last successful session-file write. Used to throttle the periodic
    /// save inside `about_to_wait()`.
    last_session_save: Instant,
}

impl App {
    fn new(
        proxy: EventLoopProxy<AppEvent>,
        pending_restore: Option<session::SavedSession>,
    ) -> Self {
        Self {
            wgpu: None,
            windows: HashMap::new(),
            pane_to_window: HashMap::new(),
            next_pane_id: 0,
            proxy,
            tabbing_id: format!("term-{}", std::process::id()),
            background: DEFAULT_BACKGROUND_APPEARANCE,
            checking_for_updates: false,
            engine: Arc::new(Engine::new()),
            renderer_shareds: HashMap::new(),
            pending_restore,
            focused_window: None,
            last_session_save: Instant::now(),
        }
    }

    /// Snapshot the current window/tab layout for persistence. Per-pane CWD
    /// is read from `terminal.state.current_dir` (kept up to date by OSC 7);
    /// title from `Pane::title()` (which falls back to CWD).
    fn snapshot(&self) -> session::SavedSession {
        let mut windows = Vec::with_capacity(self.windows.len());
        // HashMap order is non-deterministic. On macOS, preserve the native
        // tab-bar order so restore recreates tabs in the same sequence.
        let mut ids: Vec<WindowId> = self.windows.keys().copied().collect();
        #[cfg(target_os = "macos")]
        ids.sort_by_key(|id| {
            let tab_index = self
                .windows
                .get(id)
                .and_then(|tw| ns_view_ptr(&tw.window))
                .map(|view| platform::tab_index_and_count(view).0)
                .unwrap_or(usize::MAX);
            (tab_index, format!("{id:?}"))
        });
        #[cfg(not(target_os = "macos"))]
        ids.sort_by_key(|id| format!("{id:?}"));
        for id in &ids {
            let tw = match self.windows.get(id) {
                Some(w) => w,
                None => continue,
            };
            let inner = tw.window.inner_size();
            let outer = tw.window.outer_position().ok();
            windows.push(session::SavedWindow {
                outer_x: outer.map(|p| p.x),
                outer_y: outer.map(|p| p.y),
                inner_w: inner.width,
                inner_h: inner.height,
                split: tw.split.map(|d| match d {
                    SplitDir::Vertical => session::SavedSplit::Vertical,
                    SplitDir::Horizontal => session::SavedSplit::Horizontal,
                }),
                active_pane: tw.active_pane,
                panes: tw
                    .panes
                    .iter()
                    .map(|p| session::SavedPane {
                        cwd: p.terminal.state.current_dir.clone(),
                        title: p.title().to_string(),
                    })
                    .collect(),
            });
        }
        session::SavedSession {
            schema: session::SCHEMA_VERSION,
            term_version: env!("CARGO_PKG_VERSION").to_string(),
            saved_at_ms: session::now_ms(),
            focused_window: self
                .focused_window
                .and_then(|focused| ids.iter().position(|id| *id == focused)),
            windows,
        }
    }

    /// Persist `snapshot()` to disk. Failures are logged and otherwise
    /// ignored — losing a session save shouldn't take the app down.
    fn save_session_now(&mut self) {
        let snap = self.snapshot();
        if let Err(e) = session::save(&snap) {
            eprintln!("term: failed to save session: {e}");
        }
        self.last_session_save = Instant::now();
    }

    /// Return a shared renderer for `scale_factor`, building one if this is
    /// the first window at that scale.
    fn shared_renderer_for(&mut self, scale_factor: f64) -> Arc<RendererShared> {
        let key = scale_factor.to_bits();
        if let Some(shared) = self.renderer_shareds.get(&key) {
            return Arc::clone(shared);
        }
        let wgpu = self
            .wgpu
            .as_ref()
            .expect("wgpu must be initialised before constructing a renderer");
        let shared = Arc::new(RendererShared::new(
            wgpu.device.clone(),
            wgpu.queue.clone(),
            wgpu.surface_format,
            scale_factor,
        ));
        self.renderer_shareds.insert(key, Arc::clone(&shared));
        shared
    }

    fn apply_background_appearance(&mut self, background: BackgroundAppearance) {
        self.background = background;
        for tw in self.windows.values_mut() {
            tw.renderer.set_background(background);
            tw.window.request_redraw();
        }
    }

    fn alloc_pane_id(&mut self) -> u64 {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        id
    }

    fn begin_update_check(&mut self) {
        if self.checking_for_updates {
            return;
        }
        self.checking_for_updates = true;
        #[cfg(target_os = "macos")]
        platform::set_update_menu_checking(true);

        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let _ = proxy.send_event(AppEvent::UpdateCheckFinished(updater::check_for_updates()));
        });
    }

    fn finish_update_check(&mut self, result: Result<UpdateCheck, String>) {
        self.checking_for_updates = false;
        #[cfg(target_os = "macos")]
        platform::set_update_menu_checking(false);

        #[cfg(target_os = "macos")]
        match result {
            Ok(UpdateCheck::NoUpdateNeeded {
                current_version,
                latest_version,
                comparison,
            }) => {
                let current = updater::display_version(&current_version);
                let latest = updater::display_version(&latest_version);
                let message = if comparison.is_gt() {
                    format!("{current} is newer than the latest GitHub release ({latest}).")
                } else {
                    format!("Term {current} is already up to date.")
                };
                platform::show_info_alert("You're Up to Date", &message);
            }
            Ok(UpdateCheck::UpdateAvailable {
                current_version,
                release,
            }) => {
                let current = updater::display_version(&current_version);
                let latest = updater::display_version(&release.version);
                if platform::confirm_update_install(&current, &latest) {
                    match updater::spawn_background_update(&release) {
                        Ok(()) => platform::show_info_alert(
                            "Updating Term",
                            &format!(
                                "Downloading {latest} now. Term will relaunch when the update is ready."
                            ),
                        ),
                        Err(err) => platform::show_error_alert("Update Failed", &err),
                    }
                }
            }
            Err(err) => platform::show_error_alert("Update Failed", &err),
        }
    }

    fn create_pane_in(
        &mut self,
        window_id: WindowId,
        proxy: &EventLoopProxy<AppEvent>,
        cols: usize,
        rows: usize,
        cwd: Option<&str>,
    ) -> Option<Pane> {
        let pane_id = self.alloc_pane_id();

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

        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("pty clone reader: {e}");
                return None;
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("pty take writer: {e}");
                return None;
            }
        };

        let mut cmd = CommandBuilder::new("zsh");
        cmd.arg("-l");
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Honour a saved CWD when present and the directory still exists.
        // CommandBuilder defaults to the parent process's CWD otherwise.
        if let Some(dir) = cwd
            && !dir.is_empty()
            && std::path::Path::new(dir).is_dir()
        {
            cmd.cwd(dir);
        }
        setup_shell_env(&mut cmd);

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => {
                eprintln!("spawn zsh: {e}");
                return None;
            }
        };
        let child_killer = child.clone_killer();
        drop(pair.slave);

        self.pane_to_window.insert(pane_id, window_id);
        let pty_output = Arc::new(PtyOutput::default());
        let reader_output = Arc::clone(&pty_output);
        let reader_proxy = proxy.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if reader_output.push(&buf[..n])
                            && reader_proxy
                                .send_event(AppEvent::PtyReady { pane_id })
                                .is_err()
                        {
                            reader_output.close();
                            break;
                        }
                    }
                }
            }
            // Reap only after the PTY reaches EOF. This keeps all PtyReady
            // notifications ordered before PtyExit so final shell output
            // cannot be discarded during pane removal.
            let _ = child.wait();
            let _ = reader_proxy.send_event(AppEvent::PtyExit { pane_id });
        });

        Some(Pane {
            id: pane_id,
            terminal: Terminal::new(cols, rows),
            pty_master: pair.master,
            pty_writer: writer,
            child_killer,
            pty_output,
            pty_scratch: Vec::new(),
            ghost_text: None,
            engine: Arc::clone(&self.engine),
            selection: None,
            url_cache: Vec::new(),
            url_cache_gen: u64::MAX, // sentinel: force first-use recompute
            url_cache_viewport_offset: usize::MAX,
            url_cache_dims: (0, 0),
        })
    }

    fn open_tab(&mut self, event_loop: &ActiveEventLoop) {
        let _ = self.open_tab_with_saved(event_loop, None);
    }

    fn open_tab_with_saved(
        &mut self,
        event_loop: &ActiveEventLoop,
        saved: Option<&session::SavedWindow>,
    ) -> Option<(WindowId, bool)> {
        let wgpu = match &self.wgpu {
            Some(w) => w,
            None => return None,
        };

        // Find an existing window to group with
        let existing_ns_view: Option<*mut std::ffi::c_void> = {
            #[cfg(target_os = "macos")]
            {
                self.windows
                    .values()
                    .next()
                    .and_then(|tw| ns_view_ptr(&tw.window))
            }
            #[cfg(not(target_os = "macos"))]
            {
                None
            }
        };

        let mut attrs = terminal_window_attributes(saved);

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
            platform::configure_window_background(ns_view);
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
        let surface: wgpu::Surface<'static> = wgpu
            .instance
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
            alpha_mode: choose_surface_alpha_mode(&caps.alpha_modes),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&wgpu.device, &surface_config);

        let scale = window.scale_factor();
        let shared = self.shared_renderer_for(scale);
        let renderer = Renderer::new(shared, self.background);

        let (cols, rows) = {
            let cw = renderer.cell_width;
            let ch = renderer.cell_height;
            (
                (size.width as usize / cw).max(1),
                (size.height as usize / ch).max(1),
            )
        };

        let proxy = self.proxy.clone();
        let cwd = saved
            .and_then(|window| window.panes.first())
            .map(|pane| pane.cwd.as_str());
        let first_pane = self.create_pane_in(window_id, &proxy, cols, rows, cwd)?;

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
            scroll_frac: 0.0,
            cursor_visible: true,
            last_blink: Instant::now(),
        };
        tw.sync_title();
        self.windows.insert(window_id, tw);
        let layout_complete = saved
            .map(|saved| self.restore_window_layout(window_id, saved))
            .unwrap_or(true);

        // Wire the "+" button after the window is fully set up.
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = self
            .windows
            .get(&window_id)
            .and_then(|tw| ns_view_ptr(&tw.window))
        {
            platform::setup_add_tab_button(ns_view);
        }

        // Re-number all tab labels now that a new tab has been added.
        self.sync_all_titles();
        Some((window_id, layout_complete))
    }

    fn add_split(&mut self, window_id: WindowId, dir: SplitDir) {
        let _ = self.add_split_in(window_id, dir, None);
    }

    fn add_split_in(&mut self, window_id: WindowId, dir: SplitDir, cwd: Option<&str>) -> bool {
        let tw = match self.windows.get_mut(&window_id) {
            Some(w) => w,
            None => return false,
        };
        if tw.panes.len() >= 2 {
            return false; // already split
        }
        let idx = tw.panes.len(); // will be 1

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
        let new_pane = match self.create_pane_in(window_id, &proxy, cols, rows, cwd) {
            Some(p) => p,
            None => return false,
        };
        let tw = self.windows.get_mut(&window_id).unwrap();
        tw.split = Some(dir);
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
        true
    }

    /// Apply the portion of a saved window that depends on live panes. The
    /// native window's size and position are applied before creation.
    fn restore_window_layout(&mut self, window_id: WindowId, saved: &session::SavedWindow) -> bool {
        let mut complete = true;
        match (saved.split, saved.panes.get(1)) {
            (Some(saved_split), Some(second)) => {
                let split = match saved_split {
                    session::SavedSplit::Vertical => SplitDir::Vertical,
                    session::SavedSplit::Horizontal => SplitDir::Horizontal,
                };
                complete = self.add_split_in(window_id, split, Some(second.cwd.as_str()));
            }
            (None, None) => {}
            _ => complete = false,
        }

        if let Some(tw) = self.windows.get_mut(&window_id) {
            for (pane, saved_pane) in tw.panes.iter_mut().zip(&saved.panes) {
                pane.terminal.state.title = saved_pane.title.clone();
            }
            tw.active_pane = saved.active_pane.min(tw.panes.len().saturating_sub(1));
            tw.sync_title();
            tw.window.request_redraw();
            complete &= tw.panes.len() == saved.panes.len().max(1);
        } else {
            complete = false;
        }
        complete
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
            tw.update_ghost();
            tw.sync_title();
            tw.window.request_redraw();
        }
    }

    fn close_window(&mut self, window_id: WindowId, event_loop: &ActiveEventLoop) {
        if let Some(tw) = self.windows.remove(&window_id) {
            for pane in &tw.panes {
                self.pane_to_window.remove(&pane.id);
            }
        }
        if self.focused_window == Some(window_id) {
            self.focused_window = None;
        }
        if self.windows.is_empty() {
            event_loop.exit();
        } else {
            self.sync_all_titles();
        }
    }

    /// Re-sync every window's tab label.  Call this whenever the tab count
    /// changes so that all index prefixes stay accurate.
    fn sync_all_titles(&self) {
        for tw in self.windows.values() {
            tw.sync_title();
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
        let first_saved = self
            .pending_restore
            .as_ref()
            .and_then(|saved| saved.windows.first())
            .cloned();

        let mut attrs = terminal_window_attributes(first_saved.as_ref());

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

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
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
        let queue = Arc::new(queue);

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or_else(|| {
                caps.formats
                    .first()
                    .copied()
                    .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
            });
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: choose_surface_alpha_mode(&caps.alpha_modes),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // Set NSWindowTabbingModePreferred
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = ns_view_ptr(&window) {
            platform::configure_window_background(ns_view);
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
            let proxy = self.proxy.clone();
            platform::set_check_for_updates_callback(move || {
                let _ = proxy.send_event(AppEvent::CheckForUpdates);
            });
            let proxy = self.proxy.clone();
            platform::set_show_background_panel_callback(move || {
                let _ = proxy.send_event(AppEvent::ShowBackgroundAppearancePanel);
            });
            let proxy = self.proxy.clone();
            platform::set_background_changed_callback(move |background| {
                let _ = proxy.send_event(AppEvent::BackgroundAppearanceChanged(background));
            });
            platform::install_update_menu_item();
            platform::install_background_menu_item();
        }

        self.wgpu = Some(WgpuShared {
            instance,
            adapter,
            device,
            queue,
            surface_format,
        });

        let scale = window.scale_factor();
        let shared = self.shared_renderer_for(scale);
        let renderer = Renderer::new(shared, self.background);

        let (cols, rows) = {
            let cw = renderer.cell_width;
            let ch = renderer.cell_height;
            (
                (size.width as usize / cw).max(1),
                (size.height as usize / ch).max(1),
            )
        };

        let cwd = first_saved
            .as_ref()
            .and_then(|saved| saved.panes.first())
            .map(|pane| pane.cwd.as_str());

        let proxy = self.proxy.clone();
        let first_pane = match self.create_pane_in(window_id, &proxy, cols, rows, cwd) {
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
            scroll_frac: 0.0,
            cursor_visible: true,
            last_blink: Instant::now(),
        };
        tw.sync_title();
        self.windows.insert(window_id, tw);
        let first_layout_complete = first_saved
            .as_ref()
            .is_none_or(|saved| self.restore_window_layout(window_id, saved));

        // Wire the "+" button after the window is fully set up.
        #[cfg(target_os = "macos")]
        if let Some(ns_view) = self
            .windows
            .get(&window_id)
            .and_then(|tw| ns_view_ptr(&tw.window))
        {
            platform::setup_add_tab_button(ns_view);
        }

        // Re-number all tab labels now that a new tab has been added.
        self.sync_all_titles();

        // Restore additional tabs from a `--restore-session` payload. Keep
        // window ids aligned with the saved array so the focused tab can be
        // reactivated after every pane has been created.
        if let Some(saved) = self.pending_restore.take() {
            let mut restored_ids = Vec::with_capacity(saved.windows.len());
            if !saved.windows.is_empty() {
                restored_ids.push(Some(window_id));
            }
            let mut restore_complete = first_layout_complete;
            for saved_window in saved.windows.iter().skip(1) {
                let restored = self.open_tab_with_saved(event_loop, Some(saved_window));
                restore_complete &= restored
                    .as_ref()
                    .is_some_and(|(_, layout_complete)| *layout_complete);
                restored_ids.push(restored.map(|(id, _)| id));
            }
            if let Some(focused) = saved
                .focused_window
                .and_then(|index| restored_ids.get(index))
                .copied()
                .flatten()
            {
                if let Some(tw) = self.windows.get(&focused) {
                    tw.window.focus_window();
                }
                self.focused_window = Some(focused);
            }
            // Drop the file only after all recorded panes were recreated. A
            // partial failure leaves it available for a later retry.
            if restore_complete {
                session::clear();
            }
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

            // Re-sync all tab numbers when a window gains focus — this fires
            // after a drag-reorder because macOS activates the moved tab.
            WindowEvent::Focused(focused) => {
                if focused {
                    self.focused_window = Some(window_id);
                    self.sync_all_titles();
                }
                // Send focus-in / focus-out to the active pane if ?1004h is on.
                if let Some(tw) = self.windows.get_mut(&window_id) {
                    let seq: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                    if tw.active().terminal.state.focus_tracking {
                        tw.active_mut().write(seq);
                    }
                }
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
                if self.windows.contains_key(&window_id) {
                    let shared = self.shared_renderer_for(scale_factor);
                    if let Some(tw) = self.windows.get_mut(&window_id) {
                        tw.renderer = Renderer::new(shared, self.background);
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
                        if let Some(tw) = self.windows.get(&window_id)
                            && let Some(ns_view) = ns_view_ptr(&tw.window)
                        {
                            platform::select_prev_tab(ns_view);
                        }
                    }
                    KeyAction::NextTab => {
                        #[cfg(target_os = "macos")]
                        if let Some(tw) = self.windows.get(&window_id)
                            && let Some(ns_view) = ns_view_ptr(&tw.window)
                        {
                            platform::select_next_tab(ns_view);
                        }
                    }
                    KeyAction::SelectTab(n) => {
                        #[cfg(target_os = "macos")]
                        if let Some(tw) = self.windows.get(&window_id)
                            && let Some(ns_view) = ns_view_ptr(&tw.window)
                        {
                            platform::select_tab_at_index(ns_view, n);
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
                            let cell =
                                tw.pixel_to_grid_cell(position.x, position.y).or_else(|| {
                                    // Clamp to nearest edge
                                    let rects = tw.pane_rects();
                                    let ai = tw.active_pane;
                                    let (ox, oy, pw, ph) = rects.get(ai).copied().unwrap_or((
                                        0.,
                                        0.,
                                        tw.window.inner_size().width as f32,
                                        tw.window.inner_size().height as f32,
                                    ));
                                    let cw = tw.renderer.cell_width as f64;
                                    let vis_cols = (pw as usize / tw.renderer.cell_width).max(1);
                                    let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                                    let vo = tw.panes[ai].terminal.state.viewport_offset as i64;
                                    let col = ((position.x - ox as f64).max(0.) / cw) as usize;
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
                                    Some(Selection {
                                        start: anchor,
                                        end: c,
                                    })
                                } else {
                                    None
                                };
                            }

                            // Auto-scroll detection
                            let ai = tw.active_pane;
                            let rects = tw.pane_rects();
                            let (_ox, oy, _pw, ph) =
                                rects.get(ai).copied().unwrap_or((0., 0., 0., 0.));
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

                    // Cmd+click: open URL or OSC 8 link
                    let opened = if sup {
                        if let Some((pi, row, col)) = tw.pixel_to_pane_cell(mx, my) {
                            let rects = tw.pane_rects();
                            let (_, _, pw, ph) = rects.get(pi).copied().unwrap_or((0., 0., 0., 0.));
                            let vis_cols = (pw as usize / tw.renderer.cell_width).max(1);
                            let vis_rows = (ph as usize / tw.renderer.cell_height).max(1);
                            // Check OSC 8 link first (higher priority than raw URL detection).
                            let osc8_url = {
                                let cell = tw.panes[pi].terminal.state.visual_cell(row, col);
                                if cell.link_id != 0 {
                                    let idx = (cell.link_id as usize).saturating_sub(1);
                                    tw.panes[pi].terminal.state.links.get(idx).cloned()
                                } else {
                                    None
                                }
                            };
                            let url = osc8_url.or_else(|| {
                                tw.panes[pi]
                                    .urls_cached(vis_rows, vis_cols)
                                    .iter()
                                    .find(|(r, c0, c1, _)| *r == row && col >= *c0 && col < *c1)
                                    .map(|(_, _, _, u)| u.clone())
                            });
                            if let Some(u) = url {
                                // Only open http/https URLs — defence in depth against
                                // file:// or custom-scheme injection via terminal output.
                                if should_open_url(&u) {
                                    let _ = std::process::Command::new("open").arg(&u).spawn();
                                }
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
                            tw.set_active_pane(pi);
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
                    let lines =
                        mouse_wheel_lines(&delta, tw.renderer.cell_height, &mut tw.scroll_frac);
                    let cell = if lines != 0 {
                        let (mx, my) = tw.cursor_pos;
                        tw.pixel_to_pane_cell(mx, my)
                            .map(|(_, row, col)| (row, col))
                    } else {
                        None
                    };
                    let action = {
                        let state = &tw.panes[tw.active_pane].terminal.state;
                        mouse_wheel_action(state, lines, cell)
                    };
                    match action {
                        MouseWheelAction::None => {}
                        MouseWheelAction::LocalViewport(lines) => {
                            tw.active_mut().terminal.state.scroll_viewport(lines);
                            tw.window.request_redraw();
                        }
                        MouseWheelAction::PtyWrites(writes) => {
                            for seq in writes {
                                tw.panes[tw.active_pane].write(&seq);
                            }
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        const BLINK_PERIOD: Duration = Duration::from_millis(530);
        const SCROLL_PERIOD: Duration = Duration::from_millis(50);
        const SESSION_SAVE_PERIOD: Duration = Duration::from_secs(30);
        let now = Instant::now();

        // Cheap periodic snapshot of the layout so a sudden SIGTERM from the
        // updater never loses more than ~30 s of state changes. Heavy lifting
        // is in save_session_now(); here we just gate on the interval.
        if !self.windows.is_empty()
            && now.duration_since(self.last_session_save) >= SESSION_SAVE_PERIOD
        {
            self.save_session_now();
        }

        let mut earliest_deadline = now + BLINK_PERIOD;

        for tw in self.windows.values_mut() {
            // Cursor blink
            if update_cursor_blink(
                &mut tw.cursor_visible,
                &mut tw.last_blink,
                now,
                BLINK_PERIOD,
            ) {
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
                    let edge_row = if dir > 0 {
                        -vo
                    } else {
                        vis_rows as i64 - 1 - vo
                    };
                    let c = (edge_row, col);
                    tw.panes[ai].selection = if c != anchor {
                        Some(Selection {
                            start: anchor,
                            end: c,
                        })
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
            AppEvent::PtyReady { pane_id } => {
                let window_id = match self.pane_to_window.get(&pane_id).copied() {
                    Some(w) => w,
                    None => return,
                };
                let tw = match self.windows.get_mut(&window_id) {
                    Some(w) => w,
                    None => return,
                };
                let mut input_changed = false;
                let mut title_changed = false;
                if let Some(pane) = tw.panes.iter_mut().find(|p| p.id == pane_id) {
                    pane.pty_output.drain_into(&mut pane.pty_scratch);
                    if pane.pty_scratch.is_empty() {
                        return;
                    }
                    if !pane.terminal.state.is_scrolled_back() {
                        pane.terminal.state.snap_to_bottom();
                    }
                    let sync_before = pane.terminal.state.sync_output;
                    pane.terminal.process(&pane.pty_scratch);
                    let sync_after = pane.terminal.state.sync_output;
                    // If ?2026l just cleared sync mode, force a redraw now.
                    if sync_before && !sync_after {
                        tw.window.request_redraw();
                    }
                    let clipboard_payload = pane
                        .terminal
                        .state
                        .osc_52_query
                        .then(osc52_clipboard_payload);
                    let responses = drain_terminal_host_responses(
                        &mut pane.terminal.state,
                        clipboard_payload.as_deref().unwrap_or(""),
                    );
                    (input_changed, title_changed) = pane.terminal.state.take_ui_changes();
                    let has_responses = !responses.is_empty();
                    for r in responses {
                        let _ = pane.pty_writer.write_all(&r);
                    }
                    if has_responses {
                        let _ = pane.pty_writer.flush();
                    }
                }
                // Shell metadata changes are sparse compared with PTY output.
                if tw.panes.get(tw.active_pane).map(|p| p.id) == Some(pane_id) {
                    if input_changed {
                        tw.update_ghost();
                    }
                    if title_changed {
                        tw.sync_title();
                    }
                }
                tw.reset_blink();
                // Only redraw immediately if synchronized-output mode is off.
                // When ?2026h is active the application will clear it (?2026l)
                // once it has finished writing, which triggers the redraw then.
                if !tw
                    .panes
                    .iter()
                    .find(|p| p.id == pane_id)
                    .map(|p| p.terminal.state.sync_output)
                    .unwrap_or(false)
                {
                    tw.window.request_redraw();
                }
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
                            tw.update_ghost();
                            tw.sync_title();
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
            AppEvent::CheckForUpdates => {
                self.begin_update_check();
            }
            AppEvent::UpdateCheckFinished(result) => {
                self.finish_update_check(result);
            }
            AppEvent::ShowBackgroundAppearancePanel => {
                #[cfg(target_os = "macos")]
                platform::show_background_panel(self.background);
            }
            AppEvent::BackgroundAppearanceChanged(background) => {
                self.apply_background_appearance(background);
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Final flush so the most recent layout is on disk by the time the
        // process actually exits (covers Cmd+Q and close-last-window;
        // SIGTERM from the updater is covered by the periodic save).
        if !self.windows.is_empty() {
            self.save_session_now();
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
        if let Some(text) = platform::clipboard_text()
            && !text.is_empty()
        {
            return base64_encode(text.as_bytes());
        }
        String::new()
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
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
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
                if prefix
                    .iter()
                    .all(|&c| c == ' ' || c == '\0' || c.is_ascii_digit())
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

/// POSIX single-quote escape a string for safe embedding between `'...'` in shell.
/// Every `'` in the value becomes `'\''` (end quote, escaped quote, reopen quote).
fn sh_sq_escape(s: &str) -> String {
    s.replace('\'', "'\\''")
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ShellTools {
    term: Option<PathBuf>,
    tcat: Option<PathBuf>,
    tdiff: Option<PathBuf>,
    tjson: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellBootstrapFiles {
    zshenv: String,
    zshrc: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellEnvPlan {
    files: ShellBootstrapFiles,
    env: Vec<(String, String)>,
}

fn discover_shell_tools(exe_dir: Option<&Path>) -> ShellTools {
    let Some(exe_dir) = exe_dir else {
        return ShellTools::default();
    };
    let tool = |name: &str| {
        let path = exe_dir.join(name);
        path.exists().then_some(path)
    };
    ShellTools {
        term: tool("term"),
        tcat: tool("tcat"),
        tdiff: tool("tdiff"),
        tjson: tool("tjson"),
    }
}

fn build_shell_bootstrap_files(home: &str, tools: &ShellTools) -> ShellBootstrapFiles {
    let home = sh_sq_escape(home);
    let zshenv = format!("[ -f '{home}/.zshenv' ] && source '{home}/.zshenv'\n");

    let cat_fn = match tools.tcat.as_ref() {
        Some(path) => format!(
            "_TCAT='{}'\nfunction cat() {{\n  if [[ -t 1 ]] && [ $# -eq 1 ] && [ -f \"$1\" ]; then\n    \"$_TCAT\" \"$1\"\n  else\n    command cat \"$@\"\n  fi\n}}\n",
            sh_sq_escape(&path.display().to_string())
        ),
        None => String::new(),
    };
    let diff_fn = match tools.tdiff.as_ref() {
        Some(path) => format!(
            "export GIT_PAGER='{}'\nexport GIT_COLOR_UI=never\n",
            sh_sq_escape(&path.display().to_string())
        ),
        None => String::new(),
    };
    let json_fn = match tools.tjson.as_ref() {
        Some(path) => format!(
            "_TJSON='{}'\nfunction json() {{ \"$_TJSON\" \"$@\"; }}\n",
            sh_sq_escape(&path.display().to_string())
        ),
        None => String::new(),
    };
    // Default `git log` to `--reverse` so the most recent commit lands closest
    // to the prompt and scrolling up walks backward in time. Skip the
    // rewrite when the user already specified an ordering flag, and ignore
    // anything but the bare `git log ...` form — `command git log ...` is
    // always the escape hatch.
    let git_fn = r#"function git() {
  if [[ "$1" == "log" ]]; then
    local arg
    for arg in "$@"; do
      case "$arg" in --reverse|--no-reverse|--topo-order|--date-order|--author-date-order)
        command git "$@"
        return $?
      ;;
      esac
    done
    shift
    command git log --reverse "$@"
  else
    command git "$@"
  fi
}
"#;
    let suggest_extra_candidates = format!(
        "{}{}",
        if tools.tcat.is_some() {
            "        print -rl -- cat\n"
        } else {
            ""
        },
        if tools.tjson.is_some() {
            "        print -rl -- json\n"
        } else {
            ""
        }
    );
    let command_not_found_fn = match tools.term.as_ref() {
        Some(path) => format!(
            "_TERM_SELF='{}'\n\
             function command_not_found_handler() {{\n\
               emulate -L zsh\n\
               local cmd=\"$1\"\n\
               shift\n\
               print -u2 -- \"zsh: command not found: $cmd\"\n\
               if [[ -x \"$_TERM_SELF\" ]]; then\n\
                 local -a suggestions\n\
                 suggestions=(${{(@f)$({{\n\
        print -rl -- ${{(k)commands}}\n\
        print -rl -- ${{(k)builtins}}\n\
{suggest_extra_candidates}      }} | LC_ALL=C sort -u | \"$_TERM_SELF\" __suggest_command \"$cmd\" 2>/dev/null)}})\n\
                 if (( ${{#suggestions[@]}} > 0 )); then\n\
                   print -u2 -- 'Did you mean:'\n\
                   local suggestion\n\
                   for suggestion in $suggestions; do\n\
                     print -u2 -- \"  $suggestion\"\n\
                   done\n\
                   local -a retry_words\n\
                   retry_words=(\"${{suggestions[1]}}\" \"$@\")\n\
                   local retry_display=\"${{(j: :)retry_words}}\"\n\
                   if read -q \"reply?try '$retry_display'? y/n \"; then\n\
                     print -u2\n\
                     \"$retry_words[@]\"\n\
                     return $?\n\
                   fi\n\
                   print -u2\n\
                 fi\n\
               fi\n\
               return 127\n\
             }}\n",
            sh_sq_escape(&path.display().to_string())
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
    let prompt_setup = concat!(
        "setopt PROMPT_SUBST\n",
        "_term_jj() { (( ${+functions[jj_prompt]} )) && jj_prompt; }\n",
        "PROMPT=\"%{\x1b[38;2;137;180;250m%}%~%{\x1b[0m%} $(_term_jj)%{\x1b[38;2;203;166;247m%}\u{276F}%{\x1b[0m%} \"\n",
    );
    let zshrc = format!(
        "ZDOTDIR='{home}'\n\
         [ -f '{home}/.zprofile' ] && source '{home}/.zprofile'\n\
         [ -f '{home}/.zshrc' ] && source '{home}/.zshrc'\n\
         {cat_fn}{diff_fn}{json_fn}{git_fn}{command_not_found_fn}{zle_hooks}{prompt_setup}"
    );

    ShellBootstrapFiles { zshenv, zshrc }
}

fn build_shell_env_plan(
    home: &str,
    tools: &ShellTools,
    zdotdir: &Path,
    has_lang: bool,
    has_lc_all: bool,
) -> ShellEnvPlan {
    let mut env = vec![
        (
            "ZDOTDIR".to_string(),
            zdotdir.to_string_lossy().into_owned(),
        ),
        ("TERM_PROGRAM".to_string(), "ghostty".to_string()),
    ];
    if !has_lang {
        env.push(("LANG".to_string(), "en_US.UTF-8".to_string()));
    }
    if !has_lc_all {
        env.push(("LC_ALL".to_string(), "en_US.UTF-8".to_string()));
    }
    ShellEnvPlan {
        files: build_shell_bootstrap_files(home, tools),
        env,
    }
}

fn write_shell_bootstrap_files(zdotdir: &Path, files: &ShellBootstrapFiles) -> std::io::Result<()> {
    std::fs::create_dir_all(zdotdir)?;
    std::fs::write(zdotdir.join(".zshenv"), &files.zshenv)?;
    std::fs::write(zdotdir.join(".zshrc"), &files.zshrc)?;
    Ok(())
}

fn setup_shell_env(cmd: &mut CommandBuilder) {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let tools = discover_shell_tools(exe_dir.as_deref());
    let zdotdir = std::env::temp_dir().join(format!("term_zsh_{}", std::process::id()));
    let plan = build_shell_env_plan(
        &home,
        &tools,
        &zdotdir,
        std::env::var("LANG").is_ok(),
        std::env::var("LC_ALL").is_ok(),
    );
    if write_shell_bootstrap_files(&zdotdir, &plan.files).is_err() {
        return;
    }
    for (key, value) in plan.env {
        cmd.env(key, value);
    }
}

fn maybe_run_helper_mode() -> Option<i32> {
    let mut args = std::env::args();
    let _exe = args.next();
    match args.next().as_deref() {
        Some("__suggest_command") => {
            let Some(query) = args.next() else {
                eprintln!("term: missing command name for __suggest_command");
                return Some(2);
            };
            if args.next().is_some() {
                eprintln!("term: too many arguments for __suggest_command");
                return Some(2);
            }

            let mut candidates = String::new();
            if std::io::stdin().read_to_string(&mut candidates).is_err() {
                eprintln!("term: failed to read command candidates");
                return Some(1);
            }

            for suggestion in command_suggest::suggest_commands(&query, candidates.lines(), 3) {
                println!("{suggestion}");
            }
            Some(0)
        }
        _ => None,
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    if let Some(code) = maybe_run_helper_mode() {
        std::process::exit(code);
    }

    // `--restore-session` is the flag the updater script (and anyone who
    // wants a manual replay) passes via `open --args` to load the saved
    // tab layout. Without the flag we always start fresh, even if the
    // file exists — keeps the natural Cmd+Q → relaunch flow predictable.
    let restore = std::env::args().any(|a| a == "--restore-session");
    let pending_restore = if restore { session::load() } else { None };

    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let mut app = App::new(proxy, pending_restore);
    event_loop.run_app(&mut app).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{Terminal, TerminalState};
    use std::sync::mpsc;
    use tempfile::TempDir;

    #[test]
    fn pty_output_coalesces_wakes_until_drained() {
        let output = PtyOutput::default();
        assert!(output.push(b"one"));
        assert!(!output.push(b"two"));

        let mut drained = Vec::new();
        output.drain_into(&mut drained);
        assert_eq!(drained, b"onetwo");
        assert!(output.push(b"three"));
    }

    #[test]
    fn pty_output_applies_backpressure_and_unblocks_after_drain() {
        let output = Arc::new(PtyOutput::default());
        assert!(output.push(&vec![b'x'; PTY_OUTPUT_MAX_BYTES]));

        let (sent, received) = mpsc::channel();
        let writer_output = output.clone();
        let writer = std::thread::spawn(move || {
            let wake = writer_output.push(b"y");
            sent.send(wake).unwrap();
        });
        assert!(
            received.recv_timeout(Duration::from_millis(25)).is_err(),
            "writer should wait while the bounded buffer is full"
        );

        let mut drained = Vec::new();
        output.drain_into(&mut drained);
        assert_eq!(drained.len(), PTY_OUTPUT_MAX_BYTES);
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
        writer.join().unwrap();
    }

    #[test]
    fn copied_soft_wrapped_rows_join_without_newline() {
        let mut terminal = Terminal::new(4, 3);
        terminal.process(b"abcdef");
        assert_eq!(
            selection_text_for_range(&terminal.state, (0, 0, 1, 1)),
            "abcdef"
        );
    }

    #[test]
    fn copied_hard_line_break_is_preserved() {
        let mut terminal = Terminal::new(4, 3);
        terminal.process(b"ab\r\ncd");
        assert_eq!(
            selection_text_for_range(&terminal.state, (0, 0, 1, 1)),
            "ab\ncd"
        );
    }

    #[test]
    fn copied_wide_character_has_no_continuation_space() {
        let mut terminal = Terminal::new(6, 2);
        terminal.process("a界b".as_bytes());
        assert_eq!(
            selection_text_for_range(&terminal.state, (0, 0, 0, 3)),
            "a界b"
        );
    }

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
        find_urls(&s, 1, cols)
            .into_iter()
            .map(|(_, _, _, u)| u)
            .collect()
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
        assert_eq!(
            urls("https://a.com https://b.com"),
            vec!["https://a.com", "https://b.com"]
        );
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
            assert_eq!(
                encoded.len() % 4,
                0,
                "length {len} → encoded len {}",
                encoded.len()
            );
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

    // ── sh_sq_escape ──────────────────────────────────────────────────────────

    #[test]
    fn sh_sq_escape_plain_path_unchanged() {
        assert_eq!(sh_sq_escape("/Users/alice/.zshrc"), "/Users/alice/.zshrc");
    }

    #[test]
    fn sh_sq_escape_quote_in_path_becomes_posix_escaped() {
        // O'Brien → O'\''Brien  (end-quote, backslash-quote, reopen-quote)
        assert_eq!(sh_sq_escape("O'Brien"), "O'\\''Brien");
    }

    #[test]
    fn sh_sq_escape_multiple_quotes() {
        assert_eq!(sh_sq_escape("it's a 'test'"), "it'\\''s a '\\''test'\\''");
    }

    #[test]
    fn sh_sq_escape_empty_string_unchanged() {
        assert_eq!(sh_sq_escape(""), "");
    }

    #[test]
    fn sh_sq_escape_only_quote_becomes_escaped() {
        assert_eq!(sh_sq_escape("'"), "'\\''");
    }

    #[test]
    fn saved_window_geometry_uses_physical_size_and_position() {
        let saved = session::SavedWindow {
            outer_x: Some(120),
            outer_y: Some(240),
            inner_w: 960,
            inner_h: 640,
            split: None,
            panes: Vec::new(),
            active_pane: 0,
        };
        let attrs = terminal_window_attributes(Some(&saved));
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(
                960, 640
            )))
        );
        assert_eq!(
            attrs.position,
            Some(winit::dpi::Position::Physical(
                winit::dpi::PhysicalPosition::new(120, 240)
            ))
        );
    }

    #[test]
    fn saved_window_geometry_ignores_partial_position_and_clamps_zero_size() {
        let saved = session::SavedWindow {
            outer_x: Some(120),
            outer_y: None,
            inner_w: 0,
            inner_h: 0,
            split: None,
            panes: Vec::new(),
            active_pane: 0,
        };
        let attrs = terminal_window_attributes(Some(&saved));
        assert_eq!(
            attrs.inner_size,
            Some(winit::dpi::Size::Physical(winit::dpi::PhysicalSize::new(
                1, 1
            )))
        );
        assert_eq!(attrs.position, None);
    }

    // ── URL cache (urls_with_cache) ───────────────────────────────────────────

    fn make_state_with_url(url: &str) -> TerminalState {
        let cols = url.len() + 4;
        let mut s = TerminalState::new(cols, 5);
        for (i, c) in url.chars().enumerate() {
            s.grid[0][i].c = c;
        }
        s
    }

    #[test]
    fn url_cache_populated_on_first_call() {
        let mut state = make_state_with_url("https://example.com");
        state.generation = 1;
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX; // sentinel — forces initial miss
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0usize, 0usize);
        let result = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].3, "https://example.com");
        assert_eq!(cache_gen, 1);
        assert_eq!(cache_viewport_offset, 0);
        assert_eq!(cache_dims, (1, 30));
    }

    #[test]
    fn url_cache_hit_returns_same_result_without_rescan() {
        let mut state = make_state_with_url("https://example.com");
        state.generation = 5;
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        // First call — populates cache.
        urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        // Corrupt the state grid so a fresh scan would return nothing.
        for cell in &mut state.grid[0] {
            cell.c = ' ';
        }
        // Second call with same generation + dims — must return cached value.
        let result = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        assert_eq!(
            result.len(),
            1,
            "cache hit must return previously scanned URL"
        );
    }

    #[test]
    fn url_cache_miss_on_generation_change() {
        let mut state = make_state_with_url("https://example.com");
        state.generation = 1;
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        assert_eq!(cache.len(), 1);
        // Bump generation and clear the grid — rescan should return empty.
        state.generation = 2;
        for cell in &mut state.grid[0] {
            cell.c = ' ';
        }
        let result = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        assert_eq!(result.len(), 0, "stale generation must trigger rescan");
        assert_eq!(cache_gen, 2);
    }

    #[test]
    fn url_cache_miss_on_vis_rows_change() {
        let mut state = make_state_with_url("https://example.com");
        state.generation = 1;
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        // Same generation, different vis_rows — must rescan.
        let result = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            2,
            30,
        );
        assert_eq!(cache_dims, (2, 30), "dims must update after miss");
        let _ = result; // result correctness verified elsewhere
    }

    #[test]
    fn url_cache_miss_on_vis_cols_change() {
        let mut state = make_state_with_url("https://example.com");
        state.generation = 1;
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            40,
        );
        assert_eq!(cache_dims, (1, 40));
    }

    #[test]
    fn url_cache_miss_on_viewport_offset_change() {
        let mut state = make_state_with_url("https://live.example");
        let mut scrollback_row = vec![terminal::Cell::default(); state.cols];
        for (i, c) in "https://history.test".chars().enumerate() {
            scrollback_row[i].c = c;
        }
        state.scrollback.push_back(scrollback_row);
        state.generation = 7;

        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        let live = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            state.cols,
        );
        assert_eq!(live[0].3, "https://live.example");

        state.viewport_offset = 1;
        let history = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            state.cols,
        );
        assert_eq!(history[0].3, "https://history.test");
        assert_eq!(cache_viewport_offset, 1);
    }

    #[test]
    fn url_cache_sentinel_u64_max_always_misses_on_zero_generation() {
        // Terminal starts with generation 0; sentinel is u64::MAX — always a miss.
        let state = make_state_with_url("https://initial.com");
        // state.generation == 0 (default)
        let mut cache = Vec::new();
        let mut cache_gen = u64::MAX;
        let mut cache_viewport_offset = usize::MAX;
        let mut cache_dims = (0, 0);
        let result = urls_with_cache(
            &mut cache,
            &mut cache_gen,
            &mut cache_viewport_offset,
            &mut cache_dims,
            &state,
            1,
            30,
        );
        assert_eq!(cache_gen, 0, "sentinel must be replaced after first call");
        let _ = result;
    }

    // ── URL scheme guard ─────────────────────────────────────────────────────

    #[test]
    fn find_urls_never_returns_file_scheme() {
        // file:// must not be detectable — it isn't in the HTTP prefix list.
        let url = "file:///etc/passwd";
        let cols = url.len() + 2;
        let mut s = TerminalState::new(cols, 1);
        for (i, c) in url.chars().enumerate() {
            s.grid[0][i].c = c;
        }
        let urls: Vec<_> = find_urls(&s, 1, cols)
            .into_iter()
            .map(|(_, _, _, u)| u)
            .collect();
        assert!(
            urls.is_empty(),
            "file:// must not be detected as a URL: {urls:?}"
        );
    }

    #[test]
    fn find_urls_never_returns_custom_scheme() {
        for scheme in &["ftp://", "javascript:", "data:", "x-custom://"] {
            let url = format!("{scheme}evil");
            let cols = url.len() + 2;
            let mut s = TerminalState::new(cols, 1);
            for (i, c) in url.chars().enumerate() {
                s.grid[0][i].c = c;
            }
            let urls: Vec<_> = find_urls(&s, 1, cols)
                .into_iter()
                .map(|(_, _, _, u)| u)
                .collect();
            assert!(
                urls.is_empty(),
                "scheme '{scheme}' must not be detected: {urls:?}"
            );
        }
    }

    #[test]
    fn find_urls_detects_http_and_https_only() {
        let mut s = TerminalState::new(80, 1);
        let line = "http://a.com https://b.com ftp://c.com";
        for (i, c) in line.chars().enumerate() {
            s.grid[0][i].c = c;
        }
        let urls: Vec<_> = find_urls(&s, 1, 80)
            .into_iter()
            .map(|(_, _, _, u)| u)
            .collect();
        assert_eq!(urls.len(), 2, "only http and https should be detected");
        assert!(urls[0].starts_with("http://"));
        assert!(urls[1].starts_with("https://"));
    }

    // ── Bracketed paste ────────────────────────────────────────────────────────

    #[test]
    fn wrap_bracketed_paste_passthrough_when_disabled() {
        assert_eq!(wrap_bracketed_paste(b"hello", false), b"hello");
    }

    #[test]
    fn wrap_bracketed_paste_adds_wrappers_when_enabled() {
        assert_eq!(
            wrap_bracketed_paste(b"hello", true),
            b"\x1b[200~hello\x1b[201~"
        );
    }

    // ── URL opening guard ──────────────────────────────────────────────────────

    #[test]
    fn should_open_url_allows_only_http_and_https() {
        assert!(should_open_url("http://example.com"));
        assert!(should_open_url("https://example.com"));
        assert!(!should_open_url("file:///etc/passwd"));
        assert!(!should_open_url("javascript:alert(1)"));
    }

    // ── Mouse wheel helpers ────────────────────────────────────────────────────

    #[test]
    fn mouse_wheel_lines_accumulates_pixel_delta() {
        let mut frac = 0.0;
        let first = mouse_wheel_lines(
            &MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 8.0)),
            16,
            &mut frac,
        );
        let second = mouse_wheel_lines(
            &MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 8.0)),
            16,
            &mut frac,
        );
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(frac, 0.0);
    }

    #[test]
    fn mouse_wheel_action_local_viewport_scrolls_normal_screen() {
        let term = Terminal::new(80, 24);
        assert_eq!(
            mouse_wheel_action(&term.state, 3, None),
            MouseWheelAction::LocalViewport(3)
        );
    }

    #[test]
    fn mouse_wheel_action_alt_screen_uses_application_cursor_keys() {
        let mut term = Terminal::new(80, 24);
        term.process(b"\x1b[?1049h");
        assert_eq!(
            mouse_wheel_action(&term.state, -2, None),
            MouseWheelAction::PtyWrites(vec![b"\x1bOB".to_vec(), b"\x1bOB".to_vec()])
        );
    }

    #[test]
    fn mouse_wheel_action_mouse_tracking_sgr_encodes_scroll_reports() {
        let mut term = Terminal::new(80, 24);
        term.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            mouse_wheel_action(&term.state, -2, Some((3, 4))),
            MouseWheelAction::PtyWrites(
                vec![b"\x1b[<65;5;4M".to_vec(), b"\x1b[<65;5;4M".to_vec(),]
            )
        );
    }

    #[test]
    fn mouse_wheel_action_mouse_tracking_x10_caps_coordinates() {
        let mut term = Terminal::new(80, 24);
        term.process(b"\x1b[?1000h");
        assert_eq!(
            mouse_wheel_action(&term.state, 1, Some((500, 500))),
            MouseWheelAction::PtyWrites(vec![vec![0x1b, b'[', b'M', 96, 255, 255]])
        );
    }

    // ── Blink helpers ──────────────────────────────────────────────────────────

    #[test]
    fn update_cursor_blink_toggles_only_after_period() {
        let now = Instant::now();
        let mut cursor_visible = true;
        let mut last_blink = now;
        assert!(!update_cursor_blink(
            &mut cursor_visible,
            &mut last_blink,
            now + Duration::from_millis(100),
            Duration::from_millis(530),
        ));
        assert!(cursor_visible);
        assert!(update_cursor_blink(
            &mut cursor_visible,
            &mut last_blink,
            now + Duration::from_millis(600),
            Duration::from_millis(530),
        ));
        assert!(!cursor_visible);
    }

    // ── Host response helpers ──────────────────────────────────────────────────

    #[test]
    fn drain_terminal_host_responses_merges_pending_and_osc52() {
        let mut state = TerminalState::new(80, 24);
        state.pending_responses.push(b"\x1b[5;10R".to_vec());
        state.osc_52_query = true;

        let responses = drain_terminal_host_responses(&mut state, "aGVsbG8=");
        assert_eq!(
            responses,
            vec![b"\x1b[5;10R".to_vec(), b"\x1b]52;c;aGVsbG8=\x07".to_vec()]
        );
        assert!(state.pending_responses.is_empty());
        assert!(!state.osc_52_query);
    }

    // ── Shell bootstrap helpers ────────────────────────────────────────────────

    #[test]
    fn discover_shell_tools_finds_existing_neighbors() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("term"), b"").unwrap();
        std::fs::write(dir.path().join("tcat"), b"").unwrap();
        std::fs::write(dir.path().join("tdiff"), b"").unwrap();

        let tools = discover_shell_tools(Some(dir.path()));
        assert_eq!(tools.term, Some(dir.path().join("term")));
        assert_eq!(tools.tcat, Some(dir.path().join("tcat")));
        assert_eq!(tools.tdiff, Some(dir.path().join("tdiff")));
        assert_eq!(tools.tjson, None);
    }

    #[test]
    fn build_shell_bootstrap_files_include_aliases_hooks_and_escaping() {
        let tools = ShellTools {
            term: Some(PathBuf::from("/tmp/O'Brien/bin/term")),
            tcat: Some(PathBuf::from("/tmp/O'Brien/bin/tcat")),
            tdiff: Some(PathBuf::from("/tmp/O'Brien/bin/tdiff")),
            tjson: Some(PathBuf::from("/tmp/O'Brien/bin/tjson")),
        };

        let files = build_shell_bootstrap_files("/Users/O'Brien", &tools);
        assert!(files.zshenv.contains("[ -f '/Users/O'\\''Brien/.zshenv' ]"));
        assert!(
            files
                .zshrc
                .contains("_TERM_SELF='/tmp/O'\\''Brien/bin/term'")
        );
        assert!(files.zshrc.contains("_TCAT='/tmp/O'\\''Brien/bin/tcat'"));
        assert!(
            files
                .zshrc
                .contains("export GIT_PAGER='/tmp/O'\\''Brien/bin/tdiff'")
        );
        assert!(files.zshrc.contains("_TJSON='/tmp/O'\\''Brien/bin/tjson'"));
        assert!(files.zshrc.contains("function cat()"));
        assert!(
            files
                .zshrc
                .contains("if [[ -t 1 ]] && [ $# -eq 1 ] && [ -f \"$1\" ]")
        );
        assert!(files.zshrc.contains("function json()"));
        assert!(files.zshrc.contains("function git()"));
        assert!(files.zshrc.contains("command git log --reverse"));
        assert!(files.zshrc.contains("function command_not_found_handler()"));
        assert!(files.zshrc.contains("__suggest_command \"$cmd\""));
        assert!(
            files
                .zshrc
                .contains("retry_words=(\"${suggestions[1]}\" \"$@\")")
        );
        assert!(
            files
                .zshrc
                .contains("read -q \"reply?try '$retry_display'? y/n \"")
        );
        assert!(files.zshrc.contains("\"$retry_words[@]\""));
        assert!(
            files
                .zshrc
                .contains("add-zle-hook-widget zle-line-pre-redraw _term_buf_report")
        );
        assert!(
            files
                .zshrc
                .contains("preexec_functions+=( _term_preexec_clear _term_title_preexec )")
        );
        assert!(files.zshrc.contains("autoload -Uz compinit && compinit -C"));
        assert!(files.zshrc.contains("PROMPT=\"%{"));
    }

    #[test]
    fn build_shell_env_plan_injects_only_missing_locale_vars() {
        let plan = build_shell_env_plan(
            "/Users/alice",
            &ShellTools::default(),
            Path::new("/tmp/term-zdotdir"),
            false,
            true,
        );
        assert!(
            plan.env
                .contains(&(String::from("ZDOTDIR"), String::from("/tmp/term-zdotdir")))
        );
        assert!(
            plan.env
                .contains(&(String::from("TERM_PROGRAM"), String::from("ghostty")))
        );
        assert!(
            plan.env
                .contains(&(String::from("LANG"), String::from("en_US.UTF-8")))
        );
        assert!(!plan.env.iter().any(|(k, _)| k == "LC_ALL"));
    }

    #[test]
    fn write_shell_bootstrap_files_persists_expected_contents() {
        let dir = TempDir::new().unwrap();
        let files = build_shell_bootstrap_files("/Users/alice", &ShellTools::default());
        write_shell_bootstrap_files(dir.path(), &files).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".zshenv")).unwrap(),
            files.zshenv
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".zshrc")).unwrap(),
            files.zshrc
        );
    }

    #[test]
    fn choose_surface_alpha_mode_prefers_premultiplied() {
        let chosen = choose_surface_alpha_mode(&[
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Auto,
        ]);
        assert_eq!(chosen, wgpu::CompositeAlphaMode::PreMultiplied);
    }

    #[test]
    fn choose_surface_alpha_mode_falls_back_to_first_supported_mode() {
        let chosen = choose_surface_alpha_mode(&[wgpu::CompositeAlphaMode::Opaque]);
        assert_eq!(chosen, wgpu::CompositeAlphaMode::Opaque);
    }

    #[test]
    fn bootstrap_resize_triggers_for_real_growth() {
        assert!(should_bootstrap_resize((80, 24), (120, 32)));
    }

    #[test]
    fn bootstrap_resize_ignores_suspicious_one_cell_target() {
        assert!(!should_bootstrap_resize((80, 24), (1, 1)));
    }
}
