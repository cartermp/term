use crate::config::*;
use std::collections::VecDeque;
use vte::{Params, Perform};

const SCROLLBACK_MAX: usize = 10_000;

/// SGR underline style (4:N sub-parameter family).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Straight,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attrs {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline_style: UnderlineStyle,
    pub underline_color: Option<Color>,
    pub inverse: bool,
}

impl Default for Attrs {
    fn default() -> Self {
        Self {
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            italic: false,
            underline_style: UnderlineStyle::None,
            underline_color: None,
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
    /// OSC 8 hyperlink: index into `TerminalState::links` (1-based, 0 = none).
    pub link_id: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            combining: ['\0'; 3],
            combining_len: 0,
            attrs: Attrs::default(),
            link_id: 0,
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
    /// Incremented on every `Terminal::process()` call so callers can detect
    /// when content has changed and invalidate derived caches (e.g. URL spans).
    pub generation: u64,
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
    /// DECSCUSR cursor shape. 0/1=blinking block, 2=steady block,
    /// 3=blinking underline, 4=steady underline, 5=blinking bar, 6=steady bar.
    pub cursor_shape: u8,
    /// Whether focus-in/out events are enabled (?1004h).
    pub focus_tracking: bool,
    /// Whether any mouse tracking mode is active (?1000h/?1002h/?1003h).
    /// When true, scroll wheel events are sent as mouse escape sequences instead
    /// of being handled locally by the terminal.
    pub mouse_tracking: bool,
    /// Whether SGR mouse encoding is active (?1006h).
    /// SGR format: `\x1b[<btn;col;rowM/m`  — supports >223 cols/rows.
    /// When false, the legacy X10 format is used instead.
    pub mouse_sgr: bool,
    /// Whether synchronized-output mode is active (?2026h). When true,
    /// redraws are suppressed until the mode is cleared.
    pub sync_output: bool,
    /// OSC 8 hyperlink URL table. Index 0 is unused; `link_id` in Cell is
    /// 1-based into this vec.
    pub links: Vec<String>,
    /// The `links` index (1-based) for characters currently being printed.
    current_link_id: u16,
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
            generation: 0,
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
            cursor_shape: 0,
            focus_tracking: false,
            mouse_tracking: false,
            mouse_sgr: false,
            sync_output: false,
            links: Vec::new(),
            current_link_id: 0,
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
                link_id: self.current_link_id,
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
            link_id: 0,
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
            2 => {
                for r in 0..self.rows {
                    for c in 0..self.cols {
                        self.grid[r][c] = blank;
                    }
                }
            }
            3 => {
                // ED 3: erase display and clear scrollback (xterm extension).
                for r in 0..self.rows {
                    for c in 0..self.cols {
                        self.grid[r][c] = blank;
                    }
                }
                self.scrollback.clear();
                self.viewport_offset = 0;
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        // Collect parameter groups (each group: [main_param, sub1, sub2, ...]).
        // This preserves sub-parameter structure (e.g. 4:3 for curly underline)
        // while still supporting legacy multi-param forms like 38;2;r;g;b.
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut i = 0;
        while i < groups.len() {
            let g = groups[i];
            let p0 = g.first().copied().unwrap_or(0);
            match p0 {
                0 => self.attrs = Attrs::default(),
                1 => self.attrs.bold = true,
                3 => self.attrs.italic = true,
                4 => {
                    // 4 alone = straight underline; 4:N = specific style
                    self.attrs.underline_style = if g.len() >= 2 {
                        match g[1] {
                            0 => UnderlineStyle::None,
                            1 => UnderlineStyle::Straight,
                            2 => UnderlineStyle::Double,
                            3 => UnderlineStyle::Curly,
                            4 => UnderlineStyle::Dotted,
                            5 => UnderlineStyle::Dashed,
                            _ => UnderlineStyle::Straight,
                        }
                    } else {
                        UnderlineStyle::Straight
                    };
                }
                7 => self.attrs.inverse = true,
                22 => self.attrs.bold = false,
                23 => self.attrs.italic = false,
                24 => self.attrs.underline_style = UnderlineStyle::None,
                27 => self.attrs.inverse = false,
                n @ 30..=37 => self.attrs.fg = ANSI_COLORS[(n - 30) as usize],
                38 => {
                    // Sub-param form: 38:2:r:g:b or 38:5:n
                    if g.len() >= 3 && g[1] == 2 {
                        self.attrs.fg = Color::new(
                            g.get(2).copied().unwrap_or(0) as u8,
                            g.get(3).copied().unwrap_or(0) as u8,
                            g.get(4).copied().unwrap_or(0) as u8,
                        );
                    } else if g.len() >= 3 && g[1] == 5 {
                        self.attrs.fg = ansi_256_color(g[2] as u8);
                    // Legacy form: 38;2;r;g;b
                    } else if i + 4 < groups.len() && groups[i + 1].first().copied() == Some(2) {
                        let r = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        let g_ = groups[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = groups[i + 4].first().copied().unwrap_or(0) as u8;
                        self.attrs.fg = Color::new(r, g_, b);
                        i += 4;
                    // Legacy form: 38;5;n
                    } else if i + 2 < groups.len() && groups[i + 1].first().copied() == Some(5) {
                        let n = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        self.attrs.fg = ansi_256_color(n);
                        i += 2;
                    }
                }
                39 => self.attrs.fg = DEFAULT_FG,
                n @ 40..=47 => self.attrs.bg = ANSI_COLORS[(n - 40) as usize],
                48 => {
                    if g.len() >= 3 && g[1] == 2 {
                        self.attrs.bg = Color::new(
                            g.get(2).copied().unwrap_or(0) as u8,
                            g.get(3).copied().unwrap_or(0) as u8,
                            g.get(4).copied().unwrap_or(0) as u8,
                        );
                    } else if g.len() >= 3 && g[1] == 5 {
                        self.attrs.bg = ansi_256_color(g[2] as u8);
                    } else if i + 4 < groups.len() && groups[i + 1].first().copied() == Some(2) {
                        let r = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        let g_ = groups[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = groups[i + 4].first().copied().unwrap_or(0) as u8;
                        self.attrs.bg = Color::new(r, g_, b);
                        i += 4;
                    } else if i + 2 < groups.len() && groups[i + 1].first().copied() == Some(5) {
                        let n = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        self.attrs.bg = ansi_256_color(n);
                        i += 2;
                    }
                }
                49 => self.attrs.bg = DEFAULT_BG,
                // SGR 58: underline color; 59: reset underline color
                58 => {
                    if g.len() >= 3 && g[1] == 2 {
                        self.attrs.underline_color = Some(Color::new(
                            g.get(2).copied().unwrap_or(0) as u8,
                            g.get(3).copied().unwrap_or(0) as u8,
                            g.get(4).copied().unwrap_or(0) as u8,
                        ));
                    } else if g.len() >= 3 && g[1] == 5 {
                        self.attrs.underline_color = Some(ansi_256_color(g[2] as u8));
                    } else if i + 4 < groups.len() && groups[i + 1].first().copied() == Some(2) {
                        let r = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        let g_ = groups[i + 3].first().copied().unwrap_or(0) as u8;
                        let b = groups[i + 4].first().copied().unwrap_or(0) as u8;
                        self.attrs.underline_color = Some(Color::new(r, g_, b));
                        i += 4;
                    } else if i + 2 < groups.len() && groups[i + 1].first().copied() == Some(5) {
                        let n = groups[i + 2].first().copied().unwrap_or(0) as u8;
                        self.attrs.underline_color = Some(ansi_256_color(n));
                        i += 2;
                    }
                }
                59 => self.attrs.underline_color = None,
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
            (0, 'm') => self.apply_sgr(params),
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
            // DECSCUSR — cursor shape: CSI n SP q  (intermediate = 0x20 = b' ')
            (b' ', 'q') => {
                self.cursor_shape = p0.min(6) as u8;
            }
            // DECSTR — soft terminal reset: CSI ! p
            (b'!', 'p') => self.reset_soft(),
            // Private modes
            (b'?', 'h') => {
                for &param in &p {
                    match param {
                        1 => self.cursor_keys_app_mode = true,
                        1000 | 1002 | 1003 => self.mouse_tracking = true,
                        1004 => self.focus_tracking = true,
                        1006 => self.mouse_sgr = true,
                        47 | 1047 => self.enter_alt_screen(false),
                        1049 => self.enter_alt_screen(true),
                        2004 => self.bracketed_paste = true,
                        2026 => self.sync_output = true,
                        _ => {}
                    }
                }
            }
            (b'?', 'l') => {
                for &param in &p {
                    match param {
                        1 => self.cursor_keys_app_mode = false,
                        1000 | 1002 | 1003 => self.mouse_tracking = false,
                        1004 => self.focus_tracking = false,
                        1006 => self.mouse_sgr = false,
                        47 | 1047 => self.leave_alt_screen(false),
                        1049 => self.leave_alt_screen(true),
                        2004 => self.bracketed_paste = false,
                        2026 => self.sync_output = false,
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
                    && let Ok(s) = std::str::from_utf8(params[1])
                {
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
                const OSC_9001_MAX: usize = 64 * 1024;
                let content = params[1..]
                    .iter()
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                // Guard against maliciously large OSC 9001 payloads (DoS).
                if content.len() > OSC_9001_MAX {
                    return;
                }
                // \x1c (ASCII 28, FS) separates buffer from cursor position
                if let Some(sep) = content.find('\x1c') {
                    self.input_buffer = content[..sep].to_string();
                    self.input_cursor = content[sep + 1..].parse().unwrap_or(0);
                } else {
                    self.input_buffer = content;
                    self.input_cursor = self.input_buffer.len();
                }
            }
            b"8" => {
                // OSC 8 hyperlinks: OSC 8 ; params ; uri BEL
                // vte splits on ";": params[0]="8", params[1]=param_str, params[2]=uri
                if params.len() >= 3 {
                    if let Ok(uri) = std::str::from_utf8(params[2]) {
                        if uri.is_empty() {
                            self.current_link_id = 0;
                        } else if self.links.len() < u16::MAX as usize {
                            self.links.push(uri.to_owned());
                            self.current_link_id = self.links.len() as u16;
                        }
                    }
                } else {
                    self.current_link_id = 0;
                }
            }
            _ => {}
        }
    }
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

impl TerminalState {
    /// Soft terminal reset (DECSTR, `CSI ! p`): resets modes and attributes
    /// without clearing the screen, scrollback, or changing the cursor position.
    pub fn reset_soft(&mut self) {
        self.attrs = Attrs::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.cursor_keys_app_mode = false;
        self.cursor_shape = 0;
        self.mouse_tracking = false;
        self.mouse_sgr = false;
        self.wrap_next = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn t(cols: usize, rows: usize) -> Terminal {
        Terminal::new(cols, rows)
    }

    fn ch(term: &Terminal, row: usize, col: usize) -> char {
        term.state.grid[row][col].c
    }

    #[derive(Debug, PartialEq)]
    struct CellSnapshot {
        c: char,
        combining: Vec<char>,
        attrs: Attrs,
        link_id: u16,
    }

    #[derive(Debug, PartialEq)]
    struct TerminalSnapshot {
        grid: Vec<Vec<CellSnapshot>>,
        scrollback: Vec<Vec<CellSnapshot>>,
        alt_grid: Vec<Vec<CellSnapshot>>,
        viewport_offset: usize,
        cols: usize,
        rows: usize,
        cursor_row: usize,
        cursor_col: usize,
        attrs: Attrs,
        scroll_top: usize,
        scroll_bottom: usize,
        title: String,
        pending_responses: Vec<Vec<u8>>,
        input_buffer: String,
        input_cursor: usize,
        current_dir: String,
        osc_52_query: bool,
        bracketed_paste: bool,
        cursor_keys_app_mode: bool,
        cursor_shape: u8,
        focus_tracking: bool,
        mouse_tracking: bool,
        mouse_sgr: bool,
        sync_output: bool,
        links: Vec<String>,
        current_link_id: u16,
        saved_cursor: (usize, usize),
        saved_attrs: Attrs,
        wrap_next: bool,
        last_placed: (usize, usize),
        last_was_regional_indicator: bool,
        alt_screen: bool,
        alt_saved_cursor: (usize, usize),
    }

    fn snapshot_cell(cell: Cell) -> CellSnapshot {
        CellSnapshot {
            c: cell.c,
            combining: cell.combining_chars().to_vec(),
            attrs: cell.attrs,
            link_id: cell.link_id,
        }
    }

    fn snapshot_rows(rows: &[Vec<Cell>]) -> Vec<Vec<CellSnapshot>> {
        rows.iter()
            .map(|row| row.iter().copied().map(snapshot_cell).collect())
            .collect()
    }

    fn snapshot(term: &Terminal) -> TerminalSnapshot {
        let state = &term.state;
        TerminalSnapshot {
            grid: snapshot_rows(&state.grid),
            scrollback: state
                .scrollback
                .iter()
                .map(|row| row.iter().copied().map(snapshot_cell).collect())
                .collect(),
            alt_grid: snapshot_rows(&state.alt_grid),
            viewport_offset: state.viewport_offset,
            cols: state.cols,
            rows: state.rows,
            cursor_row: state.cursor_row,
            cursor_col: state.cursor_col,
            attrs: state.attrs,
            scroll_top: state.scroll_top,
            scroll_bottom: state.scroll_bottom,
            title: state.title.clone(),
            pending_responses: state.pending_responses.clone(),
            input_buffer: state.input_buffer.clone(),
            input_cursor: state.input_cursor,
            current_dir: state.current_dir.clone(),
            osc_52_query: state.osc_52_query,
            bracketed_paste: state.bracketed_paste,
            cursor_keys_app_mode: state.cursor_keys_app_mode,
            cursor_shape: state.cursor_shape,
            focus_tracking: state.focus_tracking,
            mouse_tracking: state.mouse_tracking,
            mouse_sgr: state.mouse_sgr,
            sync_output: state.sync_output,
            links: state.links.clone(),
            current_link_id: state.current_link_id,
            saved_cursor: state.saved_cursor,
            saved_attrs: state.saved_attrs,
            wrap_next: state.wrap_next,
            last_placed: state.last_placed,
            last_was_regional_indicator: state.last_was_regional_indicator,
            alt_screen: state.alt_screen,
            alt_saved_cursor: state.alt_saved_cursor,
        }
    }

    fn row_text(row: &[Cell]) -> String {
        row.iter()
            .map(|cell| cell.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn process_one_byte_at_a_time(term: &mut Terminal, bytes: &[u8]) {
        for &byte in bytes {
            term.process(std::slice::from_ref(&byte));
        }
    }

    fn process_lcg_chunks(term: &mut Terminal, bytes: &[u8], seed: u32) {
        let mut cursor = 0usize;
        let mut state = seed;
        while cursor < bytes.len() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let chunk_len = ((state >> 16) as usize % 9) + 1;
            let end = (cursor + chunk_len).min(bytes.len());
            term.process(&bytes[cursor..end]);
            cursor = end;
        }
    }

    fn process_chunk_plan(term: &mut Terminal, bytes: &[u8], chunk_lengths: &[usize]) {
        let mut cursor = 0usize;
        for &len in chunk_lengths {
            if cursor >= bytes.len() {
                break;
            }
            let end = (cursor + len.max(1)).min(bytes.len());
            term.process(&bytes[cursor..end]);
            cursor = end;
        }
        if cursor < bytes.len() {
            term.process(&bytes[cursor..]);
        }
    }

    fn assert_chunking_equivalence(cols: usize, rows: usize, bytes: &[u8]) {
        let mut whole = t(cols, rows);
        whole.process(bytes);
        let expected = snapshot(&whole);

        let mut single = t(cols, rows);
        process_one_byte_at_a_time(&mut single, bytes);
        assert_eq!(
            snapshot(&single),
            expected,
            "single-byte chunking changed the final state"
        );

        let mut pseudo_random = t(cols, rows);
        process_lcg_chunks(&mut pseudo_random, bytes, 0xC0FFEE);
        assert_eq!(
            snapshot(&pseudo_random),
            expected,
            "deterministic pseudo-random chunking changed the final state"
        );
    }

    fn assert_terminal_invariants(term: &Terminal) {
        let state = &term.state;
        assert_eq!(state.grid.len(), state.rows);
        assert_eq!(state.alt_grid.len(), state.rows);
        assert!(state.rows > 0);
        assert!(state.cols > 0);
        assert!(state.cursor_row < state.rows, "cursor_row={} rows={}", state.cursor_row, state.rows);
        assert!(state.cursor_col < state.cols, "cursor_col={} cols={}", state.cursor_col, state.cols);
        assert!(state.scroll_top <= state.scroll_bottom);
        assert!(state.scroll_bottom < state.rows);
        assert!(state.viewport_offset <= state.scrollback.len());
        assert!(state.current_link_id as usize <= state.links.len());

        for row in &state.grid {
            assert_eq!(row.len(), state.cols);
            for cell in row {
                assert!(cell.link_id as usize <= state.links.len());
            }
        }
        for row in &state.alt_grid {
            assert_eq!(row.len(), state.cols);
            for cell in row {
                assert!(cell.link_id as usize <= state.links.len());
            }
        }
        for row in &state.scrollback {
            for cell in row {
                assert!(cell.link_id as usize <= state.links.len());
            }
        }
    }

    #[derive(Clone, Debug)]
    enum RandomAction {
        Write(Vec<u8>),
        Resize { cols: usize, rows: usize },
        ScrollViewport(i32),
        SnapToBottom,
    }

    fn random_action_strategy() -> impl Strategy<Value = RandomAction> {
        prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..64).prop_map(RandomAction::Write),
            (1usize..80, 1usize..32)
                .prop_map(|(cols, rows)| RandomAction::Resize { cols, rows }),
            (-200i32..=200).prop_map(RandomAction::ScrollViewport),
            Just(RandomAction::SnapToBottom),
        ]
    }

    fn shell_session_transcript() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]7;file://localhost/Users/alice/src/term\x07");
        bytes.extend_from_slice(b"\x1b]0;~/src/term\x07");
        bytes.extend_from_slice(b"\x1b]9001;git status\x07");
        bytes.extend_from_slice(b"\x1b[?2004h");
        bytes.extend_from_slice(b"~/src/term > git status");
        bytes.extend_from_slice(b"\r\nOn branch main");
        bytes.extend_from_slice(b"\r\nChanges not staged");
        bytes.extend_from_slice(b"\r\n\x1b]8;;https://example.com\x07docs\x1b]8;;\x07");
        bytes.extend_from_slice(b"\r\nready");
        bytes.extend_from_slice(b"\x1b]9001;\x07");
        bytes
    }

    fn alt_screen_program_transcript() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"shell");
        bytes.extend_from_slice(b"\x1b[?1049h");
        bytes.extend_from_slice(b"\x1b]0;vim src/main.rs\x07");
        bytes.extend_from_slice(b"\x1b[?1000h\x1b[?1006h\x1b[?2026h");
        bytes.extend_from_slice(b"\x1b[2 q");
        bytes.extend_from_slice(b"~\r\n~\r\n:help");
        bytes.extend_from_slice(b"\x1b[?2026l\x1b[?1006l\x1b[?1000l");
        bytes.extend_from_slice(b"\x1b[?1049l");
        bytes.extend_from_slice(b"\x1b]0;~/src/term\x07");
        bytes
    }

    proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(64))]

        #[test]
        fn arbitrary_byte_stream_chunking_is_stream_equivalent(
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            chunk_lengths in proptest::collection::vec(1usize..32, 0..128),
        ) {
            let mut whole = t(48, 16);
            whole.process(&bytes);
            assert_terminal_invariants(&whole);
            let expected = snapshot(&whole);

            let mut chunked = t(48, 16);
            process_chunk_plan(&mut chunked, &bytes, &chunk_lengths);
            assert_terminal_invariants(&chunked);

            prop_assert_eq!(snapshot(&chunked), expected);
        }

        #[test]
        fn random_terminal_action_sequences_preserve_invariants(
            actions in proptest::collection::vec(random_action_strategy(), 0..128),
        ) {
            let mut term = t(48, 16);
            for action in actions {
                match action {
                    RandomAction::Write(bytes) => term.process(&bytes),
                    RandomAction::Resize { cols, rows } => term.resize(cols, rows),
                    RandomAction::ScrollViewport(delta) => term.state.scroll_viewport(delta),
                    RandomAction::SnapToBottom => term.state.snap_to_bottom(),
                }
                assert_terminal_invariants(&term);
            }
        }
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
        assert_eq!(
            t.state.cursor_col, 1,
            "cursor must not advance for combiner"
        );
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
        assert_eq!(
            t.state.grid[0][0].combining_chars(),
            &['\u{0325}', '\u{0303}']
        );
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
        assert_eq!(
            t.state.grid[0][0].combining_chars(),
            &['\u{0300}', '\u{0301}', '\u{0302}']
        );
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
        assert_eq!(
            t.state.cursor_row, 0,
            "wrap must not have fired for the combiner"
        );
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
        let bases = ['\u{1F44B}'; 5]; // 👋
        let tones = [
            '\u{1F3FB}',
            '\u{1F3FC}',
            '\u{1F3FD}',
            '\u{1F3FE}',
            '\u{1F3FF}',
        ];
        let mut t = t(80, 24);
        for (col, (&base, &tone)) in bases.iter().zip(tones.iter()).enumerate() {
            let s = format!("{base}{tone}");
            t.process(s.as_bytes());
            assert_eq!(ch(&t, 0, col), base, "col {col} base");
            assert_eq!(
                t.state.grid[0][col].combining_chars(),
                &[tone],
                "col {col} tone"
            );
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
        assert_eq!(
            t.state.grid[0][0].combining_chars(),
            &['\u{05C1}', '\u{05BC}']
        );
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
        assert_eq!(
            t.state.grid[0][0].combining_chars(),
            &['\u{0651}', '\u{0650}']
        );
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
        assert_eq!(ch(&t, 0, 0), '\u{0643}'); // kaf
        assert_eq!(ch(&t, 0, 1), '\u{062A}'); // ta
        assert_eq!(ch(&t, 0, 2), '\u{0627}'); // alef
        assert_eq!(ch(&t, 0, 3), '\u{0628}'); // ba
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
    fn shell_session_transcript_is_chunk_boundary_stable() {
        let transcript = shell_session_transcript();
        assert_chunking_equivalence(40, 4, &transcript);
    }

    #[test]
    fn shell_session_transcript_reaches_expected_state() {
        let transcript = shell_session_transcript();
        let mut t = t(40, 4);
        t.process(&transcript);

        assert_eq!(t.state.current_dir, "/Users/alice/src/term");
        assert_eq!(t.state.title, "~/src/term");
        assert_eq!(t.state.input_buffer, "");
        assert_eq!(t.state.input_cursor, 0);
        assert!(t.state.bracketed_paste);
        assert_eq!(t.state.scrollback.len(), 1);
        assert_eq!(row_text(&t.state.scrollback[0]), "~/src/term > git status");
        assert_eq!(row_text(&t.state.grid[0]), "On branch main");
        assert_eq!(row_text(&t.state.grid[1]), "Changes not staged");
        assert_eq!(row_text(&t.state.grid[2]), "docs");
        assert_eq!(row_text(&t.state.grid[3]), "ready");
        assert_eq!(t.state.links, vec!["https://example.com"]);
        for col in 0..4 {
            assert_eq!(t.state.grid[2][col].link_id, 1, "col {col}");
        }
        assert_eq!(t.state.current_link_id, 0);
    }

    #[test]
    fn alt_screen_program_transcript_is_chunk_boundary_stable() {
        let transcript = alt_screen_program_transcript();
        assert_chunking_equivalence(20, 4, &transcript);
    }

    #[test]
    fn alt_screen_program_transcript_restores_normal_screen() {
        let transcript = alt_screen_program_transcript();
        let mut t = t(20, 4);
        t.process(&transcript);

        assert!(!t.state.is_alt_screen());
        assert_eq!(t.state.title, "~/src/term");
        assert_eq!(t.state.cursor_row, 0);
        assert_eq!(t.state.cursor_col, 5);
        assert!(!t.state.mouse_tracking);
        assert!(!t.state.mouse_sgr);
        assert!(!t.state.sync_output);
        assert_eq!(t.state.cursor_shape, 2);
        assert_eq!(row_text(&t.state.grid[0]), "shell");
        assert_eq!(row_text(&t.state.alt_grid[0]), "~");
        assert_eq!(row_text(&t.state.alt_grid[1]), "~");
        assert_eq!(row_text(&t.state.alt_grid[2]), ":help");
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
        assert!(!b.bold && !b.italic && b.underline_style == UnderlineStyle::None && !b.inverse);
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

    #[test]
    fn osc_9001_oversized_payload_is_ignored() {
        let mut s = TerminalState::new(80, 24);
        // Seed a known value first.
        s.osc_dispatch(&[b"9001", b"prior\x1c0"], false);
        assert_eq!(s.input_buffer, "prior");
        // Send a payload larger than 64 KiB — input_buffer must be unchanged.
        let huge = vec![b'x'; 65 * 1024];
        s.osc_dispatch(&[b"9001", &huge], false);
        assert_eq!(
            s.input_buffer, "prior",
            "oversized OSC 9001 must not overwrite input_buffer"
        );
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
        assert_eq!(
            t.state.scrollback.len(),
            before,
            "alt-screen must not add to scrollback"
        );
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
        assert_eq!(
            t.state.attrs.fg, default_fg,
            "incomplete SGR 38;5 must not change fg"
        );
    }

    #[test]
    fn sgr_256color_bg_missing_index_is_noop() {
        let mut t = t(80, 24);
        let default_bg = t.state.attrs.bg;
        t.process(b"\x1b[48;5m");
        assert_eq!(
            t.state.attrs.bg, default_bg,
            "incomplete SGR 48;5 must not change bg"
        );
    }

    #[test]
    fn sgr_truecolor_fg_only_one_rgb_component_is_noop() {
        // \x1b[38;2;100m — R provided but G and B missing.
        let mut t = t(80, 24);
        let default_fg = t.state.attrs.fg;
        t.process(b"\x1b[38;2;100m");
        assert_eq!(
            t.state.attrs.fg, default_fg,
            "incomplete RGB (only R) must not change fg"
        );
    }

    #[test]
    fn sgr_truecolor_fg_only_two_rgb_components_is_noop() {
        // \x1b[38;2;100;150m — R and G provided but B missing.
        // Condition `i+4 < p.len()` = `4 < 4` = false → skipped.
        let mut t = t(80, 24);
        let default_fg = t.state.attrs.fg;
        t.process(b"\x1b[38;2;100;150m");
        assert_eq!(
            t.state.attrs.fg, default_fg,
            "incomplete RGB (no B) must not change fg"
        );
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
        t.process(b"ABCDE"); // fill 5-col row → wrap_next = true
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
        t.process(b"\x1b[100A"); // up 100 — should clamp at scroll_top (0)
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
        t.process(b"\x1b[s"); // save
        t.resize(40, 12);
        t.process(b"\x1b[u"); // restore — cursor clamped to (11, 39)
        assert_eq!(
            t.state.cursor_row, 11,
            "cursor row must clamp to new rows-1"
        );
        assert_eq!(
            t.state.cursor_col, 39,
            "cursor col must clamp to new cols-1"
        );
    }

    #[test]
    fn esc_8_restore_after_resize_clamped_no_panic() {
        // Same as above but using ESC 7 / ESC 8 save/restore.
        let mut t = t(80, 24);
        t.process(b"\x1b[24;80H");
        t.process(b"\x1b7"); // ESC 7: save
        t.resize(40, 12);
        t.process(b"\x1b8"); // ESC 8: restore
        assert_eq!(t.state.cursor_row, 11);
        assert_eq!(t.state.cursor_col, 39);
    }

    #[test]
    fn erase_line_after_oob_cursor_does_not_panic() {
        // Without clamping, cursor restore to row 23 in a 12-row grid followed
        // by erase_line would index self.grid[23] and panic.
        let mut t = t(80, 24);
        t.process(b"\x1b[24;6H"); // row=23, col=5 (col is within 40-col bounds)
        t.process(b"\x1b[s"); // save
        t.resize(40, 12);
        t.process(b"\x1b[u"); // restore — clamped to (11, 5)
        t.process(b"\x1b[K"); // erase line — must not panic
        assert_eq!(t.state.grid.len(), 12);
    }

    // ── ECH (erase character) ─────────────────────────────────────────────────

    #[test]
    fn ech_large_count_clamps_to_row_end() {
        let mut t = t(10, 5);
        t.process(b"ABCDEFGHIJ"); // fill row 0 (cols 0–9)
        t.process(b"\x1b[1;6H"); // col 6 (1-indexed) → cursor at col 5
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
        t.process(b"\x1b[1S"); // SU: scroll up 1 within region

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
        t.process(b"\x1b[1T"); // SD: scroll down 1 within region

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
        t.process(b"\x1b[3;5H"); // cursor at last cell (row=2, col=4)
        t.process(b"\x1b[0J"); // ED mode 0: erase from cursor to end

        assert_eq!(ch(&t, 2, 3), 'N', "cell before cursor must be untouched");
        assert_eq!(ch(&t, 2, 4), ' ', "cursor cell must be erased");
        assert_eq!(ch(&t, 1, 4), 'J', "rows above cursor must be untouched");
    }

    #[test]
    fn ed_mode1_from_first_cell_erases_only_cursor_cell() {
        let mut t = t(5, 3);
        t.process(b"\x1b[1;1HABCDE");
        t.process(b"\x1b[1;1H"); // cursor at (0, 0)
        t.process(b"\x1b[1J"); // ED mode 1: erase from start to cursor (inclusive)

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
        t.process(b"\x1b[1L"); // IL: insert 1 blank line

        assert_eq!(ch(&t, 0, 0), 'A', "above region unchanged");
        assert_eq!(ch(&t, 1, 0), 'B', "above region unchanged");
        assert_eq!(ch(&t, 2, 0), ' ', "blank inserted at cursor (scroll_top)");
        assert_eq!(
            ch(&t, 3, 0),
            'C',
            "previous scroll_top content shifted down"
        );
        assert_eq!(ch(&t, 4, 0), 'D');
        assert_eq!(
            ch(&t, 5, 0),
            'E',
            "scroll_bottom now has what was one above"
        );
        assert_eq!(ch(&t, 6, 0), 'G', "below region unchanged");
        assert_eq!(ch(&t, 7, 0), 'H');
    }

    // ── SGR italic / underline ────────────────────────────────────────────────

    #[test]
    fn sgr_italic_on_off() {
        let mut t = t(10, 5);
        t.process(b"\x1b[3m");
        assert!(t.state.attrs.italic, "SGR 3 must set italic");
        t.process(b"\x1b[23m");
        assert!(!t.state.attrs.italic, "SGR 23 must clear italic");
    }

    #[test]
    fn sgr_underline_on_off() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4m");
        assert_eq!(
            t.state.attrs.underline_style,
            UnderlineStyle::Straight,
            "SGR 4 must set straight underline"
        );
        t.process(b"\x1b[24m");
        assert_eq!(
            t.state.attrs.underline_style,
            UnderlineStyle::None,
            "SGR 24 must clear underline"
        );
    }

    #[test]
    fn sgr_fg_default_reset_restores_default_fg() {
        let mut t = t(10, 5);
        t.process(b"\x1b[31m"); // red fg
        assert_ne!(t.state.attrs.fg, DEFAULT_FG);
        t.process(b"\x1b[39m"); // reset fg
        assert_eq!(
            t.state.attrs.fg, DEFAULT_FG,
            "SGR 39 must restore default fg"
        );
    }

    #[test]
    fn sgr_bg_default_reset_restores_default_bg() {
        let mut t = t(10, 5);
        t.process(b"\x1b[41m"); // red bg
        assert_ne!(t.state.attrs.bg, DEFAULT_BG);
        t.process(b"\x1b[49m"); // reset bg
        assert_eq!(
            t.state.attrs.bg, DEFAULT_BG,
            "SGR 49 must restore default bg"
        );
    }

    #[test]
    fn sgr_bright_bg_100_to_107_uses_high_ansi() {
        let mut t = t(10, 5);
        for n in 0u8..8 {
            t.process(format!("\x1b[{}m", 100 + n).as_bytes());
            assert_eq!(
                t.state.attrs.bg,
                ANSI_COLORS[(n + 8) as usize],
                "SGR {} must use ANSI_COLORS[{}]",
                100 + n,
                n + 8
            );
        }
    }

    // ── CSI E / F (cursor next/previous line) ─────────────────────────────────

    #[test]
    fn cnl_moves_cursor_down_and_to_col_zero() {
        let mut t = t(10, 10);
        t.process(b"\x1b[5;5H"); // row 5, col 5
        t.process(b"\x1b[2E"); // CNL 2 — down 2, col 0
        assert_eq!(t.state.cursor_row, 6, "CNL 2 from row 4 must land at row 6");
        assert_eq!(t.state.cursor_col, 0, "CNL must reset col to 0");
    }

    #[test]
    fn cpl_moves_cursor_up_and_to_col_zero() {
        let mut t = t(10, 10);
        t.process(b"\x1b[5;5H"); // row 5, col 5
        t.process(b"\x1b[2F"); // CPL 2 — up 2, col 0
        assert_eq!(t.state.cursor_row, 2, "CPL 2 from row 4 must land at row 2");
        assert_eq!(t.state.cursor_col, 0, "CPL must reset col to 0");
    }

    #[test]
    fn cpl_clamped_at_top() {
        let mut t = t(10, 10);
        t.process(b"\x1b[2;5H"); // row 2, col 5
        t.process(b"\x1b[100F"); // CPL 100 — should clamp at row 0
        assert_eq!(t.state.cursor_row, 0, "CPL past top must clamp at row 0");
        assert_eq!(t.state.cursor_col, 0);
    }

    // ── CSI d (VPA — vertical position absolute) ──────────────────────────────

    #[test]
    fn vpa_sets_row_1indexed() {
        let mut t = t(10, 10);
        t.process(b"\x1b[5d"); // VPA 5 → row 4 (0-indexed)
        assert_eq!(t.state.cursor_row, 4, "VPA 5 must set cursor_row to 4");
    }

    #[test]
    fn vpa_default_goes_to_row_zero() {
        let mut t = t(10, 10);
        t.process(b"\x1b[5;1H"); // move away first
        t.process(b"\x1b[1d"); // VPA 1 → row 0
        assert_eq!(t.state.cursor_row, 0);
    }

    #[test]
    fn vpa_clamped_to_last_row() {
        let mut t = t(10, 5);
        t.process(b"\x1b[100d"); // beyond last row
        assert_eq!(t.state.cursor_row, 4, "VPA past last row must clamp");
    }

    // ── CSI S / T (scroll up / down) ─────────────────────────────────────────

    #[test]
    fn su_scroll_up_discards_top_lines() {
        let mut t = t(5, 4);
        t.process(b"A\r\nB\r\nC\r\nD");
        t.process(b"\x1b[1S"); // SU 1
        // After scrolling up 1, old row 1 ("B") is now row 0.
        assert_eq!(ch(&t, 0, 0), 'B', "after SU 1, row 0 must be 'B'");
        assert_eq!(ch(&t, 1, 0), 'C');
        assert_eq!(ch(&t, 2, 0), 'D');
        assert_eq!(ch(&t, 3, 0), ' ', "new bottom row must be blank");
    }

    #[test]
    fn sd_scroll_down_inserts_blank_at_top() {
        let mut t = t(5, 4);
        t.process(b"A\r\nB\r\nC\r\nD");
        t.process(b"\x1b[1T"); // SD 1
        // After scrolling down 1, blank row inserted at top.
        assert_eq!(ch(&t, 0, 0), ' ', "after SD 1, row 0 must be blank");
        assert_eq!(ch(&t, 1, 0), 'A');
        assert_eq!(ch(&t, 2, 0), 'B');
        assert_eq!(ch(&t, 3, 0), 'C');
    }

    // ── CSI M (DL — delete line) ──────────────────────────────────────────────

    #[test]
    fn dl_deletes_line_at_cursor() {
        let mut t = t(5, 4);
        t.process(b"A\r\nB\r\nC\r\nD");
        t.process(b"\x1b[2;1H"); // cursor to row 1 (0-indexed)
        t.process(b"\x1b[1M"); // DL 1
        // Row "B" deleted; "C","D" shift up.
        assert_eq!(ch(&t, 0, 0), 'A');
        assert_eq!(ch(&t, 1, 0), 'C', "DL must shift remaining rows up");
        assert_eq!(ch(&t, 2, 0), 'D');
        assert_eq!(ch(&t, 3, 0), ' ', "new bottom row must be blank");
    }

    // ── ESC D / E / M (IND / NEL / RI) ───────────────────────────────────────

    #[test]
    fn esc_d_index_advances_row() {
        let mut t = t(5, 5);
        t.process(b"\x1b[2;3H"); // row 1, col 2
        t.process(b"\x1bD"); // IND — newline without CR
        assert_eq!(t.state.cursor_row, 2, "ESC D must advance row");
        assert_eq!(t.state.cursor_col, 2, "ESC D must not change col");
    }

    #[test]
    fn esc_e_next_line_advances_row_and_resets_col() {
        let mut t = t(5, 5);
        t.process(b"\x1b[2;3H"); // row 1, col 2
        t.process(b"\x1bE"); // NEL
        assert_eq!(t.state.cursor_row, 2, "ESC E must advance row");
        assert_eq!(t.state.cursor_col, 0, "ESC E must reset col to 0");
    }

    #[test]
    fn esc_m_reverse_index_moves_up() {
        let mut t = t(5, 5);
        t.process(b"\x1b[3;1H"); // row 2
        t.process(b"\x1bM"); // RI — reverse index
        assert_eq!(t.state.cursor_row, 1, "ESC M must move cursor up");
    }

    #[test]
    fn esc_m_at_scroll_top_scrolls_down() {
        // At the scroll top, RI inserts a blank line (scrolls content down).
        let mut t = t(5, 4);
        t.process(b"A\r\nB\r\nC\r\nD");
        t.process(b"\x1b[1;1H"); // cursor to row 0 (scroll_top)
        t.process(b"\x1bM");
        assert_eq!(ch(&t, 0, 0), ' ', "RI at top must insert blank row");
        assert_eq!(ch(&t, 1, 0), 'A', "previous row 0 shifts down");
    }

    // ── ESC c (hard reset) ────────────────────────────────────────────────────

    #[test]
    fn esc_c_resets_to_fresh_state() {
        let mut t = t(10, 5);
        // Set some state.
        t.process(b"\x1b[31m"); // red fg
        t.process(b"\x1b[5;5H"); // move cursor
        t.process(b"HELLO");
        t.process(b"\x1bc"); // RIS — hard reset
        assert_eq!(t.state.cursor_row, 0, "RIS must reset cursor to origin");
        assert_eq!(t.state.cursor_col, 0);
        assert_eq!(
            t.state.attrs.fg, DEFAULT_FG,
            "RIS must reset SGR attributes"
        );
        assert_eq!(ch(&t, 0, 0), ' ', "RIS must clear screen");
    }

    // ── CSI c (device attributes) ─────────────────────────────────────────────

    #[test]
    fn csi_c_queues_device_attributes_response() {
        let mut t = t(10, 5);
        t.process(b"\x1b[c");
        assert!(
            !t.state.pending_responses.is_empty(),
            "CSI c must queue a device attributes response"
        );
        let resp = &t.state.pending_responses[0];
        assert_eq!(
            resp, b"\x1b[?1;2c",
            "device attributes response must be ESC[?1;2c"
        );
    }

    // ── Cursor keys application mode ──────────────────────────────────────────

    #[test]
    fn cursor_keys_app_mode_enabled_by_1h() {
        let mut t = t(10, 5);
        assert!(!t.state.cursor_keys_app_mode);
        t.process(b"\x1b[?1h");
        assert!(t.state.cursor_keys_app_mode, "?1h must enable app mode");
    }

    #[test]
    fn cursor_keys_app_mode_disabled_by_1l() {
        let mut t = t(10, 5);
        t.process(b"\x1b[?1h");
        t.process(b"\x1b[?1l");
        assert!(!t.state.cursor_keys_app_mode, "?1l must disable app mode");
    }

    // ── OSC 7 hostname stripping ──────────────────────────────────────────────

    #[test]
    fn osc_7_strips_hostname_from_file_url() {
        let mut t = t(10, 5);
        // OSC 7 ; file://hostname/home/user ST
        t.process(b"\x1b]7;file://mymac/home/user\x07");
        assert_eq!(
            t.state.current_dir, "/home/user",
            "OSC 7 must strip the hostname and keep the path"
        );
    }

    #[test]
    fn osc_7_empty_host_preserves_path() {
        let mut t = t(10, 5);
        t.process(b"\x1b]7;file:///tmp/work\x07");
        assert_eq!(t.state.current_dir, "/tmp/work");
    }

    #[test]
    fn osc_7_non_file_url_used_verbatim() {
        // No file:// prefix → content used as-is.
        let mut t = t(10, 5);
        t.process(b"\x1b]7;/just/a/path\x07");
        assert_eq!(t.state.current_dir, "/just/a/path");
    }

    // ── SGR: simultaneous bold + italic + underline ───────────────────────────

    #[test]
    fn sgr_combined_bold_italic_underline() {
        let mut t = t(10, 5);
        t.process(b"\x1b[1;3;4m");
        assert!(t.state.attrs.bold);
        assert!(t.state.attrs.italic);
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::Straight);
        // Reset clears all three.
        t.process(b"\x1b[0m");
        assert!(!t.state.attrs.bold);
        assert!(!t.state.attrs.italic);
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::None);
    }

    // ── CUF/CUB with explicit count ───────────────────────────────────────────

    #[test]
    fn cuf_with_count_moves_right_n_cols() {
        let mut t = t(20, 5);
        t.process(b"\x1b[1;1H"); // col 0
        t.process(b"\x1b[5C"); // CUF 5
        assert_eq!(t.state.cursor_col, 5);
    }

    #[test]
    fn cub_with_count_moves_left_n_cols() {
        let mut t = t(20, 5);
        t.process(b"\x1b[1;10H"); // col 9
        t.process(b"\x1b[3D"); // CUB 3
        assert_eq!(t.state.cursor_col, 6);
    }

    #[test]
    fn cuf_clamped_at_last_col() {
        let mut t = t(10, 5);
        t.process(b"\x1b[1;1H");
        t.process(b"\x1b[100C");
        assert_eq!(t.state.cursor_col, 9, "CUF past last col must clamp");
    }

    #[test]
    fn cub_clamped_at_col_zero() {
        let mut t = t(10, 5);
        t.process(b"\x1b[1;5H"); // col 4
        t.process(b"\x1b[100D"); // CUB 100
        assert_eq!(t.state.cursor_col, 0, "CUB past col 0 must clamp at 0");
    }

    // ── ED mode 3 (clear scrollback) ──────────────────────────────────────────

    #[test]
    fn ed3_clears_scrollback_buffer() {
        let mut t = t(5, 3);
        // Fill scrollback by printing more lines than the terminal height.
        for i in 0..10 {
            t.process(format!("line{i}\r\n").as_bytes());
        }
        assert!(
            !t.state.scrollback.is_empty(),
            "scrollback must be non-empty before ED 3"
        );
        t.process(b"\x1b[3J");
        assert!(
            t.state.scrollback.is_empty(),
            "ED 3 (CSI 3 J) must clear scrollback"
        );
    }

    // ── OSC 9001 boundary tests ───────────────────────────────────────────────

    #[test]
    fn osc_9001_exactly_at_limit_is_accepted() {
        // A payload of exactly 64 KiB should be stored without truncation.
        let mut s = TerminalState::new(80, 24);
        let payload = vec![b'a'; 64 * 1024];
        s.osc_dispatch(&[b"9001", &payload], false);
        assert_eq!(
            s.input_buffer.len(),
            64 * 1024,
            "64 KiB payload must be accepted"
        );
    }

    #[test]
    fn osc_9001_one_byte_over_limit_is_rejected() {
        let mut s = TerminalState::new(80, 24);
        s.osc_dispatch(&[b"9001", b"seed\x1c0"], false);
        assert_eq!(s.input_buffer, "seed");
        let over = vec![b'b'; 64 * 1024 + 1];
        s.osc_dispatch(&[b"9001", &over], false);
        assert_eq!(
            s.input_buffer, "seed",
            "one byte over 64 KiB must be rejected"
        );
    }

    // ── generation counter ───────────────────────────────────────────────────

    #[test]
    fn generation_starts_at_zero() {
        let t = Terminal::new(80, 24);
        assert_eq!(t.state.generation, 0);
    }

    #[test]
    fn generation_increments_after_process() {
        let mut t = Terminal::new(80, 24);
        t.process(b"a");
        assert_eq!(t.state.generation, 1);
    }

    #[test]
    fn generation_increments_per_process_call() {
        let mut t = Terminal::new(80, 24);
        t.process(b"a");
        t.process(b"b");
        t.process(b"c");
        assert_eq!(t.state.generation, 3);
    }

    #[test]
    fn generation_empty_process_still_increments() {
        // Even a zero-byte slice counts as one process() call.
        let mut t = Terminal::new(80, 24);
        t.process(b"");
        assert_eq!(t.state.generation, 1);
    }

    #[test]
    fn generation_wraps_without_panic() {
        let mut t = Terminal::new(80, 24);
        t.state.generation = u64::MAX;
        t.process(b"x");
        assert_eq!(
            t.state.generation, 0,
            "generation must wrap via wrapping_add"
        );
    }

    // ── DECSCUSR ─────────────────────────────────────────────────────────────

    #[test]
    fn decscusr_sets_cursor_shape() {
        let mut t = t(10, 5);
        assert_eq!(t.state.cursor_shape, 0, "default cursor shape is 0");
        t.process(b"\x1b[2 q"); // steady block
        assert_eq!(t.state.cursor_shape, 2);
        t.process(b"\x1b[4 q"); // steady underline
        assert_eq!(t.state.cursor_shape, 4);
        t.process(b"\x1b[6 q"); // steady bar
        assert_eq!(t.state.cursor_shape, 6);
        t.process(b"\x1b[0 q"); // reset to default
        assert_eq!(t.state.cursor_shape, 0);
    }

    #[test]
    fn decscusr_clamps_to_max_six() {
        let mut t = t(10, 5);
        t.process(b"\x1b[99 q");
        assert_eq!(t.state.cursor_shape, 6, "values >6 should clamp to 6");
    }

    #[test]
    fn decstr_resets_attrs_and_scroll_region() {
        let mut t = t(20, 10);
        t.process(b"\x1b[1;3;4m"); // bold, italic, underline
        t.process(b"\x1b[3;8r"); // scroll region rows 3-8
        t.process(b"\x1b[2 q"); // steady block cursor
        assert_ne!(t.state.attrs, Attrs::default());
        assert_eq!(t.state.scroll_top, 2);
        assert_eq!(t.state.scroll_bottom, 7);
        assert_eq!(t.state.cursor_shape, 2);
        // DECSTR: CSI ! p
        t.process(b"\x1b[!p");
        assert_eq!(
            t.state.attrs,
            Attrs::default(),
            "DECSTR must reset SGR attrs"
        );
        assert_eq!(t.state.scroll_top, 0, "DECSTR must reset scroll region top");
        assert_eq!(
            t.state.scroll_bottom, 9,
            "DECSTR must reset scroll region bottom"
        );
        assert_eq!(t.state.cursor_shape, 0, "DECSTR must reset cursor shape");
    }

    #[test]
    fn decstr_preserves_screen_content() {
        let mut t = t(20, 5);
        t.process(b"hello");
        t.process(b"\x1b[!p"); // soft reset
        let row: String = t.state.grid[0].iter().map(|c| c.c).collect();
        assert!(
            row.starts_with("hello"),
            "DECSTR must not clear screen content"
        );
    }

    // ── Focus tracking (?1004h) ───────────────────────────────────────────────

    #[test]
    fn focus_tracking_disabled_by_default() {
        let s = TerminalState::new(80, 24);
        assert!(!s.focus_tracking);
    }

    #[test]
    fn focus_tracking_enabled_by_1004h() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?1004h");
        assert!(t.state.focus_tracking);
        t.process(b"\x1b[?1004l");
        assert!(!t.state.focus_tracking);
    }

    // ── Synchronized output (?2026h) ──────────────────────────────────────────

    #[test]
    fn sync_output_disabled_by_default() {
        let s = TerminalState::new(80, 24);
        assert!(!s.sync_output);
    }

    #[test]
    fn sync_output_toggled_by_2026() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?2026h");
        assert!(t.state.sync_output);
        t.process(b"\x1b[?2026l");
        assert!(!t.state.sync_output);
    }

