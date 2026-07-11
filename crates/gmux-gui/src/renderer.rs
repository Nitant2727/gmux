//! wgpu renderer: draws a [`PaneSnapshot`] as background cell quads + glyph quads (from the
//! [`Atlas`]) + a block cursor + an attention ring. Two pipelines (opaque bg, alpha-blended
//! glyphs). Vertex buffers are rebuilt per frame (damage tracking is a later optimization).

use bytemuck::{Pod, Zeroable};
use gmux_mux::{Attention, PaneSnapshot, Rect, Rgb};
use wgpu::util::DeviceExt;

use crate::atlas::{Atlas, GlyphLookup};

// Design tokens (single source of truth). Colors are Catppuccin-Mocha-derived; see the spec.
const BG_APP: Rgb = Rgb { r: 0x11, g: 0x11, b: 0x1b }; // window / between-pane background
const BG_SIDEBAR: Rgb = Rgb { r: 0x18, g: 0x18, b: 0x25 };
const BG_PANE: Rgb = Rgb { r: 0x1e, g: 0x1e, b: 0x2e }; // pane fill + letterbox
const SIDEBAR_ROW_ACTIVE: Rgb = Rgb { r: 0x31, g: 0x32, b: 0x44 };
const SIDEBAR_ROW_HOVER: Rgb = Rgb { r: 0x24, g: 0x24, b: 0x3a }; // #24243a — between BG_SIDEBAR and active
const ACCENT: Rgb = Rgb { r: 0x89, g: 0xb4, b: 0xfa }; // active borders / highlights
const TEXT: Rgb = Rgb { r: 0xcd, g: 0xd6, b: 0xf4 };
const TEXT_DIM: Rgb = Rgb { r: 0x7f, g: 0x84, b: 0x9c };
const ATTENTION: Rgb = Rgb { r: 0xf3, g: 0x8b, b: 0xa8 }; // attention dot / ring
const PROGRESS: Rgb = Rgb { r: 0xa6, g: 0xe3, b: 0xa1 };
const ERROR: Rgb = Rgb { r: 0xf3, g: 0x8b, b: 0xa8 };
const PANE_BORDER_INACTIVE: Rgb = Rgb { r: 0x31, g: 0x32, b: 0x44 };
const CURSOR: Rgb = Rgb { r: 0xcd, g: 0xd6, b: 0xf4 };

// Spacing (8px grid).
const MARGIN: f32 = 8.0; // outer margin around the pane area
const GAP: f32 = 4.0; // gap between split panes
const INSET: f32 = 8.0; // cell area inset inside the pane border
const BORDER: f32 = 1.0; // pane border width
const ATTN_BORDER: f32 = 2.0; // attention ring width (overrides border)
const SIDEBAR_W: u32 = 220; // fixed sidebar width (app caps it to 1/3 window)
const SIDEBAR_PAD_TOP: f32 = 16.0;
const ROW_H: f32 = 48.0;
const ROW_GAP: f32 = 4.0;
const ROW_PAD_H: f32 = 12.0; // horizontal padding inside a sidebar row
const ACCENT_BAR_W: f32 = 3.0; // active-row left-edge bar
const ATTN_DOT: f32 = 8.0;
const RADIUS: f32 = 6.0; // rounded corner radius for sidebar rows + pane chrome
const BADGE_RADIUS: f32 = 4.0; // scroll badge chip
const TITLE_STRIP: f32 = 22.0; // pane title band inside the border, above the cells

const fn clear_of(c: Rgb) -> wgpu::Color {
    wgpu::Color { r: c.r as f64 / 255.0, g: c.g as f64 / 255.0, b: c.b as f64 / 255.0, a: 1.0 }
}
const DEFAULT_CLEAR: wgpu::Color = clear_of(BG_APP);

/// CPU-side alpha blend `fg` over `bg` (the bg pipeline is opaque, so the cursor is pre-mixed).
fn blend(fg: Rgb, bg: Rgb, a: f32) -> Rgb {
    let m = |f: u8, b: u8| ((f as f32 * a) + (b as f32 * (1.0 - a))).round() as u8;
    Rgb { r: m(fg.r, bg.r), g: m(fg.g, bg.g), b: m(fg.b, bg.b) }
}

/// Border width + color for a pane: attention ring overrides the active/inactive border.
fn border_style(active: bool, attention: Attention) -> (f32, Rgb) {
    if attention.is_pending() {
        (ATTN_BORDER, ATTENTION)
    } else if active {
        (BORDER, ACCENT)
    } else {
        (BORDER, PANE_BORDER_INACTIVE)
    }
}

/// One pane to draw in a multi-pane frame.
pub struct PaneView<'a> {
    pub snap: &'a PaneSnapshot,
    pub attention: Attention,
    pub active: bool,
    pub rect: Rect,
    /// Scrollback offset: 0 = live tail; >0 draws a '+{n}' badge top-right of the pane.
    pub scrolled: u32,
    /// Title shown in the pane's title strip (daemon-provided; short cwd / pane name).
    pub title: String,
    /// Selected cell range `((start_col,start_row),(end_col,end_row))`, normalized start<=end in
    /// reading order, in viewport cell coords. Those cells get fg/bg swapped + an ACCENT tint.
    pub selection: Option<((u16, u16), (u16, u16))>,
}

