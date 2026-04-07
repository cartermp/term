use crate::config::*;
use crate::terminal::TerminalState;
use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;
use std::sync::Arc;

static FONT_BYTES: &[u8] = include_bytes!("../assets/JetBrainsMono-Regular.ttf");

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
pub struct RectInst {
    pub pos:   [f32; 2],
    pub sz:    [f32; 2],
    pub color: [f32; 4],
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

// ── Ligature shaping ──────────────────────────────────────────────────────────

/// A single glyph produced by text shaping, mapped back to the input cells.
struct ShapedGlyph {
    /// OpenType glyph index (u16 fits all fonts; 0 means .notdef/missing).
    glyph_id: u16,
    /// Index into the input `chars` slice where this glyph starts.
    /// For a ligature, the cells between this and the next glyph's `char_idx`
    /// are covered — they receive no `GlyphOp::Glyph` and stay `Skip`.
    char_idx: usize,
}

/// Per-cell render decision computed before the draw loop.
#[derive(Copy, Clone)]
enum GlyphOp {
    /// Nothing to draw (space, null, or a cell consumed by a multi-cell ligature).
    Skip,
    /// Block or Braille character — rendered as fill rects, not from the atlas.
    Block,
    /// Draw glyph `id` from the atlas at this cell's pixel origin.
    Glyph(u16),
}

// ── Color helpers ─────────────────────────────────────────────────────────────

fn c2f(c: Color) -> [f32; 4] {
    [c.r as f32 / 255., c.g as f32 / 255., c.b as f32 / 255., 1.]
}

fn c2fa(c: Color, a: f32) -> [f32; 4] {
    [c.r as f32 / 255., c.g as f32 / 255., c.b as f32 / 255., a]
}

pub fn rgb_f(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255., g as f32 / 255., b as f32 / 255., 1.]
}

// ── Instance list helpers ─────────────────────────────────────────────────────

pub fn push_rect(v: &mut Vec<RectInst>, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
    if w > 0. && h > 0. {
        v.push(RectInst { pos: [x, y], sz: [w, h], color });
    }
}

// ── Shaping helpers (free functions so tests can call them without a GPU) ─────

fn shape_run_impl(face: &rustybuzz::Face<'_>, chars: &[char]) -> Vec<ShapedGlyph> {
    if chars.is_empty() {
        return Vec::new();
    }

    let text: String = chars.iter().collect();

    // Build a byte-offset → char-index mapping for the UTF-8 string.
    let mut byte_to_char = vec![0usize; text.len() + 1];
    for (char_idx, (byte_off, c)) in text.char_indices().enumerate() {
        for b in byte_off..byte_off + c.len_utf8() {
            byte_to_char[b] = char_idx;
        }
    }
    byte_to_char[text.len()] = chars.len();

    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(&text);
    let output = rustybuzz::shape(face, &[], buf);
    let infos = output.glyph_infos();

    let mut result = Vec::with_capacity(infos.len());
    for i in 0..infos.len() {
        let cluster_byte = infos[i].cluster as usize;
        let char_idx = byte_to_char[cluster_byte.min(text.len())];
        result.push(ShapedGlyph {
            glyph_id: infos[i].glyph_id as u16,
            char_idx,
        });
    }
    result
}

