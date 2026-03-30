use crate::config::*;
use crate::terminal::TerminalState;
use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;

// ── tcat gutter detection ─────────────────────────────────────────────────────

/// Find the first content column of a tcat gutter line, or 0 if this row is
/// not tcat output.
fn tcat_gutter_end(state: &TerminalState, row: usize, vis_cols: usize) -> usize {
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
        let mut prefix = 0..c - 1;
        let has_digit = prefix.clone().any(|i| state.visual_cell(row, i).c.is_ascii_digit());
        let all_ok = prefix
            .all(|i| matches!(state.visual_cell(row, i).c, ' ' | '\0' | '0'..='9'));
        if has_digit && all_ok {
            return c + 2;
        }
    }
    0
}

// ── Shader sources ────────────────────────────────────────────────────────────

const RECT_SHADER: &str = r#"
struct Uni { res: vec4<f32> }
@group(0) @binding(0) var<uniform> uni: Uni;

struct Inst {
    @location(0) pos:   vec2<f32>,
    @location(1) sz:    vec2<f32>,
    @location(2) color: vec4<f32>,
}
struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) col: vec4<f32>,
}

@vertex fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
    let uv = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = i.pos + uv * i.sz;
    let ndc = px / uni.res.xy * vec2(2., -2.) + vec2(-1., 1.);
    return Out(vec4(ndc, 0., 1.), i.color);
}
@fragment fn fs(o: Out) -> @location(0) vec4<f32> { return o.col; }
"#;

const GLYPH_SHADER: &str = r#"
struct Uni { res: vec4<f32> }
@group(0) @binding(0) var<uniform> uni: Uni;
@group(1) @binding(0) var atlas_t: texture_2d<f32>;
@group(1) @binding(1) var atlas_s: sampler;

struct Inst {
    @location(0) pos:     vec2<f32>,
    @location(1) sz:      vec2<f32>,
    @location(2) uv_pos:  vec2<f32>,
    @location(3) uv_sz:   vec2<f32>,
    @location(4) fg:      vec4<f32>,
}
struct Out {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) fg: vec4<f32>,
}

@vertex fn vs(@builtin(vertex_index) vi: u32, i: Inst) -> Out {
    let uv = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = i.pos + uv * i.sz;
    let ndc = px / uni.res.xy * vec2(2., -2.) + vec2(-1., 1.);
    return Out(vec4(ndc, 0., 1.), i.uv_pos + uv * i.uv_sz, i.fg);
}
@fragment fn fs(o: Out) -> @location(0) vec4<f32> {
    let a = textureSample(atlas_t, atlas_s, o.uv).r;
    return vec4(o.fg.rgb, o.fg.a * a);
}
"#;

// ── Instance types ────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct RectInst {
    pos:   [f32; 2],
    sz:    [f32; 2],
    color: [f32; 4],
}

impl RectInst {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct GlyphInst {
    pos:    [f32; 2],
    sz:     [f32; 2],
    uv_pos: [f32; 2],
    uv_sz:  [f32; 2],
    fg:     [f32; 4],
}

impl GlyphInst {
    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 8,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 16,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 24,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: 32,
                    shader_location: 4,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

// ── Atlas ─────────────────────────────────────────────────────────────────────

const ATLAS_SIZE: u32 = 1024;

#[derive(Clone)]
struct AtlasEntry {
    uv_x: f32,
    uv_y: f32,
    uv_w: f32,
    uv_h: f32,
    // fontdue placement offsets (pixels from cell origin)
    glyph_x: i32,
    glyph_y: i32,
    w: u32,
    h: u32,
}

// ── Color helpers ─────────────────────────────────────────────────────────────

fn c2f(c: Color) -> [f32; 4] {
    [c.r as f32 / 255., c.g as f32 / 255., c.b as f32 / 255., 1.]
}

fn c2fa(c: Color, a: f32) -> [f32; 4] {
    [c.r as f32 / 255., c.g as f32 / 255., c.b as f32 / 255., a]
}

fn rgb_f(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255., g as f32 / 255., b as f32 / 255., 1.]
}

// ── Instance list helpers ─────────────────────────────────────────────────────

fn push_rect(v: &mut Vec<RectInst>, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
    if w > 0. && h > 0. {
        v.push(RectInst { pos: [x, y], sz: [w, h], color });
    }
}

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    pub device: wgpu::Device,
    queue: wgpu::Queue,

    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,

    uni_buf: wgpu::Buffer,
    uni_bg: wgpu::BindGroup,

    atlas_tex: wgpu::Texture,
    _atlas_view: wgpu::TextureView,
    _atlas_sampler: wgpu::Sampler,
    atlas_bg: wgpu::BindGroup,
    _atlas_bgl: wgpu::BindGroupLayout,
    atlas_cache: HashMap<char, Option<AtlasEntry>>,
    atlas_x: u32,
    atlas_y: u32,
    atlas_row_h: u32,