    // ── SGR underline styles ──────────────────────────────────────────────────

    #[test]
    fn sgr_4_straight_underline() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4m");
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::Straight);
    }

    #[test]
    fn sgr_4_subparam_underline_styles() {
        let cases: &[(&[u8], UnderlineStyle)] = &[
            (b"\x1b[4:0m", UnderlineStyle::None),
            (b"\x1b[4:1m", UnderlineStyle::Straight),
            (b"\x1b[4:2m", UnderlineStyle::Double),
            (b"\x1b[4:3m", UnderlineStyle::Curly),
            (b"\x1b[4:4m", UnderlineStyle::Dotted),
            (b"\x1b[4:5m", UnderlineStyle::Dashed),
        ];
        for (seq, expected) in cases {
            let mut t = t(10, 5);
            t.process(seq);
            assert_eq!(
                t.state.attrs.underline_style,
                *expected,
                "sequence {:?} should give {expected:?}",
                std::str::from_utf8(seq).unwrap_or("?")
            );
        }
    }

    #[test]
    fn sgr_24_clears_underline_style() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4:3m"); // curly
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::Curly);
        t.process(b"\x1b[24m");
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::None);
    }

    #[test]
    fn sgr_58_underline_color_truecolor() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:2:255:128:0m");
        assert_eq!(t.state.attrs.underline_color, Some(Color::new(255, 128, 0)));
    }

    #[test]
    fn sgr_58_underline_color_256() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:5:196m"); // bright red in 256-color palette
        assert_eq!(t.state.attrs.underline_color, Some(ansi_256_color(196)));
    }

    #[test]
    fn sgr_59_clears_underline_color() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:2:255:0:0m");
        assert!(t.state.attrs.underline_color.is_some());
        t.process(b"\x1b[59m");
        assert_eq!(t.state.attrs.underline_color, None);
    }

    #[test]
    fn sgr_0_reset_clears_underline_color() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:2:255:0:0m");
        t.process(b"\x1b[0m");
        assert_eq!(t.state.attrs.underline_color, None);
    }

    // ── OSC 8 hyperlinks ─────────────────────────────────────────────────────

    #[test]
    fn osc8_sets_current_link_id() {
        let mut s = TerminalState::new(80, 5);
        s.osc_dispatch(&[b"8", b"", b"https://example.com"], false);
        assert_eq!(s.current_link_id, 1);
        assert_eq!(s.links[0], "https://example.com");
    }

    #[test]
    fn osc8_empty_uri_closes_link() {
        let mut s = TerminalState::new(80, 5);
        s.osc_dispatch(&[b"8", b"", b"https://example.com"], false);
        s.osc_dispatch(&[b"8", b"", b""], false);
        assert_eq!(s.current_link_id, 0, "empty uri should clear current link");
    }

    #[test]
    fn osc8_short_params_closes_link() {
        let mut s = TerminalState::new(80, 5);
        s.osc_dispatch(&[b"8", b"", b"https://example.com"], false);
        s.osc_dispatch(&[b"8", b""], false); // only 2 params
        assert_eq!(s.current_link_id, 0, "missing uri param should close link");
    }

    #[test]
    fn osc8_link_id_stamped_on_cells() {
        let mut t = t(40, 5);
        t.process(b"\x1b]8;;https://rust-lang.org\x07");
        t.process(b"Rust");
        t.process(b"\x1b]8;;\x07"); // close link
        t.process(b"plain");
        // Cells 0-3 in row 0 should carry link_id = 1
        for col in 0..4 {
            assert_eq!(
                t.state.grid[0][col].link_id, 1,
                "col {col} should have link_id 1"
            );
        }
        // Cell 4 onward should have link_id 0
        assert_eq!(t.state.grid[0][4].link_id, 0, "col 4 should have no link");
    }

    #[test]
    fn osc8_sgr_reset_does_not_clear_link() {
        let mut t = t(20, 5);
        t.process(b"\x1b]8;;https://example.com\x07");
        assert_eq!(t.state.current_link_id, 1);
        t.process(b"\x1b[0m"); // SGR reset
        // link should still be active
        assert_eq!(
            t.state.current_link_id, 1,
            "SGR reset must not clear the OSC 8 link"
        );
    }

    #[test]
    fn osc8_multiple_links_get_distinct_ids() {
        let mut s = TerminalState::new(80, 5);
        s.osc_dispatch(&[b"8", b"", b"https://a.com"], false);
        let id1 = s.current_link_id;
        s.osc_dispatch(&[b"8", b"", b"https://b.com"], false);
        let id2 = s.current_link_id;
        assert_ne!(id1, id2, "two different links must get distinct ids");
        assert_eq!(s.links[0], "https://a.com");
        assert_eq!(s.links[1], "https://b.com");
    }

    // ── DECSCUSR comprehensive ────────────────────────────────────────────────

    #[test]
    fn decscusr_all_values_individually() {
        for shape in 0u8..=6 {
            let mut t = t(10, 5);
            let seq = format!("\x1b[{} q", shape);
            t.process(seq.as_bytes());
            assert_eq!(
                t.state.cursor_shape, shape,
                "shape {shape} must be stored as-is"
            );
        }
    }

    #[test]
    fn decscusr_last_write_wins() {
        let mut t = t(10, 5);
        t.process(b"\x1b[2 q");
        t.process(b"\x1b[5 q");
        assert_eq!(t.state.cursor_shape, 5, "last DECSCUSR write must win");
    }

    // ── DECSTR comprehensive ──────────────────────────────────────────────────

    #[test]
    fn decstr_preserves_cursor_position() {
        let mut t = t(20, 10);
        t.process(b"\x1b[5;8H"); // move to row 5, col 8 (1-based → row=4, col=7)
        assert_eq!(t.state.cursor_row, 4);
        assert_eq!(t.state.cursor_col, 7);
        t.process(b"\x1b[!p");
        assert_eq!(t.state.cursor_row, 4, "DECSTR must not change cursor row");
        assert_eq!(t.state.cursor_col, 7, "DECSTR must not change cursor col");
    }

    #[test]
    fn decstr_preserves_scrollback() {
        let mut t = t(20, 5);
        for i in 0..10 {
            t.process(format!("line{i}\r\n").as_bytes());
        }
        let sb_len = t.state.scrollback.len();
        assert!(sb_len > 0, "scrollback must be non-empty before DECSTR");
        t.process(b"\x1b[!p");
        assert_eq!(
            t.state.scrollback.len(),
            sb_len,
            "DECSTR must not clear scrollback"
        );
    }

    #[test]
    fn decstr_resets_cursor_keys_app_mode() {
        let mut t = t(10, 5);
        t.process(b"\x1b[?1h"); // enable DECCKM
        assert!(t.state.cursor_keys_app_mode);
        t.process(b"\x1b[!p");
        assert!(
            !t.state.cursor_keys_app_mode,
            "DECSTR must reset cursor key mode"
        );
    }

    #[test]
    fn decstr_resets_wrap_next_flag() {
        // Fill a row to trigger wrap_next, then DECSTR should clear it.
        let mut t = t(5, 5);
        t.process(b"ABCDE"); // fills row 0, wrap_next=true on last char
        assert!(
            t.state.wrap_next,
            "wrap_next should be set after filling a row"
        );
        t.process(b"\x1b[!p");
        assert!(!t.state.wrap_next, "DECSTR must clear wrap_next");
    }

    #[test]
    fn decstr_subsequent_print_uses_default_attrs() {
        let mut t = t(10, 5);
        t.process(b"\x1b[1;3;4:3m"); // bold, italic, curly underline
        t.process(b"\x1b[!p");
        t.process(b"X");
        let cell = t.state.grid[0][0];
        assert!(!cell.attrs.bold, "DECSTR must reset bold before next print");
        assert!(
            !cell.attrs.italic,
            "DECSTR must reset italic before next print"
        );
        assert_eq!(
            cell.attrs.underline_style,
            UnderlineStyle::None,
            "DECSTR must reset underline_style before next print"
        );
    }

    // ── Focus tracking comprehensive ──────────────────────────────────────────

    #[test]
    fn focus_tracking_double_enable_is_idempotent() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?1004h");
        t.process(b"\x1b[?1004h");
        assert!(
            t.state.focus_tracking,
            "double-enable must still leave focus_tracking on"
        );
    }

    #[test]
    fn focus_tracking_double_disable_is_safe() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?1004l"); // disable when already off
        assert!(
            !t.state.focus_tracking,
            "disable when already off must not panic"
        );
    }

    #[test]
    fn focus_tracking_enable_then_disable_then_enable() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?1004h");
        t.process(b"\x1b[?1004l");
        t.process(b"\x1b[?1004h");
        assert!(
            t.state.focus_tracking,
            "re-enabling after disable must work"
        );
    }

    // ── Synchronized output comprehensive ─────────────────────────────────────

    #[test]
    fn sync_output_double_enable_is_idempotent() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?2026h");
        t.process(b"\x1b[?2026h");
        assert!(t.state.sync_output);
    }

    #[test]
    fn sync_output_content_is_processed_while_active() {
        let mut t = t(20, 5);
        t.process(b"\x1b[?2026h");
        t.process(b"hello");
        // Content must appear in the grid even though sync mode is active.
        let row: String = t.state.grid[0][..5].iter().map(|c| c.c).collect();
        assert_eq!(row, "hello", "characters must be placed during sync output");
    }

    #[test]
    fn sync_output_off_after_clear() {
        let mut t = t(80, 24);
        t.process(b"\x1b[?2026h");
        assert!(t.state.sync_output);
        t.process(b"\x1b[?2026l");
        assert!(
            !t.state.sync_output,
            "sync_output must be false after ?2026l"
        );
    }

    // ── SGR underline styles comprehensive ───────────────────────────────────

    #[test]
    fn underline_color_defaults_to_none() {
        let s = TerminalState::new(80, 24);
        assert_eq!(s.attrs.underline_color, None);
    }

    #[test]
    fn sgr_4_does_not_clear_existing_underline_color() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:2:255:0:0m"); // set red underline color
        t.process(b"\x1b[4m"); // set underline style
        assert_eq!(
            t.state.attrs.underline_color,
            Some(Color::new(255, 0, 0)),
            "SGR 4 must not clear existing underline color"
        );
    }

    #[test]
    fn sgr_58_legacy_truecolor_format() {
        // 58;2;r;g;b — legacy form using semicolons, not sub-params
        let mut t = t(10, 5);
        t.process(b"\x1b[58;2;100;150;200m");
        assert_eq!(
            t.state.attrs.underline_color,
            Some(Color::new(100, 150, 200))
        );
    }

    #[test]
    fn sgr_58_legacy_256_format() {
        // 58;5;n — legacy form using semicolons
        let mut t = t(10, 5);
        t.process(b"\x1b[58;5;196m");
        assert_eq!(t.state.attrs.underline_color, Some(ansi_256_color(196)));
    }

    #[test]
    fn cell_carries_underline_style_on_print() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4:3m"); // curly
        t.process(b"A");
        assert_eq!(
            t.state.grid[0][0].attrs.underline_style,
            UnderlineStyle::Curly,
            "printed cell must carry the active underline style"
        );
    }

    #[test]
    fn cell_carries_underline_color_on_print() {
        let mut t = t(10, 5);
        t.process(b"\x1b[58:2:255:128:64m");
        t.process(b"A");
        assert_eq!(
            t.state.grid[0][0].attrs.underline_color,
            Some(Color::new(255, 128, 64)),
            "printed cell must carry the active underline color"
        );
    }

    #[test]
    fn cell_after_underline_clear_has_no_underline() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4:3m"); // curly underline
        t.process(b"\x1b[24m"); // clear underline
        t.process(b"A");
        assert_eq!(
            t.state.grid[0][0].attrs.underline_style,
            UnderlineStyle::None,
            "cell must have no underline style after SGR 24"
        );
    }

    #[test]
    fn sgr_reset_clears_underline_style_and_color() {
        let mut t = t(10, 5);
        t.process(b"\x1b[4:2m\x1b[58:2:1:2:3m"); // double + color
        t.process(b"\x1b[0m");
        assert_eq!(t.state.attrs.underline_style, UnderlineStyle::None);
        assert_eq!(t.state.attrs.underline_color, None);
    }

    // ── OSC 8 hyperlinks comprehensive ───────────────────────────────────────

    #[test]
    fn osc8_cells_after_link_close_have_no_link_id() {
        let mut t = t(40, 5);
        t.process(b"\x1b]8;;https://example.com\x07");
        t.process(b"link");
        t.process(b"\x1b]8;;\x07"); // close
        t.process(b"plain text");
        for col in 4..14 {
            assert_eq!(
                t.state.grid[0][col].link_id, 0,
                "col {col} after link close must have link_id 0"
            );
        }
    }

    #[test]
    fn osc8_link_url_retrievable_from_table() {
        let mut t = t(40, 5);
        t.process(b"\x1b]8;;https://rust-lang.org\x07");
        let id = t.state.current_link_id as usize;
        assert!(id > 0);
        assert_eq!(t.state.links[id - 1], "https://rust-lang.org");
    }

    #[test]
    fn osc8_visual_cell_preserves_link_id() {
        let mut t = t(40, 5);
        t.process(b"\x1b]8;;https://example.com\x07");
        t.process(b"AB");
        t.process(b"\x1b]8;;\x07");
        // visual_cell must return the same link_id as grid
        assert_eq!(t.state.visual_cell(0, 0).link_id, 1);
        assert_eq!(t.state.visual_cell(0, 1).link_id, 1);
        assert_eq!(t.state.visual_cell(0, 2).link_id, 0);
    }

    #[test]
    fn osc8_reopen_link_gives_new_id() {
        let mut t = t(40, 5);
        t.process(b"\x1b]8;;https://a.com\x07");
        let id1 = t.state.current_link_id;
        t.process(b"\x1b]8;;\x07"); // close
        t.process(b"\x1b]8;;https://b.com\x07");
        let id2 = t.state.current_link_id;
        assert_ne!(id1, 0);
        assert_ne!(id2, 0);
        assert_ne!(id1, id2, "re-opened link must get a new ID");
    }

    #[test]
    fn osc8_params_field_is_ignored_for_id_assignment() {
        // OSC 8 allows optional params like id=foo before the URI
        let mut s = TerminalState::new(80, 5);
        s.osc_dispatch(&[b"8", b"id=foo", b"https://example.com"], false);
        assert_eq!(
            s.current_link_id, 1,
            "non-empty params field must not affect link id"
        );
        assert_eq!(s.links[0], "https://example.com");
    }

    #[test]
    fn osc8_no_link_by_default() {
        let s = TerminalState::new(80, 5);
        assert_eq!(s.current_link_id, 0);
        assert!(s.links.is_empty());
    }

    // ── Mouse tracking ────────────────────────────────────────────────────────

    #[test]
    fn mouse_tracking_off_by_default() {
        let s = TerminalState::new(80, 24);
        assert!(!s.mouse_tracking);
        assert!(!s.mouse_sgr);
    }

    #[test]
    fn mouse_tracking_enabled_by_1000h() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000h");
        assert!(t.state.mouse_tracking);
    }

    #[test]
    fn mouse_tracking_enabled_by_1002h() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1002h");
        assert!(t.state.mouse_tracking);
    }

    #[test]
    fn mouse_tracking_enabled_by_1003h() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1003h");
        assert!(t.state.mouse_tracking);
    }

    #[test]
    fn mouse_tracking_disabled_by_1000l() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000h");
        assert!(t.state.mouse_tracking);
        t.process(b"\x1b[?1000l");
        assert!(!t.state.mouse_tracking);
    }

    #[test]
    fn mouse_sgr_enabled_by_1006h() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1006h");
        assert!(t.state.mouse_sgr);
    }

    #[test]
    fn mouse_sgr_disabled_by_1006l() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1006h");
        t.process(b"\x1b[?1006l");
        assert!(!t.state.mouse_sgr);
    }

    #[test]
    fn mouse_tracking_and_sgr_enabled_together() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000h\x1b[?1006h");
        assert!(t.state.mouse_tracking);
        assert!(t.state.mouse_sgr);
    }

    #[test]
    fn mouse_tracking_does_not_affect_cursor_keys() {
        // ?1000h must not change cursor_keys_app_mode
        let mut t = Terminal::new(80, 24);
        assert!(!t.state.cursor_keys_app_mode);
        t.process(b"\x1b[?1000h");
        assert!(!t.state.cursor_keys_app_mode);
    }

    #[test]
    fn mouse_tracking_reset_by_soft_reset() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000h\x1b[?1006h");
        t.state.reset_soft();
        assert!(!t.state.mouse_tracking);
        assert!(!t.state.mouse_sgr);
    }

    #[test]
    fn mouse_tracking_double_enable_is_idempotent() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000h");
        t.process(b"\x1b[?1000h");
        assert!(t.state.mouse_tracking);
    }

    #[test]
    fn mouse_tracking_disable_without_enable_is_safe() {
        let mut t = Terminal::new(80, 24);
        t.process(b"\x1b[?1000l");
        assert!(!t.state.mouse_tracking);
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
        self.state.generation = self.state.generation.wrapping_add(1);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }
}
