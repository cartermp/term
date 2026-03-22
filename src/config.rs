#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

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

// Catppuccin Mocha — soft, warm dark theme
pub const DEFAULT_FG: Color = Color::new(0xcd, 0xd6, 0xf4);
pub const DEFAULT_BG: Color = Color::new(0x1e, 0x1e, 0x2e);
pub const CURSOR_COLOR: Color = Color::new(0xf5, 0xc2, 0xe7);

pub const ANSI_COLORS: [Color; 16] = [
    Color::new(0x45, 0x47, 0x5a), // 0  black    (Surface1)
    Color::new(0xf3, 0x8b, 0xa8), // 1  red      (Red)
    Color::new(0xa6, 0xe3, 0xa1), // 2  green    (Green)
    Color::new(0xf9, 0xe2, 0xaf), // 3  yellow   (Yellow)
    Color::new(0x89, 0xb4, 0xfa), // 4  blue     (Blue)
    Color::new(0xcb, 0xa6, 0xf7), // 5  magenta  (Mauve)
    Color::new(0x89, 0xdc, 0xeb), // 6  cyan     (Sky)
    Color::new(0xba, 0xc2, 0xde), // 7  white    (Subtext1)
    Color::new(0x58, 0x5b, 0x70), // 8  br.black (Surface2)
    Color::new(0xf3, 0x8b, 0xa8), // 9  br.red
    Color::new(0xa6, 0xe3, 0xa1), // 10 br.green
    Color::new(0xf9, 0xe2, 0xaf), // 11 br.yellow
    Color::new(0x89, 0xb4, 0xfa), // 12 br.blue
    Color::new(0xcb, 0xa6, 0xf7), // 13 br.magenta
    Color::new(0x89, 0xdc, 0xeb), // 14 br.cyan
    Color::new(0xa6, 0xad, 0xc8), // 15 br.white (Subtext0)
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

// Ghost text color — Overlay0, clearly dimmer than regular text
pub const GHOST_COLOR: Color = Color::new(0x6c, 0x70, 0x86);


pub const FONT_SIZE_PT: f32 = 14.0;
pub const WINDOW_WIDTH: u32 = 960;
pub const WINDOW_HEIGHT: u32 = 640;
