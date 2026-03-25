use crate::config::*;
use crate::terminal::TerminalState;
use std::collections::HashMap;

// ── Internal glyph cache ──────────────────────────────────────────────────────

struct Glyph {
    width: usize,
    height: usize,
    xmin: i32,
    ymin: i32,
    data: Vec<u8>,
}

pub struct Renderer {
    font: fontdue::Font,
    cache: HashMap<char, Glyph>,
    pub cell_width: usize,
    pub cell_height: usize,
    pub baseline: i32,
    pub tab_bar_height: usize,
    font_size: f32,
}

/// Find the first content column of a tcat gutter line, or 0 if this row is
/// not tcat output.  tcat renders: `  {digits} │ content` — we detect the
/// BOX-DRAWINGS LIGHT VERTICAL (U+2502) with a space on each side, preceded
/// only by spaces and digits.
fn tcat_gutter_end(state: &TerminalState, row: usize, vis_cols: usize) -> usize {
    // │ can't appear at col 0 (needs a space before it) or at the last col
    for c in 1..vis_cols.saturating_sub(1) {
        if state.visual_cell(row, c).c != '\u{2502}' {
            continue;
        }
        if state.visual_cell(row, c - 1).c != ' ' {
            continue;
        }
        if state.visual_cell(row, c + 1).c != ' ' {
            continue;
        }
        // Everything in 0..c-1 must be spaces/null/digits, with at least one digit
        let mut prefix = 0..c - 1;
        let has_digit = prefix.clone().any(|i| state.visual_cell(row, i).c.is_ascii_digit());
        let all_ok = prefix.all(|i| {
            matches!(state.visual_cell(row, i).c, ' ' | '\0' | '0'..='9')
        });
        if has_digit && all_ok {
            return c + 2; // column after "│ "
        }
    }
    0
}

impl Renderer {
    pub fn new(scale_factor: f64) -> Self {
        let font_size = (FONT_SIZE_PT * scale_factor as f32).round();
        let font_data = Self::load_font();
        let settings = fontdue::FontSettings {
            scale: font_size,
            collection_index: 0,
            load_substitutions: true,
        };
        let font =
            fontdue::Font::from_bytes(font_data.as_slice(), settings).expect("failed to load font");

        let (m, _) = font.rasterize('M', font_size);
        let cell_width = m.advance_width.ceil() as usize;
        let lm = font.horizontal_line_metrics(font_size).unwrap();
        let ascent = lm.ascent.ceil() as i32;
        let descent = (-lm.descent).ceil() as i32;
        let gap = lm.line_gap.ceil() as i32;
        let cell_height = (ascent + descent + gap).max(font_size as i32 + 4) as usize;
        let tab_bar_height = cell_height + 16;

        Self {
            font,
            cache: HashMap::new(),
            cell_width,
            cell_height,
            baseline: ascent,
            tab_bar_height,
            font_size,
        }
    }

    fn load_font() -> Vec<u8> {
        // Embedded JetBrains Mono — always available, excellent Unicode coverage.
        include_bytes!("../assets/JetBrainsMono-Regular.ttf").to_vec()
    }

    // ── Low-level blit ────────────────────────────────────────────────────────

    fn ensure(&mut self, c: char) {
        if !self.cache.contains_key(&c) {
            let (m, bm) = self.font.rasterize(c, self.font_size);
            self.cache.insert(
                c,
                Glyph {
                    width: m.width,
                    height: m.height,
                    xmin: m.xmin,
                    ymin: m.ymin,
                    data: bm,
                },
            );
        }
    }

    fn blit(
        &mut self,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        px: usize,
        py: usize,
        c: char,
        fg: Color,
    ) {
        // Skip characters not in the font — avoids rendering the .notdef box.
        if self.font.lookup_glyph_index(c) == 0 {
            return;
        }
        self.ensure(c);
        let g = &self.cache[&c];
        if g.width == 0 || g.height == 0 {
            return;
        }
        let gx0 = px as i32 + g.xmin;
        let gy0 = py as i32 + self.baseline - (g.ymin + g.height as i32);
        for gy in 0..g.height {
            let sy = gy0 + gy as i32;
            if sy < 0 || sy >= bh as i32 {
                continue;
            }
            let sy = sy as usize;
            for gx in 0..g.width {
                let alpha = g.data[gy * g.width + gx];
                if alpha == 0 {
                    continue;
                }
                let sx = gx0 + gx as i32;
                if sx < 0 || sx >= bw as i32 {
                    continue;
                }
                let sx = sx as usize;
                let idx = sy * bw + sx;
                let ex = buf[idx];
                let bg = Color {
                    r: ((ex >> 16) & 0xff) as u8,
                    g: ((ex >> 8) & 0xff) as u8,
                    b: (ex & 0xff) as u8,
                };
                buf[idx] = bg.blend(fg, alpha).to_u32();
            }
        }
    }

