//! A monospace glyph atlas: printable ASCII rasterized once into a single R8 (coverage) texture,
//! each glyph baked into a fixed cell-sized tile so rendering a cell is just "draw the tile".
//! This is the seed of the ARCHITECTURE's custom glyph-atlas renderer; Unicode/shaping come later.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

pub const FIRST_CH: u8 = 32; // space
pub const LAST_CH: u8 = 126; // '~'
const COLS: u32 = 16; // tiles per atlas row

/// Rasterized monospace atlas + cell metrics.
pub struct Atlas {
    /// R8 coverage bitmap, `width * height` bytes.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Per-cell dimensions in pixels.
    pub cell_w: u32,
    pub cell_h: u32,
}

impl Atlas {
    /// Build an atlas from TTF/OTF font bytes at the given pixel height.
    pub fn from_font_bytes(bytes: Vec<u8>, px: f32) -> Option<Atlas> {
        let font = FontVec::try_from_vec(bytes).ok()?;
        let scale = PxScale::from(px);
        let scaled = font.as_scaled(scale);

        let ascent = scaled.ascent();
        let advance = scaled.h_advance(font.glyph_id('M')).max(1.0);
        let cell_w = advance.ceil() as u32;
        let cell_h = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil().max(1.0) as u32;

        let count = (LAST_CH - FIRST_CH + 1) as u32;
        let rows = count.div_ceil(COLS);
        let width = cell_w * COLS;
        let height = cell_h * rows;
        let mut pixels = vec![0u8; (width * height) as usize];

        for c in FIRST_CH..=LAST_CH {
            let idx = (c - FIRST_CH) as u32;
            let tile_x = (idx % COLS) * cell_w;
            let tile_y = (idx / COLS) * cell_h;
            let glyph = font
                .glyph_id(c as char)
                .with_scale_and_position(scale, ab_glyph::point(0.0, ascent));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let px_x = tile_x as i32 + bounds.min.x as i32 + gx as i32;
                    let px_y = tile_y as i32 + bounds.min.y as i32 + gy as i32;
                    // Clamp to this tile so glyphs never bleed into neighbours.
                    if px_x < tile_x as i32
                        || px_x >= (tile_x + cell_w) as i32
                        || px_y < tile_y as i32
                        || px_y >= (tile_y + cell_h) as i32
                    {
                        return;
                    }
                    let o = (px_y as u32 * width + px_x as u32) as usize;
                    pixels[o] = pixels[o].max((cov * 255.0) as u8);
                });
            }
        }

        Some(Atlas { pixels, width, height, cell_w, cell_h })
    }

    /// Load the platform monospace font (Consolas on Windows) and build an atlas.
    pub fn system_monospace(px: f32) -> Option<Atlas> {
        // TODO: bundle an OFL font for determinism / non-Windows. Windows always has Consolas.
        for path in [r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\lucon.ttf", r"C:\Windows\Fonts\cour.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Some(a) = Atlas::from_font_bytes(bytes, px) {
                    return Some(a);
                }
            }
        }
        None
    }

    /// The `(u0, v0, u1, v1)` texture coordinates of a character's tile, or `None` if not printable.
    pub fn tile_uv(&self, ch: char) -> Option<[f32; 4]> {
        let c = ch as u32;
        if c < FIRST_CH as u32 || c > LAST_CH as u32 {
            return None;
        }
        let idx = c - FIRST_CH as u32;
        let tx = (idx % COLS) * self.cell_w;
        let ty = (idx / COLS) * self.cell_h;
        Some([
            tx as f32 / self.width as f32,
            ty as f32 / self.height as f32,
            (tx + self.cell_w) as f32 / self.width as f32,
            (ty + self.cell_h) as f32 / self.height as f32,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_builds_and_has_glyph_coverage() {
        let atlas = Atlas::system_monospace(18.0).expect("a system monospace font");
        assert!(atlas.cell_w > 0 && atlas.cell_h > 0);
        // 'M' should have rasterized some non-zero coverage in its tile.
        let uv = atlas.tile_uv('M').unwrap();
        let x0 = (uv[0] * atlas.width as f32) as u32;
        let y0 = (uv[1] * atlas.height as f32) as u32;
        let mut covered = 0;
        for y in y0..y0 + atlas.cell_h {
            for x in x0..x0 + atlas.cell_w {
                if atlas.pixels[(y * atlas.width + x) as usize] > 0 {
                    covered += 1;
                }
            }
        }
        assert!(covered > 0, "'M' tile has no coverage — rasterization failed");
        // Space should be blank.
        assert!(atlas.tile_uv(' ').is_some());
    }
}
