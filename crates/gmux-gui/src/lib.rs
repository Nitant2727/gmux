//! gmux-gui — the winit + wgpu renderer for gmux panes.
//!
//! [`renderer::Renderer`] draws a [`gmux_mux::PaneSnapshot`]; [`render_offscreen`] renders to a
//! texture and reads the pixels back (used for headless verification, since the agent harness has
//! no display). The windowed app lives in [`app`].

pub mod app;
pub mod atlas;
pub mod config;
pub mod daemon_client;
pub mod renderer;

pub use app::run;
pub use renderer::Renderer;

use gmux_mux::{Attention, PaneSnapshot};

fn align_up(v: u32, align: u32) -> u32 {
    v.div_ceil(align) * align
}

/// Render a snapshot to an offscreen RGBA8 texture and return `(width, height, rgba_bytes)`.
/// Returns `None` if no GPU adapter / monospace font is available.
pub fn render_offscreen(
    snap: &PaneSnapshot,
    attention: Attention,
    px_w: u32,
    px_h: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let r = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0)?;

    let tex = r.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("gmux-offscreen"),
        size: wgpu::Extent3d { width: px_w, height: px_h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    r.render(&view, snap, attention, px_w, px_h);
    Some((px_w, px_h, read_rgba(&r, &tex, px_w, px_h)?))
}