    fn fill_rect(
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        px: usize,
        py: usize,
        rw: usize,
        rh: usize,
        color: u32,
    ) {
        for y in py..(py + rh).min(bh) {
            let base = y * bw;
            for x in px..(px + rw).min(bw) {
                buf[base + x] = color;
            }
        }
    }

    /// Draw a Unicode Block Element (U+2580–U+259F) as a direct pixel fill.
    /// Returns true if the character was handled; false means fall through to fontdue.
    fn draw_block_char(
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        px: usize, // cell left pixel
        py: usize, // cell top pixel
        cw: usize,
        ch: usize,
        c: char,
        fg: u32,
    ) -> bool {
        // Fill [x0..x1) × [y0..y1) within the cell with fg, clipped.
        fn fill(
            buf: &mut [u32],
            bw: usize,
            bh: usize,
            px: usize,
            py: usize,
            cw: usize,
            ch: usize,
            x0: usize,
            y0: usize,
            x1: usize,
            y1: usize,
            fg: u32,
        ) {
            let x1 = x1.min(cw);
            let y1 = y1.min(ch);
            if x0 >= x1 || y0 >= y1 {
                return;
            }
            for dy in y0..y1 {
                let y = py + dy;
                if y >= bh {
                    break;
                }
                let row = y * bw;
                for dx in x0..x1 {
                    let x = px + dx;
                    if x < bw {
                        buf[row + x] = fg;
                    }
                }
            }
        }
        macro_rules! f {
            ($x0:expr,$y0:expr,$x1:expr,$y1:expr) => {
                fill(buf, bw, bh, px, py, cw, ch, $x0, $y0, $x1, $y1, fg)
            };
        }
        match c {
            // ── Vertical (lower N/8) ──────────────────────────────────────────
            '\u{2581}' => f!(0, ch * 7 / 8, cw, ch),
            '\u{2582}' => f!(0, ch * 3 / 4, cw, ch),
            '\u{2583}' => f!(0, ch * 5 / 8, cw, ch),
            '\u{2584}' => f!(0, ch / 2, cw, ch),
            '\u{2585}' => f!(0, ch * 3 / 8, cw, ch),
            '\u{2586}' => f!(0, ch / 4, cw, ch),
            '\u{2587}' => f!(0, ch / 8, cw, ch),
            '\u{2588}' => f!(0, 0, cw, ch),
            // ── Upper half / upper 1/8 ───────────────────────────────────────
            '\u{2580}' => f!(0, 0, cw, ch / 2),
            '\u{2594}' => f!(0, 0, cw, (ch / 8).max(1)),
            // ── Horizontal (left N/8) ─────────────────────────────────────────
            '\u{258F}' => f!(0, 0, (cw / 8).max(1), ch),
            '\u{258E}' => f!(0, 0, cw / 4, ch),
            '\u{258D}' => f!(0, 0, cw * 3 / 8, ch),
            '\u{258C}' => f!(0, 0, cw / 2, ch),
            '\u{258B}' => f!(0, 0, cw * 5 / 8, ch),
            '\u{258A}' => f!(0, 0, cw * 3 / 4, ch),
            '\u{2589}' => f!(0, 0, cw * 7 / 8, ch),
            // ── Right half / right 1/8 ───────────────────────────────────────
            '\u{2590}' => f!(cw / 2, 0, cw, ch),
            '\u{2595}' => f!(cw * 7 / 8, 0, cw, ch),
            // ── Quadrants ────────────────────────────────────────────────────
            '\u{2596}' => f!(0, ch / 2, cw / 2, ch),
            '\u{2597}' => f!(cw / 2, ch / 2, cw, ch),
            '\u{2598}' => f!(0, 0, cw / 2, ch / 2),
            '\u{259D}' => f!(cw / 2, 0, cw, ch / 2),
            '\u{2599}' => {
                f!(0, 0, cw / 2, ch / 2);
                f!(0, ch / 2, cw, ch);
            }
            '\u{259A}' => {
                f!(0, 0, cw / 2, ch / 2);
                f!(cw / 2, ch / 2, cw, ch);
            }
            '\u{259B}' => {
                f!(0, 0, cw, ch / 2);
                f!(0, ch / 2, cw / 2, ch);
            }
            '\u{259C}' => {
                f!(0, 0, cw, ch / 2);
                f!(cw / 2, ch / 2, cw, ch);
            }
            '\u{259E}' => {
                f!(cw / 2, 0, cw, ch / 2);
                f!(0, ch / 2, cw / 2, ch);
            }
            '\u{259F}' => {
                f!(cw / 2, 0, cw, ch / 2);
                f!(0, ch / 2, cw, ch);
            }
            // ── Braille patterns U+2800–U+28FF ───────────────────────────────
            // Each character encodes an 8-dot (2×4) grid.
            // Bit layout: 0=r0c0, 1=r1c0, 2=r2c0, 3=r0c1,
            //              4=r1c1, 5=r2c1, 6=r3c0, 7=r3c1
            '\u{2800}'..='\u{28FF}' => {
                let bits = c as u32 - 0x2800;
                if bits == 0 {
                    return true; // blank braille — nothing to draw
                }
                // dot column widths / row heights (split cell evenly into 2×4)
                let col0_w = cw / 2;
                let col1_w = cw - col0_w;
                let row_h = [ch / 4, ch / 4, ch / 4, ch - 3 * (ch / 4)];
                let mut ry = 0usize;
                for row in 0..4usize {
                    let rh = row_h[row];
                    // col 0: bits 0,1,2,6
                    let bit_c0 = [0u32, 1, 2, 6][row];
                    if bits & (1 << bit_c0) != 0 {
                        f!(0, ry, col0_w, ry + rh);
                    }
                    // col 1: bits 3,4,5,7
                    let bit_c1 = [3u32, 4, 5, 7][row];
                    if bits & (1 << bit_c1) != 0 {
                        f!(col0_w, ry, col0_w + col1_w, ry + rh);
                    }
                    ry += rh;
                }
            }
            _ => return false,
        }
        true
    }