    // Reusable GPU instance buffers (grown on demand)
    rect_buf: wgpu::Buffer,
    rect_buf_cap: usize,
    glyph_buf: wgpu::Buffer,
    glyph_buf_cap: usize,

    font: fontdue::Font,
    pub cell_width: usize,
    pub cell_height: usize,
    pub baseline: i32,
    pub tab_bar_height: usize,
    font_size: f32,
    surface_format: wgpu::TextureFormat,
}

impl Renderer {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface_format: wgpu::TextureFormat,
        scale_factor: f64,
    ) -> Self {
        // ── Font ──────────────────────────────────────────────────────────────
        let font_size = (FONT_SIZE_PT * scale_factor as f32).round();
        let font_data = include_bytes!("../assets/JetBrainsMono-Regular.ttf").to_vec();
        let font = fontdue::Font::from_bytes(
            font_data.as_slice(),
            fontdue::FontSettings {
                scale: font_size,
                collection_index: 0,
                load_substitutions: true,
            },
        )
        .expect("font load");

        let (m, _) = font.rasterize('M', font_size);
        let cell_width = m.advance_width.ceil() as usize;
        let lm = font.horizontal_line_metrics(font_size).unwrap();
        let ascent = lm.ascent.ceil() as i32;
        let descent = (-lm.descent).ceil() as i32;
        let gap = lm.line_gap.ceil() as i32;
        let cell_height =
            (ascent + descent + gap).max(font_size as i32 + 4) as usize;
        let tab_bar_height = cell_height + 16;

        // ── Uniform buffer (resolution) ───────────────────────────────────────
        let uni_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uni"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uni_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("uni_bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let uni_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uni_bg"),
            layout: &uni_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uni_buf.as_entire_binding(),
            }],
        });

        // ── Atlas texture ─────────────────────────────────────────────────────
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view =
            atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_smp"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let atlas_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas_bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float {
                            filterable: true,
                        },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(
                        wgpu::SamplerBindingType::Filtering,
                    ),
                    count: None,
                },
            ],
        });
        let atlas_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas_bg"),
            layout: &atlas_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });

        // ── Pipelines ─────────────────────────────────────────────────────────
        let rect_pipeline = Self::make_rect_pipeline(
            &device,
            surface_format,
            &uni_bgl,
        );
        let glyph_pipeline = Self::make_glyph_pipeline(
            &device,
            surface_format,
            &uni_bgl,
            &atlas_bgl,
        );

        // ── Instance buffers (pre-allocate for typical terminal) ───────────────
        let init_rect = 8192usize;
        let init_glyph = 8192usize;
        let rect_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rect_buf"),
            size: (init_rect * std::mem::size_of::<RectInst>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let glyph_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("glyph_buf"),
            size: (init_glyph * std::mem::size_of::<GlyphInst>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            device,
            queue,
            rect_pipeline,
            glyph_pipeline,
            uni_buf,
            uni_bg,
            atlas_tex,
            _atlas_view: atlas_view,
            _atlas_sampler: atlas_sampler,
            atlas_bg,
            _atlas_bgl: atlas_bgl,
            atlas_cache: HashMap::new(),
            atlas_x: 0,
            atlas_y: 0,
            atlas_row_h: 0,
            rect_buf,
            rect_buf_cap: init_rect,
            glyph_buf,
            glyph_buf_cap: init_glyph,
            font,
            cell_width,
            cell_height,
            baseline: ascent,
            tab_bar_height,
            font_size,
            surface_format,
        }
    }

    /// Rebuild the renderer for a new DPI scale, reusing the existing GPU device/queue.
    pub fn rescale(self, scale_factor: f64) -> Self {
        let fmt = self.surface_format;
        Self::new(self.device, self.queue, fmt, scale_factor)
    }

    // ── Pipeline constructors ─────────────────────────────────────────────────

    fn make_rect_pipeline(
        device: &wgpu::Device,
        fmt: wgpu::TextureFormat,
        uni_bgl: &wgpu::BindGroupLayout,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rect_shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[uni_bgl],
            push_constant_ranges: &[],
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rect_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                buffers: &[RectInst::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format: fmt,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        })
    }

    fn make_glyph_pipeline(
        device: &wgpu::Device,
        fmt: wgpu::TextureFormat,
        uni_bgl: &wgpu::BindGroupLayout,
        atlas_bgl: &wgpu::BindGroupLayout,
    ) -> wgpu::RenderPipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph_shader"),
            source: wgpu::ShaderSource::Wgsl(GLYPH_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[uni_bgl, atlas_bgl],
            push_constant_ranges: &[],
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph_pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                buffers: &[GlyphInst::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format: fmt,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        })
    }

    // ── Atlas management ──────────────────────────────────────────────────────

    /// Ensure glyph `c` is in the atlas. Returns a clone of the entry, or None
    /// if the character has no visible glyph (space, control, .notdef).
    fn ensure_glyph(&mut self, c: char) -> Option<AtlasEntry> {
        if let Some(entry) = self.atlas_cache.get(&c) {
            return entry.clone();
        }
        // Skip chars not in font
        if self.font.lookup_glyph_index(c) == 0 {
            self.atlas_cache.insert(c, None);
            return None;
        }
        let (m, bitmap) = self.font.rasterize(c, self.font_size);
        if m.width == 0 || m.height == 0 {
            self.atlas_cache.insert(c, None);
            return None;
        }
        let gw = m.width as u32;
        let gh = m.height as u32;

        // Advance to next row if needed (1px padding)
        if self.atlas_x + gw + 1 > ATLAS_SIZE {
            self.atlas_y += self.atlas_row_h + 1;
            self.atlas_x = 0;
            self.atlas_row_h = 0;
        }
        if self.atlas_y + gh > ATLAS_SIZE {
            // Atlas full — evict and restart (rare; terminals have small glyph sets)
            self.atlas_cache.clear();
            self.atlas_x = 0;
            self.atlas_y = 0;
            self.atlas_row_h = 0;
        }

        let ax = self.atlas_x;
        let ay = self.atlas_y;

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas_tex,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ax, y: ay, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &bitmap,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(gw),
                rows_per_image: Some(gh),
            },
            wgpu::Extent3d { width: gw, height: gh, depth_or_array_layers: 1 },
        );

        self.atlas_x += gw + 1;
        self.atlas_row_h = self.atlas_row_h.max(gh);

        // Compute pixel offset to place glyph at the correct position relative
        // to the cell origin (top-left of the cell).
        // baseline is measured from cell top. fontdue ymin is from baseline up.
        let glyph_y = self.baseline - (m.ymin + m.height as i32);
        let glyph_x = m.xmin;

        let entry = AtlasEntry {
            uv_x: ax as f32 / ATLAS_SIZE as f32,
            uv_y: ay as f32 / ATLAS_SIZE as f32,
            uv_w: gw as f32 / ATLAS_SIZE as f32,
            uv_h: gh as f32 / ATLAS_SIZE as f32,
            glyph_x,
            glyph_y,
            w: gw,
            h: gh,
        };
        self.atlas_cache.insert(c, Some(entry.clone()));
        Some(entry)
    }

    // ── Glyph emit helper ─────────────────────────────────────────────────────

    fn emit_char(
        &mut self,
        glyphs: &mut Vec<GlyphInst>,
        c: char,
        px: f32,
        py: f32,
        fg: [f32; 4],
    ) {
        if let Some(e) = self.ensure_glyph(c) {
            glyphs.push(GlyphInst {
                pos: [px + e.glyph_x as f32, py + e.glyph_y as f32],
                sz: [e.w as f32, e.h as f32],
                uv_pos: [e.uv_x, e.uv_y],
                uv_sz: [e.uv_w, e.uv_h],
                fg,
            });
        }
    }

    // ── Block character rects ─────────────────────────────────────────────────

    /// Decompose a Unicode block/Braille char into colored fill rects.
    /// Returns false if `c` is not a handled block character.
    fn push_block_char(
        block_rects: &mut Vec<RectInst>,
        px: f32,
        py: f32,
        cw: f32,
        ch: f32,
        c: char,
        fg: [f32; 4],
    ) -> bool {
        // Fill [x0..x1) × [y0..y1) within the cell (in cell-relative pixels).
        macro_rules! f {
            ($x0:expr, $y0:expr, $x1:expr, $y1:expr) => {{
                let x0 = $x0 as f32;
                let y0 = $y0 as f32;
                let x1 = ($x1 as f32).min(cw);
                let y1 = ($y1 as f32).min(ch);
                if x1 > x0 && y1 > y0 {
                    block_rects.push(RectInst {
                        pos: [px + x0, py + y0],
                        sz: [x1 - x0, y1 - y0],
                        color: fg,
                    });
                }
            }};
        }
        let cwu = cw as usize;
        let chu = ch as usize;
        match c {
            '\u{2581}' => f!(0, chu * 7 / 8, cwu, chu),
            '\u{2582}' => f!(0, chu * 3 / 4, cwu, chu),
            '\u{2583}' => f!(0, chu * 5 / 8, cwu, chu),
            '\u{2584}' => f!(0, chu / 2, cwu, chu),
            '\u{2585}' => f!(0, chu * 3 / 8, cwu, chu),
            '\u{2586}' => f!(0, chu / 4, cwu, chu),
            '\u{2587}' => f!(0, chu / 8, cwu, chu),
            '\u{2588}' => f!(0, 0, cwu, chu),
            '\u{2580}' => f!(0, 0, cwu, chu / 2),
            '\u{2594}' => f!(0, 0, cwu, (chu / 8).max(1)),
            '\u{258F}' => f!(0, 0, (cwu / 8).max(1), chu),
            '\u{258E}' => f!(0, 0, cwu / 4, chu),
            '\u{258D}' => f!(0, 0, cwu * 3 / 8, chu),
            '\u{258C}' => f!(0, 0, cwu / 2, chu),
            '\u{258B}' => f!(0, 0, cwu * 5 / 8, chu),
            '\u{258A}' => f!(0, 0, cwu * 3 / 4, chu),
            '\u{2589}' => f!(0, 0, cwu * 7 / 8, chu),
            '\u{2590}' => f!(cw / 2., 0., cwu, chu),
            '\u{2595}' => f!(cwu * 7 / 8, 0, cwu, chu),
            '\u{2596}' => f!(0, chu / 2, cwu / 2, chu),
            '\u{2597}' => f!(cwu / 2, chu / 2, cwu, chu),
            '\u{2598}' => f!(0, 0, cwu / 2, chu / 2),
            '\u{259D}' => f!(cwu / 2, 0, cwu, chu / 2),
            '\u{2599}' => {
                f!(0, 0, cwu / 2, chu / 2);
                f!(0, chu / 2, cwu, chu);
            }
            '\u{259A}' => {
                f!(0, 0, cwu / 2, chu / 2);
                f!(cwu / 2, chu / 2, cwu, chu);
            }
            '\u{259B}' => {
                f!(0, 0, cwu, chu / 2);
                f!(0, chu / 2, cwu / 2, chu);
            }
            '\u{259C}' => {
                f!(0, 0, cwu, chu / 2);
                f!(cwu / 2, chu / 2, cwu, chu);
            }
            '\u{259E}' => {
                f!(cwu / 2, 0, cwu, chu / 2);
                f!(0, chu / 2, cwu / 2, chu);
            }
            '\u{259F}' => {
                f!(cwu / 2, 0, cwu, chu / 2);
                f!(0, chu / 2, cwu, chu);
            }
            '\u{2800}'..='\u{28FF}' => {
                let bits = c as u32 - 0x2800;
                if bits == 0 {
                    return true;
                }
                let col0_w = cwu / 2;
                let col1_w = cwu - col0_w;
                let row_h = [chu / 4, chu / 4, chu / 4, chu - 3 * (chu / 4)];
                let mut ry = 0usize;
                for row in 0..4usize {
                    let rh = row_h[row];
                    let bit_c0 = [0u32, 1, 2, 6][row];
                    if bits & (1 << bit_c0) != 0 {
                        f!(0, ry, col0_w, ry + rh);
                    }
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

    // ── Buffer resize helper ──────────────────────────────────────────────────

    fn ensure_rect_buf(&mut self, need: usize) {
        if need > self.rect_buf_cap {
            let cap = need.next_power_of_two().max(8192);
            self.rect_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rect_buf"),
                size: (cap * std::mem::size_of::<RectInst>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.rect_buf_cap = cap;
        }
    }

    fn ensure_glyph_buf(&mut self, need: usize) {
        if need > self.glyph_buf_cap {
            let cap = need.next_power_of_two().max(8192);
            self.glyph_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("glyph_buf"),
                size: (cap * std::mem::size_of::<GlyphInst>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.glyph_buf_cap = cap;
        }
    }

    // ── Tab bar builder ───────────────────────────────────────────────────────

} // end impl Renderer (temporarily close so tab_bar_rects can be a free fn)

/// Pure rect geometry for the tab bar — no GPU, no glyph cache, fully testable.
///
/// Returns `(bg_rects, fg_rects)`.  **bg_rects must be submitted to the GPU
/// before the glyph pass** so that tab titles are drawn on top of the pill
/// backgrounds.  fg_rects (separators, drag overlays, + button) go after.
fn tab_bar_rects(
    tby: f32,
    bw: f32,
    n_tabs: usize,
    active: usize,
    hover: Option<usize>,
    drag: Option<(usize, usize, f64)>,
) -> (Vec<RectInst>, Vec<RectInst>) {
    let mut bg: Vec<RectInst> = Vec::new();
    let mut fg: Vec<RectInst> = Vec::new();

    let bar_bg   = rgb_f(0x14, 0x14, 0x14);
    let outline  = rgb_f(0x58, 0x58, 0x58);
    let hover_bg = rgb_f(0x26, 0x26, 0x26);
    let sep_col  = rgb_f(0x2e, 0x2e, 0x2e);
    let bottom   = rgb_f(0x2e, 0x2e, 0x2e);

    // Bar background + bottom border
    push_rect(&mut bg, 0., 0., bw, tby, bar_bg);
    push_rect(&mut bg, 0., tby - 1., bw, 1., bottom);

    let plus_area = tby;
    let tabs_w    = bw - plus_area;
    let n         = n_tabs.max(1);
    let tab_w     = tabs_w / n as f32;
    let pad_v     = 4.;
    let pill_h    = tby - pad_v * 2.;

    let visual_order: Vec<usize> = if let Some((from, to, _)) = drag {
        let mut order: Vec<usize> = (0..n_tabs).collect();
        if from < order.len() {
            let item = order.remove(from);
            order.insert(to.min(order.len()), item);
        }
        order
    } else {
        (0..n_tabs).collect()
    };
    let visual_active = visual_order.iter().position(|&i| i == active).unwrap_or(active);
    let drag_orig = drag.map(|(from, _, _)| from);

    for (vi, &orig_idx) in visual_order.iter().enumerate() {
        let is_active   = orig_idx == active;
        let is_dragging = drag_orig == Some(orig_idx);
        let is_hover    = drag.is_none() && hover == Some(vi) && !is_active;

        let tx = vi as f32 * tab_w;
        if tx >= tabs_w { break; }
        let tw    = if vi + 1 == n { tabs_w - tx } else { tab_w };
        let pill_x = tx + 4.;
        let pill_w = tw - 8.;

        if is_dragging {
            if vi > 0 && vi != visual_active && vi != visual_active + 1 {
                push_rect(&mut fg, tx, pad_v + 4., 1., tby - (pad_v + 4.) * 2., sep_col);
            }
            let ghost = rgb_f(0x38, 0x38, 0x38);
            if pill_w > 2. && pill_h > 2. {
                push_rect(&mut fg, pill_x, pad_v, pill_w, pill_h, ghost);
                push_rect(&mut fg, pill_x + 1., pad_v + 1., pill_w - 2., pill_h - 2., bar_bg);
            }
            continue;
        }

        // Active pill and hover fill go in bg_rects so glyphs render on top.
        if is_hover {
            push_rect(&mut bg, pill_x, pad_v, pill_w, pill_h, hover_bg);
        }
        if is_active && pill_w > 2. && pill_h > 2. {
            push_rect(&mut bg, pill_x, pad_v, pill_w, pill_h, outline);
            push_rect(&mut bg, pill_x + 1., pad_v + 1., pill_w - 2., pill_h - 2., bar_bg);
        }

        // Separator
        if vi > 0 && vi != visual_active && vi != visual_active + 1 {
            push_rect(&mut fg, tx, pad_v + 4., 1., tby - (pad_v + 4.) * 2., sep_col);
        }
    }

    // Floating dragged tab pill
    if let Some((_, _, cursor_x)) = drag {
        let half       = tab_w / 2.;
        let float_left = (cursor_x as f32 - half).max(0.).min(tabs_w - tab_w);
        let pill_x     = float_left + 4.;
        let pill_w     = tab_w - 8.;
        let lifted_outline = rgb_f(0xa0, 0xa0, 0xa0);
        let lifted_bg      = rgb_f(0x20, 0x20, 0x20);
        if pill_w > 2. && pill_h > 2. {
            push_rect(&mut fg, pill_x, pad_v, pill_w, pill_h, lifted_outline);
            push_rect(&mut fg, pill_x + 1., pad_v + 1., pill_w - 2., pill_h - 2., lifted_bg);
        }
    }

    // + button (two thin rects forming a cross)
    let plus_hover = hover == Some(n_tabs);
    let plus_col   = if plus_hover { rgb_f(0x88, 0x88, 0x88) } else { rgb_f(0x44, 0x44, 0x44) };
    let plus_cx    = bw - plus_area / 2.;
    let plus_cy    = tby / 2.;
    let arm        = 5.;
    push_rect(&mut fg, plus_cx - arm, plus_cy - 1., arm * 2., 2., plus_col);
    push_rect(&mut fg, plus_cx - 1., plus_cy - arm, 2., arm * 2., plus_col);

    (bg, fg)
}

impl Renderer { // re-open impl

    fn build_tab_bar(
        &mut self,
        bg_rects: &mut Vec<RectInst>,
        fg_rects: &mut Vec<RectInst>,
        glyphs: &mut Vec<GlyphInst>,
        bw: f32,
        tabs: &[String],
        active: usize,
        hover: Option<usize>,
        drag: Option<(usize, usize, f64)>,
    ) {
        let tby = self.tab_bar_height as f32;
        let cw = self.cell_width as f32;
        let ch = self.cell_height as f32;

        // All rect geometry delegated to the pure helper (testable without GPU).
        let (new_bg, new_fg) = tab_bar_rects(tby, bw, tabs.len(), active, hover, drag);
        bg_rects.extend(new_bg);
        fg_rects.extend(new_fg);

        // ── Glyph rendering ───────────────────────────────────────────────────
        let fg_act   = c2f(DEFAULT_FG);
        let fg_inact = rgb_f(0x66, 0x66, 0x66);
        let fg_sc    = rgb_f(0x3a, 0x3a, 0x3a);

        let plus_area = tby;
        let tabs_w    = bw - plus_area;
        let n         = tabs.len().max(1);
        let tab_w     = tabs_w / n as f32;
        let text_y    = (tby - ch) / 2.;
        let shortcut_w = 3. * cw;

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
        let drag_orig = drag.map(|(from, _, _)| from);

        for (vi, &orig_idx) in visual_order.iter().enumerate() {
            let title = &tabs[orig_idx];
            let is_active  = orig_idx == active;
            let is_dragging = drag_orig == Some(orig_idx);

            let tx = vi as f32 * tab_w;
            if tx >= tabs_w { break; }
            let tw = if vi + 1 == n { tabs_w - tx } else { tab_w };

            if is_dragging { continue; }

            // ⌘N shortcut
            let shortcut = format!("\u{2318}{}", orig_idx + 1);
            let sc_x = tx + tw - shortcut_w;
            let mut col_x = sc_x;
            for c in shortcut.chars() {
                if col_x + cw > tx + tw { break; }
                self.emit_char(glyphs, c, col_x, text_y, fg_sc);
                col_x += cw;
            }

            // Title
            let fg = if is_active { fg_act } else { fg_inact };
            let left_pad  = tx + cw;
            let right_edge = tx + tw - shortcut_w - cw;
            let max_cols  = ((right_edge - left_pad) / cw).max(0.) as usize;
            let chars: Vec<char> = title.chars().collect();
            let show_n    = chars.len().min(max_cols);
            let truncated = show_n < chars.len();
            for (ci, &c) in chars[..show_n].iter().enumerate() {
                let cpx    = left_pad + ci as f32 * cw;
                let draw_c = if truncated && ci + 1 == show_n { '\u{2026}' } else { c };
                self.emit_char(glyphs, draw_c, cpx, text_y, fg);
            }
        }

        // Floating dragged tab title
        if let Some((from_orig, _, cursor_x)) = drag {
            let title     = &tabs[from_orig];
            let is_active = from_orig == active;
            let half      = tab_w / 2.;
            let float_left = (cursor_x as f32 - half).max(0.).min(tabs_w - tab_w);
            let left_pad  = float_left + cw;
            let right_edge = float_left + tab_w - shortcut_w - cw;
            let max_cols  = ((right_edge - left_pad) / cw).max(0.) as usize;
            let chars: Vec<char> = title.chars().collect();
            let show_n    = chars.len().min(max_cols);
            let truncated = show_n < chars.len();
            let fg = if is_active { fg_act } else { rgb_f(0xcc, 0xcc, 0xcc) };
            for (ci, &c) in chars[..show_n].iter().enumerate() {
                let cpx    = left_pad + ci as f32 * cw;
                let draw_c = if truncated && ci + 1 == show_n { '\u{2026}' } else { c };
                self.emit_char(glyphs, draw_c, cpx, text_y, fg);
            }
        }
    }

    // ── Public render ─────────────────────────────────────────────────────────

    pub fn render(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        state: &TerminalState,
        show_cursor: bool,
        ghost: Option<&str>,
        tabs: &[String],
        active_tab: usize,
        hover: Option<usize>,
        selection: Option<(usize, usize, usize, usize)>,
        drag: Option<(usize, usize, f64)>,
        url_underlines: &[(usize, usize, usize)],
    ) {
        let bw = width as f32;
        let bh = height as f32;
        let tby = self.tab_bar_height as f32;
        let cw = self.cell_width as f32;
        let ch = self.cell_height as f32;

        // Update resolution uniform
        let res: [f32; 4] = [bw, bh, 0., 0.];
        self.queue.write_buffer(&self.uni_buf, 0, bytemuck::cast_slice(&res));

        let mut bg_rects: Vec<RectInst> = Vec::with_capacity(4096);
        let mut block_rects: Vec<RectInst> = Vec::new();
        let mut glyphs: Vec<GlyphInst> = Vec::with_capacity(4096);
        let mut fg_rects: Vec<RectInst> = Vec::new();

        // ── Tab bar ───────────────────────────────────────────────────────────
        self.build_tab_bar(
            &mut bg_rects,
            &mut fg_rects,
            &mut glyphs,
            bw,
            tabs,
            active_tab,
            hover,
            drag,
        );

        // ── Terminal grid ─────────────────────────────────────────────────────
        let term_h = bh - tby;
        let vis_rows = ((term_h / ch) as usize).min(state.rows);
        let vis_cols = ((bw / cw) as usize).min(state.cols);
        let sel_bg = c2fa(Color::new(0x26, 0x4a, 0x7a), 1.0);

        for row in 0..vis_rows {
            let gutter = if selection.is_some() {
                tcat_gutter_end(state, row, vis_cols)
            } else {
                0
            };

            for col in 0..vis_cols {
                let cell = state.visual_cell(row, col);
                let mut fg_color = cell.attrs.fg;
                let mut bg_color = cell.attrs.bg;
                if cell.attrs.inverse {
                    std::mem::swap(&mut fg_color, &mut bg_color);
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

                let px = col as f32 * cw;
                let py = tby + row as f32 * ch;

                let bg = if selected {
                    sel_bg
                } else {
                    let a = if bg_color == DEFAULT_BG {
                        BG_ALPHA as f32 / 255.
                    } else {
                        1.
                    };
                    c2fa(bg_color, a)
                };
                push_rect(&mut bg_rects, px, py, cw, ch, bg);

                let c = cell.c;
                if c != ' ' && c != '\0' {
                    let fg = c2f(fg_color);
                    if !Self::push_block_char(&mut block_rects, px, py, cw, ch, c, fg) {
                        self.emit_char(&mut glyphs, c, px, py, fg);
                        // Render combining / extending codepoints at the same
                        // cell position (overlaid on the base glyph).
                        for &combining in cell.combining_chars() {
                            self.emit_char(&mut glyphs, combining, px, py, fg);
                        }
                    }
                }
            }
        }

        // ── URL underlines ────────────────────────────────────────────────────
        if !url_underlines.is_empty() {
            let u_col = rgb_f(0x58, 0x9a, 0xdd);
            for &(row, c0, c1) in url_underlines {
                if row >= vis_rows { continue; }
                let uy = tby + row as f32 * ch + ch - 2.;
                let x0 = c0 as f32 * cw;
                let x1 = c1.min(vis_cols) as f32 * cw;
                push_rect(&mut fg_rects, x0, uy, x1 - x0, 2., u_col);
            }
        }

        // ── Ghost text ────────────────────────────────────────────────────────
        if !state.is_scrolled_back() {
            if let Some(g) = ghost {
                let py = tby + state.cursor_row as f32 * ch;
                for (i, c) in g.chars().enumerate() {
                    let col = state.cursor_col + i;
                    if col >= vis_cols { break; }
                    self.emit_char(&mut glyphs, c, col as f32 * cw, py, c2f(GHOST_COLOR));
                }
            }
        }

        // ── Cursor (2px vertical bar) ─────────────────────────────────────────
        if !state.is_scrolled_back()
            && show_cursor
            && state.cursor_row < vis_rows
            && state.cursor_col < vis_cols
        {
            let px = state.cursor_col as f32 * cw;
            let py = tby + state.cursor_row as f32 * ch;
            push_rect(&mut fg_rects, px, py, 2., ch, c2f(CURSOR_COLOR));
        }

        // ── Scrollbar ─────────────────────────────────────────────────────────
        let sb_total = state.scrollback.len();
        if sb_total > 0 {
            let total = sb_total + state.rows;
            let term_h_u = term_h as usize;
            let vis_rows_u = vis_rows;
            let thumb_h = ((term_h_u * vis_rows_u) / total).max(8).min(term_h_u);
            let view_top = sb_total.saturating_sub(state.viewport_offset);
            let thumb_y = tby + (view_top * (term_h_u - thumb_h)) as f32 / total.max(1) as f32;

            let bar_x = bw - 3.;
            let track_col = rgb_f(0x2a, 0x2a, 0x2a);
            let thumb_col = if state.is_scrolled_back() {
                rgb_f(0x66, 0x66, 0x66)
            } else {
                rgb_f(0x44, 0x44, 0x44)
            };
            // Track
            push_rect(&mut fg_rects, bar_x, tby, 2., bh - tby, track_col);
            // Thumb
            push_rect(
                &mut fg_rects,
                bar_x,
                thumb_y,
                2.,
                thumb_h as f32,
                thumb_col,
            );
        }

        // ── Upload instance data ──────────────────────────────────────────────

        // Pack all rects: [bg_rects | block_rects | fg_rects] into rect_buf
        let bg_count = bg_rects.len();
        let block_count = block_rects.len();
        let fg_count = fg_rects.len();
        let total_rects = bg_count + block_count + fg_count;

        self.ensure_rect_buf(total_rects.max(1));
        let glyph_count = glyphs.len();
        self.ensure_glyph_buf(glyph_count.max(1));

        let ri_size = std::mem::size_of::<RectInst>();
        let gi_size = std::mem::size_of::<GlyphInst>();

        if !bg_rects.is_empty() {
            self.queue.write_buffer(
                &self.rect_buf,
                0,
                bytemuck::cast_slice(&bg_rects),
            );
        }
        if !block_rects.is_empty() {
            self.queue.write_buffer(
                &self.rect_buf,
                (bg_count * ri_size) as u64,
                bytemuck::cast_slice(&block_rects),
            );
        }
        if !fg_rects.is_empty() {
            self.queue.write_buffer(
                &self.rect_buf,
                ((bg_count + block_count) * ri_size) as u64,
                bytemuck::cast_slice(&fg_rects),
            );
        }
        if !glyphs.is_empty() {
            self.queue.write_buffer(
                &self.glyph_buf,
                0,
                bytemuck::cast_slice(&glyphs),
            );
        }

        // ── Record render pass ────────────────────────────────────────────────

        let mut encoder = self.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("frame") },
        );
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("term_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: DEFAULT_BG.r as f64 / 255.,
                            g: DEFAULT_BG.g as f64 / 255.,
                            b: DEFAULT_BG.b as f64 / 255.,
                            a: 1.,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_bind_group(0, &self.uni_bg, &[]);
            pass.set_pipeline(&self.rect_pipeline);

            // Draw call 1: background rects (cell BGs + tab bar)
            if bg_count > 0 {
                pass.set_vertex_buffer(
                    0,
                    self.rect_buf.slice(..(bg_count * ri_size) as u64),
                );
                pass.draw(0..4, 0..bg_count as u32);
            }

            // Draw call 2: block character fills (on top of BG)
            if block_count > 0 {
                let start = (bg_count * ri_size) as u64;
                let end = start + (block_count * ri_size) as u64;
                pass.set_vertex_buffer(0, self.rect_buf.slice(start..end));
                pass.draw(0..4, 0..block_count as u32);
            }

            // Draw call 3: glyphs (alpha-blended over backgrounds)
            if glyph_count > 0 {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(1, &self.atlas_bg, &[]);
                pass.set_vertex_buffer(
                    0,
                    self.glyph_buf.slice(..(glyph_count * gi_size) as u64),
                );
                pass.draw(0..4, 0..glyph_count as u32);
            }

            // Draw call 4: foreground overlays (cursor, underlines, scrollbar, tab UI)
            if fg_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_bind_group(0, &self.uni_bg, &[]);
                let start = ((bg_count + block_count) * ri_size) as u64;
                let end = start + (fg_count * ri_size) as u64;
                pass.set_vertex_buffer(0, self.rect_buf.slice(start..end));
                pass.draw(0..4, 0..fg_count as u32);
            }
        }

        self.queue.submit(std::iter::once(encoder.finish()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: call tab_bar_rects with typical screen dimensions.
    fn geom(
        n_tabs: usize,
        active: usize,
        hover: Option<usize>,
        drag: Option<(usize, usize, f64)>,
    ) -> (Vec<RectInst>, Vec<RectInst>) {
        tab_bar_rects(
            36.,   // tby — typical tab bar height
            800.,  // bw  — typical window width
            n_tabs,
            active,
            hover,
            drag,
        )
    }

    // ── Regression: active tab pill must not overwrite tab title text ─────────
    //
    // Draw order: bg_rects (pass 1) → glyphs (pass 3) → fg_rects (pass 4).
    // If the active pill lands in fg_rects it is drawn AFTER the glyphs and
    // paints over the title, making it invisible.

    #[test]
    fn active_pill_outline_in_bg_not_fg() {
        let outline = rgb_f(0x58, 0x58, 0x58);
        let (bg, fg) = geom(2, 0, None, None);
        assert!(
            bg.iter().any(|r| r.color == outline),
            "active tab outline must be in bg_rects so glyphs render on top"
        );
        assert!(
            !fg.iter().any(|r| r.color == outline),
            "active tab outline in fg_rects — it would be drawn after glyphs and cover the title"
        );
    }

    #[test]
    fn active_pill_only_one_outline_rect() {
        // Exactly one outline rect regardless of how many tabs exist.
        let outline = rgb_f(0x58, 0x58, 0x58);
        for n in 1..=5 {
            let (bg, _) = geom(n, 0, None, None);
            let count = bg.iter().filter(|r| r.color == outline).count();
            assert_eq!(count, 1, "expected exactly 1 outline rect with {n} tabs");
        }
    }

    #[test]
    fn hover_bg_in_bg_not_fg() {
        let hover_bg = rgb_f(0x26, 0x26, 0x26);
        // Hover over tab 1 while tab 0 is active.
        let (bg, fg) = geom(3, 0, Some(1), None);
        assert!(
            bg.iter().any(|r| r.color == hover_bg),
            "hover background must be in bg_rects"
        );
        assert!(
            !fg.iter().any(|r| r.color == hover_bg),
            "hover background in fg_rects — it would cover the tab title"
        );
    }

    #[test]
    fn inactive_tab_gets_no_outline() {
        // Only the active tab should have an outline rect.
        let outline = rgb_f(0x58, 0x58, 0x58);
        let (bg, _) = geom(3, 1, None, None); // active = 1
        let count = bg.iter().filter(|r| r.color == outline).count();
        assert_eq!(count, 1, "inactive tabs must not get an outline pill");
    }

    #[test]
    fn bar_background_is_first_bg_rect() {
        let bar_bg = rgb_f(0x14, 0x14, 0x14);
        let (bg, _) = geom(1, 0, None, None);
        assert!(!bg.is_empty());
        assert_eq!(bg[0].color, bar_bg, "first bg rect must be the bar background fill");
    }

    #[test]
    fn plus_button_rects_are_in_fg() {
        // The + button is always in fg_rects (it's an overlay, not a background).
        let plus_col = rgb_f(0x44, 0x44, 0x44); // non-hover colour
        let (_, fg) = geom(1, 0, None, None);
        let count = fg.iter().filter(|r| r.color == plus_col).count();
        assert_eq!(count, 2, "plus button needs 2 fg rects (horizontal + vertical arm)");
    }
}
