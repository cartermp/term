use crate::config::*;
use std::collections::VecDeque;
use vte::{Params, Perform};

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
        Self {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Cell {
    pub c: char,
    /// Combining / extending codepoints that form the rest of this grapheme
    /// cluster (e.g. skin-tone modifiers, variation selectors, diacritics).
    /// Zero-initialised; only the first `combining_len` slots are valid.
    combining: [char; 3],
    combining_len: u8,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            combining: ['\0'; 3],
            combining_len: 0,
            attrs: Attrs::default(),
        }
    }
}

impl Cell {
    /// Append a combining codepoint to this grapheme cluster (noop when full).
    pub fn push_combining(&mut self, c: char) {
        if (self.combining_len as usize) < self.combining.len() {
            self.combining[self.combining_len as usize] = c;
            self.combining_len += 1;
        }
    }

    /// The combining / extending codepoints that follow the base character.
    pub fn combining_chars(&self) -> &[char] {
        &self.combining[..self.combining_len as usize]
    }
}

pub struct TerminalState {
    pub grid: Vec<Vec<Cell>>,
    pub scrollback: VecDeque<Vec<Cell>>,
    pub viewport_offset: usize, // 0 = live view; N = scrolled N rows above live bottom
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
    /// Set by OSC 52 `?` query; cleared after the host responds.
    pub osc_52_query: bool,
    /// Whether the app has enabled bracketed paste mode (?2004h).
    pub bracketed_paste: bool,
    /// Whether cursor key application mode is active (?1h = DECCKM).
    /// When true, arrow keys send \x1bO[ABCD] instead of \x1b[[ABCD].
    pub cursor_keys_app_mode: bool,
    saved_cursor: (usize, usize),
    saved_attrs: Attrs,
    wrap_next: bool,
    /// Grid coordinates of the most recently printed cell (for combining-char attachment).
    last_placed: (usize, usize),
    /// True immediately after placing a regional-indicator codepoint (U+1F1E6–U+1F1FF),
    /// so the next regional indicator can be folded into the same cell as a flag emoji.
    last_was_regional_indicator: bool,
    // Alternate screen buffer (?1049h / ?47h)
    alt_screen: bool,
    alt_grid: Vec<Vec<Cell>>,
    /// Cursor position saved on ?1049h entry, restored on ?1049l exit.
    alt_saved_cursor: (usize, usize),
}