/// Copy a rendered RGBA8 texture back to CPU memory (row-unpadded `w*h*4` bytes).
fn read_rgba(r: &Renderer, tex: &wgpu::Texture, px_w: u32, px_h: u32) -> Option<Vec<u8>> {
    let unpadded = px_w * 4;
    let padded = align_up(unpadded, 256);
    let buf = r.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gmux-readback"),
        size: (padded * px_h) as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc =
        r.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gmux-copy") });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(px_h),
            },
        },
        wgpu::Extent3d { width: px_w, height: px_h, depth_or_array_layers: 1 },
    );
    r.queue.submit([enc.finish()]);

    let slice = buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    let _ = r.device.poll(wgpu::PollType::wait_indefinitely());
    rx.recv().ok()?.ok()?;

    let data = slice.get_mapped_range().ok()?;
    let mut out = Vec::with_capacity((unpadded * px_h) as usize);
    for row in 0..px_h {
        let start = (row * padded) as usize;
        out.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    drop(data);
    buf.unmap();
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_mux::{Cell, Rgb};

    fn cell(ch: char, fg: Rgb, bg: Rgb) -> Cell {
        Cell { ch, fg, bg, bold: false, italic: false, underline: false, inverse: false, wide: false }
    }

    fn pixel(px: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * w + x) * 4) as usize;
        [px[o], px[o + 1], px[o + 2], px[o + 3]]
    }

    #[test]
    fn renders_background_cell_colors() {
        let red = Rgb { r: 200, g: 20, b: 20 };
        let blue = Rgb { r: 20, g: 20, b: 200 };
        let bg_default = Rgb { r: 0x11, g: 0x11, b: 0x11 };
        let snap = PaneSnapshot {
            cells: vec![vec![cell(' ', bg_default, red), cell(' ', bg_default, blue)]],
            cursor: (99, 99), // off-grid so no cursor override
            cols: 2,
            rows: 1,
            cursor_style: 0,
        };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        drop(r);
        let (w, h, px) = render_offscreen(&snap, Attention::Quiet, cw * 2, ch).expect("render");

        let left = pixel(&px, w, cw / 2, h / 2);
        let right = pixel(&px, w, cw + cw / 2, h / 2);
        assert!(left[0] > 150 && left[2] < 80, "cell(0,0) should be red, got {left:?}");
        assert!(right[2] > 150 && right[0] < 80, "cell(1,0) should be blue, got {right:?}");
    }

    #[test]
    fn attention_ring_draws_attention_border() {
        let bg = Rgb { r: 0x11, g: 0x11, b: 0x11 };
        let fg = Rgb { r: 0xcc, g: 0xcc, b: 0xcc };
        let snap = PaneSnapshot {
            cells: vec![vec![cell(' ', fg, bg); 4]; 2],
            cursor: (99, 99),
            cols: 4,
            rows: 2,
            cursor_style: 0,
        };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w() * 4, r.cell_h() * 2);
        drop(r);
        let (w, _h, px) = render_offscreen(&snap, Attention::Pending, cw, ch).expect("render");
        let corner = pixel(&px, w, 1, 1);
        // Attention ring is now the pink ATTENTION token (#f38ba8), not blue.
        assert!(corner[0] > 150 && corner[1] > 80 && corner[2] > 100, "expected pink ring, got {corner:?}");
    }

    #[test]
    fn rounded_sidebar_row_cuts_corner() {
        // Verifies the alpha-blended rounded-quad pipeline: an active sidebar row's corner is
        // masked away (shows the panel bg), while its centre is the solid active fill.
        use crate::renderer::SidebarRow;
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let sw = r.sidebar_width();
        let rows_y0 = 24 + r.cell_h(); // SIDEBAR_PAD_TOP(16) + cell_h + 8
        let row_h = 48u32;
        let (w, h) = (sw, rows_y0 + row_h + 20);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-frame-test"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let rows = vec![SidebarRow {
            name: "ws".into(),
            branch: None,
            attention: false,
            active: true,
            hover: false,
            progress: None,
            progress_error: false,
        }];
        r.render_frame(&view, &rows, sw, &[], w, h, "", false, None, None, None);
        let px = read_rgba(&r, &tex, w, h).expect("readback");
        // Panel bg is #181825 (r≈24); active fill is #313244 (r≈49). Threshold 40 splits them.
        let corner = pixel(&px, w, 1, rows_y0 + 1);
        let center = pixel(&px, w, sw / 2, rows_y0 + row_h / 2);
        assert!(corner[0] < 40, "active-row corner should round away to panel bg, got {corner:?}");
        assert!(center[0] > 40, "active-row centre should be the solid fill, got {center:?}");
    }

    #[test]
    fn renders_glyph_coverage_over_background() {
        let bg = Rgb { r: 0, g: 0, b: 0 };
        let fg = Rgb { r: 255, g: 255, b: 255 };
        let snap = PaneSnapshot { cells: vec![vec![cell('M', fg, bg)]], cursor: (99, 99), cols: 1, rows: 1, cursor_style: 0 };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        drop(r);
        let (_w, _h, px) = render_offscreen(&snap, Attention::Quiet, cw, ch).expect("render");
        let bright = px.chunks(4).filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150).count();
        assert!(bright > 5, "'M' glyph produced no bright pixels ({bright})");
    }

    #[test]
    fn selection_highlights_cells() {
        // 3 blank cells in a row; select only the middle one. Its bg is fg/bg-swapped then tinted
        // toward ACCENT, so it must differ from the untouched dark bg of the unselected cells.
        use crate::renderer::PaneView;
        use gmux_mux::Rect;
        let bg = Rgb { r: 20, g: 20, b: 20 };
        let fg = Rgb { r: 210, g: 210, b: 210 };
        let snap = PaneSnapshot { cells: vec![vec![cell(' ', fg, bg); 3]], cursor: (99, 99), cols: 3, rows: 1, cursor_style: 0 };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        let (w, h) = (cw * 3, ch);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-sel-test"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let pv = PaneView {
            snap: &snap,
            attention: Attention::Quiet,
            active: true,
            rect: Rect { x: 0, y: 0, w, h },
            scrolled: 0,
            history: 0,
            title: String::new(),
            selection: Some(((1, 0), (1, 0))),
        };
        r.render_panes(&view, &[pv], w, h);
        let px = read_rgba(&r, &tex, w, h).expect("readback");
        let unselected = pixel(&px, w, cw / 2, ch / 2);
        let selected = pixel(&px, w, cw + cw / 2, ch / 2);
        assert_ne!(selected, unselected, "selected cell bg should differ from unselected");
        assert!(
            selected[0] as i32 - unselected[0] as i32 > 60,
            "selected cell should be visibly highlighted, got sel={selected:?} unsel={unselected:?}"
        );
    }

    #[test]
    fn search_bar_renders_band() {
        // A frame with a SearchBar on the active pane draws a band at the pane bottom whose text
        // glyphs (the TEXT-white query "foo_") appear only there — absent without a SearchBar.
        use crate::renderer::{PaneView, SearchBar};
        use gmux_mux::Rect;
        let bg = Rgb { r: 200, g: 20, b: 20 }; // distinct cell bg (vs the band's dark BG_SIDEBAR)
        let fg = Rgb { r: 0, g: 0, b: 0 };
        let snap = PaneSnapshot { cells: vec![vec![cell(' ', fg, bg); 8]; 4], cursor: (99, 99), cols: 8, rows: 4, cursor_style: 0 };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        let (w, h) = (cw * 8 + 60, ch * 4 + 100);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-search-test"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let pv = || PaneView {
            snap: &snap,
            attention: Attention::Quiet,
            active: true,
            rect: Rect { x: 0, y: 0, w, h },
            scrolled: 0,
            history: 0,
            title: String::new(),
            selection: None,
        };
        // Band spans y in [h-31, h-9] (cy=MARGIN=8, border=1, SEARCH_BAR=22). Count TEXT-white
        // pixels there (the query glyphs) — none appear without a SearchBar.
        let white_in_band = |px: &[u8]| {
            let mut n = 0;
            for y in (h - 30)..(h - 10) {
                for x in 0..w {
                    let p = pixel(px, w, x, y);
                    if p[0] > 150 && p[1] > 150 && p[2] > 150 {
                        n += 1;
                    }
                }
            }
            n
        };
        let sb = SearchBar { label: "find:".into(), query: "foo".into(), current: 1, total: 5 };
        r.render_frame(&view, &[], 0, &[pv()], w, h, "", false, Some(&sb), None, None);
        let with = white_in_band(&read_rgba(&r, &tex, w, h).expect("readback"));
        r.render_frame(&view, &[], 0, &[pv()], w, h, "", false, None, None, None);
        let without = white_in_band(&read_rgba(&r, &tex, w, h).expect("readback"));
        assert!(with > 3, "search band should draw the query text ({with} white px)");
        assert_eq!(without, 0, "no search bar → no band text ({without} white px)");
    }

    /// The cell width the dynamic atlas assigns to `ch` when asked for a wide tile: 2 if a font in
    /// the fallback chain can rasterize it, else 1 (box fallback). Mirrors the renderer's font
    /// preference so the wide-cell render test can honestly skip on a CJK-font-less runner.
    fn probe_wide_cells(ch: char) -> u8 {
        use crate::atlas::{Atlas, GlyphLookup};
        let atlas = [r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\CascadiaCode.ttf"]
            .iter()
            .find_map(|p| std::fs::read(p).ok().and_then(|b| Atlas::from_font_bytes(b, 18.0)))
            .or_else(|| Atlas::system_monospace(18.0))
            .expect("a monospace font");
        match atlas.glyph(ch, true) {
            GlyphLookup::Ready { cells, .. } | GlyphLookup::Upload { cells, .. } => cells,
            GlyphLookup::Blank => 0,
        }
    }

    #[test]
    fn wide_cell_draws_across_two_cells() {
        // A wide CJK glyph must render into BOTH its own cell and the spacer cell to its right.
        if probe_wide_cells('中') != 2 {
            eprintln!("no CJK/wide glyph on this runner; skipping cross-cell assertion");
            return;
        }
        let bg = Rgb { r: 0, g: 0, b: 0 };
        let fg = Rgb { r: 255, g: 255, b: 255 };
        // '中' is wide; the daemon sends a ' ' spacer in the next cell.
        let mut wide = cell('中', fg, bg);
        wide.wide = true;
        let snap = PaneSnapshot {
            cells: vec![vec![wide, cell(' ', fg, bg)]],
            cursor: (99, 99),
            cols: 2,
            rows: 1,
            cursor_style: 0,
        };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        drop(r);
        let (w, h, px) = render_offscreen(&snap, Attention::Quiet, cw * 2, ch).expect("render");

        // Count bright (white glyph) pixels in each cell's interior x-range (avoid the edge border).
        let bright_in = |x0: u32, x1: u32| {
            let mut n = 0;
            for y in 2..h.saturating_sub(2) {
                for x in x0..x1 {
                    let p = pixel(&px, w, x, y);
                    if p[0] > 150 && p[1] > 150 && p[2] > 150 {
                        n += 1;
                    }
                }
            }
            n
        };
        let first = bright_in(1, cw);
        let second = bright_in(cw, (cw * 2).saturating_sub(1));
        assert!(first > 0, "wide glyph produced no coverage in its own cell ({first})");
        assert!(second > 0, "wide glyph did not extend into the spacer cell ({second})");
    }
}