    /// Thin circle outline (1–2 px) centred at (cx, cy) with radius r.
    fn stroke_circle(
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        cx: usize,
        cy: usize,
        r: usize,
        color: u32,
    ) {
        let ro = (r as i32) + 1;
        let ri = (r as i32) - 1;
        for dy in -ro..=ro {
            for dx in -ro..=ro {
                let d2 = dx * dx + dy * dy;
                if d2 <= ro * ro && d2 > ri * ri {
                    let px = cx as i32 + dx;
                    let py = cy as i32 + dy;
                    if px >= 0 && px < bw as i32 && py >= 0 && py < bh as i32 {
                        buf[py as usize * bw + px as usize] = color;
                    }
                }
            }
        }
    }

    /// Filled rounded rectangle with radius `r` pixels (circular corners).
    fn fill_rounded(
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        r: usize,
        color: u32,
    ) {
        let r2 = (r * r) as i32;
        for dy in 0..h {
            for dx in 0..w {
                let in_x_corner = dx < r || dx + r >= w;
                let in_y_corner = dy < r || dy + r >= h;
                if in_x_corner && in_y_corner {
                    let cx = if dx < r {
                        (r - dx) as i32
                    } else {
                        (dx + r + 1 - w) as i32
                    };
                    let cy = if dy < r {
                        (r - dy) as i32
                    } else {
                        (dy + r + 1 - h) as i32
                    };
                    if cx * cx + cy * cy > r2 {
                        continue;
                    }
                }
                let px = x + dx;
                let py = y + dy;
                if px < bw && py < bh {
                    buf[py * bw + px] = color;
                }
            }
        }
    }

    // ── Tab bar ───────────────────────────────────────────────────────────────