/// One workspace (window/tab) row in the sidebar.
pub struct SidebarRow {
    pub name: String,
    pub branch: Option<String>,
    pub attention: bool,
    pub active: bool,
    /// Cursor is hovering this row: draws a subtle hover fill (ignored when `active`).
    pub hover: bool,
    /// Aggregate agent progress: `Some(pct)` renders " 42%" after the name.
    pub progress: Option<u8>,
    /// A pane reported a progress error: renders " !" after the name (takes precedence over pct).
    pub progress_error: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

fn rgba(c: Rgb) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, 1.0]
}

/// A rounded-rect quad for the SDF chrome pipeline. `local` is the fragment's pixel offset from
/// the rect centre (interpolated); `half`/`radius` are constant per quad — the fragment computes
/// a rounded-box signed distance and alpha-masks the corners.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RoundedVertex {
    pos: [f32; 2],
    local: [f32; 2],
    half: [f32; 2],
    radius: f32,
    color: [f32; 4],
}

/// Push a rounded rect (`x0,y0`..`x1,y1` in pixels) into a rounded-pipeline vertex buffer.
fn push_rounded(out: &mut Vec<RoundedVertex>, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, color: [f32; 4], fw: f32, fh: f32) {
    let to_ndc = |x: f32, y: f32| [x / fw * 2.0 - 1.0, 1.0 - y / fh * 2.0];
    let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
    let (hx, hy) = ((x1 - x0) * 0.5, (y1 - y0) * 0.5);
    let corners = [(-hx, -hy), (hx, -hy), (hx, hy), (-hx, -hy), (hx, hy), (-hx, hy)];
    for (lx, ly) in corners {
        out.push(RoundedVertex {
            pos: to_ndc(cx + lx, cy + ly),
            local: [lx, ly],
            half: [hx, hy],
            radius,
            color,
        });
    }
}

/// Prefer Cascadia (Mono, then Code) from the Windows fonts dir; fall back to the platform
/// monospace (Consolas). Same ab_glyph path as the atlas — ASCII-only, unchanged.
fn load_atlas(px: f32) -> Atlas {
    for path in [r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\CascadiaCode.ttf"] {
        if let Ok(bytes) = std::fs::read(path) {
            if let Some(a) = Atlas::from_font_bytes(bytes, px) {
                return a;
            }
        }
    }
    Atlas::system_monospace(px).expect("a system monospace font")
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    format: wgpu::TextureFormat,
    bg_pipeline: wgpu::RenderPipeline,
    rounded_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    atlas_bind_group: wgpu::BindGroup,
    atlas_tex: wgpu::Texture, // dynamic glyph tiles are written here incrementally
    atlas: Atlas,
    // Theme knobs (see `set_theme`). Cell fg/bg still come from the daemon's grid; these only drive
    // the window clear color and the sidebar text color.
    clear: wgpu::Color,
    text: Rgb,
}