impl TerminalState {
    #[allow(dead_code)]
    pub fn is_alt_screen(&self) -> bool {
        self.alt_screen
    }
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
            last_placed: (0, 0),
            last_was_regional_indicator: false,
            alt_screen: false,
            alt_grid: vec![vec![Cell::default(); cols]; rows],
            alt_saved_cursor: (0, 0),
            osc_52_query: false,
            bracketed_paste: false,
            cursor_keys_app_mode: false,
        }
    }

    /// Switch to the alternate screen.  When `save_cursor` is true (?1049h)
    /// the current cursor position is remembered for restoration on exit.
    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt_screen {
            return;
        }
        if save_cursor {
            self.alt_saved_cursor = (self.cursor_row, self.cursor_col);
        }
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        self.alt_screen = true;
        self.viewport_offset = 0;
        // Clear the now-active alt grid
        let blank = Cell::default();
        for row in &mut self.grid {
            row.fill(blank);
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.wrap_next = false;
    }

    /// Switch back to the normal screen.  When `restore_cursor` is true
    /// (?1049l) the cursor is returned to where it was before ?1049h.
    fn leave_alt_screen(&mut self, restore_cursor: bool) {
        if !self.alt_screen {
            return;
        }
        std::mem::swap(&mut self.grid, &mut self.alt_grid);
        self.alt_screen = false;
        if restore_cursor {
            (self.cursor_row, self.cursor_col) = self.alt_saved_cursor;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.wrap_next = false;
    }

    /// Adjust the viewport. Positive = scroll toward older content; negative = toward live.
    pub fn scroll_viewport(&mut self, delta: i32) {
        if delta > 0 {
            self.viewport_offset =
                (self.viewport_offset + delta as usize).min(self.scrollback.len());
        } else {
            self.viewport_offset = self.viewport_offset.saturating_sub((-delta) as usize);
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
            self.scrollback
                .get(sb_idx)
                .and_then(|r| r.get(col))
                .copied()
                .unwrap_or_default()
        } else {
            // Row is in the live grid
            let grid_row = row - vo;
            self.grid
                .get(grid_row)
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
        for row in &mut self.alt_grid {
            row.resize(cols, Cell::default());
        }
        self.alt_grid.resize(rows, vec![Cell::default(); cols]);
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
            // Capture into scrollback only for full-screen scrolls on the normal screen
            if self.scroll_top == 0 && !self.alt_screen {
                self.scrollback.push_back(row);
                if self.scrollback.len() > SCROLLBACK_MAX {
                    self.scrollback.pop_front();
                }
                // Keep the viewport pinned to the same content when user is scrolled back
                if self.viewport_offset > 0 {
                    self.viewport_offset = (self.viewport_offset + 1).min(self.scrollback.len());
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
            self.last_placed = (self.cursor_row, self.cursor_col);
            self.grid[self.cursor_row][self.cursor_col] = Cell {
                c,
                combining: ['\0'; 3],
                combining_len: 0,
                attrs: self.attrs,
            };
            if self.cursor_col + 1 >= self.cols {
                self.wrap_next = true;
            } else {
                self.cursor_col += 1;
            }
        }
    }

    /// Attach a combining codepoint to the most recently placed cell without
    /// advancing the cursor.
    fn extend_last_cluster(&mut self, c: char) {
        let (row, col) = self.last_placed;
        if row < self.rows && col < self.cols {
            self.grid[row][col].push_combining(c);
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
        Cell {
            c: ' ',
            combining: ['\0'; 3],
            combining_len: 0,
            attrs: Attrs {
                fg: DEFAULT_FG,
                bg: self.attrs.bg,
                ..Default::default()
            },
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let row = self.cursor_row;
        let col = self.cursor_col;
        match mode {
            0 => {
                for c in col..self.cols {
                    self.grid[row][c] = blank;
                }
            }
            1 => {
                for c in 0..=col.min(self.cols.saturating_sub(1)) {
                    self.grid[row][c] = blank;
                }
            }
            2 => {
                for c in 0..self.cols {
                    self.grid[row][c] = blank;
                }
            }
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let blank = self.blank_cell();
        match mode {
            0 => {
                let (row, col) = (self.cursor_row, self.cursor_col);
                for c in col..self.cols {
                    self.grid[row][c] = blank;
                }
                for r in (row + 1)..self.rows {
                    for c in 0..self.cols {
                        self.grid[r][c] = blank;
                    }
                }
            }
            1 => {
                let (row, col) = (self.cursor_row, self.cursor_col);
                for r in 0..row {
                    for c in 0..self.cols {
                        self.grid[r][c] = blank;
                    }
                }
                for c in 0..=col.min(self.cols.saturating_sub(1)) {
                    self.grid[row][c] = blank;
                }
            }
            2 | 3 => {
                for r in 0..self.rows {
                    for c in 0..self.cols {
                        self.grid[r][c] = blank;
                    }
                }
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, p: &[u16]) {
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.bold = true,
                3 => self.attrs.italic = true,
                4 => self.attrs.underline = true,
                7 => self.attrs.inverse = true,
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
                        self.attrs.fg = Color::new(p[i + 2] as u8, p[i + 3] as u8, p[i + 4] as u8);
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
                        self.attrs.bg = Color::new(p[i + 2] as u8, p[i + 3] as u8, p[i + 4] as u8);
                        i += 4;
                    }
                }
                49 => self.attrs.bg = DEFAULT_BG,
                n @ 90..=97 => self.attrs.fg = ANSI_COLORS[(n - 90 + 8) as usize],
                n @ 100..=107 => self.attrs.bg = ANSI_COLORS[(n - 100 + 8) as usize],
                _ => {}
            }
            i += 1;
        }
    }
}

// ── Grapheme cluster helpers ──────────────────────────────────────────────────

/// Returns `true` for codepoints that extend a grapheme cluster without
/// advancing the cursor: combining diacritical marks, variation selectors,
/// ZWJ, emoji skin-tone modifiers, flag tag characters, etc.
fn is_grapheme_extend(c: char) -> bool {
    matches!(c,
        // Combining Diacritical Marks (Latin, Greek, Cyrillic…)
        '\u{0300}'..='\u{036F}' |
        // Hebrew combining marks (niqqud, cantillation signs)
        '\u{0591}'..='\u{05BD}' | '\u{05BF}' |
        '\u{05C1}'..='\u{05C2}' | '\u{05C4}'..='\u{05C5}' | '\u{05C7}' |
        // Arabic combining marks (harakat, shadda, etc.)
        '\u{0610}'..='\u{061A}' | '\u{064B}'..='\u{065F}' | '\u{0670}' |
        '\u{06D6}'..='\u{06DC}' | '\u{06DF}'..='\u{06E4}' |
        '\u{06E7}'..='\u{06E8}' | '\u{06EA}'..='\u{06ED}' |
        // Combining Diacritical Marks Extended
        '\u{1AB0}'..='\u{1AFF}' |
        // Combining Diacritical Marks Supplement
        '\u{1DC0}'..='\u{1DFF}' |
        // Combining Diacritical Marks for Symbols
        '\u{20D0}'..='\u{20FF}' |
        // Zero Width Non-Joiner / Zero Width Joiner
        '\u{200C}'..='\u{200D}' |
        // Variation Selectors (text vs. emoji presentation)
        '\u{FE00}'..='\u{FE0F}' |
        // Combining Half Marks
        '\u{FE20}'..='\u{FE2F}' |
        // Emoji skin-tone modifiers (Fitzpatrick scale 1–5)
        '\u{1F3FB}'..='\u{1F3FF}' |
        // Tags used in subdivision flags (England 🏴󠁧󠁢󠁥󠁮󠁧󠁿, Scotland, Wales)
        '\u{E0020}'..='\u{E007F}' |
        // Variation Selectors Supplement
        '\u{E0100}'..='\u{E01EF}'
    )
}

/// Returns `true` for Unicode Regional Indicator letters (🇦–🇿, U+1F1E6–U+1F1FF).
/// Two consecutive regional indicators form a country flag emoji and should
/// share a single cell.
fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

// ─────────────────────────────────────────────────────────────────────────────

impl Perform for TerminalState {
    fn print(&mut self, c: char) {
        if is_grapheme_extend(c) {
            // Zero-width combiner: attach to the previous cell, do not advance cursor.
            self.extend_last_cluster(c);
        } else if is_regional_indicator(c) && self.last_was_regional_indicator {
            // Second regional indicator completes a country flag — fold into same cell.
            self.extend_last_cluster(c);
            self.last_was_regional_indicator = false;
        } else {
            self.put_char(c);
            self.last_was_regional_indicator = is_regional_indicator(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        self.wrap_next = false;
        self.last_was_regional_indicator = false;
        match byte {
            0x08 => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
            }
            0x09 => {
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols.saturating_sub(1));
            }
            0x0a..=0x0c => self.do_newline(),
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
                    self.grid
                        .insert(self.cursor_row, vec![Cell::default(); self.cols]);
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
                self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
                self.cursor_col = self.cursor_col.min(self.cols.saturating_sub(1));
            }
            // Device attributes
            (0, 'c') => {
                self.pending_responses.push(b"\x1b[?1;2c".to_vec());
            }
            // Private modes
            (b'?', 'h') => {
                for &param in &p {
                    match param {
                        1 => self.cursor_keys_app_mode = true,
                        47 | 1047 => self.enter_alt_screen(false),
                        1049 => self.enter_alt_screen(true),
                        2004 => self.bracketed_paste = true,
                        _ => {}
                    }
                }
            }
            (b'?', 'l') => {
                for &param in &p {
                    match param {
                        1 => self.cursor_keys_app_mode = false,
                        47 | 1047 => self.leave_alt_screen(false),
                        1049 => self.leave_alt_screen(true),
                        2004 => self.bracketed_paste = false,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => {
                self.saved_cursor = (self.cursor_row, self.cursor_col);
                self.saved_attrs = self.attrs;
            }
            b'8' => {
                (self.cursor_row, self.cursor_col) = self.saved_cursor;
                self.attrs = self.saved_attrs;
                self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
                self.cursor_col = self.cursor_col.min(self.cols.saturating_sub(1));
            }
            b'D' => self.do_newline(),
            b'E' => {
                self.cursor_col = 0;
                self.do_newline();
            }
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
        if params.is_empty() {
            return;
        }
        match params[0] {
            b"0" | b"2" => {
                if params.len() >= 2
                    && let Ok(s) = std::str::from_utf8(params[1]) {
                        self.title = s.to_string();
                    }
            }
            b"7" => {
                // OSC 7: shell reports current directory as file://hostname/path
                let content = params[1..]
                    .iter()
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                let path = content
                    .strip_prefix("file://")
                    .and_then(|s| s.split_once('/').map(|x| x.1))
                    .map(|s| format!("/{s}"))
                    .unwrap_or(content);
                if !path.is_empty() {
                    self.current_dir = path;
                }
            }
            b"52" => {
                // OSC 52 clipboard access.
                // params: ["52", <selections>, <data>]
                // A "?" payload is a read query; anything else is a write.
                if params.len() >= 3 && params[2] == b"?" {
                    self.osc_52_query = true;
                }
            }
            b"9001" => {
                // Shell integration: ZLE hook sends current buffer + cursor.
                // Format: OSC 9001 ; <buffer_with_semicolons_rejoined> \x1c <cursor> ST
                // (vte splits on ";", so we rejoin params[1..] with ";")
                let content = params[1..]
                    .iter()
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn t(cols: usize, rows: usize) -> Terminal {
        Terminal::new(cols, rows)
    }

    fn ch(term: &Terminal, row: usize, col: usize) -> char {
        term.state.grid[row][col].c
    }

    // ── Basic character output ────────────────────────────────────────────────

    #[test]
    fn print_places_char_and_advances() {
        let mut t = t(80, 24);
        t.process(b"A");
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn two_chars_side_by_side() {
        let mut t = t(80, 24);
        t.process(b"AB");
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 0, 1), 'B');
    }

    // ── Grapheme clustering ───────────────────────────────────────────────────

    #[test]
    fn combining_diacritic_does_not_advance_cursor() {
        // 'e' followed by combining acute (U+0301) should stay in column 0.
        let mut t = t(80, 24);
        t.process("e\u{0301}".as_bytes());
        assert_eq!(ch(&t, 0, 0), 'e');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0301}']);
        assert_eq!(t.state.cursor_col, 1, "cursor must not advance for combiner");
    }

    #[test]
    fn variation_selector_attached_to_base() {
        // Emoji variation selector (U+FE0F) must not occupy a separate cell.
        let mut t = t(80, 24);
        t.process("\u{2764}\u{FE0F}".as_bytes()); // ❤️
        assert_eq!(ch(&t, 0, 0), '\u{2764}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{FE0F}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn skin_tone_modifier_attached_to_base() {
        // Wave hand + medium-dark skin tone (U+1F3FE).
        let mut t = t(80, 24);
        t.process("\u{1F44B}\u{1F3FE}".as_bytes()); // 👋🏾
        assert_eq!(ch(&t, 0, 0), '\u{1F44B}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{1F3FE}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn flag_emoji_two_regional_indicators_share_one_cell() {
        // 🇺🇸 = U+1F1FA U+1F1F8 (two regional indicators).
        let mut t = t(80, 24);
        t.process("\u{1F1FA}\u{1F1F8}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{1F1FA}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{1F1F8}']);
        assert_eq!(t.state.cursor_col, 1, "flag counts as one cell");
    }

    #[test]
    fn third_regional_indicator_starts_new_cell() {
        // Three regional indicators → first flag + one lone RI.
        let mut t = t(80, 24);
        t.process("\u{1F1FA}\u{1F1F8}\u{1F1E9}".as_bytes()); // 🇺🇸🇩
        assert_eq!(ch(&t, 0, 0), '\u{1F1FA}'); // first RI
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{1F1F8}']); // second RI combined
        assert_eq!(ch(&t, 0, 1), '\u{1F1E9}'); // third RI in new cell
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn hebrew_niqqud_does_not_advance_cursor() {
        // Alef (U+05D0) + dagesh (U+05BC) must share a cell.
        let mut t = t(80, 24);
        t.process("\u{05D0}\u{05BC}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{05D0}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{05BC}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn zwj_does_not_advance_cursor() {
        // ZWJ (U+200D) is zero-width and must not create a new cell.
        let mut t = t(80, 24);
        t.process("\u{1F468}\u{200D}".as_bytes()); // man + ZWJ
        assert_eq!(ch(&t, 0, 0), '\u{1F468}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{200D}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn newline_resets_regional_indicator_state() {
        // A lone regional indicator followed by \r\n should not combine with
        // the next regional indicator on the following line.
        let mut t = t(80, 24);
        t.process("\u{1F1FA}\r\n\u{1F1F8}".as_bytes());
        // First RI is alone on row 0
        assert_eq!(ch(&t, 0, 0), '\u{1F1FA}');
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 0);
        // Second RI starts a new cell on row 1
        assert_eq!(ch(&t, 1, 0), '\u{1F1F8}');
    }

    // ── Grapheme clustering — exhaustive ─────────────────────────────────────

    // --- Combining diacritical marks -----------------------------------------

    #[test]
    fn multiple_combiners_on_one_base() {
        // 'a' + combining ring below (U+0325) + combining tilde above (U+0303).
        // Both combiners must land in the same cell; cursor must sit at col 1.
        let mut t = t(80, 24);
        t.process("a\u{0325}\u{0303}".as_bytes());
        assert_eq!(ch(&t, 0, 0), 'a');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0325}', '\u{0303}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn combiner_overflow_is_silent() {
        // Cell holds at most 3 combiners; a 4th is silently dropped rather than
        // panicking or corrupting neighbouring cells.
        let mut t = t(80, 24);
        // U+0300–U+0303 = four combining graves/accents
        t.process("e\u{0300}\u{0301}\u{0302}\u{0303}".as_bytes());
        assert_eq!(ch(&t, 0, 0), 'e');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0300}', '\u{0301}', '\u{0302}']);
        // 4th combiner dropped; cursor still at 1
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn combiner_at_start_of_line_no_panic() {
        // A combining character with no preceding base in the grid should not
        // panic; it is attached to cell (0,0) whose base is still a space.
        let mut t = t(80, 24);
        t.process("\u{0301}".as_bytes()); // lone combining acute
        // Should not panic; cursor must not have moved.
        assert_eq!(t.state.cursor_col, 0);
    }

    #[test]
    fn combiner_after_line_wrap_attaches_to_last_cell_of_previous_row() {
        // Fill a 4-wide terminal so the last char lands at col 3 with wrap_next set,
        // then send a combining mark.  It must attach to col 3, not the next row.
        let mut t = t(4, 4);
        t.process("ABCD\u{0301}".as_bytes()); // 4 ASCII chars fill row 0; combiner follows
        assert_eq!(ch(&t, 0, 3), 'D');
        assert_eq!(t.state.grid[0][3].combining_chars(), &['\u{0301}']);
        assert_eq!(t.state.cursor_row, 0, "wrap must not have fired for the combiner");
    }

    #[test]
    fn two_independent_clusters_on_same_line() {
        // 'a' + acute, then 'b' + grave — two separate base+combiner pairs.
        let mut t = t(80, 24);
        t.process("a\u{0301}b\u{0300}".as_bytes());
        assert_eq!(ch(&t, 0, 0), 'a');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0301}']);
        assert_eq!(ch(&t, 0, 1), 'b');
        assert_eq!(t.state.grid[0][1].combining_chars(), &['\u{0300}']);
        assert_eq!(t.state.cursor_col, 2);
    }

    // --- Variation selectors -------------------------------------------------

    #[test]
    fn text_variation_selector_vs15_attached() {
        // U+FE0E (VS-15) selects text presentation; must not advance cursor.
        let mut t = t(80, 24);
        t.process("\u{2603}\u{FE0E}".as_bytes()); // ☃︎ (snowman, text form)
        assert_eq!(ch(&t, 0, 0), '\u{2603}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{FE0E}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn emoji_variation_selector_vs16_attached() {
        // U+FE0F (VS-16) selects emoji presentation; must not advance cursor.
        let mut t = t(80, 24);
        t.process("\u{2603}\u{FE0F}".as_bytes()); // ☃️ (snowman, emoji form)
        assert_eq!(ch(&t, 0, 0), '\u{2603}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{FE0F}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn variation_selector_supplement_attached() {
        // U+E0100 is the first Variation Selector Supplement codepoint.
        let mut t = t(80, 24);
        t.process("\u{845B}\u{E0100}".as_bytes()); // 葛󠄀 (Unified CJK + IVS)
        assert_eq!(ch(&t, 0, 0), '\u{845B}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{E0100}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Zero Width Joiner ---------------------------------------------------

    #[test]
    fn zwj_sequence_three_parts_share_cell() {
        // Man (U+1F468) + ZWJ (U+200D) + laptop (U+1F4BB) — a "man technologist"
        // ZWJ sequence.  ZWJ and the second emoji are both grapheme extenders
        // here because ZWJ is zero-width; the second emoji after ZWJ is a
        // regular (non-zero-width) codepoint so it gets its own cell.
        // What we test: ZWJ itself does NOT advance the cursor.
        let mut t = t(80, 24);
        t.process("\u{1F468}\u{200D}\u{1F4BB}".as_bytes());
        // man emoji in col 0, ZWJ stored as combiner
        assert_eq!(ch(&t, 0, 0), '\u{1F468}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{200D}']);
        // laptop emoji in col 1 (non-zero-width codepoint after ZWJ)
        assert_eq!(ch(&t, 0, 1), '\u{1F4BB}');
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn zero_width_non_joiner_attached() {
        // U+200C (ZWNJ) is zero-width and must not occupy a separate cell.
        let mut t = t(80, 24);
        t.process("\u{0627}\u{200C}".as_bytes()); // Arabic alef + ZWNJ
        assert_eq!(ch(&t, 0, 0), '\u{0627}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{200C}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Emoji skin-tone modifiers -------------------------------------------

    #[test]
    fn all_five_skin_tones_attached() {
        // Five separate base+modifier pairs on the same row.
        let bases  = ['\u{1F44B}'; 5]; // 👋
        let tones  = ['\u{1F3FB}', '\u{1F3FC}', '\u{1F3FD}', '\u{1F3FE}', '\u{1F3FF}'];
        let mut t = t(80, 24);
        for (col, (&base, &tone)) in bases.iter().zip(tones.iter()).enumerate() {
            let s = format!("{base}{tone}");
            t.process(s.as_bytes());
            assert_eq!(ch(&t, 0, col), base, "col {col} base");
            assert_eq!(t.state.grid[0][col].combining_chars(), &[tone], "col {col} tone");
        }
        assert_eq!(t.state.cursor_col, 5);
    }

    #[test]
    fn skin_tone_without_base_emoji_attaches_to_previous_cell() {
        // A skin-tone modifier sent without a valid preceding emoji still must
        // not advance the cursor — it attaches to whatever cell is at last_placed.
        let mut t = t(80, 24);
        t.process("A\u{1F3FB}".as_bytes()); // ASCII 'A' + skin tone
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{1F3FB}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Flag emoji (regional indicators) ------------------------------------

    #[test]
    fn multiple_flags_in_sequence() {
        // 🇺🇸🇩🇪 — two flags side by side.
        let mut t = t(80, 24);
        t.process("\u{1F1FA}\u{1F1F8}\u{1F1E9}\u{1F1EA}".as_bytes());
        // First flag in col 0
        assert_eq!(ch(&t, 0, 0), '\u{1F1FA}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{1F1F8}']);
        // Second flag in col 1
        assert_eq!(ch(&t, 0, 1), '\u{1F1E9}');
        assert_eq!(t.state.grid[0][1].combining_chars(), &['\u{1F1EA}']);
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn lone_regional_indicator_occupies_one_cell() {
        // A single RI not followed by another RI must still advance the cursor once.
        let mut t = t(80, 24);
        t.process("\u{1F1FA}A".as_bytes()); // RI then regular char
        assert_eq!(ch(&t, 0, 0), '\u{1F1FA}');
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 0);
        assert_eq!(ch(&t, 0, 1), 'A');
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn carriage_return_resets_regional_indicator_state() {
        // CR between two RIs must prevent them from combining into a flag.
        // Sequence: 🇺 CR 🇸
        //   - 🇺 (U+1F1FA) lands at col 0, cursor advances to col 1
        //   - CR resets cursor to col 0 and clears last_was_regional_indicator
        //   - 🇸 (U+1F1F8) overwrites col 0 as a new, independent cell
        let mut t = t(80, 24);
        t.process("\u{1F1FA}\r\u{1F1F8}".as_bytes());
        // Second RI sits alone at col 0 — no combiner from the first RI.
        assert_eq!(ch(&t, 0, 0), '\u{1F1F8}');
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 0);
    }

    // --- Subdivision / tag flags ---------------------------------------------

    #[test]
    fn subdivision_flag_tags_attached() {
        // England flag: black flag (U+1F3F4) + tag sequence + cancel tag (U+E007F).
        // All tag characters (U+E0020–U+E007F) are grapheme extenders.
        // Black flag base = '\u{1F3F4}', followed by: g=E0067 b=E0062 e=E0065 n=E006E g=E0067 + cancel=E007F
        let flag = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        let mut t = t(80, 24);
        t.process(flag.as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{1F3F4}');
        // First three tag chars stored (cell holds max 3 combiners)
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 3);
        assert_eq!(t.state.grid[0][0].combining_chars()[0], '\u{E0067}');
        // All tag chars are combiners → cursor at col 1
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Hebrew --------------------------------------------------------------

    #[test]
    fn hebrew_multiple_niqqud_on_one_letter() {
        // Shin (U+05E9) + shin dot (U+05C1) + dagesh (U+05BC).
        let mut t = t(80, 24);
        t.process("\u{05E9}\u{05C1}\u{05BC}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{05E9}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{05C1}', '\u{05BC}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn hebrew_cantillation_sign_attached() {
        // Alef (U+05D0) + etnahta (U+0591, a cantillation mark).
        let mut t = t(80, 24);
        t.process("\u{05D0}\u{0591}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{05D0}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0591}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn hebrew_word_with_niqqud_cursor_positions() {
        // Spell שָׁלוֹם (shalom) with niqqud: shin+shin-dot+qamats, lamed, vav+holam, mem.
        // Cursor must advance by 4 (one per base letter), not more.
        let word = "\u{05E9}\u{05C1}\u{05B8}\u{05DC}\u{05D5}\u{05B9}\u{05DD}";
        let mut t = t(80, 24);
        t.process(word.as_bytes());
        // col 0: shin with two niqqud
        assert_eq!(ch(&t, 0, 0), '\u{05E9}');
        // col 1: lamed (no niqqud)
        assert_eq!(ch(&t, 0, 1), '\u{05DC}');
        // col 2: vav with holam
        assert_eq!(ch(&t, 0, 2), '\u{05D5}');
        // col 3: mem
        assert_eq!(ch(&t, 0, 3), '\u{05DD}');
        assert_eq!(t.state.cursor_col, 4);
    }

    // --- Arabic --------------------------------------------------------------

    #[test]
    fn arabic_harakat_attached() {
        // Arabic letter ba (U+0628) + fathah (U+064E, a haraka vowel mark).
        let mut t = t(80, 24);
        t.process("\u{0628}\u{064E}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{0628}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{064E}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn arabic_shadda_plus_kasra_attached() {
        // Shadda (U+0651) + kasra (U+0650) stacked on one base letter.
        let mut t = t(80, 24);
        t.process("\u{0628}\u{0651}\u{0650}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{0628}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0651}', '\u{0650}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn arabic_word_with_tashkeel_cursor_positions() {
        // كِتَابٌ (kitabun) — 5 base letters with harakat;
        // cursor must advance by 5 base letters only.
        // k=0643 i=0650 t=062A a=064E A=0627 b=0628 un=064C
        let word = "\u{0643}\u{0650}\u{062A}\u{064E}\u{0627}\u{0628}\u{064C}";
        let mut t = t(80, 24);
        t.process(word.as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{0643}');  // kaf
        assert_eq!(ch(&t, 0, 1), '\u{062A}');  // ta
        assert_eq!(ch(&t, 0, 2), '\u{0627}');  // alef
        assert_eq!(ch(&t, 0, 3), '\u{0628}');  // ba
        assert_eq!(t.state.cursor_col, 4);
    }

    #[test]
    fn arabic_extended_combining_above_attached() {
        // U+0610 (Arabic sign sallallahou alayhe wassallam) is in the Arabic
        // extended combining range U+0610–U+061A.
        let mut t = t(80, 24);
        t.process("\u{0645}\u{0610}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '\u{0645}');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{0610}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Combining Diacritical Marks for Symbols / Half Marks ----------------

    #[test]
    fn combining_half_mark_attached() {
        // U+FE20 (combining ligature left half) is in the Combining Half Marks block.
        let mut t = t(80, 24);
        t.process("f\u{FE20}".as_bytes());
        assert_eq!(ch(&t, 0, 0), 'f');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{FE20}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn combining_enclosing_mark_attached() {
        // U+20DD (combining enclosing circle) is a Combining Diacritical Mark
        // for Symbols (U+20D0–U+20FF).
        let mut t = t(80, 24);
        t.process("1\u{20DD}".as_bytes());
        assert_eq!(ch(&t, 0, 0), '1');
        assert_eq!(t.state.grid[0][0].combining_chars(), &['\u{20DD}']);
        assert_eq!(t.state.cursor_col, 1);
    }

    // --- Non-combining chars still start new cells ---------------------------

    #[test]
    fn regular_ascii_never_combines() {
        // Sanity: plain ASCII letters must never fold into a previous cell.
        let mut t = t(80, 24);
        t.process(b"XYZ");
        assert_eq!(ch(&t, 0, 0), 'X');
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 0);
        assert_eq!(ch(&t, 0, 1), 'Y');
        assert_eq!(ch(&t, 0, 2), 'Z');
        assert_eq!(t.state.cursor_col, 3);
    }

    #[test]
    fn non_modifier_emoji_does_not_combine() {
        // Two unrelated emoji (pizza + rocket) must each occupy their own cell.
        let mut t = t(80, 24);
        t.process("\u{1F355}\u{1F680}".as_bytes()); // 🍕🚀
        assert_eq!(ch(&t, 0, 0), '\u{1F355}');
        assert_eq!(t.state.grid[0][0].combining_chars().len(), 0);
        assert_eq!(ch(&t, 0, 1), '\u{1F680}');
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn crlf_advances_row() {
        let mut t = t(80, 24);
        t.process(b"A\r\nB");
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 1, 0), 'B');
        assert_eq!(t.state.cursor_row, 1);
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn line_wrap_at_right_edge() {
        let mut t = t(4, 4);
        t.process(b"ABCDE"); // 5 chars into a 4-wide terminal
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 0, 3), 'D');
        assert_eq!(ch(&t, 1, 0), 'E');
    }

    #[test]
    fn carriage_return_resets_col() {
        let mut t = t(80, 24);
        t.process(b"ABC\rX");
        assert_eq!(ch(&t, 0, 0), 'X'); // overwrote 'A'
        assert_eq!(t.state.cursor_col, 1);
    }

    #[test]
    fn backspace_moves_cursor_left() {
        let mut t = t(80, 24);
        t.process(b"AB\x08X"); // write AB, backspace, write X
        assert_eq!(ch(&t, 0, 1), 'X');
        assert_eq!(t.state.cursor_col, 2);
    }

    #[test]
    fn tab_advances_to_next_tabstop() {
        let mut t = t(80, 24);
        t.process(b"\t");
        assert_eq!(t.state.cursor_col, 8);
        t.process(b"\t");
        assert_eq!(t.state.cursor_col, 16);
    }

    // ── Scrollback ────────────────────────────────────────────────────────────

    #[test]
    fn scroll_captures_line_to_scrollback() {
        let mut t = t(80, 3);
        // 4 lines → first line scrolls off
        t.process(b"line1\r\nline2\r\nline3\r\nline4");
        assert_eq!(t.state.scrollback.len(), 1);
        let row: String = t.state.scrollback[0]
            .iter()
            .map(|c| c.c)
            .collect::<String>()
            .trim_end_matches(' ')
            .to_string();
        assert_eq!(row, "line1");
    }

    #[test]
    fn scrollback_capped_at_max() {
        let mut t = t(80, 1);
        for _ in 0..SCROLLBACK_MAX + 50 {
            t.process(b"x\r\n");
        }
        assert_eq!(t.state.scrollback.len(), SCROLLBACK_MAX);
    }

    #[test]
    fn scroll_region_does_not_fill_scrollback() {
        // When scroll_top != 0, lines leave the viewport but shouldn't go to scrollback
        let mut t = t(80, 6);
        t.process(b"\x1b[3;6r"); // scroll region rows 3-6 (0-indexed: 2-5)
        let sb_before = t.state.scrollback.len();
        // Force several scrolls within the region
        for _ in 0..5 {
            t.process(b"\x1b[6;1H\r\n"); // cursor to bottom of region + newline
        }
        assert_eq!(t.state.scrollback.len(), sb_before);
    }

    // ── Viewport scrolling ────────────────────────────────────────────────────

    #[test]
    fn scroll_viewport_sets_offset() {
        let mut t = t(80, 3);
        t.process(b"a\r\nb\r\nc\r\nd");
        assert!(!t.state.is_scrolled_back());
        t.state.scroll_viewport(1);
        assert!(t.state.is_scrolled_back());
        assert_eq!(t.state.viewport_offset, 1);
    }

    #[test]
    fn snap_to_bottom_clears_offset() {
        let mut t = t(80, 3);
        t.process(b"a\r\nb\r\nc\r\nd");
        t.state.scroll_viewport(1);
        t.state.snap_to_bottom();
        assert!(!t.state.is_scrolled_back());
        assert_eq!(t.state.viewport_offset, 0);
    }

    #[test]
    fn viewport_clamped_to_scrollback_len() {
        let mut t = t(80, 3);
        t.process(b"a\r\nb\r\nc\r\nd");
        let sb_len = t.state.scrollback.len();
        t.state.scroll_viewport(9999);
        assert_eq!(t.state.viewport_offset, sb_len);
    }

    #[test]
    fn visual_cell_live_view() {
        let mut t = t(80, 3);
        t.process(b"Z");
        assert_eq!(t.state.visual_cell(0, 0).c, 'Z');
    }

    #[test]
    fn visual_cell_shows_scrollback_when_scrolled() {
        let mut t = t(80, 3);
        t.process(b"line1\r\nline2\r\nline3\r\nline4");
        // "line1" is in scrollback; scroll back 1 row
        t.state.scroll_viewport(1);
        assert_eq!(t.state.visual_cell(0, 0).c, 'l'); // first char of "line1"
    }

    // ── Erase ─────────────────────────────────────────────────────────────────

    #[test]
    fn el0_erases_from_cursor_to_eol() {
        let mut t = t(10, 5);
        t.process(b"ABCDE\x1b[1;3H\x1b[K"); // write, move to col 3, EL0
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 0, 1), 'B');
        assert_eq!(ch(&t, 0, 2), ' '); // erased
        assert_eq!(ch(&t, 0, 4), ' '); // erased
    }

    #[test]
    fn el1_erases_from_bol_to_cursor() {
        let mut t = t(10, 5);
        t.process(b"ABCDE\x1b[1;3H\x1b[1K"); // write, move to col 3, EL1
        assert_eq!(ch(&t, 0, 0), ' ');
        assert_eq!(ch(&t, 0, 1), ' ');
        assert_eq!(ch(&t, 0, 2), ' ');
        assert_eq!(ch(&t, 0, 3), 'D'); // not erased
    }

    #[test]
    fn el2_erases_whole_line() {
        let mut t = t(10, 5);
        t.process(b"ABCDE\x1b[1;1H\x1b[2K");
        for col in 0..5 {
            assert_eq!(ch(&t, 0, col), ' ', "col {col}");
        }
    }

    #[test]
    fn ed2_clears_screen() {
        let mut t = t(10, 3);
        t.process(b"AAA\r\nBBB\r\nCCC\x1b[2J");
        for row in 0..3 {
            for col in 0..3 {
                assert_eq!(ch(&t, row, col), ' ', "({row},{col})");
            }
        }
    }

    // ── SGR attributes ────────────────────────────────────────────────────────

    #[test]
    fn sgr_bold_on_off() {
        let mut t = t(80, 24);
        t.process(b"\x1b[1mA\x1b[22mB");
        assert!(t.state.grid[0][0].attrs.bold, "A should be bold");
        assert!(!t.state.grid[0][1].attrs.bold, "B should not be bold");
    }

    #[test]
    fn sgr_inverse_on_off() {
        let mut t = t(80, 24);
        t.process(b"\x1b[7mA\x1b[27mB");
        assert!(t.state.grid[0][0].attrs.inverse);
        assert!(!t.state.grid[0][1].attrs.inverse);
    }

    #[test]
    fn sgr_fg_ansi_color() {
        let mut t = t(80, 24);
        t.process(b"\x1b[31mA"); // ANSI red = index 1
        assert_eq!(
            t.state.grid[0][0].attrs.fg.to_u32(),
            ANSI_COLORS[1].to_u32()
        );
    }

    #[test]
    fn sgr_bg_ansi_color() {
        let mut t = t(80, 24);
        t.process(b"\x1b[41mA"); // ANSI red bg = index 1
        assert_eq!(
            t.state.grid[0][0].attrs.bg.to_u32(),
            ANSI_COLORS[1].to_u32()
        );
    }

    #[test]
    fn sgr_256_fg_color() {
        let mut t = t(80, 24);
        t.process(b"\x1b[38;5;200mA");
        assert_eq!(
            t.state.grid[0][0].attrs.fg.to_u32(),
            ansi_256_color(200).to_u32()
        );
    }

    #[test]
    fn sgr_truecolor_fg() {
        let mut t = t(80, 24);
        t.process(b"\x1b[38;2;10;20;30mA");
        let c = t.state.grid[0][0].attrs.fg;
        assert_eq!((c.r, c.g, c.b), (10, 20, 30));
    }

    #[test]
    fn sgr_truecolor_bg() {
        let mut t = t(80, 24);
        t.process(b"\x1b[48;2;50;60;70mA");
        let c = t.state.grid[0][0].attrs.bg;
        assert_eq!((c.r, c.g, c.b), (50, 60, 70));
    }

    #[test]
    fn sgr_reset_clears_all() {
        let mut t = t(80, 24);
        t.process(b"\x1b[1;3;4;7mA\x1b[0mB");
        let b = t.state.grid[0][1].attrs;
        assert!(!b.bold && !b.italic && !b.underline && !b.inverse);
    }

    #[test]
    fn sgr_bright_fg_uses_high_ansi() {
        let mut t = t(80, 24);
        t.process(b"\x1b[91mA"); // bright red = index 9
        assert_eq!(
            t.state.grid[0][0].attrs.fg.to_u32(),
            ANSI_COLORS[9].to_u32()
        );
    }

    // ── Cursor movement ───────────────────────────────────────────────────────

    #[test]
    fn cup_positions_cursor_1indexed() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;10H");
        assert_eq!(t.state.cursor_row, 4);
        assert_eq!(t.state.cursor_col, 9);
    }

    #[test]
    fn cup_default_params_go_to_home() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;10H\x1b[H");
        assert_eq!(t.state.cursor_row, 0);
        assert_eq!(t.state.cursor_col, 0);
    }

    #[test]
    fn csi_abcd_relative_moves() {
        let mut t = t(80, 24);
        t.process(b"\x1b[10;10H"); // row 9, col 9
        t.process(b"\x1b[2A"); // up 2 → row 7
        assert_eq!(t.state.cursor_row, 7);
        t.process(b"\x1b[3B"); // down 3 → row 10
        assert_eq!(t.state.cursor_row, 10);
        t.process(b"\x1b[4D"); // left 4 → col 5
        assert_eq!(t.state.cursor_col, 5);
        t.process(b"\x1b[2C"); // right 2 → col 7
        assert_eq!(t.state.cursor_col, 7);
    }

    #[test]
    fn cursor_up_clamped_at_scroll_top() {
        let mut t = t(80, 24);
        t.process(b"\x1b[1;1H\x1b[5A"); // at row 0, move up 5
        assert_eq!(t.state.cursor_row, 0);
    }

    #[test]
    fn cha_sets_column() {
        let mut t = t(80, 24);
        t.process(b"\x1b[10;5H\x1b[20G"); // move to col 20 (1-indexed → 19)
        assert_eq!(t.state.cursor_col, 19);
    }

    #[test]
    fn esc_save_restore_cursor() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;10H\x1b7\x1b[1;1H\x1b8");
        assert_eq!(t.state.cursor_row, 4);
        assert_eq!(t.state.cursor_col, 9);
    }

    #[test]
    fn csi_s_u_save_restore_cursor() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;10H\x1b[s\x1b[1;1H\x1b[u");
        assert_eq!(t.state.cursor_row, 4);
        assert_eq!(t.state.cursor_col, 9);
    }

    // ── Scroll region (DECSTBM) ───────────────────────────────────────────────

    #[test]
    fn decstbm_sets_region_and_homes_cursor() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;15r");
        assert_eq!(t.state.scroll_top, 4);
        assert_eq!(t.state.scroll_bottom, 14);
        assert_eq!(t.state.cursor_row, 0);
        assert_eq!(t.state.cursor_col, 0);
    }

    // ── OSC handlers ─────────────────────────────────────────────────────────

    #[test]
    fn osc_0_sets_title() {
        let mut t = t(80, 24);
        t.process(b"\x1b]0;hello world\x07");
        assert_eq!(t.state.title, "hello world");
    }

    #[test]
    fn osc_2_sets_title() {
        let mut t = t(80, 24);
        t.process(b"\x1b]2;my tab\x07");
        assert_eq!(t.state.title, "my tab");
    }

    #[test]
    fn osc_7_sets_cwd() {
        let mut t = t(80, 24);
        t.process(b"\x1b]7;file://localhost/home/user/repos\x07");
        assert_eq!(t.state.current_dir, "/home/user/repos");
    }

    #[test]
    fn osc_9001_sets_input_buffer() {
        // vte strips C0 control chars (including \x1c) from OSC content,
        // so we exercise the no-separator branch — cursor is set to buffer len.
        let mut t = t(80, 24);
        t.process(b"\x1b]9001;hello world\x07");
        assert_eq!(t.state.input_buffer, "hello world");
        assert_eq!(t.state.input_cursor, 11);
    }

    #[test]
    fn osc_9001_separator_parsed_when_injected_directly() {
        // Call osc_dispatch directly (bypassing vte) to test the \x1c branch.
        let mut s = TerminalState::new(80, 24);
        s.osc_dispatch(&[b"9001", b"hello\x1c5"], false);
        assert_eq!(s.input_buffer, "hello");
        assert_eq!(s.input_cursor, 5);
    }

    // ── Device status report ──────────────────────────────────────────────────

    #[test]
    fn dsr_replies_with_cursor_position() {
        let mut t = t(80, 24);
        t.process(b"\x1b[5;10H\x1b[6n");
        assert!(!t.state.pending_responses.is_empty());
        let resp = std::str::from_utf8(&t.state.pending_responses[0]).unwrap();
        assert_eq!(resp, "\x1b[5;10R");
    }

    // ── Insert / delete characters ────────────────────────────────────────────

    #[test]
    fn dch_deletes_chars_at_cursor() {
        let mut t = t(10, 5);
        t.process(b"ABCDE\x1b[1;2H\x1b[2P"); // cursor to col 2, delete 2 chars
        // "ABCDE" → after deleting 2 at col 1: "ADEX  "
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 0, 1), 'D'); // was 'B','C' — both deleted
        assert_eq!(ch(&t, 0, 2), 'E');
        assert_eq!(ch(&t, 0, 3), ' '); // blank filled from right
    }

    #[test]
    fn ich_inserts_blank_at_cursor() {
        let mut t = t(10, 5);
        t.process(b"ABCDE\x1b[1;2H\x1b[2@"); // cursor to col 2, insert 2 blanks
        // "ABCDE" → after inserting 2 blanks at col 1: "A  BCD" (E shifted off)
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 0, 1), ' ');
        assert_eq!(ch(&t, 0, 2), ' ');
        assert_eq!(ch(&t, 0, 3), 'B');
    }

    // ── Resize ────────────────────────────────────────────────────────────────

    #[test]
    fn resize_adjusts_grid_dimensions() {
        let mut t = t(80, 24);
        t.resize(40, 12);
        assert_eq!(t.state.cols, 40);
        assert_eq!(t.state.rows, 12);
        assert_eq!(t.state.grid.len(), 12);
        assert!(t.state.grid.iter().all(|r| r.len() == 40));
    }

    #[test]
    fn resize_clamps_cursor_into_new_bounds() {
        let mut t = t(80, 24);
        t.process(b"\x1b[20;70H"); // cursor at (19, 69)
        t.resize(40, 10);
        assert!(
            t.state.cursor_row < 10,
            "row {} out of bounds",
            t.state.cursor_row
        );
        assert!(
            t.state.cursor_col < 40,
            "col {} out of bounds",
            t.state.cursor_col
        );
    }

    // ── Alternate screen ──────────────────────────────────────────────────────

    /// Feed the ?1049h / ?1049l sequences as the real shell would.
    const ENTER_ALT: &[u8] = b"\x1b[?1049h";
    const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
    const ENTER_ALT47: &[u8] = b"\x1b[?47h";
    const LEAVE_ALT47: &[u8] = b"\x1b[?47l";

    #[test]
    fn alt_screen_enter_sets_flag() {
        let mut t = t(80, 24);
        t.process(ENTER_ALT);
        assert!(t.state.is_alt_screen());
    }

    #[test]
    fn alt_screen_leave_clears_flag() {
        let mut t = t(80, 24);
        t.process(ENTER_ALT);
        t.process(LEAVE_ALT);
        assert!(!t.state.is_alt_screen());
    }

    #[test]
    fn alt_screen_is_cleared_on_entry() {
        let mut t = t(80, 24);
        t.process(b"hello");
        t.process(ENTER_ALT);
        // The alt screen should be blank — row 0, col 0 must be space
        assert_eq!(ch(&t, 0, 0), ' ');
    }

    #[test]
    fn alt_screen_cursor_resets_to_origin_on_entry() {
        let mut t = t(80, 24);
        t.process(b"ABC"); // moves cursor to col 3
        t.process(ENTER_ALT);
        assert_eq!(t.state.cursor_row, 0);
        assert_eq!(t.state.cursor_col, 0);
    }

    #[test]
    fn alt_screen_1049_saves_and_restores_cursor() {
        let mut t = t(80, 24);
        // Position cursor at (2, 5) on the normal screen
        t.process(b"\x1b[3;6H"); // CSI row;col H (1-indexed)
        assert_eq!(t.state.cursor_row, 2);
        assert_eq!(t.state.cursor_col, 5);
        t.process(ENTER_ALT);
        // Move somewhere else in alt screen
        t.process(b"\x1b[10;10H");
        t.process(LEAVE_ALT);
        // Cursor should be back at (2, 5)
        assert_eq!(t.state.cursor_row, 2);
        assert_eq!(t.state.cursor_col, 5);
    }

    #[test]
    fn alt_screen_content_invisible_on_normal_screen() {
        let mut t = t(80, 24);
        t.process(b"NORMAL");
        t.process(ENTER_ALT);
        t.process(b"ALT");
        t.process(LEAVE_ALT);
        // Normal screen row 0 should still start with 'N', not 'A'
        assert_eq!(ch(&t, 0, 0), 'N');
    }

    #[test]
    fn normal_screen_content_preserved_after_alt_exit() {
        let mut t = t(80, 24);
        t.process(b"KEEP");
        t.process(ENTER_ALT);
        t.process(b"DISCARD");
        t.process(LEAVE_ALT);
        assert_eq!(ch(&t, 0, 0), 'K');
        assert_eq!(ch(&t, 0, 1), 'E');
        assert_eq!(ch(&t, 0, 2), 'E');
        assert_eq!(ch(&t, 0, 3), 'P');
    }

    #[test]
    fn alt_screen_does_not_fill_scrollback() {
        let mut t = t(80, 3); // 3-row terminal — easy to force scrolling
        t.process(ENTER_ALT);
        let before = t.state.scrollback.len();
        // Force several scroll events on the alt screen
        t.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5");
        assert_eq!(t.state.scrollback.len(), before, "alt-screen must not add to scrollback");
    }

    #[test]
    fn alt_screen_47h_47l_no_cursor_restore() {
        let mut t = t(80, 24);
        t.process(b"\x1b[3;6H"); // row 2, col 5
        t.process(ENTER_ALT47);
        assert!(t.state.is_alt_screen());
        t.process(b"\x1b[10;10H");
        t.process(LEAVE_ALT47);
        assert!(!t.state.is_alt_screen());
        // ?47 does NOT restore cursor — it should be wherever it was left
        assert_eq!(t.state.cursor_row, 9);
        assert_eq!(t.state.cursor_col, 9);
    }

    #[test]
    fn enter_alt_screen_idempotent() {
        let mut t = t(80, 24);
        t.process(b"CONTENT");
        t.process(ENTER_ALT);
        t.process(b"ALT");
        // Second enter should be a no-op
        t.process(ENTER_ALT);
        assert_eq!(ch(&t, 0, 0), 'A'); // alt content still there
        t.process(LEAVE_ALT);
        assert_eq!(ch(&t, 0, 0), 'C'); // normal content restored
    }

    // ── Bracketed paste mode ──────────────────────────────────────────────────

    #[test]
    fn bracketed_paste_enabled_by_2004h() {
        let mut t = t(80, 24);
        assert!(!t.state.bracketed_paste);
        t.process(b"\x1b[?2004h");
        assert!(t.state.bracketed_paste);
    }

    #[test]
    fn bracketed_paste_disabled_by_2004l() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?2004h");
        t.process(b"\x1b[?2004l");
        assert!(!t.state.bracketed_paste);
    }

    #[test]
    fn bracketed_paste_toggle_independent_of_alt_screen() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?2004h");
        t.process(ENTER_ALT);
        assert!(t.state.bracketed_paste);
        t.process(LEAVE_ALT);
        assert!(t.state.bracketed_paste);
    }

    // ── OSC 52 clipboard query ────────────────────────────────────────────────

    #[test]
    fn osc_52_query_sets_flag() {
        let mut t = t(80, 24);
        assert!(!t.state.osc_52_query);
        // vte splits OSC params on ';', so params will be ["52", "c", "?"]
        t.process(b"\x1b]52;c;?\x07");
        assert!(t.state.osc_52_query);
    }

    #[test]
    fn osc_52_query_flag_via_dispatch() {
        // Direct osc_dispatch call — isolates parsing from vte.
        let mut s = TerminalState::new(80, 24);
        s.osc_dispatch(&[b"52", b"c", b"?"], false);
        assert!(s.osc_52_query);
    }

    #[test]
    fn osc_52_non_query_does_not_set_flag() {
        let mut s = TerminalState::new(80, 24);
        // A write (non-"?") payload must not set the query flag.
        s.osc_dispatch(&[b"52", b"c", b"aGVsbG8="], false);
        assert!(!s.osc_52_query);
    }

    #[test]
    fn osc_52_query_flag_cleared_externally() {
        let mut t = t(80, 24);
        t.process(b"\x1b]52;c;?\x07");
        assert!(t.state.osc_52_query);
        // Simulate host clearing the flag after responding.
        t.state.osc_52_query = false;
        assert!(!t.state.osc_52_query);
    }

    // ── SGR robustness ────────────────────────────────────────────────────────

    #[test]
    fn sgr_256color_fg_missing_index_is_noop() {
        // \x1b[38;5m has no color index — condition `i+2 < p.len()` = `2 < 2` = false.
        let mut t = t(80, 24);
        let default_fg = t.state.attrs.fg;
        t.process(b"\x1b[38;5m");
        assert_eq!(t.state.attrs.fg, default_fg, "incomplete SGR 38;5 must not change fg");
    }

    #[test]
    fn sgr_256color_bg_missing_index_is_noop() {
        let mut t = t(80, 24);
        let default_bg = t.state.attrs.bg;
        t.process(b"\x1b[48;5m");
        assert_eq!(t.state.attrs.bg, default_bg, "incomplete SGR 48;5 must not change bg");
    }

    #[test]
    fn sgr_truecolor_fg_only_one_rgb_component_is_noop() {
        // \x1b[38;2;100m — R provided but G and B missing.
        let mut t = t(80, 24);
        let default_fg = t.state.attrs.fg;
        t.process(b"\x1b[38;2;100m");
        assert_eq!(t.state.attrs.fg, default_fg, "incomplete RGB (only R) must not change fg");
    }

    #[test]
    fn sgr_truecolor_fg_only_two_rgb_components_is_noop() {
        // \x1b[38;2;100;150m — R and G provided but B missing.
        // Condition `i+4 < p.len()` = `4 < 4` = false → skipped.
        let mut t = t(80, 24);
        let default_fg = t.state.attrs.fg;
        t.process(b"\x1b[38;2;100;150m");
        assert_eq!(t.state.attrs.fg, default_fg, "incomplete RGB (no B) must not change fg");
    }

    #[test]
    fn sgr_truecolor_full_rgb_applies() {
        let mut t = t(80, 24);
        t.process(b"\x1b[38;2;100;150;200m");
        assert_eq!(t.state.attrs.fg.r, 100);
        assert_eq!(t.state.attrs.fg.g, 150);
        assert_eq!(t.state.attrs.fg.b, 200);
    }

    // ── Wrap-pending state ────────────────────────────────────────────────────

    #[test]
    fn wrap_next_not_cleared_by_cursor_movement_sequences() {
        // CUP (H) explicitly clears wrap_next. Cursor movement sequences
        // A/B/C/D do not — consistent with xterm behaviour.
        let mut t = t(5, 5);
        t.process(b"\x1b[2;1H"); // cursor to row 1, col 0
        t.process(b"ABCDE");      // fill 5-col row → wrap_next = true
        assert!(t.state.wrap_next);

        // CUP must clear it
        t.process(b"\x1b[2;1H");
        assert!(!t.state.wrap_next, "CUP must clear wrap_next");

        // Fill again
        t.process(b"ABCDE");
        assert!(t.state.wrap_next);

        // CUU does NOT clear it
        t.process(b"\x1b[1A");
        assert!(t.state.wrap_next, "CUU must not affect pending wrap");
    }

    // ── Cursor clamping ───────────────────────────────────────────────────────

    #[test]
    fn cup_out_of_bounds_clamps_to_grid_edges() {
        let mut t = t(80, 24);
        t.process(b"\x1b[999;999H");
        assert_eq!(t.state.cursor_row, 23, "row must clamp to rows-1");
        assert_eq!(t.state.cursor_col, 79, "col must clamp to cols-1");
    }

    #[test]
    fn cursor_up_past_scroll_top_clamps_at_zero() {
        let mut t = t(80, 24);
        t.process(b"\x1b[10;1H"); // row 10 (1-indexed) = row 9
        t.process(b"\x1b[100A");  // up 100 — should clamp at scroll_top (0)
        assert_eq!(t.state.cursor_row, 0);
    }

    #[test]
    fn cursor_down_clamped_at_scroll_bottom() {
        let mut t = t(80, 24);
        t.process(b"\x1b[1;1H"); // row 0
        t.process(b"\x1b[100B"); // down 100 — should clamp at scroll_bottom (23)
        assert_eq!(t.state.cursor_row, 23);
    }

    // ── Cursor save/restore across resize ────────────────────────────────────

    #[test]
    fn cursor_restore_after_resize_clamped_no_panic() {
        // Restore places cursor at a position valid at save time but outside
        // the shrunken grid. The cursor must be clamped to the new bounds.
        let mut t = t(80, 24);
        t.process(b"\x1b[24;80H"); // last cell (row=23, col=79)
        t.process(b"\x1b[s");       // save
        t.resize(40, 12);
        t.process(b"\x1b[u");       // restore — cursor clamped to (11, 39)
        assert_eq!(t.state.cursor_row, 11, "cursor row must clamp to new rows-1");
        assert_eq!(t.state.cursor_col, 39, "cursor col must clamp to new cols-1");
    }

    #[test]
    fn esc_8_restore_after_resize_clamped_no_panic() {
        // Same as above but using ESC 7 / ESC 8 save/restore.
        let mut t = t(80, 24);
        t.process(b"\x1b[24;80H");
        t.process(b"\x1b7");        // ESC 7: save
        t.resize(40, 12);
        t.process(b"\x1b8");        // ESC 8: restore
        assert_eq!(t.state.cursor_row, 11);
        assert_eq!(t.state.cursor_col, 39);
    }

    #[test]
    fn erase_line_after_oob_cursor_does_not_panic() {
        // Without clamping, cursor restore to row 23 in a 12-row grid followed
        // by erase_line would index self.grid[23] and panic.
        let mut t = t(80, 24);
        t.process(b"\x1b[24;6H"); // row=23, col=5 (col is within 40-col bounds)
        t.process(b"\x1b[s");      // save
        t.resize(40, 12);
        t.process(b"\x1b[u");      // restore — clamped to (11, 5)
        t.process(b"\x1b[K");      // erase line — must not panic
        assert_eq!(t.state.grid.len(), 12);
    }

    // ── ECH (erase character) ─────────────────────────────────────────────────

    #[test]
    fn ech_large_count_clamps_to_row_end() {
        let mut t = t(10, 5);
        t.process(b"ABCDEFGHIJ");  // fill row 0 (cols 0–9)
        t.process(b"\x1b[1;6H");  // col 6 (1-indexed) → cursor at col 5
        t.process(b"\x1b[1000X"); // erase 1000 chars — only cols 5–9 remain in range
        assert_eq!(ch(&t, 0, 4), 'E', "col before cursor must be untouched");
        assert_eq!(ch(&t, 0, 5), ' ', "col at cursor must be erased");
        assert_eq!(ch(&t, 0, 9), ' ', "last col must be erased");
    }

    // ── Scroll region behaviour ───────────────────────────────────────────────

    #[test]
    fn scroll_up_restricted_region_leaves_outer_rows_intact() {
        // DECSTBM rows 3–5 (1-indexed) → scroll_top=2, scroll_bottom=4.
        let mut t = t(5, 8);
        for row in 0..8u8 {
            let seq = format!("\x1b[{};1H{}", row + 1, (b'A' + row) as char);
            t.process(seq.as_bytes());
        }
        t.process(b"\x1b[3;5r"); // set scroll region rows 3–5
        t.process(b"\x1b[1S");   // SU: scroll up 1 within region

        // Above region: untouched
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 1, 0), 'B');
        // Region content shifted up by 1; blank inserted at scroll_bottom
        assert_eq!(ch(&t, 2, 0), 'D');
        assert_eq!(ch(&t, 3, 0), 'E');
        assert_eq!(ch(&t, 4, 0), ' ', "scroll_bottom of region must be blank");
        // Below region: untouched
        assert_eq!(ch(&t, 5, 0), 'F');
        assert_eq!(ch(&t, 6, 0), 'G');
        assert_eq!(ch(&t, 7, 0), 'H');
    }

    #[test]
    fn scroll_down_restricted_region_leaves_outer_rows_intact() {
        let mut t = t(5, 8);
        for row in 0..8u8 {
            let seq = format!("\x1b[{};1H{}", row + 1, (b'A' + row) as char);
            t.process(seq.as_bytes());
        }
        t.process(b"\x1b[3;5r"); // scroll region rows 3–5
        t.process(b"\x1b[1T");   // SD: scroll down 1 within region

        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 1, 0), 'B');
        assert_eq!(ch(&t, 2, 0), ' ', "scroll_top of region must be blank");
        assert_eq!(ch(&t, 3, 0), 'C');
        assert_eq!(ch(&t, 4, 0), 'D');
        // 'E' was at scroll_bottom and got pushed off; rows below region untouched
        assert_eq!(ch(&t, 5, 0), 'F');
        assert_eq!(ch(&t, 6, 0), 'G');
        assert_eq!(ch(&t, 7, 0), 'H');
    }

    // ── Erase display edge cases ──────────────────────────────────────────────

    #[test]
    fn ed_mode0_from_last_cell_erases_only_from_cursor() {
        let mut t = t(5, 3);
        t.process(b"\x1b[1;1HABCDE");
        t.process(b"\x1b[2;1HFGHIJ");
        t.process(b"\x1b[3;1HKLMNO");
        t.process(b"\x1b[3;5H");  // cursor at last cell (row=2, col=4)
        t.process(b"\x1b[0J");    // ED mode 0: erase from cursor to end

        assert_eq!(ch(&t, 2, 3), 'N', "cell before cursor must be untouched");
        assert_eq!(ch(&t, 2, 4), ' ', "cursor cell must be erased");
        assert_eq!(ch(&t, 1, 4), 'J', "rows above cursor must be untouched");
    }

    #[test]
    fn ed_mode1_from_first_cell_erases_only_cursor_cell() {
        let mut t = t(5, 3);
        t.process(b"\x1b[1;1HABCDE");
        t.process(b"\x1b[1;1H");  // cursor at (0, 0)
        t.process(b"\x1b[1J");    // ED mode 1: erase from start to cursor (inclusive)

        assert_eq!(ch(&t, 0, 0), ' ', "cursor cell must be erased");
        assert_eq!(ch(&t, 0, 1), 'B', "cells after cursor must be untouched");
    }

    // ── IL / DL within scroll region ─────────────────────────────────────────

    #[test]
    fn il_at_scroll_top_shifts_region_content_down() {
        let mut t = t(5, 8);
        for row in 0..8u8 {
            let seq = format!("\x1b[{};1H{}", row + 1, (b'A' + row) as char);
            t.process(seq.as_bytes());
        }
        t.process(b"\x1b[3;6r"); // scroll region rows 3–6 (scroll_top=2, scroll_bottom=5)
        t.process(b"\x1b[3;1H"); // cursor to scroll_top
        t.process(b"\x1b[1L");   // IL: insert 1 blank line

        assert_eq!(ch(&t, 0, 0), 'A', "above region unchanged");
        assert_eq!(ch(&t, 1, 0), 'B', "above region unchanged");
        assert_eq!(ch(&t, 2, 0), ' ', "blank inserted at cursor (scroll_top)");
        assert_eq!(ch(&t, 3, 0), 'C', "previous scroll_top content shifted down");
        assert_eq!(ch(&t, 4, 0), 'D');
        assert_eq!(ch(&t, 5, 0), 'E', "scroll_bottom now has what was one above");
        assert_eq!(ch(&t, 6, 0), 'G', "below region unchanged");
        assert_eq!(ch(&t, 7, 0), 'H');
    }
}

pub struct Terminal {
    parser: vte::Parser,
    pub state: TerminalState,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            state: TerminalState::new(cols, rows),
        }
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