    /// `hover` — visual index of hovered tab, or `tabs.len()` for the + button.
    /// `drag`  — `(from_orig, to_visual, cursor_x)` while a tab is being dragged.
    fn draw_tab_bar(
        &mut self,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        tabs: &[String],
        active: usize,
        hover: Option<usize>,
        drag: Option<(usize, usize, f64)>,
    ) {
        let tby = self.tab_bar_height;
        let cw = self.cell_width;
        let ch = self.cell_height;

        // Neutral dark palette — mirrors terminal background aesthetic
        let bar_bg = Color::new(0x14, 0x14, 0x14).to_u32(); // slightly darker than bg
        let outline_col = Color::new(0x58, 0x58, 0x58).to_u32(); // medium gray — active outline
        let hover_bg = Color::new(0x26, 0x26, 0x26).to_u32(); // subtle hover fill
        let sep_col = Color::new(0x2e, 0x2e, 0x2e).to_u32(); // dim separator
        let bottom_col = Color::new(0x2e, 0x2e, 0x2e).to_u32(); // bottom rule
        let fg_active = DEFAULT_FG;
        let fg_inactive = Color::new(0x66, 0x66, 0x66); // dim gray
        let fg_shortcut = Color::new(0x3a, 0x3a, 0x3a); // very dim

        // Bar fill + bottom border
        Self::fill_rect(buf, bw, bh, 0, 0, bw, tby, bar_bg);
        Self::fill_rect(buf, bw, bh, 0, tby.saturating_sub(1), bw, 1, bottom_col);

        // Reserve right side for + button
        let plus_area = tby;
        let tabs_w = bw.saturating_sub(plus_area);
        let n = tabs.len().max(1);
        let tab_w = tabs_w / n;
        let pad_v = 4usize;
        let pill_h = tby.saturating_sub(pad_v * 2);
        let text_y = (tby.saturating_sub(ch)) / 2;
        let shortcut_w = 3 * cw; // ⌘ + digit + 1 pad

        // Compute visual tab order for drag preview.
        // visual_order[vi] = original tab index shown at visual position vi.
        let visual_order: Vec<usize> = if let Some((from, to, _)) = drag {
            let mut order: Vec<usize> = (0..tabs.len()).collect();
            if from < order.len() {
                let item = order.remove(from);
                order.insert(to.min(order.len()), item);
            }
            order
        } else {
            (0..tabs.len()).collect()
        };
        let visual_active = visual_order
            .iter()
            .position(|&i| i == active)
            .unwrap_or(active);
        let drag_orig = drag.map(|(from, _, _)| from);

        for (vi, &orig_idx) in visual_order.iter().enumerate() {
            let title = &tabs[orig_idx];
            let is_active = orig_idx == active;
            let is_dragging = drag_orig == Some(orig_idx);
            // Suppress hover highlight while dragging
            let is_hover = drag.is_none() && hover == Some(vi) && !is_active;

            let tx = vi * tab_w;
            if tx >= tabs_w {
                break;
            }
            let tw = if vi + 1 == n {
                tabs_w.saturating_sub(tx)
            } else {
                tab_w
            };
            let pill_x = tx + 4;
            let pill_w = tw.saturating_sub(8);

            // ── Dragging: draw a dim placeholder "drop here" slot ─────────────
            if is_dragging {
                // Separator rules
                if vi > 0 && vi != visual_active && vi != visual_active + 1 {
                    let sep_top = pad_v + 4;
                    let sep_bottom = tby.saturating_sub(pad_v + 4);
                    for y in sep_top..sep_bottom {
                        if tx < bw {
                            buf[y * bw + tx] = sep_col;
                        }
                    }
                }
                // Dim dashed-ish outline as drop-target hint
                let ghost_col = Color::new(0x38, 0x38, 0x38).to_u32();
                if pill_w > 2 && pill_h > 2 {
                    Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 5, ghost_col);
                    Self::fill_rounded(buf, bw, bh, pill_x + 1, pad_v + 1, pill_w - 2, pill_h - 2, 4, bar_bg);
                }
                continue;
            }

            // ── Hover fill ────────────────────────────────────────────────────
            if is_hover {
                Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 4, hover_bg);
            }

