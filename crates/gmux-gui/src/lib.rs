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
            texture: &tex,
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
    Some((px_w, px_h, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_mux::{Cell, Rgb};

    fn cell(ch: char, fg: Rgb, bg: Rgb) -> Cell {
        Cell { ch, fg, bg, bold: false, italic: false, underline: false, inverse: false }
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
    fn renders_glyph_coverage_over_background() {
        let bg = Rgb { r: 0, g: 0, b: 0 };
        let fg = Rgb { r: 255, g: 255, b: 255 };
        let snap = PaneSnapshot { cells: vec![vec![cell('M', fg, bg)]], cursor: (99, 99), cols: 1, rows: 1 };
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            return;
        };
        let (cw, ch) = (r.cell_w(), r.cell_h());
        drop(r);
        let (_w, _h, px) = render_offscreen(&snap, Attention::Quiet, cw, ch).expect("render");
        let bright = px.chunks(4).filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150).count();
        assert!(bright > 5, "'M' glyph produced no bright pixels ({bright})");
    }
}
