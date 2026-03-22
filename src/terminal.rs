use std::collections::VecDeque;
use vte::{Params, Perform};
use crate::config::*;

const SCROLLBACK_MAX: usize = 10_000;

#[derive(Clone, Copy, Debug)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for Attrs {
    fn default() -> Self {
        Self { fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false, italic: false, underline: false, inverse: false }
    }
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub c: char,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self { c: ' ', attrs: Attrs::default() }
    }
}

pub struct TerminalState {
    pub grid: Vec<Vec<Cell>>,
    pub scrollback: VecDeque<Vec<Cell>>,
    pub viewport_offset: usize,   // 0 = live view; N = scrolled N rows above live bottom
    pub cols: usize,
    pub rows: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub attrs: Attrs,
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    pub title: String,
    pub pending_responses: Vec<Vec<u8>>,
    /// Current shell input buffer (sent via OSC 9001 by the ZLE hook).
    pub input_buffer: String,
    /// Cursor offset within `input_buffer` (0 = start).
    pub input_cursor: usize,
    /// Current working directory (sent via OSC 7 on each chpwd).
    pub current_dir: String,
    saved_cursor: (usize, usize),
    saved_attrs: Attrs,
    wrap_next: bool,
}

impl TerminalState {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: vec![vec![Cell::default(); cols]; rows],
            scrollback: VecDeque::new(),
            viewport_offset: 0,
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            attrs: Attrs::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            title: String::from("term"),
            pending_responses: Vec::new(),
            input_buffer: String::new(),
            input_cursor: 0,
            current_dir: std::env::var("HOME").unwrap_or_default(),
            saved_cursor: (0, 0),
            saved_attrs: Attrs::default(),
            wrap_next: false,
        }
    }

    /// Adjust the viewport. Positive = scroll toward older content; negative = toward live.
    pub fn scroll_viewport(&mut self, delta: i32) {
        if delta > 0 {
            self.viewport_offset =
                (self.viewport_offset + delta as usize).min(self.scrollback.len());
        } else {
            self.viewport_offset =
                self.viewport_offset.saturating_sub((-delta) as usize);
        }
    }

    pub fn snap_to_bottom(&mut self) {
        self.viewport_offset = 0;
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.viewport_offset > 0
    }

    /// Return the cell at visual row `row` (0 = top of current viewport), column `col`.
    pub fn visual_cell(&self, row: usize, col: usize) -> Cell {
        let vo = self.viewport_offset.min(self.scrollback.len());
        if row < vo {
            // Row is in the scrollback buffer
            let sb_idx = self.scrollback.len() - vo + row;
            self.scrollback.get(sb_idx)
                .and_then(|r| r.get(col))
                .copied()
                .unwrap_or_default()
        } else {
            // Row is in the live grid
            let grid_row = row - vo;
            self.grid.get(grid_row)
                .and_then(|r| r.get(col))
                .copied()
                .unwrap_or_default()
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        for row in &mut self.grid {
            row.resize(cols, Cell::default());
        }
        self.grid.resize(rows, vec![Cell::default(); cols]);
        self.cols = cols;
        self.rows = rows;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.wrap_next = false;
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let row = self.grid.remove(self.scroll_top);
            // Capture into scrollback only for full-screen scrolls (no active scroll region)
            if self.scroll_top == 0 {
                self.scrollback.push_back(row);
                if self.scrollback.len() > SCROLLBACK_MAX {
                    self.scrollback.pop_front();
                }
                // Keep the viewport pinned to the same content when user is scrolled back
                if self.viewport_offset > 0 {
                    self.viewport_offset =
                        (self.viewport_offset + 1).min(self.scrollback.len());
                }
            }
            let blank = vec![Cell::default(); self.cols];
            let insert_at = self.scroll_bottom.min(self.grid.len());
            self.grid.insert(insert_at, blank);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            let remove_at = self.scroll_bottom.min(self.grid.len().saturating_sub(1));
            self.grid.remove(remove_at);
            let blank = vec![Cell::default(); self.cols];
            self.grid.insert(self.scroll_top, blank);
        }
    }

    fn put_char(&mut self, c: char) {
        if self.wrap_next {
            self.wrap_next = false;
            self.cursor_col = 0;
            self.do_newline();
        }
        if self.cursor_row < self.rows && self.cursor_col < self.cols {
            self.grid[self.cursor_row][self.cursor_col] = Cell { c, attrs: self.attrs };
            if self.cursor_col + 1 >= self.cols {
                self.wrap_next = true;
            } else {
                self.cursor_col += 1;
            }
        }
    }

    fn do_newline(&mut self) {
        if self.cursor_row >= self.scroll_bottom {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    fn blank_cell(&self) -> Cell {
        Cell { c: ' ', attrs: Attrs { fg: DEFAULT_FG, bg: self.attrs.bg, ..Default::default() } }
    }

    fn erase_line(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let row = self.cursor_row;
        let col = self.cursor_col;
        match mode {
            0 => { for c in col..self.cols { self.grid[row][c] = blank; } }
            1 => { for c in 0..=col.min(self.cols.saturating_sub(1)) { self.grid[row][c] = blank; } }
            2 => { for c in 0..self.cols { self.grid[row][c] = blank; } }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let blank = self.blank_cell();
        match mode {
            0 => {
                let (row, col) = (self.cursor_row, self.cursor_col);
                for c in col..self.cols { self.grid[row][c] = blank; }
                for r in (row + 1)..self.rows {
                    for c in 0..self.cols { self.grid[r][c] = blank; }
                }
            }
            1 => {
                let (row, col) = (self.cursor_row, self.cursor_col);
                for r in 0..row { for c in 0..self.cols { self.grid[r][c] = blank; } }
                for c in 0..=col.min(self.cols.saturating_sub(1)) { self.grid[row][c] = blank; }
            }
            2 | 3 => {
                for r in 0..self.rows { for c in 0..self.cols { self.grid[r][c] = blank; } }
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, p: &[u16]) {
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                0  => self.attrs = Attrs::default(),
                1  => self.attrs.bold = true,
                3  => self.attrs.italic = true,
                4  => self.attrs.underline = true,
                7  => self.attrs.inverse = true,
                22 => self.attrs.bold = false,
                23 => self.attrs.italic = false,
                24 => self.attrs.underline = false,
                27 => self.attrs.inverse = false,
                n @ 30..=37 => self.attrs.fg = ANSI_COLORS[(n - 30) as usize],
                38 => {
                    if i + 2 < p.len() && p[i + 1] == 5 {
                        self.attrs.fg = ansi_256_color(p[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < p.len() && p[i + 1] == 2 {
                        self.attrs.fg = Color::new(p[i+2] as u8, p[i+3] as u8, p[i+4] as u8);
                        i += 4;
                    }
                }
                39 => self.attrs.fg = DEFAULT_FG,
                n @ 40..=47 => self.attrs.bg = ANSI_COLORS[(n - 40) as usize],
                48 => {
                    if i + 2 < p.len() && p[i + 1] == 5 {
                        self.attrs.bg = ansi_256_color(p[i + 2] as u8);
                        i += 2;
                    } else if i + 4 < p.len() && p[i + 1] == 2 {
                        self.attrs.bg = Color::new(p[i+2] as u8, p[i+3] as u8, p[i+4] as u8);
                        i += 4;
                    }
                }
                49 => self.attrs.bg = DEFAULT_BG,
                n @ 90..=97  => self.attrs.fg = ANSI_COLORS[(n - 90 + 8) as usize],
                n @ 100..=107 => self.attrs.bg = ANSI_COLORS[(n - 100 + 8) as usize],
                _ => {}
            }
            i += 1;
        }
    }
}

impl Perform for TerminalState {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.wrap_next = false;
        match byte {
            0x08 => { if self.cursor_col > 0 { self.cursor_col -= 1; } }
            0x09 => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols.saturating_sub(1));
            }
            0x0a | 0x0b | 0x0c => self.do_newline(),
            0x0d => self.cursor_col = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let p: Vec<u16> = params.iter().flat_map(|s| s.iter().copied()).collect();
        let p0 = p.first().copied().unwrap_or(0);
        let p1 = p.get(1).copied().unwrap_or(0);

        let inter = intermediates.first().copied().unwrap_or(0);

        match (inter, action) {
            (0, 'A') => {
                let n = p0.max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n).max(self.scroll_top);
            }
            (0, 'B') => {
                let n = p0.max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.scroll_bottom);
            }
            (0, 'C') => {
                let n = p0.max(1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.cols.saturating_sub(1));
            }
            (0, 'D') => {
                let n = p0.max(1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            (0, 'E') => {
                let n = p0.max(1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.rows.saturating_sub(1));
                self.cursor_col = 0;
            }
            (0, 'F') => {
                let n = p0.max(1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
                self.cursor_col = 0;
            }
            (0, 'G') => {
                self.cursor_col = (p0.max(1) as usize - 1).min(self.cols.saturating_sub(1));
            }
            (0, 'H') | (0, 'f') => {
                self.cursor_row = (p0.max(1) as usize - 1).min(self.rows.saturating_sub(1));
                self.cursor_col = (p1.max(1) as usize - 1).min(self.cols.saturating_sub(1));
                self.wrap_next = false;
            }
            (0, 'J') => self.erase_display(p0),
            (0, 'K') => self.erase_line(p0),
            (0, 'L') => {
                let n = p0.max(1) as usize;
                for _ in 0..n {
                    let remove = self.scroll_bottom.min(self.grid.len().saturating_sub(1));
                    self.grid.remove(remove);
                    self.grid.insert(self.cursor_row, vec![Cell::default(); self.cols]);
                }
            }
            (0, 'M') => {
                let n = p0.max(1) as usize;
                for _ in 0..n {
                    if self.cursor_row < self.grid.len() {
                        self.grid.remove(self.cursor_row);
                        let ins = self.scroll_bottom.min(self.grid.len());
                        self.grid.insert(ins, vec![Cell::default(); self.cols]);
                    }
                }
            }
            (0, 'P') => {
                let n = p0.max(1) as usize;
                let row = self.cursor_row;
                let col = self.cursor_col;
                if row < self.rows {
                    for _ in 0..n {
                        if col < self.grid[row].len() {
                            self.grid[row].remove(col);
                            self.grid[row].push(Cell::default());
                        }
                    }
                }
            }
            (0, '@') => {
                let n = p0.max(1) as usize;
                let row = self.cursor_row;
                let col = self.cursor_col;
                for _ in 0..n {
                    if col < self.cols {
                        self.grid[row].insert(col, Cell::default());
                        if self.grid[row].len() > self.cols {
                            self.grid[row].pop();
                        }
                    }
                }
            }
            (0, 'S') => self.scroll_up(p0.max(1) as usize),
            (0, 'T') => self.scroll_down(p0.max(1) as usize),
            (0, 'X') => {
                let blank = self.blank_cell();
                let n = p0.max(1) as usize;
                let row = self.cursor_row;
                let col = self.cursor_col;
                for i in col..(col + n).min(self.cols) {
                    self.grid[row][i] = blank;
                }
            }
            (0, 'd') => {
                self.cursor_row = (p0.max(1) as usize - 1).min(self.rows.saturating_sub(1));
            }
            (0, 'm') => self.apply_sgr(&p),
            (0, 'n') => {
                if p0 == 6 {
                    let resp = format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1);
                    self.pending_responses.push(resp.into_bytes());
                }
            }
            (0, 'r') => {
                let top = p0.max(1) as usize - 1;
                let bot = (if p1 == 0 { self.rows } else { p1 as usize }) - 1;
                if top < bot && bot < self.rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                }
            }
            (0, 's') => {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
                self.saved_attrs = self.attrs;
            }
            (0, 'u') => {
                (self.cursor_row, self.cursor_col) = self.saved_cursor;
                self.attrs = self.saved_attrs;
            }
            // Device attributes
            (0, 'c') => {
                self.pending_responses.push(b"\x1b[?1;2c".to_vec());
            }
            // Private modes — accept but mostly ignore
            (b'?', 'h') | (b'?', 'l') => {}
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => { self.saved_cursor = (self.cursor_row, self.cursor_col); self.saved_attrs = self.attrs; }
            b'8' => { (self.cursor_row, self.cursor_col) = self.saved_cursor; self.attrs = self.saved_attrs; }
            b'D' => self.do_newline(),
            b'E' => { self.cursor_col = 0; self.do_newline(); }
            b'M' => {
                if self.cursor_row <= self.scroll_top {
                    self.scroll_down(1);
                } else {
                    self.cursor_row = self.cursor_row.saturating_sub(1);
                }
            }
            b'c' => *self = TerminalState::new(self.cols, self.rows),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.is_empty() { return; }
        match params[0] {
            b"0" | b"2" => {
                if params.len() >= 2 {
                    if let Ok(s) = std::str::from_utf8(params[1]) {
                        self.title = s.to_string();
                    }
                }
            }
            b"7" => {
                // OSC 7: shell reports current directory as file://hostname/path
                let content = params[1..].iter()
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                let path = content
                    .strip_prefix("file://")
                    .and_then(|s| s.splitn(2, '/').nth(1))
                    .map(|s| format!("/{s}"))
                    .unwrap_or(content);
                if !path.is_empty() { self.current_dir = path; }
            }
            b"9001" => {
                // Shell integration: ZLE hook sends current buffer + cursor.
                // Format: OSC 9001 ; <buffer_with_semicolons_rejoined> \x1c <cursor> ST
                // (vte splits on ";", so we rejoin params[1..] with ";")
                let content = params[1..].iter()
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                // \x1c (ASCII 28, FS) separates buffer from cursor position
                if let Some(sep) = content.find('\x1c') {
                    self.input_buffer = content[..sep].to_string();
                    self.input_cursor = content[sep + 1..].parse().unwrap_or(0);
                } else {
                    self.input_buffer = content;
                    self.input_cursor = self.input_buffer.len();
                }
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

pub struct Terminal {
    parser: vte::Parser,
    pub state: TerminalState,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { parser: vte::Parser::new(), state: TerminalState::new(cols, rows) }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.parser.advance(&mut self.state, b);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }
}
