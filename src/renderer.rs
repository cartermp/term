use std::collections::HashMap;
use crate::config::*;
use crate::terminal::TerminalState;

// ── Internal glyph cache ──────────────────────────────────────────────────────

struct Glyph { width: usize, height: usize, xmin: i32, ymin: i32, data: Vec<u8> }

pub struct Renderer {
    font:            fontdue::Font,
    cache:           HashMap<char, Glyph>,
    pub cell_width:  usize,
    pub cell_height: usize,
    pub baseline:    i32,
    pub tab_bar_height: usize,
    font_size:       f32,
}

impl Renderer {
    pub fn new(scale_factor: f64) -> Self {
        let font_size = (FONT_SIZE_PT * scale_factor as f32).round();
        let font_data = Self::load_font();
        let settings = fontdue::FontSettings {
            scale: font_size, collection_index: 0, load_substitutions: true,
        };
        let font = fontdue::Font::from_bytes(font_data.as_slice(), settings)
            .expect("failed to load font");

        let (m, _) = font.rasterize('M', font_size);
        let cell_width = m.advance_width.ceil() as usize;
        let lm = font.horizontal_line_metrics(font_size).unwrap();
        let ascent  = lm.ascent.ceil() as i32;
        let descent = (-lm.descent).ceil() as i32;
        let gap     = lm.line_gap.ceil() as i32;
        let cell_height = (ascent + descent + gap).max(font_size as i32 + 4) as usize;
        let tab_bar_height = cell_height + 16;

        Self {
            font, cache: HashMap::new(),
            cell_width, cell_height, baseline: ascent,
            tab_bar_height,
            font_size,
        }
    }

    fn load_font() -> Vec<u8> {
        for path in &[
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Monaco.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ] {
            if let Ok(data) = std::fs::read(path) { return data; }
        }
        panic!("No monospace font found");
    }

    // ── Low-level blit ────────────────────────────────────────────────────────

    fn ensure(&mut self, c: char) {
        if !self.cache.contains_key(&c) {
            let (m, bm) = self.font.rasterize(c, self.font_size);
            self.cache.insert(c, Glyph {
                width: m.width, height: m.height, xmin: m.xmin, ymin: m.ymin, data: bm,
            });
        }
    }

    fn blit(
        &mut self,
        buf: &mut [u32], bw: usize, bh: usize,
        px: usize, py: usize,
        c: char, fg: Color,
    ) {
        self.ensure(c);
        let g = &self.cache[&c];
        if g.width == 0 || g.height == 0 { return; }
        let gx0 = px as i32 + g.xmin;
        let gy0 = py as i32 + self.baseline - (g.ymin + g.height as i32);
        for gy in 0..g.height {
            let sy = gy0 + gy as i32;
            if sy < 0 || sy >= bh as i32 { continue; }
            let sy = sy as usize;
            for gx in 0..g.width {
                let alpha = g.data[gy * g.width + gx];
                if alpha == 0 { continue; }
                let sx = gx0 + gx as i32;
                if sx < 0 || sx >= bw as i32 { continue; }
                let sx = sx as usize;
                let idx = sy * bw + sx;
                let ex  = buf[idx];
                let bg  = Color { r: ((ex>>16)&0xff) as u8, g: ((ex>>8)&0xff) as u8, b: (ex&0xff) as u8 };
                buf[idx] = bg.blend(fg, alpha).to_u32();
            }
        }
    }

    fn fill_rect(buf: &mut [u32], bw: usize, bh: usize,
                 px: usize, py: usize, rw: usize, rh: usize, color: u32) {
        for y in py..(py + rh).min(bh) {
            let base = y * bw;
            for x in px..(px + rw).min(bw) { buf[base + x] = color; }
        }
    }