fn compute_row_glyph_ops_impl(
    face: &rustybuzz::Face<'_>,
    state: &TerminalState,
    row: usize,
    vis_cols: usize,
) -> Vec<GlyphOp> {
    let mut ops = vec![GlyphOp::Skip; vis_cols];

    let mut col = 0;
    while col < vis_cols {
        let cell = state.visual_cell(row, col);
        let c = cell.c;

        if c == ' ' || c == '\0' {
            col += 1;
            continue;
        }

        if is_block_char(c) {
            ops[col] = GlyphOp::Block;
            col += 1;
            continue;
        }

        // Effective fg (after inverse) — run boundary changes on fg change.
        let fg_color = if cell.attrs.inverse { cell.attrs.bg } else { cell.attrs.fg };

        // Extend the run as far as chars share the same fg and are printable.
        let run_start = col;
        let mut run_end = col + 1;
        while run_end < vis_cols {
            let nc = state.visual_cell(row, run_end);
            let nc_c = nc.c;
            if nc_c == ' ' || nc_c == '\0' || is_block_char(nc_c) {
                break;
            }
            let nc_fg = if nc.attrs.inverse { nc.attrs.bg } else { nc.attrs.fg };
            if nc_fg != fg_color {
                break;
            }
            run_end += 1;
        }

        // Collect chars and shape.
        let run_chars: Vec<char> = (run_start..run_end)
            .map(|c| state.visual_cell(row, c).c)
            .collect();
        let shaped = shape_run_impl(face, &run_chars);

        // Map shaped glyphs back to column positions.
        // Cells covered by a ligature (not the first) stay as GlyphOp::Skip.
        for sg in &shaped {
            let abs_col = run_start + sg.char_idx;
            if abs_col < vis_cols {
                ops[abs_col] = if sg.glyph_id == 0 {
                    GlyphOp::Skip
                } else {
                    GlyphOp::Glyph(sg.glyph_id)
                };
            }
        }

        col = run_end;
    }

    ops
}

fn is_block_char(c: char) -> bool {
    matches!(c, '\u{2580}'..='\u{259F}' | '\u{2800}'..='\u{28FF}')
}

// ── PaneView ─────────────────────────────────────────────────────────────────

/// A single terminal pane to be drawn within the surface.
pub struct PaneView<'a> {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub state: &'a crate::terminal::TerminalState,
    pub show_cursor: bool,
    pub ghost: Option<&'a str>,
    pub selection: Option<(usize, usize, usize, usize)>,
    pub url_underlines: &'a [(usize, usize, usize)],
}

// ── Renderer ──────────────────────────────────────────────────────────────────

pub struct Renderer {
    pub device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,

    rect_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,

    uni_buf: wgpu::Buffer,
    uni_bg: wgpu::BindGroup,

    atlas_tex: wgpu::Texture,
    _atlas_view: wgpu::TextureView,
    _atlas_sampler: wgpu::Sampler,
    atlas_bg: wgpu::BindGroup,
    _atlas_bgl: wgpu::BindGroupLayout,
    /// Atlas cache keyed by OpenType glyph ID (not char) so shaped ligature
    /// glyphs share entries with their constituent characters when applicable.
    atlas_cache: HashMap<u16, Option<AtlasEntry>>,
    atlas_x: u32,
    atlas_y: u32,
    atlas_row_h: u32,

    // Reusable GPU instance buffers (grown on demand)
    rect_buf: wgpu::Buffer,
    rect_buf_cap: usize,
    glyph_buf: wgpu::Buffer,
    glyph_buf_cap: usize,

    font: fontdue::Font,
    /// HarfBuzz-compatible shaper for OpenType ligature / calt substitution.
    rb_face: rustybuzz::Face<'static>,
    pub cell_width: usize,
    pub cell_height: usize,
    pub baseline: i32,
    font_size: f32,
    pub surface_format: wgpu::TextureFormat,
}