            // ── Active: outlined rounded rect ─────────────────────────────────
            if is_active && pill_w > 2 && pill_h > 2 {
                Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 5, outline_col);
                Self::fill_rounded(
                    buf,
                    bw,
                    bh,
                    pill_x + 1,
                    pad_v + 1,
                    pill_w - 2,
                    pill_h - 2,
                    4,
                    bar_bg,
                );
            }

            // ── Separator (hide on both sides of the visual active position) ──
            if vi > 0 && vi != visual_active && vi != visual_active + 1 {
                let sep_top = pad_v + 4;
                let sep_bottom = tby.saturating_sub(pad_v + 4);
                for y in sep_top..sep_bottom {
                    if tx < bw {
                        buf[y * bw + tx] = sep_col;
                    }
                }
            }

            // ── ⌘N shortcut (original index so ⌘1 stays bound to tab[0]) ─────
            let shortcut = format!("\u{2318}{}", orig_idx + 1);
            let sc_x = tx + tw.saturating_sub(shortcut_w);
            let mut col_x = sc_x;
            for c in shortcut.chars() {
                if col_x + cw > tx + tw {
                    break;
                }
                self.blit(buf, bw, bh, col_x, text_y, c, fg_shortcut);
                col_x += cw;
            }

            // ── Title — left-aligned, truncated ───────────────────────────────
            let fg = if is_active { fg_active } else { fg_inactive };
            let left_pad = tx + cw;
            let right_edge = tx + tw.saturating_sub(shortcut_w + cw);
            let max_cols = right_edge.saturating_sub(left_pad) / cw;
            let chars: Vec<char> = title.chars().collect();
            let show_n = chars.len().min(max_cols);
            let truncated = show_n < chars.len();
            for (ci, &c) in chars[..show_n].iter().enumerate() {
                let px = left_pad + ci * cw;
                if truncated && ci + 1 == show_n {
                    self.blit(buf, bw, bh, px, text_y, '\u{2026}', fg);
                } else {
                    self.blit(buf, bw, bh, px, text_y, c, fg);
                }
            }
        }

        // ── Floating dragged tab (rendered on top, follows cursor) ────────────
        if let Some((from_orig, _, cursor_x)) = drag {
            let title = &tabs[from_orig];
            let is_active = from_orig == active;

            // Center the floating tab under the cursor, clamped inside tab area
            let half = (tab_w / 2) as isize;
            let float_left = ((cursor_x as isize) - half)
                .max(0)
                .min(tabs_w.saturating_sub(tab_w) as isize) as usize;
            let pill_x = float_left + 4;
            let pill_w = tab_w.saturating_sub(8);

            // Bright lifted outline with a slightly lighter fill
            let lifted_outline = Color::new(0xa0, 0xa0, 0xa0).to_u32();
            let lifted_bg = Color::new(0x20, 0x20, 0x20).to_u32();
            if pill_w > 2 && pill_h > 2 {
                Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 5, lifted_outline);
                Self::fill_rounded(buf, bw, bh, pill_x + 1, pad_v + 1, pill_w - 2, pill_h - 2, 4, lifted_bg);
            }

            // Title (active color always — it's the "held" tab)
            let fg = if is_active { fg_active } else { Color::new(0xcc, 0xcc, 0xcc) };
            let left_pad = float_left + cw;
            let right_edge = float_left + tab_w.saturating_sub(shortcut_w + cw);
            let max_cols = right_edge.saturating_sub(left_pad) / cw;
            let chars: Vec<char> = title.chars().collect();
            let show_n = chars.len().min(max_cols);
            let truncated = show_n < chars.len();
            for (ci, &c) in chars[..show_n].iter().enumerate() {
                let px = left_pad + ci * cw;
                if truncated && ci + 1 == show_n {
                    self.blit(buf, bw, bh, px, text_y, '\u{2026}', fg);
                } else {
                    self.blit(buf, bw, bh, px, text_y, c, fg);
                }
            }
        }

        // ── + button (circle outline + cross) ────────────────────────────────
        let plus_hover = hover == Some(tabs.len());
        let plus_col = if plus_hover {
            Color::new(0x88, 0x88, 0x88).to_u32() // hovered
        } else {
            Color::new(0x44, 0x44, 0x44).to_u32() // normal
        };
        let plus_cx = bw.saturating_sub(plus_area / 2);
        let plus_cy = tby / 2;
        let plus_r = 9usize;
        Self::stroke_circle(buf, bw, bh, plus_cx, plus_cy, plus_r, plus_col);
        let arm = (plus_r / 2) as i32;
        for d in -arm..=arm {
            let px = (plus_cx as i32 + d) as usize;
            let py = (plus_cy as i32 + d) as usize;
            if px < bw && plus_cy < bh {
                buf[plus_cy * bw + px] = plus_col;
            }
            if plus_cx < bw && py < bh {
                buf[py * bw + plus_cx] = plus_col;
            }
        }
    }

    // ── Public render ─────────────────────────────────────────────────────────

    /// `hover`     — visual index of hovered tab, or `tabs.len()` for the + button.
    /// `selection` — normalized (r0, c0, r1, c1) in viewport coordinates, if any.
    /// `drag`      — (from_orig, to_visual, cursor_x) preview while dragging a tab.
    pub fn render(
        &mut self,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        state: &TerminalState,
        show_cursor: bool,
        ghost: Option<&str>,
        tabs: &[String],
        active_tab: usize,
        hover: Option<usize>,
        selection: Option<(usize, usize, usize, usize)>,
        drag: Option<(usize, usize, f64)>,
    ) {
        buf.fill(DEFAULT_BG.to_u32());

        // ── Tab bar ───────────────────────────────────────────────────────────
        self.draw_tab_bar(buf, bw, bh, tabs, active_tab, hover, drag);

        let tby = self.tab_bar_height; // y offset for terminal content
        let cw = self.cell_width;
        let ch = self.cell_height;
        let term_h = bh.saturating_sub(tby);
        let vis_rows = (term_h / ch).min(state.rows);
        let vis_cols = (bw / cw).min(state.cols);

        // ── 1. Terminal grid ──────────────────────────────────────────────────
        let sel_bg = Color::new(0x26, 0x4a, 0x7a).to_u32(); // muted blue selection

        for row in 0..vis_rows {
            // Per-row: detect tcat gutter so it's excluded from selection highlight.
            let gutter = if selection.is_some() {
                tcat_gutter_end(state, row, vis_cols)
            } else {
                0
            };

            for col in 0..vis_cols {
                let cell = state.visual_cell(row, col);
                let mut fg = cell.attrs.fg;
                let mut bg = cell.attrs.bg;
                if cell.attrs.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }

                let selected = if let Some((r0, c0, r1, c1)) = selection {
                    row >= r0
                        && row <= r1
                        && !(row == r0 && col < c0)
                        && !(row == r1 && col > c1)
                        && col >= gutter
                } else {
                    false
                };

                let px = col * cw;
                let py = tby + row * ch;
                let bg32 = if selected { sel_bg } else { bg.to_u32() };
                for dy in 0..ch {
                    let y = py + dy;
                    if y >= bh {
                        break;
                    }
                    let base = y * bw;
                    for dx in 0..cw {
                        let x = px + dx;
                        if x < bw {
                            buf[base + x] = bg32;
                        }
                    }
                }
                let c = cell.c;
                if c != ' ' && c != '\0' {
                    // Block elements are drawn as direct pixel fills — fontdue
                    // can't rasterize them correctly.
                    if !Self::draw_block_char(buf, bw, bh, px, py, cw, ch, c, fg.to_u32()) {
                        self.blit(buf, bw, bh, px, py, c, fg);
                    }
                }
            }
        }

        // ── 2. Ghost text ─────────────────────────────────────────────────────
        if !state.is_scrolled_back()
            && let Some(g) = ghost {
                let py = tby + state.cursor_row * ch;
                for (i, c) in g.chars().enumerate() {
                    let col = state.cursor_col + i;
                    if col >= vis_cols {
                        break;
                    }
                    self.blit(buf, bw, bh, col * cw, py, c, GHOST_COLOR);
                }
            }

        // ── 3. Cursor ─────────────────────────────────────────────────────────
        if !state.is_scrolled_back()
            && show_cursor
            && state.cursor_row < vis_rows
            && state.cursor_col < vis_cols
        {
            let px = state.cursor_col * cw;
            let py = tby + state.cursor_row * ch;
            let c32 = CURSOR_COLOR.to_u32();
            for dy in 0..ch {
                let y = py + dy;
                if y >= bh {
                    break;
                }
                let base = y * bw;
                for dx in 0..2 {
                    let x = px + dx;
                    if x < bw {
                        buf[base + x] = c32;
                    }
                }
            }
        }

        // ── 4. Scrollbar ──────────────────────────────────────────────────────
        let sb_total = state.scrollback.len();
        if sb_total > 0 {
            let total = sb_total + state.rows;
            let thumb_h = ((term_h * vis_rows) / total).max(8).min(term_h);
            let view_top = sb_total.saturating_sub(state.viewport_offset);
            let thumb_y = tby + (view_top * (term_h - thumb_h)) / total.max(1);

            let bar_x = bw.saturating_sub(3);
            let track_col = Color::new(0x2a, 0x2a, 0x2a).to_u32();
            let thumb_col = if state.is_scrolled_back() {
                Color::new(0x66, 0x66, 0x66).to_u32()
            } else {
                Color::new(0x44, 0x44, 0x44).to_u32()
            };
            for y in tby..bh {
                let color = if y >= thumb_y && y < thumb_y + thumb_h {
                    thumb_col
                } else {
                    track_col
                };
                if bar_x < bw {
                    buf[y * bw + bar_x] = color;
                }
                if bar_x + 1 < bw {
                    buf[y * bw + bar_x + 1] = color;
                }
            }
        }
    }
}