    /// Thin circle outline (1–2 px) centred at (cx, cy) with radius r.
    fn stroke_circle(buf: &mut [u32], bw: usize, bh: usize,
                     cx: usize, cy: usize, r: usize, color: u32) {
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
    fn fill_rounded(buf: &mut [u32], bw: usize, bh: usize,
                    x: usize, y: usize, w: usize, h: usize, r: usize, color: u32) {
        let r2 = (r * r) as i32;
        for dy in 0..h {
            for dx in 0..w {
                let in_x_corner = dx < r || dx + r >= w;
                let in_y_corner = dy < r || dy + r >= h;
                if in_x_corner && in_y_corner {
                    let cx = if dx < r { (r - dx) as i32 } else { (dx + r + 1 - w) as i32 };
                    let cy = if dy < r { (r - dy) as i32 } else { (dy + r + 1 - h) as i32 };
                    if cx * cx + cy * cy > r2 { continue; }
                }
                let px = x + dx; let py = y + dy;
                if px < bw && py < bh { buf[py * bw + px] = color; }
            }
        }
    }

    // ── Tab bar ───────────────────────────────────────────────────────────────

    /// `hover` — index of hovered tab, or `tabs.len()` for the + button.
    fn draw_tab_bar(
        &mut self,
        buf: &mut [u32], bw: usize, bh: usize,
        tabs: &[String], active: usize,
        hover: Option<usize>,
    ) {
        let tby = self.tab_bar_height;
        let cw  = self.cell_width;
        let ch  = self.cell_height;

        // Neutral dark palette — mirrors terminal background aesthetic
        let bar_bg       = Color::new(0x14, 0x14, 0x14).to_u32(); // slightly darker than bg
        let outline_col  = Color::new(0x58, 0x58, 0x58).to_u32(); // medium gray — active outline
        let hover_bg     = Color::new(0x26, 0x26, 0x26).to_u32(); // subtle hover fill
        let sep_col      = Color::new(0x2e, 0x2e, 0x2e).to_u32(); // dim separator
        let bottom_col   = Color::new(0x2e, 0x2e, 0x2e).to_u32(); // bottom rule
        let fg_active    = DEFAULT_FG;
        let fg_inactive  = Color::new(0x66, 0x66, 0x66); // dim gray
        let fg_shortcut  = Color::new(0x3a, 0x3a, 0x3a); // very dim

        // Bar fill + bottom border
        Self::fill_rect(buf, bw, bh, 0, 0, bw, tby, bar_bg);
        Self::fill_rect(buf, bw, bh, 0, tby.saturating_sub(1), bw, 1, bottom_col);

        // Reserve right side for + button
        let plus_area = tby;
        let tabs_w    = bw.saturating_sub(plus_area);
        let n         = tabs.len().max(1);
        let tab_w     = tabs_w / n;
        let pad_v     = 4usize;
        let pill_h    = tby.saturating_sub(pad_v * 2);
        let text_y    = (tby.saturating_sub(ch)) / 2;
        let shortcut_w = 3 * cw; // ⌘ + digit + 1 pad

        for (i, title) in tabs.iter().enumerate() {
            let is_active = i == active;
            let is_hover  = hover == Some(i) && !is_active;
            let tx = i * tab_w;
            if tx >= tabs_w { break; }
            let tw = if i + 1 == n { tabs_w.saturating_sub(tx) } else { tab_w };
            let pill_x = tx + 4;
            let pill_w = tw.saturating_sub(8);

            // ── Hover fill ────────────────────────────────────────────────────
            if is_hover {
                Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 4, hover_bg);
            }

            // ── Active: outlined rounded rect (border only) ───────────────────
            if is_active && pill_w > 2 && pill_h > 2 {
                // Draw full rounded rect in outline colour, then overdraw interior
                Self::fill_rounded(buf, bw, bh, pill_x, pad_v, pill_w, pill_h, 5, outline_col);
                Self::fill_rounded(buf, bw, bh, pill_x + 1, pad_v + 1,
                                   pill_w - 2, pill_h - 2, 4, bar_bg);
            }

            // ── Separator (skip first; hide on both sides of active) ──────────
            if i > 0 && i != active && i != active + 1 {
                let sep_top    = pad_v + 4;
                let sep_bottom = tby.saturating_sub(pad_v + 4);
                for y in sep_top..sep_bottom {
                    if tx < bw { buf[y * bw + tx] = sep_col; }
                }
            }

            // ── ⌘N shortcut — right-aligned ───────────────────────────────────
            let shortcut = format!("\u{2318}{}", i + 1);
            let sc_x = tx + tw.saturating_sub(shortcut_w);
            let mut col_x = sc_x;
            for c in shortcut.chars() {
                if col_x + cw > tx + tw { break; }
                self.blit(buf, bw, bh, col_x, text_y, c, fg_shortcut);
                col_x += cw;
            }

            // ── Title — left-aligned, truncated ───────────────────────────────
            let fg = if is_active { fg_active } else { fg_inactive };
            let left_pad   = tx + cw;
            let right_edge = tx + tw.saturating_sub(shortcut_w + cw);
            let max_cols   = right_edge.saturating_sub(left_pad) / cw;
            let chars: Vec<char> = title.chars().collect();
            let show_n    = chars.len().min(max_cols);
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
        let plus_r  = 9usize;
        Self::stroke_circle(buf, bw, bh, plus_cx, plus_cy, plus_r, plus_col);
        let arm = (plus_r / 2) as i32;
        for d in -arm..=arm {
            let px = (plus_cx as i32 + d) as usize;
            let py = (plus_cy as i32 + d) as usize;
            if px < bw && plus_cy < bh { buf[plus_cy * bw + px] = plus_col; }
            if plus_cx < bw && py < bh { buf[py * bw + plus_cx] = plus_col; }
        }
    }

    // ── Public render ─────────────────────────────────────────────────────────

    /// `hover` — hovered tab index, or `tabs.len()` for the + button.
    pub fn render(
        &mut self,
        buf: &mut [u32], bw: usize, bh: usize,
        state: &TerminalState,
        show_cursor: bool,
        ghost: Option<&str>,
        tabs: &[String],
        active_tab: usize,
        hover: Option<usize>,
    ) {
        buf.fill(DEFAULT_BG.to_u32());

        // ── Tab bar ───────────────────────────────────────────────────────────
        self.draw_tab_bar(buf, bw, bh, tabs, active_tab, hover);

        let tby = self.tab_bar_height;   // y offset for terminal content
        let cw  = self.cell_width;
        let ch  = self.cell_height;
        let term_h  = bh.saturating_sub(tby);
        let vis_rows = (term_h / ch).min(state.rows);
        let vis_cols = (bw / cw).min(state.cols);

        // ── 1. Terminal grid ──────────────────────────────────────────────────
        for row in 0..vis_rows {
            for col in 0..vis_cols {
                let cell = state.visual_cell(row, col);
                let mut fg = cell.attrs.fg;
                let mut bg = cell.attrs.bg;
                if cell.attrs.inverse { std::mem::swap(&mut fg, &mut bg); }

                let px = col * cw;
                let py = tby + row * ch;
                let bg32 = bg.to_u32();
                for dy in 0..ch {
                    let y = py + dy; if y >= bh { break; }
                    let base = y * bw;
                    for dx in 0..cw {
                        let x = px + dx; if x < bw { buf[base + x] = bg32; }
                    }
                }
                let c = cell.c;
                if c != ' ' && c != '\0' { self.blit(buf, bw, bh, px, py, c, fg); }
            }
        }

        // ── 2. Ghost text ─────────────────────────────────────────────────────
        if !state.is_scrolled_back() {
            if let Some(g) = ghost {
                let py = tby + state.cursor_row * ch;
                for (i, c) in g.chars().enumerate() {
                    let col = state.cursor_col + i;
                    if col >= vis_cols { break; }
                    self.blit(buf, bw, bh, col * cw, py, c, GHOST_COLOR);
                }
            }
        }

        // ── 3. Cursor ─────────────────────────────────────────────────────────
        if !state.is_scrolled_back()
            && show_cursor
            && state.cursor_row < vis_rows
            && state.cursor_col < vis_cols
        {
            let px  = state.cursor_col * cw;
            let py  = tby + state.cursor_row * ch;
            let c32 = CURSOR_COLOR.to_u32();
            for dy in 0..ch {
                let y = py + dy; if y >= bh { break; }
                let base = y * bw;
                for dx in 0..2 { let x = px + dx; if x < bw { buf[base + x] = c32; } }
            }
        }

        // ── 4. Scrollbar ──────────────────────────────────────────────────────
        let sb_total = state.scrollback.len();
        if sb_total > 0 {
            let total    = sb_total + state.rows;
            let thumb_h  = ((term_h * vis_rows) / total).max(8).min(term_h);
            let view_top = sb_total.saturating_sub(state.viewport_offset);
            let thumb_y  = tby + (view_top * (term_h - thumb_h)) / total.max(1);

            let bar_x      = bw.saturating_sub(3);
            let track_col  = Color::new(0x2a, 0x2a, 0x2a).to_u32();
            let thumb_col  = if state.is_scrolled_back() {
                Color::new(0x66, 0x66, 0x66).to_u32()
            } else {
                Color::new(0x44, 0x44, 0x44).to_u32()
            };
            for y in tby..bh {
                let color = if y >= thumb_y && y < thumb_y + thumb_h { thumb_col } else { track_col };
                if bar_x < bw     { buf[y * bw + bar_x]     = color; }
                if bar_x + 1 < bw { buf[y * bw + bar_x + 1] = color; }
            }
        }
    }
}