impl Renderer {
    /// Create a headless renderer (its own device) — used for offscreen rendering + tests.
    pub fn new_headless(format: wgpu::TextureFormat, font_px: f32) -> Option<Renderer> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gmux-headless"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .ok()?;
        Some(Self::from_device(device, queue, format, font_px))
    }

    /// Build a renderer on an existing device/queue (used by the windowed app with its surface).
    pub fn from_device(
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        font_px: f32,
    ) -> Renderer {
        let atlas = load_atlas(font_px);

        // Full-size R8 coverage texture. Only the initial region (ASCII grid + box tile) is uploaded
        // now; dynamic glyph tiles are written incrementally into the shelves below via
        // `queue.write_texture` (see `glyph_uv`). The rest zero-inits on first sample.
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-atlas"),
            size: wgpu::Extent3d { width: atlas.width, height: atlas.height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &atlas.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(atlas.width),
                rows_per_image: Some(atlas.init_h),
            },
            wgpu::Extent3d { width: atlas.width, height: atlas.init_h, depth_or_array_layers: 1 },
        );
        let tex_view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("gmux-atlas-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gmux-atlas-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gmux-atlas-bg"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&tex_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gmux-shaders"),
            source: wgpu::ShaderSource::Wgsl(SHADERS.into()),
        });

        // Background pipeline (opaque colored quads).
        let bg_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gmux-bg-layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gmux-bg-pipeline"),
            layout: Some(&bg_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("bg_vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<BgVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("bg_fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Rounded-chrome pipeline (alpha-blended SDF quads: sidebar rows, pane fills/borders,
        // badges). Shares the empty bind layout with bg; only the vertex format + blend differ.
        let rounded_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gmux-rounded-pipeline"),
            layout: Some(&bg_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("rounded_vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RoundedVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32, 4 => Float32x4],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("rounded_fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Glyph pipeline (alpha-blended textured quads).
        let glyph_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gmux-glyph-layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("gmux-glyph-pipeline"),
            layout: Some(&glyph_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("glyph_vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<GlyphVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("glyph_fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Renderer {
            device,
            queue,
            format,
            bg_pipeline,
            rounded_pipeline,
            glyph_pipeline,
            atlas_bind_group,
            atlas_tex: tex,
            atlas,
            clear: DEFAULT_CLEAR,
            text: TEXT,
        }
    }

    /// Apply theme colors: `bg` becomes the window clear color, `fg` the sidebar text color.
    /// (Terminal cell colors are owned by the daemon's grid, so they are unaffected.)
    pub fn set_theme(&mut self, fg: Rgb, bg: Rgb) {
        self.text = fg;
        self.clear = wgpu::Color {
            r: bg.r as f64 / 255.0,
            g: bg.g as f64 / 255.0,
            b: bg.b as f64 / 255.0,
            a: 1.0,
        };
    }

    pub fn cell_w(&self) -> u32 {
        self.atlas.cell_w
    }
    pub fn cell_h(&self) -> u32 {
        self.atlas.cell_h
    }
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Resolve a character to its atlas UV + cell width (1 or 2), rasterizing + uploading a new tile
    /// on a miss. Returns `None` for blanks (space/null). Called during vertex building (before the
    /// render pass), so any `write_texture` here is queued ahead of the draw that samples it.
    fn glyph_uv(&self, ch: char, wide: bool) -> Option<([f32; 4], u8)> {
        match self.atlas.glyph(ch, wide) {
            GlyphLookup::Blank => None,
            GlyphLookup::Ready { uv, cells } => Some((uv, cells)),
            GlyphLookup::Upload { uv, cells, x, y, w, tile } => {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.atlas_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x, y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &tile,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(w),
                        rows_per_image: Some(self.atlas.cell_h),
                    },
                    wgpu::Extent3d { width: w, height: self.atlas.cell_h, depth_or_array_layers: 1 },
                );
                Some((uv, cells))
            }
        }
    }

    /// Build the per-frame vertex data for a snapshot at the given pixel size.
    fn build_vertices(
        &self,
        snap: &PaneSnapshot,
        attention: Attention,
        active: bool,
        px_w: u32,
        px_h: u32,
        draw_border: bool,
        selection: Option<((u16, u16), (u16, u16))>,
    ) -> (Vec<BgVertex>, Vec<GlyphVertex>) {
        let cw = self.atlas.cell_w as f32;
        let ch = self.atlas.cell_h as f32;
        let (fw, fh) = (px_w.max(1) as f32, px_h.max(1) as f32);
        // Pixel-rect -> NDC (y down in pixels, up in NDC).
        let to_ndc = |x: f32, y: f32| [x / fw * 2.0 - 1.0, 1.0 - y / fh * 2.0];
        let push_quad = |v: &mut Vec<BgVertex>, x0: f32, y0: f32, x1: f32, y1: f32, color: [f32; 4]| {
            let (a, b, c, d) = (to_ndc(x0, y0), to_ndc(x1, y0), to_ndc(x1, y1), to_ndc(x0, y1));
            for p in [a, b, c, a, c, d] {
                v.push(BgVertex { pos: p, color });
            }
        };

        let mut bg = Vec::new();
        let mut glyphs = Vec::new();
        let (cursor_col, cursor_row) = snap.cursor;

        for (r, row) in snap.cells.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                let x0 = c as f32 * cw;
                let y0 = r as f32 * ch;
                // A wide glyph spans this cell + the next spacer: cursor/selection on EITHER cell
                // must style BOTH, or half the glyph highlights (the spacer inherits the lead's
                // state, and a lead is styled when its spacer is hit).
                let lead_wide = c > 0 && row.get(c - 1).is_some_and(|p| p.wide);
                let hits = |col: usize| -> bool {
                    col as u16 == cursor_col && r as u16 == cursor_row
                };
                let is_cursor = hits(c)
                    || (lead_wide && hits(c - 1))
                    || (cell.wide && hits(c + 1));
                let selected = in_selection(selection, r, c)
                    || (lead_wide && in_selection(selection, r, c - 1))
                    || (cell.wide && in_selection(selection, r, c + 1));
                // Selection: swap fg/bg and tint the (swapped) bg 30% toward ACCENT so the
                // highlight reads over any content (blank cells, dark-on-dark, etc.).
                let (mut cell_bg, mut cell_fg) = (cell.bg, cell.fg);
                if selected {
                    std::mem::swap(&mut cell_bg, &mut cell_fg);
                    cell_bg = blend(ACCENT, cell_bg, 0.3);
                }
                // Cursor: pre-blend at ~70% over the cell bg (opaque bg pipeline can't alpha-blend).
                let bg_color = if is_cursor { blend(CURSOR, cell_bg, 0.7) } else { cell_bg };
                push_quad(&mut bg, x0, y0, x0 + cw, y0 + ch, rgba(bg_color));

                // Wide (CJK) glyphs span two cells; the daemon sends a ' ' spacer in the next cell
                // (which draws no glyph of its own). The bg quad above is still drawn for both cells.
                if let Some((uv, cells)) = self.glyph_uv(cell.ch, cell.wide) {
                    let gw = cw * cells as f32;
                    let (a, b, cc, d) = (
                        (to_ndc(x0, y0), [uv[0], uv[1]]),
                        (to_ndc(x0 + gw, y0), [uv[2], uv[1]]),
                        (to_ndc(x0 + gw, y0 + ch), [uv[2], uv[3]]),
                        (to_ndc(x0, y0 + ch), [uv[0], uv[3]]),
                    );
                    // Inactive panes dim their text so the focused pane pops.
                    let mut fg = rgba(cell_fg);
                    if !active {
                        for ch in fg.iter_mut().take(3) {
                            *ch *= 0.8;
                        }
                    }
                    for (p, t) in [a, b, cc, a, cc, d] {
                        glyphs.push(GlyphVertex { pos: p, uv: t, color: fg });
                    }
                }
            }
        }

        // Border drawn at the viewport edges (used by the offscreen/test path; the windowed frame
        // draws its own chrome border around the inset cell rect, so it passes `draw_border=false`).
        if draw_border {
            let (bw, bc) = border_style(active, attention);
            let c = rgba(bc);
            push_quad(&mut bg, 0.0, 0.0, fw, bw, c); // top
            push_quad(&mut bg, 0.0, fh - bw, fw, fh, c); // bottom
            push_quad(&mut bg, 0.0, 0.0, bw, fh, c); // left
            push_quad(&mut bg, fw - bw, 0.0, fw, fh, c); // right
        }

        (bg, glyphs)
    }

    /// Render a single snapshot filling `view` (used by the offscreen tests).
    pub fn render(
        &self,
        view: &wgpu::TextureView,
        snap: &PaneSnapshot,
        attention: Attention,
        px_w: u32,
        px_h: u32,
    ) {
        self.render_panes(
            view,
            &[PaneView {
                snap,
                attention,
                active: true,
                rect: Rect { x: 0, y: 0, w: px_w, h: px_h },
                scrolled: 0,
                title: String::new(),
                selection: None,
            }],
            px_w,
            px_h,
        );
    }

    /// Render multiple panes, each into its rectangle within a `surf_w × surf_h` surface, in one
    /// pass. Gaps between panes show the clear colour.
    pub fn render_panes(&self, view: &wgpu::TextureView, panes: &[PaneView], surf_w: u32, surf_h: u32) {
        // Build every pane's vertex buffers first (can't create buffers mid-pass).
        struct Draw {
            rect: Rect,
            bg: wgpu::Buffer,
            bg_n: u32,
            glyph: wgpu::Buffer,
            glyph_n: u32,
        }
        let mut draws = Vec::with_capacity(panes.len());
        for pv in panes {
            let (bg, glyphs) = self.build_vertices(
                pv.snap,
                pv.attention,
                pv.active,
                pv.rect.w.max(1),
                pv.rect.h.max(1),
                true,
                pv.selection,
            );
            draws.push(Draw {
                rect: pv.rect,
                bg_n: bg.len() as u32,
                glyph_n: glyphs.len() as u32,
                bg: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("gmux-bg-vb"),
                    contents: bytemuck::cast_slice(&bg),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
                glyph: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("gmux-glyph-vb"),
                    contents: bytemuck::cast_slice(&glyphs),
                    usage: wgpu::BufferUsages::VERTEX,
                }),
            });
        }

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gmux-enc") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gmux-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            for d in &draws {
                let (x, y, w, h) = (d.rect.x, d.rect.y, d.rect.w.max(1), d.rect.h.max(1));
                // Clamp to the surface so a viewport never exceeds the attachment.
                let w = w.min(surf_w.saturating_sub(x)).max(1);
                let h = h.min(surf_h.saturating_sub(y)).max(1);
                pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                pass.set_scissor_rect(x, y, w, h);
                if d.bg_n > 0 {
                    pass.set_pipeline(&self.bg_pipeline);
                    pass.set_vertex_buffer(0, d.bg.slice(..));
                    pass.draw(0..d.bg_n, 0..1);
                }
                if d.glyph_n > 0 {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                    pass.set_vertex_buffer(0, d.glyph.slice(..));
                    pass.draw(0..d.glyph_n, 0..1);
                }
            }
        }
        self.queue.submit([enc.finish()]);
    }

    /// The sidebar width (fixed; the app caps it to 1/3 of the window).
    /// Total per-axis chrome around a pane's cell area (margin + border + inset on both sides).
    /// The GUI reports this to the daemon so grids are sized to the *visible* cell area instead
    /// of the full rect (cells were silently scissored off otherwise).
    pub fn pane_chrome_px(&self) -> u32 {
        (2.0 * (MARGIN + BORDER + INSET)) as u32
    }

    /// Vertical chrome around a pane's cell area: [`pane_chrome_px`] plus the 22px title strip.
    /// The GUI reports this to the daemon so rows are sized to the cell area *below* the strip.
    pub fn pane_chrome_y_px(&self) -> u32 {
        (2.0 * (MARGIN + BORDER) + INSET * 2.0 + TITLE_STRIP) as u32
    }

    /// Map a y coordinate (px, window space) to a sidebar row index — the single source of truth
    /// for click hit-testing, using the same metrics `build_sidebar` draws with.
    pub fn sidebar_row_at(&self, y: f32, row_count: usize) -> Option<usize> {
        let rows_y0 = SIDEBAR_PAD_TOP + self.cell_h() as f32 + 8.0;
        if y < rows_y0 {
            return None;
        }
        let stride = ROW_H + ROW_GAP;
        let rel = y - rows_y0;
        let idx = (rel / stride) as usize;
        if rel - idx as f32 * stride >= ROW_H || idx >= row_count {
            return None;
        }
        Some(idx)
    }

    /// Hit-test the '+ new tab' row drawn immediately after the last workspace row (same 48px
    /// metrics as `sidebar_row_at`, so the two never overlap).
    pub fn sidebar_new_tab_at(&self, y: f32, row_count: usize) -> bool {
        let rows_y0 = SIDEBAR_PAD_TOP + self.cell_h() as f32 + 8.0;
        let top = rows_y0 + row_count as f32 * (ROW_H + ROW_GAP);
        y >= top && y < top + ROW_H
    }

    pub fn sidebar_width(&self) -> u32 {
        SIDEBAR_W
    }

    /// Append glyph quads for `s` starting at pixel `(x, y)` (monospace advance), full-surface NDC.
    fn text_run(&self, s: &str, x: f32, y: f32, color: [f32; 4], fw: f32, fh: f32, out: &mut Vec<GlyphVertex>) {
        let cw = self.atlas.cell_w as f32;
        let ch = self.atlas.cell_h as f32;
        let to_ndc = |x: f32, y: f32| [x / fw * 2.0 - 1.0, 1.0 - y / fh * 2.0];
        for (i, c) in s.chars().enumerate() {
            // Chrome text is monospace (one cell advance) even for wide glyphs, so pass wide=false.
            if let Some((uv, _cells)) = self.glyph_uv(c, false) {
                let x0 = x + i as f32 * cw;
                let corners = [
                    (to_ndc(x0, y), [uv[0], uv[1]]),
                    (to_ndc(x0 + cw, y), [uv[2], uv[1]]),
                    (to_ndc(x0 + cw, y + ch), [uv[2], uv[3]]),
                    (to_ndc(x0, y), [uv[0], uv[1]]),
                    (to_ndc(x0 + cw, y + ch), [uv[2], uv[3]]),
                    (to_ndc(x0, y + ch), [uv[0], uv[3]]),
                ];
                for (p, t) in corners {
                    out.push(GlyphVertex { pos: p, uv: t, color });
                }
            }
        }
    }

    fn build_sidebar(&self, rows: &[SidebarRow], sidebar_w: u32, plus_hover: bool, fw: f32, fh: f32) -> (Vec<BgVertex>, Vec<RoundedVertex>, Vec<GlyphVertex>) {
        let mut bg = Vec::new();
        let mut rd = Vec::new(); // rounded chrome (row fills, accent bar, attention dot)
        let mut gl = Vec::new();
        let sw = sidebar_w as f32;
        let ch = self.atlas.cell_h as f32;
        let to_ndc = |x: f32, y: f32| [x / fw * 2.0 - 1.0, 1.0 - y / fh * 2.0];
        let quad = |bg: &mut Vec<BgVertex>, x0: f32, y0: f32, x1: f32, y1: f32, c: [f32; 4]| {
            let (a, b, cc, d) = (to_ndc(x0, y0), to_ndc(x1, y0), to_ndc(x1, y1), to_ndc(x0, y1));
            for p in [a, b, cc, a, cc, d] {
                bg.push(BgVertex { pos: p, color: c });
            }
        };
        let cw = self.atlas.cell_w as f32;
        quad(&mut bg, 0.0, 0.0, sw, fh, rgba(BG_SIDEBAR));

        // Section label: "WORKSPACES" in dim uppercase.
        self.text_run("WORKSPACES", ROW_PAD_H, SIDEBAR_PAD_TOP, rgba(TEXT_DIM), fw, fh, &mut gl);
        let rows_y0 = SIDEBAR_PAD_TOP + ch + 8.0;
        let text_x = ROW_PAD_H;
        let pad_v = ((ROW_H - 2.0 * ch) / 2.0).max(2.0); // vertically center the two text lines
        let right_edge = sw - ROW_PAD_H;
        let stride = ROW_H + ROW_GAP;

        for (i, r) in rows.iter().enumerate() {
            let top = rows_y0 + i as f32 * stride;
            let line1 = top + pad_v;
            let line2 = line1 + ch;
            // Row fill: active wins over hover. Accent bar sits in the straight span so its sharp
            // corners never poke past the rounded fill.
            if r.active {
                push_rounded(&mut rd, 0.0, top, sw, top + ROW_H, RADIUS, rgba(SIDEBAR_ROW_ACTIVE), fw, fh);
                push_rounded(&mut rd, 0.0, top + RADIUS, ACCENT_BAR_W, top + ROW_H - RADIUS, 0.0, rgba(ACCENT), fw, fh);
            } else if r.hover {
                push_rounded(&mut rd, 0.0, top, sw, top + ROW_H, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
            }
            self.text_run(&r.name, text_x, line1, rgba(self.text), fw, fh, &mut gl);
            if let Some(b) = &r.branch {
                self.text_run(&format!("git:{b}"), text_x, line2, rgba(TEXT_DIM), fw, fh, &mut gl);
            }

            // Right-aligned indicators on line 1: progress text (PROGRESS / ERROR), then the
            // attention dot (a round SDF chip) to its left.
            let mut cursor_right = right_edge;
            if r.progress_error || r.progress.is_some() {
                let (txt, col) = if r.progress_error {
                    ("!".to_string(), ERROR)
                } else {
                    (format!("{}%", r.progress.unwrap()), PROGRESS)
                };
                let w = txt.chars().count() as f32 * cw;
                self.text_run(&txt, cursor_right - w, line1, rgba(col), fw, fh, &mut gl);
                cursor_right -= w + 4.0;
            }
            if r.attention {
                let x1 = cursor_right;
                let y0 = line1 + (ch - ATTN_DOT) / 2.0;
                push_rounded(&mut rd, x1 - ATTN_DOT, y0, x1, y0 + ATTN_DOT, ATTN_DOT / 2.0, rgba(ATTENTION), fw, fh);
            }
        }

        // '+ new tab' row, immediately after the last workspace row (matches sidebar_new_tab_at).
        let plus_top = rows_y0 + rows.len() as f32 * stride;
        if plus_hover {
            push_rounded(&mut rd, 0.0, plus_top, sw, plus_top + ROW_H, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
        }
        self.text_run("+ new tab", text_x, plus_top + (ROW_H - ch) / 2.0, rgba(TEXT_DIM), fw, fh, &mut gl);

        (bg, rd, gl)
    }

    /// Render a full frame: the sidebar (left column) plus the panes.
    pub fn render_frame(
        &self,
        view: &wgpu::TextureView,
        sidebar: &[SidebarRow],
        sidebar_w: u32,
        panes: &[PaneView],
        surf_w: u32,
        surf_h: u32,
        empty_msg: &str,
        plus_hover: bool,
    ) {
        let (fw, fh) = (surf_w.max(1) as f32, surf_h.max(1) as f32);
        // `sbg` is the opaque sidebar panel; `srd` is the rounded chrome (sidebar rows + pane
        // fills/borders); `sgl` is the sidebar text plus any empty-state message. `obg`/`ogl` are
        // the scroll-badge overlay, drawn last so they sit above the pane cells.
        let (sbg, mut srd, mut sgl) = self.build_sidebar(sidebar, sidebar_w, plus_hover, fw, fh);
        let mut obg: Vec<RoundedVertex> = Vec::new();
        let mut ogl: Vec<GlyphVertex> = Vec::new();
        let (cw_cell, ch_cell) = (self.atlas.cell_w as f32, self.atlas.cell_h as f32);
        let vb = |data: &[u8]| {
            self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("gmux-vb"),
                contents: data,
                usage: wgpu::BufferUsages::VERTEX,
            })
        };

        struct Draw {
            rect: Rect,
            bg: wgpu::Buffer,
            bg_n: u32,
            glyph: wgpu::Buffer,
            glyph_n: u32,
        }
        let mut draws = Vec::with_capacity(panes.len());
        for pv in panes {
            // Daemon rects tile the content area edge-to-edge. Shrink each edge: MARGIN at the
            // content boundary, GAP/2 at an interior split edge (so neighbours share a GAP gap).
            let (ox, oy, ow, oh) = (pv.rect.x as f32, pv.rect.y as f32, pv.rect.w as f32, pv.rect.h as f32);
            let l = if pv.rect.x <= sidebar_w { MARGIN } else { GAP / 2.0 };
            let t = if pv.rect.y == 0 { MARGIN } else { GAP / 2.0 };
            let rgt = if pv.rect.x + pv.rect.w >= surf_w { MARGIN } else { GAP / 2.0 };
            let bot = if pv.rect.y + pv.rect.h >= surf_h { MARGIN } else { GAP / 2.0 };
            let (cx, cy) = (ox + l, oy + t);
            let (cw_, ch_) = ((ow - l - rgt).max(1.0), (oh - t - bot).max(1.0));

            // Pane chrome: a rounded border ring (outer) with the BG_PANE fill (inner, inset by the
            // border width) drawn on top. The fill also letterboxes the cell-grid remainder.
            let (bw, bc) = border_style(pv.active, pv.attention);
            push_rounded(&mut srd, cx, cy, cx + cw_, cy + ch_, RADIUS, rgba(bc), fw, fh);
            push_rounded(&mut srd, cx + bw, cy + bw, cx + cw_ - bw, cy + ch_ - bw, (RADIUS - bw).max(0.0), rgba(BG_PANE), fw, fh);

            // Title strip: a TITLE_STRIP-tall BG_SIDEBAR band inside the border. First quad rounds
            // the top corners (radius RADIUS-bw); the second (radius 0) squares off the bottom edge
            // where it meets the cell area. Active pane gets an ACCENT dot before the title text.
            let (sx0, sx1) = (cx + bw, cx + cw_ - bw);
            let (sy0, sy1) = (cy + bw, cy + bw + TITLE_STRIP);
            let sr = (RADIUS - bw).max(0.0);
            push_rounded(&mut srd, sx0, sy0, sx1, sy1, sr, rgba(BG_SIDEBAR), fw, fh);
            push_rounded(&mut srd, sx0, sy0 + sr, sx1, sy1, 0.0, rgba(BG_SIDEBAR), fw, fh);
            let ty = (sy0 + (TITLE_STRIP - ch_cell) / 2.0).max(sy0);
            let mut tx = sx0 + 12.0;
            if pv.active {
                let dot = 6.0;
                let dy = sy0 + (TITLE_STRIP - dot) / 2.0;
                push_rounded(&mut srd, tx, dy, tx + dot, dy + dot, dot / 2.0, rgba(ACCENT), fw, fh);
                tx += dot + 5.0;
            }
            let max_chars = ((sx1 - 8.0 - tx).max(0.0) / cw_cell) as usize;
            let title = truncate_ellipsis(&pv.title, max_chars);
            if !title.is_empty() {
                self.text_run(&title, tx, ty, rgba(TEXT_DIM), fw, fh, &mut sgl);
            }

            // Scroll badge: '+{n}' chip top-right inside the pane, below the title strip (drawn
            // later, above the cells).
            if pv.scrolled > 0 {
                let label = format!("+{}", pv.scrolled);
                let (bpx, bpy) = (4.0, 2.0);
                let bw_chip = label.chars().count() as f32 * cw_cell + 2.0 * bpx;
                let bh_chip = ch_cell + 2.0 * bpy;
                let br = cx + cw_ - bw - 4.0;
                let bt = cy + bw + TITLE_STRIP + 4.0;
                push_rounded(&mut obg, br - bw_chip, bt, br, bt + bh_chip, BADGE_RADIUS, rgba(BG_SIDEBAR), fw, fh);
                self.text_run(&label, br - bw_chip + bpx, bt + bpy, rgba(ACCENT), fw, fh, &mut ogl);
            }

            // Cell area: inset INSET on the sides and bottom, and below the title strip on top.
            // Cells draw at fixed size from its top-left; the viewport clips overflow, the BG_PANE
            // fill shows through any remainder.
            let pad = bw + INSET;
            let (ix, iy) = (cx + pad, cy + bw + TITLE_STRIP + INSET);
            let (iw, ih) = ((cw_ - 2.0 * pad).max(1.0), (ch_ - bw - TITLE_STRIP - INSET - pad).max(1.0));
            let (bg, glyphs) =
                self.build_vertices(pv.snap, pv.attention, pv.active, iw as u32, ih as u32, false, pv.selection);
            draws.push(Draw {
                rect: Rect { x: ix as u32, y: iy as u32, w: iw as u32, h: ih as u32 },
                bg_n: bg.len() as u32,
                glyph_n: glyphs.len() as u32,
                bg: vb(bytemuck::cast_slice(&bg)),
                glyph: vb(bytemuck::cast_slice(&glyphs)),
            });
        }

        // Empty state: no panes to draw.
        if panes.is_empty() {
            let msg = empty_msg;
            let tw = msg.chars().count() as f32 * self.atlas.cell_w as f32;
            let content_w = fw - sidebar_w as f32;
            let x = sidebar_w as f32 + ((content_w - tw) / 2.0).max(0.0);
            let y = (fh - self.atlas.cell_h as f32) / 2.0;
            self.text_run(msg, x, y, rgba(TEXT_DIM), fw, fh, &mut sgl);
        }

        let sbg_buf = vb(bytemuck::cast_slice(&sbg));
        let srd_buf = vb(bytemuck::cast_slice(&srd));
        let sgl_buf = vb(bytemuck::cast_slice(&sgl));
        let obg_buf = vb(bytemuck::cast_slice(&obg));
        let ogl_buf = vb(bytemuck::cast_slice(&ogl));

        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gmux-frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gmux-frame-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(self.clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Sidebar (full-surface viewport).
            pass.set_viewport(0.0, 0.0, fw, fh, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, surf_w, surf_h);
            if !sbg.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_vertex_buffer(0, sbg_buf.slice(..));
                pass.draw(0..sbg.len() as u32, 0..1);
            }
            if !srd.is_empty() {
                pass.set_pipeline(&self.rounded_pipeline);
                pass.set_vertex_buffer(0, srd_buf.slice(..));
                pass.draw(0..srd.len() as u32, 0..1);
            }
            if !sgl.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                pass.set_vertex_buffer(0, sgl_buf.slice(..));
                pass.draw(0..sgl.len() as u32, 0..1);
            }
            // Panes (viewport per pane).
            for d in &draws {
                let (x, y) = (d.rect.x, d.rect.y);
                if x >= surf_w || y >= surf_h {
                    continue; // inset origin off-surface (absurdly small window): nothing to draw
                }
                let w = d.rect.w.max(1).min(surf_w.saturating_sub(x)).max(1);
                let h = d.rect.h.max(1).min(surf_h.saturating_sub(y)).max(1);
                pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                pass.set_scissor_rect(x, y, w, h);
                if d.bg_n > 0 {
                    pass.set_pipeline(&self.bg_pipeline);
                    pass.set_vertex_buffer(0, d.bg.slice(..));
                    pass.draw(0..d.bg_n, 0..1);
                }
                if d.glyph_n > 0 {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                    pass.set_vertex_buffer(0, d.glyph.slice(..));
                    pass.draw(0..d.glyph_n, 0..1);
                }
            }
            // Scroll-badge overlay (full-surface viewport, above the pane cells).
            if !obg.is_empty() || !ogl.is_empty() {
                pass.set_viewport(0.0, 0.0, fw, fh, 0.0, 1.0);
                pass.set_scissor_rect(0, 0, surf_w, surf_h);
                if !obg.is_empty() {
                    pass.set_pipeline(&self.rounded_pipeline);
                    pass.set_vertex_buffer(0, obg_buf.slice(..));
                    pass.draw(0..obg.len() as u32, 0..1);
                }
                if !ogl.is_empty() {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, &self.atlas_bind_group, &[]);
                    pass.set_vertex_buffer(0, ogl_buf.slice(..));
                    pass.draw(0..ogl.len() as u32, 0..1);
                }
            }
        }
        self.queue.submit([enc.finish()]);
    }
}

