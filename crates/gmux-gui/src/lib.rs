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
// Attention is re-exported for the CLI's `gmux screenshot` (render_offscreen takes one).
pub use gmux_mux::Attention;

use gmux_mux::PaneSnapshot;

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

    /// Wrap plain rows as sidebar items (these tests exercise rows, not group headers).
    fn items(rows: Vec<crate::renderer::SidebarRow>) -> Vec<crate::renderer::SidebarItem> {
        rows.into_iter().map(crate::renderer::SidebarItem::Row).collect()
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
        // The attention ring is cmux's notification blue (systemBlue #0a84ff): blue-dominant.
        assert!(
            corner[2] > 150 && corner[2] > corner[0] + 60,
            "expected a blue ring, got {corner:?}"
        );
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
            unread: 0,
            color: None,
            busy: false,
            dragging: false,
            pr: None,
            active: true,
            hover: false,
            progress: None,
            progress_error: false,
        }];
        r.render_frame(&view, &items(rows), sw, &[], w, h, "", false, None, None, None, None, None, None);
        let px = read_rgba(&r, &tex, w, h).expect("readback");
        // The active row is a solid accent pill (cmux blue #0091ff) inset ROW_OUTER_PAD from the
        // panel edge. x=1 is outside the pill entirely (neutral panel gray); the centre is accent.
        let outside = pixel(&px, w, 1, rows_y0 + row_h / 2);
        let center = pixel(&px, w, sw / 2, rows_y0 + row_h / 2);
        assert!(
            outside[2] < 60 && outside[2].abs_diff(outside[0]) < 12,
            "left of the pill should be the neutral panel, got {outside:?}"
        );
        assert!(
            center[2] > 150 && center[2] > center[0] + 80,
            "active-row centre should be the solid accent fill, got {center:?}"
        );
    }

    #[test]
    fn hit_test_walks_mixed_item_heights() {
        // A 24px header followed by two 48px rows: every hit must land on the item actually under
        // the cursor, and the gaps between items must hit nothing.
        use crate::renderer::{GroupHeader, SidebarItem, SidebarRow};
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let row = || SidebarRow {
            name: "w".into(),
            branch: None,
            attention: false,
            unread: 0,
            color: None,
            busy: false,
            dragging: false,
            pr: None,
            active: false,
            hover: false,
            progress: None,
            progress_error: false,
        };
        let items = vec![
            SidebarItem::Header(GroupHeader {
                name: "api".into(),
                collapsed: false,
                members: 2,
                unread: 0,
                hover: false,
            }),
            SidebarItem::Row(row()),
            SidebarItem::Row(row()),
        ];
        let heights = Renderer::sidebar_item_heights(&items);
        assert_eq!(heights, vec![24.0, 48.0, 48.0]);
        let top = 24.0 + r.cell_h() as f32; // SIDEBAR_PAD_TOP(16) + cell_h + 8
        assert_eq!(r.sidebar_item_at(top - 1.0, &heights), None, "above the list");
        assert_eq!(r.sidebar_item_at(top + 1.0, &heights), Some(0), "header");
        assert_eq!(r.sidebar_item_at(top + 23.0, &heights), Some(0), "header, last pixel");
        assert_eq!(r.sidebar_item_at(top + 25.0, &heights), None, "gap under the header");
        assert_eq!(r.sidebar_item_at(top + 30.0, &heights), Some(1), "first row");
        assert_eq!(r.sidebar_item_at(top + 80.0, &heights), Some(2), "second row");
        assert_eq!(r.sidebar_item_at(top + 400.0, &heights), None, "past the list");
        // '+ new tab' sits directly after the last item, never overlapping it.
        assert!(!r.sidebar_new_tab_at(top + 80.0, &heights));
        assert!(r.sidebar_new_tab_at(top + 24.0 + 4.0 + 2.0 * 52.0 + 1.0, &heights));
    }

    #[test]
    fn close_button_hit_is_where_it_is_drawn() {
        // The hover close button sits at the row's right edge on the first text line; clicks
        // elsewhere in the row must fall through to selecting the workspace.
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let sw = r.sidebar_width();
        let (cw, ch) = (r.cell_w() as f32, r.cell_h() as f32);
        let top = 100.0;
        let line1 = top + 8.0; // ROW_PAD_V
        let right = sw as f32 - 6.0 - 10.0; // ROW_OUTER_PAD + ROW_PAD_H
        assert!(r.close_button_hit(right - cw / 2.0, line1 + ch / 2.0, top, sw), "centre hits");
        assert!(!r.close_button_hit(right - 3.0 * cw, line1 + ch / 2.0, top, sw), "left of it misses");
        assert!(!r.close_button_hit(right - cw / 2.0, line1 + ch + 8.0, top, sw), "second line misses");
        assert!(!r.close_button_hit(right - cw / 2.0, top - 6.0, top, sw), "above the row misses");
    }

    #[test]
    fn pr_chip_hit_matches_where_it_is_drawn() {
        // The clickable chip must be exactly the drawn chip: on the row's SECOND line, starting at
        // the text column, and shifted right when a color rail is present.
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w() as f32, r.cell_h() as f32);
        let top = 100.0;
        let line2 = top + 8.0 + ch; // ROW_PAD_V + one line
        let x0 = 6.0 + 10.0; // ROW_OUTER_PAD + ROW_PAD_H
        // "#42" is 3 cells wide plus 5px padding each side.
        let mid = x0 + (3.0 * cw + 10.0) / 2.0;
        assert!(r.pr_chip_hit(mid, line2 + ch / 2.0, top, false, 42), "centre of the chip hits");
        assert!(!r.pr_chip_hit(x0 - 4.0, line2 + ch / 2.0, top, false, 42), "left of the chip misses");
        assert!(!r.pr_chip_hit(mid, top + 2.0, top, false, 42), "the first line is not the chip");
        assert!(
            !r.pr_chip_hit(x0 + 3.0 * cw + 30.0, line2 + ch / 2.0, top, false, 42),
            "right of the chip misses"
        );
        // With a color rail the chip shifts right by the rail width + inset, so the old x misses.
        assert!(!r.pr_chip_hit(x0 + 1.0, line2 + ch / 2.0, top, true, 42));
        assert!(r.pr_chip_hit(mid + 7.0, line2 + ch / 2.0, top, true, 42));
        // A wider number makes a wider chip.
        assert!(r.pr_chip_hit(x0 + 5.0 * cw, line2 + ch / 2.0, top, false, 12345));
        assert!(!r.pr_chip_hit(x0 + 5.0 * cw, line2 + ch / 2.0, top, false, 4));
    }

    #[test]
    fn sidebar_panel_is_top_lit() {
        // The chrome gradients run token-color-at-top to darker-at-bottom. Sample the panel's own
        // column (x=2, left of any row content) near the top and near the bottom.
        use crate::renderer::SidebarRow;
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let sw = r.sidebar_width();
        let (w, h) = (sw, 400u32);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-gradient-test"),
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
            unread: 0,
            color: None,
            busy: false,
            dragging: false,
            pr: None,
            active: false,
            hover: false,
            progress: None,
            progress_error: false,
        }];
        r.render_frame(&view, &items(rows), sw, &[], w, h, "", false, None, None, None, None, None, None);
        let px = read_rgba(&r, &tex, w, h).expect("readback");
        let top = pixel(&px, w, 2, 2);
        let bottom = pixel(&px, w, 2, h - 3);
        assert!(
            top[0] > bottom[0] + 3,
            "sidebar should fade downward: top {top:?} vs bottom {bottom:?}"
        );
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
            show_close: false,
            drop_target: false,
            dragging: false,
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
            show_close: false,
            drop_target: false,
            dragging: false,
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
        let sb = SearchBar { label: "find:".into(), query: "foo".into(), current: 1, total: 5, overlay_only: false };
        r.render_frame(&view, &[], 0, &[pv()], w, h, "", false, None, None, Some(&sb), None, None, None);
        let with = white_in_band(&read_rgba(&r, &tex, w, h).expect("readback"));
        r.render_frame(&view, &[], 0, &[pv()], w, h, "", false, None, None, None, None, None, None);
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

    /// Not a test: dumps a representative full-chrome frame to `chrome_preview.ppm` for
    /// eyeballing theme changes headlessly (WDAC blocks fresh example binaries on the dev
    /// machine; this rides the always-runnable test binary). Run explicitly:
    /// `cargo test -p gmux-gui --lib dump_chrome_preview -- --ignored`.
    #[test]
    #[ignore = "artifact dump, not an assertion; run explicitly"]
    fn dump_chrome_preview() {
        use crate::renderer::{PaneView, SearchBar, SidebarRow};
        use gmux_mux::{PaneSnapshot, Rect};
        let Some(mut r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (w, h) = (960u32, 600u32);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("chrome-preview"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let rows = vec![
            SidebarRow {
                name: "backend".into(),
                branch: Some("main".into()),
                attention: false,
                unread: 0,
                color: Some("#e0533d".into()),
                busy: true,
                dragging: false,
                pr: None,
                active: true,
                hover: false,
                progress: Some(42),
                progress_error: false,
            },
            SidebarRow {
                name: "web".into(),
                branch: Some("feat/ui".into()),
                attention: true,
                unread: 3,
                color: None,
                busy: false,
                dragging: false,
                pr: Some((128, "open".into())),
                active: false,
                hover: false,
                progress: None,
                progress_error: false,
            },
            SidebarRow {
                name: "agents".into(),
                branch: None,
                attention: false,
                unread: 0,
                color: Some("#3d7de0".into()),
                busy: false,
                dragging: false,
                pr: None,
                active: false,
                hover: true,
                progress: None,
                progress_error: false,
            },
        ];
        let mk = |ch: char| {
            cell(ch, Rgb { r: 0xcc, g: 0xcc, b: 0xcc }, Rgb { r: 0x11, g: 0x11, b: 0x11 })
        };
        let mut cells = Vec::new();
        for line in [
            "PS C:\\work> cargo build",
            "   Compiling gmux v0.1.0",
            "warning: unused variable",
            "PS C:\\work> _",
        ] {
            let mut row: Vec<Cell> = line.chars().map(mk).collect();
            row.resize(70, mk(' '));
            cells.push(row);
        }
        cells.resize(24, vec![mk(' '); 70]);
        let snap = PaneSnapshot { cells, cursor: (12, 3), cursor_style: 0, cols: 70, rows: 24 };
        // Fold the last two rows under a collapsible group header, so the preview covers both
        // sidebar item kinds.
        let mut list: Vec<crate::renderer::SidebarItem> = Vec::new();
        let mut rows = rows.into_iter();
        list.push(crate::renderer::SidebarItem::Row(rows.next().unwrap()));
        list.push(crate::renderer::SidebarItem::Header(crate::renderer::GroupHeader {
            name: "frontend".into(),
            collapsed: false,
            members: 2,
            unread: 3,
            hover: false,
        }));
        list.extend(rows.map(crate::renderer::SidebarItem::Row));
        let rows = list;
        let sw = r.sidebar_width();
        let pane = PaneView {
            snap: &snap,
            attention: Attention::Quiet,
            active: true,
            rect: Rect { x: sw, y: 0, w: w - sw, h },
            scrolled: 0,
            history: 120,
            title: "powershell — build".into(),
            selection: None,
            show_close: true,
            drop_target: false,
            dragging: false,
        };
        let sb = SearchBar {
            label: "find:".into(),
            query: "warn".into(),
            current: 1,
            total: 3,
            overlay_only: false,
        };
        r.advance_spinner(); // step off frame 0 so a lit spoke shows in the busy row
        // Drop indicator above the last item, as a reorder drag would show it.
        let drop_at = Some(rows.len().saturating_sub(1));
        // Settings panel over the frame, as Ctrl+, shows it.
        let sv = crate::renderer::SettingsView {
            tabs: vec!["theme".into(), "keys".into(), "schemes".into()],
            tab: 2,
            rows: crate::config::preset_names()
                .into_iter()
                .map(|name| crate::renderer::SettingsRow {
                    label: name.into(),
                    value: if name == "nord" { "in use" } else { "" }.into(),
                    swatch: crate::config::preset_swatch(name)
                        .into_iter()
                        .map(|[r, g, b]| gmux_mux::Rgb { r, g, b })
                        .collect(),
                })
                .collect(),
            selected: 4,
            footer: "click or arrow to try one on  ·  enter keeps it  ·  esc restores".into(),
        };
        r.render_frame(&view, &rows, sw, &[pane], w, h, "", false, drop_at, Some("ag"), Some(&sb), None, None, Some(&sv));
        let px = read_rgba(&r, &tex, w, h).expect("readback");
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for p in px.chunks(4) {
            out.extend_from_slice(&p[..3]);
        }
        std::fs::write("chrome_preview.ppm", out).expect("write");
    }
}