impl Renderer {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        surface_format: wgpu::TextureFormat,
        scale_factor: f64,
    ) -> Self {
        // ── Font ──────────────────────────────────────────────────────────────
        let font_size = (FONT_SIZE_PT * scale_factor as f32).round();
        let font = fontdue::Font::from_bytes(
            FONT_BYTES,
            fontdue::FontSettings {
                scale: font_size,
                collection_index: 0,
                load_substitutions: true,
            },
        )
        .expect("font load");
        let rb_face = rustybuzz::Face::from_slice(FONT_BYTES, 0)
            .expect("rustybuzz face load");

        let (m, _) = font.rasterize('M', font_size);
        let cell_width = m.advance_width.ceil() as usize;
        let lm = font.horizontal_line_metrics(font_size).unwrap();
        let ascent = lm.ascent.ceil() as i32;
        let descent = (-lm.descent).ceil() as i32;
        let gap = lm.line_gap.ceil() as i32;
        let cell_height =
            (ascent + descent + gap).max(font_size as i32 + 4) as usize;

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
            rb_face,
            cell_width,
            cell_height,
            baseline: ascent,
            font_size,
            surface_format,
        }
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

    /// Ensure OpenType glyph `id` is in the atlas. Returns the entry, or `None`
    /// if the glyph is missing or invisible (id 0, zero-size bitmap).
    fn ensure_glyph_id(&mut self, id: u16) -> Option<AtlasEntry> {
        if id == 0 {
            return None;
        }
        if let Some(entry) = self.atlas_cache.get(&id) {
            return entry.clone();
        }
        let (m, bitmap) = self.font.rasterize_indexed(id, self.font_size);
        if m.width == 0 || m.height == 0 {
            self.atlas_cache.insert(id, None);
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
        self.atlas_cache.insert(id, Some(entry.clone()));
        Some(entry)
    }

    // ── Glyph emit helpers ────────────────────────────────────────────────────

    fn emit_glyph_id(
        &mut self,
        glyphs: &mut Vec<GlyphInst>,
        id: u16,
        px: f32,
        py: f32,
        fg: [f32; 4],
    ) {
        if let Some(e) = self.ensure_glyph_id(id) {
            glyphs.push(GlyphInst {
                pos: [px + e.glyph_x as f32, py + e.glyph_y as f32],
                sz: [e.w as f32, e.h as f32],
                uv_pos: [e.uv_x, e.uv_y],
                uv_sz: [e.uv_w, e.uv_h],
                fg,
            });
        }
    }

    fn emit_char(
        &mut self,
        glyphs: &mut Vec<GlyphInst>,
        c: char,
        px: f32,
        py: f32,
        fg: [f32; 4],
    ) {
        let id = self.font.lookup_glyph_index(c);
        self.emit_glyph_id(glyphs, id, px, py, fg);
    }

    // ── Ligature shaping ──────────────────────────────────────────────────────

    /// Pre-compute per-column glyph operations for a single terminal row.
    ///
    /// Cells are grouped into same-fg-color runs.  Each run is shaped through
    /// the OpenType engine; ligature glyphs that span multiple cells produce
    /// `GlyphOp::Glyph` at the first cell and `GlyphOp::Skip` at subsequent
    /// covered cells.
    fn compute_row_glyph_ops(
        &self,
        state: &TerminalState,
        row: usize,
        vis_cols: usize,
    ) -> Vec<GlyphOp> {
        compute_row_glyph_ops_impl(&self.rb_face, state, row, vis_cols)
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

    // ── Public render ─────────────────────────────────────────────────────────

    /// Render all `panes` into `view`.  `dividers` are solid rectangles
    /// (x, y, w, h) drawn between panes — typically 2 px wide/tall separator lines.
    pub fn render(
        &mut self,
        view: &wgpu::TextureView,
        surface_w: u32,
        surface_h: u32,
        panes: &[PaneView<'_>],
        dividers: &[(f32, f32, f32, f32)],
    ) {
        let bw = surface_w as f32;
        let bh = surface_h as f32;
        let cw = self.cell_width as f32;
        let ch = self.cell_height as f32;

        let res: [f32; 4] = [bw, bh, 0., 0.];
        self.queue.write_buffer(&self.uni_buf, 0, bytemuck::cast_slice(&res));

        let mut bg_rects: Vec<RectInst>    = Vec::with_capacity(4096);
        let mut block_rects: Vec<RectInst> = Vec::new();
        let mut glyphs: Vec<GlyphInst>     = Vec::with_capacity(4096);
        let mut fg_rects: Vec<RectInst>    = Vec::new();

        let sel_bg = c2fa(Color::new(0x26, 0x4a, 0x7a), 1.0);

        for pane in panes {
            let ox = pane.x;
            let oy = pane.y;
            let vis_rows = ((pane.height / ch) as usize).min(pane.state.rows);
            let vis_cols = ((pane.width  / cw) as usize).min(pane.state.cols);

            // ── Cell grid ─────────────────────────────────────────────────────────
            for row in 0..vis_rows {
                let gutter = if pane.selection.is_some() {
                    tcat_gutter_end(pane.state, row, vis_cols)
                } else {
                    0
                };

                // Pre-compute ligature-aware glyph ops for the row.
                // This is a pure shaping step (no atlas mutation) so it can
                // borrow &self while the draw loop later uses &mut self.
                let glyph_ops = self.compute_row_glyph_ops(pane.state, row, vis_cols);

                for col in 0..vis_cols {
                    let cell = pane.state.visual_cell(row, col);
                    let mut fg_color = cell.attrs.fg;
                    let mut bg_color = cell.attrs.bg;
                    if cell.attrs.inverse {
                        std::mem::swap(&mut fg_color, &mut bg_color);
                    }

                    let selected = if let Some((r0, c0, r1, c1)) = pane.selection {
                        row >= r0
                            && row <= r1
                            && !(row == r0 && col < c0)
                            && !(row == r1 && col > c1)
                            && col >= gutter
                    } else {
                        false
                    };

                    let px = ox + col as f32 * cw;
                    let py = oy + row as f32 * ch;

                    let bg = if selected {
                        sel_bg
                    } else {
                        let a = if bg_color == DEFAULT_BG { BG_ALPHA as f32 / 255. } else { 1. };
                        c2fa(bg_color, a)
                    };
                    push_rect(&mut bg_rects, px, py, cw, ch, bg);

                    let fg = c2f(fg_color);
                    let c = cell.c;
                    match glyph_ops[col] {
                        GlyphOp::Skip => {}
                        GlyphOp::Block => {
                            // Block chars can't participate in ligatures; skip
                            // the GlyphOp::Skip guard above — we always render.
                            if c != ' ' && c != '\0' {
                                Self::push_block_char(&mut block_rects, px, py, cw, ch, c, fg);
                            }
                        }
                        GlyphOp::Glyph(glyph_id) => {
                            self.emit_glyph_id(&mut glyphs, glyph_id, px, py, fg);
                            // Combining chars overlay at the same cell origin.
                            for &combining in cell.combining_chars() {
                                let comb_id = self.font.lookup_glyph_index(combining);
                                self.emit_glyph_id(&mut glyphs, comb_id, px, py, fg);
                            }
                        }
                    }
                }
            }

            // ── URL underlines ────────────────────────────────────────────────────
            if !pane.url_underlines.is_empty() {
                let u_col = rgb_f(0x58, 0x9a, 0xdd);
                for &(row, c0, c1) in pane.url_underlines {
                    if row >= vis_rows { continue; }
                    let uy = oy + row as f32 * ch + ch - 2.;
                    let x0 = ox + c0 as f32 * cw;
                    let x1 = ox + c1.min(vis_cols) as f32 * cw;
                    push_rect(&mut fg_rects, x0, uy, x1 - x0, 2., u_col);
                }
            }

            // ── Ghost text ────────────────────────────────────────────────────────
            if !pane.state.is_scrolled_back() {
                if let Some(g) = pane.ghost {
                    let py = oy + pane.state.cursor_row as f32 * ch;
                    for (i, c) in g.chars().enumerate() {
                        let col = pane.state.cursor_col + i;
                        if col >= vis_cols { break; }
                        self.emit_char(&mut glyphs, c, ox + col as f32 * cw, py, c2f(GHOST_COLOR));
                    }
                }
            }

            // ── Cursor ────────────────────────────────────────────────────────────
            if !pane.state.is_scrolled_back()
                && pane.show_cursor
                && pane.state.cursor_row < vis_rows
                && pane.state.cursor_col < vis_cols
            {
                let px = ox + pane.state.cursor_col as f32 * cw;
                let py = oy + pane.state.cursor_row as f32 * ch;
                push_rect(&mut fg_rects, px, py, 2., ch, c2f(CURSOR_COLOR));
            }

            // ── Scrollbar ─────────────────────────────────────────────────────────
            let sb_total = pane.state.scrollback.len();
            if sb_total > 0 {
                let total    = sb_total + pane.state.rows;
                let ph_u     = pane.height as usize;
                let thumb_h  = ((ph_u * vis_rows) / total).max(8).min(ph_u);
                let view_top = sb_total.saturating_sub(pane.state.viewport_offset);
                let thumb_y  = oy + (view_top * (ph_u - thumb_h)) as f32 / total.max(1) as f32;
                let bar_x    = ox + pane.width - 3.;
                let track    = rgb_f(0x2a, 0x2a, 0x2a);
                let thumb    = if pane.state.is_scrolled_back() {
                    rgb_f(0x66, 0x66, 0x66)
                } else {
                    rgb_f(0x44, 0x44, 0x44)
                };
                push_rect(&mut fg_rects, bar_x, oy, 2., pane.height, track);
                push_rect(&mut fg_rects, bar_x, thumb_y, 2., thumb_h as f32, thumb);
            }
        }

        // ── Split dividers ────────────────────────────────────────────────────────
        let div_col = rgb_f(0x3a, 0x3a, 0x3a);
        for &(x, y, w, h) in dividers {
            push_rect(&mut fg_rects, x, y, w, h, div_col);
        }

        // ── Upload ────────────────────────────────────────────────────────────────
        let bg_count    = bg_rects.len();
        let block_count = block_rects.len();
        let fg_count    = fg_rects.len();
        let total_rects = bg_count + block_count + fg_count;

        self.ensure_rect_buf(total_rects.max(1));
        let glyph_count = glyphs.len();
        self.ensure_glyph_buf(glyph_count.max(1));

        let ri_size = std::mem::size_of::<RectInst>();
        let gi_size = std::mem::size_of::<GlyphInst>();

        if !bg_rects.is_empty() {
            self.queue.write_buffer(&self.rect_buf, 0, bytemuck::cast_slice(&bg_rects));
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
            self.queue.write_buffer(&self.glyph_buf, 0, bytemuck::cast_slice(&glyphs));
        }

        // ── Render pass ───────────────────────────────────────────────────────────
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
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

            if bg_count > 0 {
                pass.set_vertex_buffer(0, self.rect_buf.slice(..(bg_count * ri_size) as u64));
                pass.draw(0..4, 0..bg_count as u32);
            }
            if block_count > 0 {
                let start = (bg_count * ri_size) as u64;
                let end   = start + (block_count * ri_size) as u64;
                pass.set_vertex_buffer(0, self.rect_buf.slice(start..end));
                pass.draw(0..4, 0..block_count as u32);
            }
            if glyph_count > 0 {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(1, &self.atlas_bg, &[]);
                pass.set_vertex_buffer(0, self.glyph_buf.slice(..(glyph_count * gi_size) as u64));
                pass.draw(0..4, 0..glyph_count as u32);
            }
            if fg_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_bind_group(0, &self.uni_bg, &[]);
                let start = ((bg_count + block_count) * ri_size) as u64;
                let end   = start + (fg_count * ri_size) as u64;
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
    use crate::terminal::Terminal;

    fn test_face() -> rustybuzz::Face<'static> {
        rustybuzz::Face::from_slice(FONT_BYTES, 0)
            .expect("bundled JetBrains Mono must parse as a valid font")
    }

    fn make_term(cols: usize, rows: usize) -> Terminal {
        Terminal::new(cols, rows)
    }

    // ── Color helpers ─────────────────────────────────────────────────────────

    #[test]
    fn c2f_black_is_zero_rgb_full_alpha() {
        let r = c2f(Color::new(0, 0, 0));
        assert_eq!(r, [0., 0., 0., 1.]);
    }

    #[test]
    fn c2f_white_is_one_rgb_full_alpha() {
        let r = c2f(Color::new(255, 255, 255));
        assert_eq!(r, [1., 1., 1., 1.]);
    }

    #[test]
    fn c2f_channel_conversion_is_divide_by_255() {
        let r = c2f(Color::new(255, 0, 128));
        assert_eq!(r[0], 1.0);
        assert_eq!(r[1], 0.0);
        assert!((r[2] - 128. / 255.).abs() < 1e-6);
        assert_eq!(r[3], 1.0);
    }

    #[test]
    fn c2fa_carries_alpha() {
        let r = c2fa(Color::new(255, 0, 0), 0.5);
        assert_eq!(r[0], 1.0);
        assert_eq!(r[1], 0.0);
        assert_eq!(r[2], 0.0);
        assert_eq!(r[3], 0.5);
    }

    #[test]
    fn c2fa_zero_alpha() {
        let r = c2fa(Color::new(255, 255, 255), 0.0);
        assert_eq!(r[3], 0.0);
    }

    #[test]
    fn rgb_f_converts_components() {
        let r = rgb_f(0, 128, 255);
        assert_eq!(r[0], 0.0);
        assert!((r[1] - 128. / 255.).abs() < 1e-6);
        assert_eq!(r[2], 1.0);
        assert_eq!(r[3], 1.0);
    }

    #[test]
    fn rgb_f_black() {
        assert_eq!(rgb_f(0, 0, 0), [0., 0., 0., 1.]);
    }

    // ── push_rect ─────────────────────────────────────────────────────────────

    #[test]
    fn divider_rect_is_pushed_into_fg_rects() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 400., 0., 2., 600., rgb_f(0x3a, 0x3a, 0x3a));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].pos, [400., 0.]);
        assert_eq!(v[0].sz,  [2., 600.]);
    }

    #[test]
    fn push_rect_zero_width_is_rejected() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 0., 0., 0., 100., rgb_f(255, 0, 0));
        assert!(v.is_empty(), "zero-width rect must not be pushed");
    }

    #[test]
    fn push_rect_zero_height_is_rejected() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 0., 0., 100., 0., rgb_f(255, 0, 0));
        assert!(v.is_empty(), "zero-height rect must not be pushed");
    }

    #[test]
    fn push_rect_negative_width_is_rejected() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 0., 0., -1., 100., rgb_f(255, 0, 0));
        assert!(v.is_empty(), "negative-width rect must not be pushed");
    }

    #[test]
    fn push_rect_negative_height_is_rejected() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 0., 0., 100., -1., rgb_f(255, 0, 0));
        assert!(v.is_empty(), "negative-height rect must not be pushed");
    }

    #[test]
    fn push_rect_stores_position_and_size() {
        let mut v: Vec<RectInst> = Vec::new();
        push_rect(&mut v, 10., 20., 30., 40., [1., 0., 0., 1.]);
        assert_eq!(v[0].pos, [10., 20.]);
        assert_eq!(v[0].sz, [30., 40.]);
        assert_eq!(v[0].color, [1., 0., 0., 1.]);
    }

    // ── is_block_char ─────────────────────────────────────────────────────────

    #[test]
    fn is_block_char_lower_block_boundary() {
        assert!(is_block_char('\u{2580}'), "U+2580 UPPER HALF BLOCK must be a block char");
    }

    #[test]
    fn is_block_char_upper_block_boundary() {
        assert!(is_block_char('\u{259F}'), "U+259F QUADRANT LOWER-RIGHT must be a block char");
    }

    #[test]
    fn is_block_char_just_before_block_range_is_false() {
        assert!(!is_block_char('\u{257F}'), "U+257F must not be a block char");
    }

    #[test]
    fn is_block_char_just_after_block_range_is_false() {
        assert!(!is_block_char('\u{25A0}'), "U+25A0 BLACK SQUARE must not be a block char");
    }

    #[test]
    fn is_block_char_braille_lower_boundary() {
        assert!(is_block_char('\u{2800}'), "U+2800 BRAILLE BLANK must be a block char");
    }

    #[test]
    fn is_block_char_braille_upper_boundary() {
        assert!(is_block_char('\u{28FF}'), "U+28FF BRAILLE 8-DOT must be a block char");
    }

    #[test]
    fn is_block_char_just_after_braille_range_is_false() {
        assert!(!is_block_char('\u{2900}'), "U+2900 must not be a block char");
    }

    #[test]
    fn is_block_char_ascii_is_false() {
        for c in ' '..='~' {
            assert!(!is_block_char(c), "ASCII '{c}' must not be a block char");
        }
    }

    #[test]
    fn is_block_char_mid_block_range() {
        // U+2588 FULL BLOCK is the canonical block character.
        assert!(is_block_char('\u{2588}'));
    }

    // ── shape_run_impl ────────────────────────────────────────────────────────

    #[test]
    fn shape_run_empty_input_returns_empty() {
        let face = test_face();
        let result = shape_run_impl(&face, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn shape_run_single_ascii_char_returns_one_glyph_at_index_zero() {
        let face = test_face();
        let result = shape_run_impl(&face, &['A']);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].char_idx, 0);
        assert_ne!(result[0].glyph_id, 0, "A must have a real glyph in JetBrains Mono");
    }

    #[test]
    fn shape_run_multiple_ascii_chars_each_have_valid_char_idx() {
        let face = test_face();
        let chars: Vec<char> = "hello".chars().collect();
        let result = shape_run_impl(&face, &chars);
        assert!(!result.is_empty());
        // All char_idxes must be in-range.
        for sg in &result {
            assert!(sg.char_idx < chars.len(), "char_idx {} out of range", sg.char_idx);
        }
    }

    #[test]
    fn shape_run_char_idxes_are_monotonically_nondecreasing() {
        let face = test_face();
        let chars: Vec<char> = "fn foo() ->".chars().collect();
        let result = shape_run_impl(&face, &chars);
        let mut prev = 0usize;
        for sg in &result {
            assert!(
                sg.char_idx >= prev,
                "char_idx went backwards: {} < {}",
                sg.char_idx, prev
            );
            prev = sg.char_idx;
        }
    }

    #[test]
    fn shape_run_output_count_does_not_exceed_input_count() {
        let face = test_face();
        // Ligatures may reduce glyph count; they must never increase it.
        let chars: Vec<char> = "->>=!=".chars().collect();
        let result = shape_run_impl(&face, &chars);
        assert!(
            result.len() <= chars.len(),
            "shaped output ({}) must not exceed input ({}) chars",
            result.len(), chars.len()
        );
    }

    #[test]
    fn shape_run_all_glyph_ids_nonzero_for_ascii() {
        // Every printable ASCII char must be present in JetBrains Mono.
        let face = test_face();
        for c in '!'..='~' {
            let result = shape_run_impl(&face, &[c]);
            assert_eq!(result.len(), 1);
            assert_ne!(
                result[0].glyph_id, 0,
                "ASCII '{c}' (U+{:04X}) must have a glyph in JetBrains Mono",
                c as u32
            );
        }
    }

    #[test]
    fn shape_run_unicode_multibyte_char_has_correct_char_idx() {
        // U+00E9 (é) is 2 bytes in UTF-8; its cluster byte offset must map to char index 0.
        let face = test_face();
        let chars = vec!['\u{00E9}'];
        let result = shape_run_impl(&face, &chars);
        assert!(!result.is_empty());
        assert_eq!(result[0].char_idx, 0, "char_idx for single multibyte char must be 0");
    }

    #[test]
    fn shape_run_mixed_ascii_and_unicode_char_idxes_are_correct() {
        let face = test_face();
        // "aé" — 'a' is 1 byte, 'é' is 2 bytes; char indices must be 0 and 1.
        let chars = vec!['a', '\u{00E9}'];
        let result = shape_run_impl(&face, &chars);
        assert!(!result.is_empty());
        // The first glyph must come from char 0, second from char 1.
        let idxes: Vec<usize> = result.iter().map(|s| s.char_idx).collect();
        assert!(idxes.contains(&0), "char_idx 0 must appear in result");
        // Char idx 1 might be merged if a ligature forms (unlikely for 'aé').
        assert!(idxes.iter().all(|&i| i < 2), "all char_idxes must be < 2");
    }

    // ── compute_row_glyph_ops_impl ────────────────────────────────────────────

    #[test]
    fn glyph_ops_space_cells_are_skip() {
        let face = test_face();
        let term = make_term(10, 5);
        // Default cells contain ' '.
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        for (i, op) in ops.iter().enumerate() {
            assert!(
                matches!(op, GlyphOp::Skip),
                "col {i}: space must produce Skip, not Glyph/Block"
            );
        }
    }

    #[test]
    fn glyph_ops_null_cell_is_skip() {
        let face = test_face();
        let term = make_term(4, 2);
        // A fresh terminal has ' ' in every cell — all must be Skip.
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 4);
        assert!(ops.iter().all(|o| matches!(o, GlyphOp::Skip)));
        drop(term);
    }

    #[test]
    fn glyph_ops_printable_ascii_produces_glyph() {
        let face = test_face();
        let mut term = make_term(10, 5);
        // Write 'A' at (0,0).
        term.process(b"A");
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        assert!(
            matches!(ops[0], GlyphOp::Glyph(_)),
            "col 0 with 'A' must produce Glyph, got Skip/Block"
        );
        // Remaining cols were not written → Skip.
        for i in 1..10 {
            assert!(matches!(ops[i], GlyphOp::Skip), "col {i} must be Skip");
        }
    }

    #[test]
    fn glyph_ops_block_char_is_block() {
        let face = test_face();
        let mut term = make_term(10, 5);
        // Write U+2588 FULL BLOCK.
        term.process("\u{2588}".as_bytes());
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        assert!(
            matches!(ops[0], GlyphOp::Block),
            "U+2588 must produce Block op"
        );
    }

    #[test]
    fn glyph_ops_different_fg_colors_break_shaping_run() {
        // Two chars with different fg colors must each produce an independent
        // Glyph op (no cross-color ligature).
        let face = test_face();
        let mut term = make_term(10, 5);
        // Write '-' in red.
        term.process(b"\x1b[31m-");
        // Write '>' in green.
        term.process(b"\x1b[32m>");
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        // Both cols must be Glyph (independent runs, not ligature-merged Skip).
        assert!(matches!(ops[0], GlyphOp::Glyph(_)), "col 0 '-' red must be Glyph");
        assert!(matches!(ops[1], GlyphOp::Glyph(_)), "col 1 '>' green must be Glyph");
    }

    #[test]
    fn glyph_ops_same_fg_color_allows_ligature_run() {
        // '-' and '>' in the same fg color must be shaped as one run.
        // If JetBrains Mono produces a true single-glyph ligature, col 1 will be
        // Skip.  If it uses calt (2 contextual glyphs), both will be Glyph.
        // Either way, neither should be Block, and col 0 must be Glyph.
        let face = test_face();
        let mut term = make_term(10, 5);
        term.process(b"->");
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        assert!(matches!(ops[0], GlyphOp::Glyph(_)), "first char must be Glyph");
        assert!(
            matches!(ops[1], GlyphOp::Glyph(_) | GlyphOp::Skip),
            "second char must be Glyph or Skip (ligature)"
        );
    }

    #[test]
    fn glyph_ops_space_breaks_run() {
        // 'a', ' ', 'b' — the space must break the shaping run so 'a' and 'b'
        // are shaped independently.
        let face = test_face();
        let mut term = make_term(10, 5);
        term.process(b"a b");
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        assert!(matches!(ops[0], GlyphOp::Glyph(_)), "col 0 'a' must be Glyph");
        assert!(matches!(ops[1], GlyphOp::Skip), "col 1 ' ' must be Skip");
        assert!(matches!(ops[2], GlyphOp::Glyph(_)), "col 2 'b' must be Glyph");
    }

    #[test]
    fn glyph_ops_block_char_breaks_run() {
        // 'a', U+2588, 'b' — the block char must break shaping runs.
        let face = test_face();
        let mut term = make_term(10, 5);
        term.process("a\u{2588}b".as_bytes());
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 10);
        assert!(matches!(ops[0], GlyphOp::Glyph(_)), "col 0 'a' must be Glyph");
        assert!(matches!(ops[1], GlyphOp::Block), "col 1 U+2588 must be Block");
        assert!(matches!(ops[2], GlyphOp::Glyph(_)), "col 2 'b' must be Glyph");
    }

    #[test]
    fn glyph_ops_vis_cols_zero_returns_empty() {
        let face = test_face();
        let term = make_term(10, 5);
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 0, 0);
        assert!(ops.is_empty());
    }

    #[test]
    fn glyph_ops_row_out_of_visible_range_returns_all_skip() {
        // Requesting a row beyond the grid should not panic; visual_cell returns
        // a default (space) cell for out-of-range rows.
        let face = test_face();
        let term = make_term(5, 3);
        let ops = compute_row_glyph_ops_impl(&face, &term.state, 10, 5);
        assert!(ops.iter().all(|o| matches!(o, GlyphOp::Skip)));
    }
}