/// Reading-order (row-major) hit-test: is cell (row `r`, col `c`) inside the selection range?
/// `sel` is `((start_col,start_row),(end_col,end_row))`, normalized start<=end in reading order.
fn in_selection(sel: Option<((u16, u16), (u16, u16))>, r: usize, c: usize) -> bool {
    match sel {
        Some(((sc, sr), (ec, er))) => {
            let pos = (r as u16, c as u16);
            pos >= (sr, sc) && pos <= (er, ec)
        }
        None => false,
    }
}

/// Truncate `s` to at most `max` display cells, appending "..." when it would overflow.
fn truncate_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return s.chars().take(max).collect();
    }
    let mut t: String = s.chars().take(max - 3).collect();
    t.push_str("...");
    t
}

const SHADERS: &str = r#"
struct BgOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn bg_vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> BgOut {
    var o: BgOut; o.pos = vec4<f32>(pos, 0.0, 1.0); o.color = color; return o;
}
@fragment fn bg_fs(i: BgOut) -> @location(0) vec4<f32> { return i.color; }

struct RoundedOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) half: vec2<f32>,
    @location(2) radius: f32,
    @location(3) color: vec4<f32>,
};
@vertex fn rounded_vs(@location(0) pos: vec2<f32>, @location(1) local: vec2<f32>, @location(2) half: vec2<f32>, @location(3) radius: f32, @location(4) color: vec4<f32>) -> RoundedOut {
    var o: RoundedOut;
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    o.local = local; o.half = half; o.radius = radius; o.color = color;
    return o;
}
@fragment fn rounded_fs(i: RoundedOut) -> @location(0) vec4<f32> {
    // Signed distance to a rounded box; 1px anti-aliased alpha mask at the edge.
    let q = abs(i.local) - (i.half - vec2<f32>(i.radius));
    let d = min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0))) - i.radius;
    let aa = clamp(0.5 - d, 0.0, 1.0);
    return vec4<f32>(i.color.rgb, i.color.a * aa);
}

struct GlyphOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec4<f32> };
@group(0) @binding(0) var atlas_tex: texture_2d<f32>;
@group(0) @binding(1) var atlas_samp: sampler;
@vertex fn glyph_vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>) -> GlyphOut {
    var o: GlyphOut; o.pos = vec4<f32>(pos, 0.0, 1.0); o.uv = uv; o.color = color; return o;
}
@fragment fn glyph_fs(i: GlyphOut) -> @location(0) vec4<f32> {
    let cov = textureSample(atlas_tex, atlas_samp, i.uv).r;
    return vec4<f32>(i.color.rgb, i.color.a * cov);
}
"#;
