#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    #[allow(dead_code)]
    pub fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    #[allow(dead_code)]
    pub fn blend(self, fg: Color, alpha: u8) -> Color {
        let a = alpha as u32;
        let ia = 255 - a;
        Color {
            r: ((fg.r as u32 * a + self.r as u32 * ia) / 255) as u8,
            g: ((fg.g as u32 * a + self.g as u32 * ia) / 255) as u8,
            b: ((fg.b as u32 * a + self.b as u32 * ia) / 255) as u8,
        }
    }
}

// Neutral dark theme — matches the screenshot aesthetic
// Near-black background, vivid ANSI palette, warm amber yellow, bright green
pub const DEFAULT_FG: Color = Color::new(0xc8, 0xc8, 0xc8); // light gray
pub const DEFAULT_BG: Color = Color::new(0x1a, 0x1a, 0x1a); // near-black neutral
pub const CURSOR_COLOR: Color = Color::new(0xae, 0xaf, 0xad); // soft gray block

pub const ANSI_COLORS: [Color; 16] = [
    Color::new(0x3d, 0x3d, 0x3d), // 0  black
    Color::new(0xe0, 0x52, 0x52), // 1  red
    Color::new(0x5a, 0xf7, 0x8e), // 2  green     — vivid bright green
    Color::new(0xf0, 0xc6, 0x74), // 3  yellow    — warm amber/gold
    Color::new(0x82, 0xaa, 0xff), // 4  blue      — cornflower blue
    Color::new(0xc7, 0x92, 0xea), // 5  magenta
    Color::new(0x56, 0xb6, 0xc2), // 6  cyan
    Color::new(0xc8, 0xc8, 0xc8), // 7  white
    Color::new(0x63, 0x63, 0x63), // 8  br.black
    Color::new(0xff, 0x5c, 0x57), // 9  br.red
    Color::new(0x5a, 0xf7, 0x8e), // 10 br.green
    Color::new(0xf0, 0xc6, 0x74), // 11 br.yellow
    Color::new(0x82, 0xaa, 0xff), // 12 br.blue
    Color::new(0xc7, 0x92, 0xea), // 13 br.magenta
    Color::new(0x56, 0xb6, 0xc2), // 14 br.cyan
    Color::new(0xee, 0xee, 0xee), // 15 br.white
];

pub fn ansi_256_color(index: u8) -> Color {
    if (index as usize) < ANSI_COLORS.len() {
        return ANSI_COLORS[index as usize];
    }
    if index >= 232 {
        let v = (index - 232) * 10 + 8;
        return Color::new(v, v, v);
    }
    let idx = index - 16;
    let b = idx % 6;
    let g = (idx / 6) % 6;
    let r = idx / 36;
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    Color::new(CUBE[r as usize], CUBE[g as usize], CUBE[b as usize])
}

// Ghost text — dim gray, clearly below regular text brightness
pub const GHOST_COLOR: Color = Color::new(0x55, 0x55, 0x55);

/// Alpha applied to terminal background cells (0xFF = fully opaque).
pub const BG_ALPHA: u8 = 0xFF;

pub const FONT_SIZE_PT: f32 = 14.0;
pub const WINDOW_WIDTH: u32 = 960;
pub const WINDOW_HEIGHT: u32 = 640;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Color packing ─────────────────────────────────────────────────────────

    #[test]
    fn to_u32_packs_rgb() {
        assert_eq!(Color::new(0xff, 0x00, 0x00).to_u32(), 0x00ff0000);
        assert_eq!(Color::new(0x00, 0xff, 0x00).to_u32(), 0x0000ff00);
        assert_eq!(Color::new(0x00, 0x00, 0xff).to_u32(), 0x000000ff);
        assert_eq!(Color::new(0x12, 0x34, 0x56).to_u32(), 0x00123456);
    }

    // ── Alpha blending ────────────────────────────────────────────────────────

    #[test]
    fn blend_fully_opaque_replaces_bg() {
        let bg = Color::new(0, 0, 0);
        let fg = Color::new(200, 100, 50);
        let out = bg.blend(fg, 255);
        assert_eq!((out.r, out.g, out.b), (200, 100, 50));
    }

    #[test]
    fn blend_fully_transparent_keeps_bg() {
        let bg = Color::new(80, 90, 100);
        let fg = Color::new(200, 200, 200);
        let out = bg.blend(fg, 0);
        assert_eq!((out.r, out.g, out.b), (80, 90, 100));
    }

    #[test]
    fn blend_half_alpha_mixes() {
        let bg = Color::new(0, 0, 0);
        let fg = Color::new(100, 100, 100);
        let out = bg.blend(fg, 128);
        // 100 * 128 / 255 ≈ 50
        assert!(out.r >= 49 && out.r <= 51, "expected ~50, got {}", out.r);
    }

    // ── 256-colour lookup ─────────────────────────────────────────────────────

    #[test]
    fn ansi_256_first_16_match_ansi_table() {
        for i in 0u8..16 {
            assert_eq!(
                ansi_256_color(i).to_u32(),
                ANSI_COLORS[i as usize].to_u32(),
                "index {i}"
            );
        }
    }

    #[test]
    fn ansi_256_grayscale_ramp() {
        // index 232 → v = 0*10+8 = 8
        let c = ansi_256_color(232);
        assert_eq!((c.r, c.g, c.b), (8, 8, 8));
        // index 255 → v = 23*10+8 = 238
        let c = ansi_256_color(255);
        assert_eq!((c.r, c.g, c.b), (238, 238, 238));
    }

    #[test]
    fn ansi_256_color_cube_corners() {
        // index 16  = (r=0,g=0,b=0) — black corner of cube
        let c = ansi_256_color(16);
        assert_eq!((c.r, c.g, c.b), (0, 0, 0));
        // index 21  = (r=0,g=0,b=5) — pure blue corner
        let c = ansi_256_color(21);
        assert_eq!((c.r, c.g, c.b), (0, 0, 255));
        // index 231 = (r=5,g=5,b=5) — white corner
        let c = ansi_256_color(231);
        assert_eq!((c.r, c.g, c.b), (255, 255, 255));
    }
}
