//! A monospace glyph atlas. Printable ASCII (32..=126) is rasterized once into a fixed cell grid at
//! the top of a large R8 (coverage) texture; every other codepoint is rasterized on demand through a
//! font fallback chain and packed into shelves below. A hollow-box tile is the last-resort glyph for
//! codepoints no font can draw (or color-only emoji, whose outlines are empty in ab_glyph).
//!
//! The renderer owns the GPU texture + queue and uploads new tiles incrementally: [`Atlas::glyph`]
//! returns a [`GlyphLookup`] that is either already resident or carries the freshly rasterized tile
//! bytes for the caller to `queue.write_texture` before the frame's render pass samples it.

use std::cell::RefCell;
use std::collections::HashMap;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

pub const FIRST_CH: u8 = 32; // space
pub const LAST_CH: u8 = 126; // '~'
const COLS: u32 = 16; // ASCII tiles per atlas row
const ATLAS_SIZE: u32 = 2048; // fixed texture size; dynamic tiles pack into shelves, never grows

/// Result of a glyph lookup. `Blank` = nothing to draw (space/null). `Ready` = the tile is already
/// resident in the texture (ASCII, box fallback, or a previously-cached dynamic glyph). `Upload` =
/// a newly rasterized tile the caller must `queue.write_texture` at `(x, y)` before drawing.
pub enum GlyphLookup {
    Blank,
    Ready { uv: [f32; 4], cells: u8 },
    Upload { uv: [f32; 4], cells: u8, x: u32, y: u32, w: u32, tile: Vec<u8> },
}

/// Dynamic (on-demand) region state, behind a `RefCell` so lookups mutate through `&Atlas`.
struct Dynamic {
    map: HashMap<char, ([f32; 4], u8)>, // char -> (uv, cell width 1|2)
    fallbacks: Vec<FontVec>,            // loaded lazily on first miss
    loaded: bool,
    // Shelf packer: all tiles are `cell_h` tall, so shelves are uniform height.
    next_x: u32,
    next_y: u32,
    full: bool,
    warned: bool,
}

/// Rasterized monospace atlas + cell metrics.
pub struct Atlas {
    /// R8 coverage bitmap for the *initial* region only (ASCII grid + box tile), `width * init_h`
    /// bytes. Dynamic tiles are uploaded incrementally and never mirrored here.
    pub pixels: Vec<u8>,
    pub width: u32,  // texture width (= ATLAS_SIZE); also the UV denominator and `pixels` stride
    pub height: u32, // texture height (= ATLAS_SIZE); UV denominator
    pub init_h: u32, // rows present in `pixels` / uploaded at init
    pub cell_w: u32,
    pub cell_h: u32,
    font: FontVec, // primary font, kept for on-demand rasterization
    px: f32,
    box_uv: [f32; 4], // hollow-box fallback tile
    dynamic: RefCell<Dynamic>,
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

        let width = ATLAS_SIZE;
        let height = ATLAS_SIZE;
        let ascii_rows = ((LAST_CH - FIRST_CH) as u32 + 1).div_ceil(COLS);
        let box_y = (ascii_rows + 1) * cell_h; // one blank gap row between ASCII and the box tile
        let init_h = box_y + cell_h;
        let mut pixels = vec![0u8; (width * init_h) as usize];

        // Bake printable ASCII into the top grid — identical layout/rasterization to the seed atlas,
        // just into a wider (ATLAS_SIZE) buffer, so the sampled coverage is byte-for-byte the same.
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

        // Hollow-box fallback tile at (0, box_y): a 1px outline inset 2px on all sides.
        draw_box(&mut pixels, width, 0, box_y, cell_w, cell_h);
        let box_uv = [
            0.0,
            box_y as f32 / height as f32,
            cell_w as f32 / width as f32,
            (box_y + cell_h) as f32 / height as f32,
        ];

        Some(Atlas {
            pixels,
            width,
            height,
            init_h,
            cell_w,
            cell_h,
            font,
            px,
            box_uv,
            dynamic: RefCell::new(Dynamic {
                map: HashMap::new(),
                fallbacks: Vec::new(),
                loaded: false,
                next_x: 0,
                next_y: init_h, // dynamic shelves start below the ASCII+box region
                full: false,
                warned: false,
            }),
        })
    }

    /// Load the platform monospace font (Consolas on Windows) and build an atlas.
    pub fn system_monospace(px: f32) -> Option<Atlas> {
        for path in [r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\lucon.ttf", r"C:\Windows\Fonts\cour.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Some(a) = Atlas::from_font_bytes(bytes, px) {
                    return Some(a);
                }
            }
        }
        None
    }

    /// The `(u0, v0, u1, v1)` texture coordinates of a *printable ASCII* tile, or `None` otherwise.
    /// Side-effect free — the byte-identical fast path for the common case.
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

    /// UV of the hollow-box fallback tile (exposed for tests / callers that want to detect it).
    pub fn box_tile_uv(&self) -> [f32; 4] {
        self.box_uv
    }

    /// Look up (and rasterize on demand) the tile for `ch`. `wide` selects a 2-cell-wide tile for
    /// East-Asian wide glyphs. ASCII and cached glyphs return `Ready`; a fresh raster returns
    /// `Upload` (caller uploads the bytes); anything unrasterizable returns the box fallback.
    pub fn glyph(&self, ch: char, wide: bool) -> GlyphLookup {
        if ch == ' ' || ch == '\0' {
            return GlyphLookup::Blank;
        }
        if let Some(uv) = self.tile_uv(ch) {
            return GlyphLookup::Ready { uv, cells: 1 };
        }
        let mut d = self.dynamic.borrow_mut();
        if let Some(&(uv, cells)) = d.map.get(&ch) {
            return GlyphLookup::Ready { uv, cells };
        }

        let cells: u8 = if wide { 2 } else { 1 };
        let tile_w = self.cell_w * cells as u32;
        if !d.loaded {
            d.fallbacks = load_fallbacks();
            d.loaded = true;
        }

        // Rasterize through the fallback chain: primary font, then the lazily-loaded fallbacks.
        let tile = raster_tile(&self.font, self.px, ch, tile_w, self.cell_h)
            .or_else(|| d.fallbacks.iter().find_map(|f| raster_tile(f, self.px, ch, tile_w, self.cell_h)));

        let Some(bytes) = tile else {
            // No font drew it (or an empty color-emoji outline): cache + return the hollow box.
            d.map.insert(ch, (self.box_uv, 1));
            return GlyphLookup::Ready { uv: self.box_uv, cells: 1 };
        };

        match d.alloc(tile_w, self.cell_h, self.width, self.height) {
            Some((x, y)) => {
                let uv = [
                    x as f32 / self.width as f32,
                    y as f32 / self.height as f32,
                    (x + tile_w) as f32 / self.width as f32,
                    (y + self.cell_h) as f32 / self.height as f32,
                ];
                d.map.insert(ch, (uv, cells));
                GlyphLookup::Upload { uv, cells, x, y, w: tile_w, tile: bytes }
            }
            None => {
                if !d.warned {
                    eprintln!("gmux: glyph atlas full ({ATLAS_SIZE}x{ATLAS_SIZE}); rendering box fallback");
                    d.warned = true;
                }
                d.map.insert(ch, (self.box_uv, 1));
                GlyphLookup::Ready { uv: self.box_uv, cells: 1 }
            }
        }
    }
}

impl Dynamic {
    /// Shelf-pack a `w x h` tile (h is always `cell_h`, so shelves are uniform). `None` when full.
    fn alloc(&mut self, w: u32, h: u32, atlas_w: u32, atlas_h: u32) -> Option<(u32, u32)> {
        if self.full {
            return None;
        }
        if self.next_x + w > atlas_w {
            self.next_x = 0;
            self.next_y += h;
        }
        if self.next_y + h > atlas_h {
            self.full = true;
            return None;
        }
        let (x, y) = (self.next_x, self.next_y);
        self.next_x += w;
        Some((x, y))
    }
}

/// Draw a hollow box (1px outline inset 2px) into the tile at `(tx, ty)` within a `stride`-wide R8.
fn draw_box(pixels: &mut [u8], stride: u32, tx: u32, ty: u32, cell_w: u32, cell_h: u32) {
    if cell_w < 5 || cell_h < 5 {
        return; // too small for an inset outline; leave blank
    }
    let (x0, x1) = (tx + 2, tx + cell_w - 3);
    let (y0, y1) = (ty + 2, ty + cell_h - 3);
    let mut set = |x: u32, y: u32| pixels[(y * stride + x) as usize] = 255;
    for x in x0..=x1 {
        set(x, y0);
        set(x, y1);
    }
    for y in y0..=y1 {
        set(x0, y);
        set(x1, y);
    }
}

/// Rasterize `ch` from `font` into a fresh `tile_w x cell_h` R8 coverage tile. Returns `None` if the
/// font lacks the glyph (`.notdef`) or the outline is empty (e.g. a color-only emoji) — the caller
/// then tries the next font, and ultimately the box fallback.
fn raster_tile(font: &FontVec, px: f32, ch: char, tile_w: u32, cell_h: u32) -> Option<Vec<u8>> {
    if font.glyph_id(ch).0 == 0 {
        return None;
    }
    let scale = PxScale::from(px);
    let ascent = font.as_scaled(scale).ascent();
    let glyph = font
        .glyph_id(ch)
        .with_scale_and_position(scale, ab_glyph::point(0.0, ascent));
    let outline = font.outline_glyph(glyph)?;
    let bounds = outline.px_bounds();
    let mut tile = vec![0u8; (tile_w * cell_h) as usize];
    let mut any = false;
    outline.draw(|gx, gy, cov| {
        let px_x = bounds.min.x as i32 + gx as i32;
        let px_y = bounds.min.y as i32 + gy as i32;
        if px_x < 0 || px_x >= tile_w as i32 || px_y < 0 || px_y >= cell_h as i32 {
            return;
        }
        let o = (px_y as u32 * tile_w + px_x as u32) as usize;
        let v = (cov * 255.0) as u8;
        if v > tile[o] {
            tile[o] = v;
        }
        any |= v > 0;
    });
    any.then_some(tile)
}

/// Fallback fonts, in priority order after the primary: Segoe UI Symbol (symbols / box-drawing),
/// then CJK (MS Gothic, then Microsoft YaHei). TTCs are opened at collection index 0. Missing files
/// are simply skipped, so the chain degrades to the box fallback on a font-less runner.
fn load_fallbacks() -> Vec<FontVec> {
    let mut v = Vec::new();
    if let Ok(b) = std::fs::read(r"C:\Windows\Fonts\seguisym.ttf") {
        if let Ok(f) = FontVec::try_from_vec(b) {
            v.push(f);
        }
    }
    for path in [r"C:\Windows\Fonts\msgothic.ttc", r"C:\Windows\Fonts\msyh.ttc"] {
        if let Ok(b) = std::fs::read(path) {
            if let Ok(f) = FontVec::try_from_vec_and_index(b, 0) {
                v.push(f);
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load() -> Atlas {
        for path in [r"C:\Windows\Fonts\CascadiaMono.ttf", r"C:\Windows\Fonts\CascadiaCode.ttf"] {
            if let Ok(b) = std::fs::read(path) {
                if let Some(a) = Atlas::from_font_bytes(b, 18.0) {
                    return a;
                }
            }
        }
        Atlas::system_monospace(18.0).expect("a system monospace font")
    }

    #[test]
    fn atlas_builds_and_has_glyph_coverage() {
        let atlas = load();
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
        assert!(atlas.tile_uv(' ').is_some());
    }

    #[test]
    fn cjk_glyph_rasterizes_or_falls_back_to_box() {
        // U+4E2D 中 goes through the fallback chain (CJK font). If the runner has a CJK font it must
        // rasterize a wide (2-cell) tile with real coverage; if not, it must return the box tile.
        let atlas = load();
        match atlas.glyph('中', true) {
            GlyphLookup::Upload { tile, cells, w, .. } => {
                assert_eq!(cells, 2, "wide CJK glyph must occupy two cells");
                assert_eq!(w, atlas.cell_w * 2);
                assert!(tile.iter().any(|&b| b > 0), "CJK tile has no coverage");
            }
            GlyphLookup::Ready { uv, .. } => {
                assert_eq!(uv, atlas.box_tile_uv(), "no CJK font present -> must be the box fallback");
            }
            GlyphLookup::Blank => panic!("CJK char must not be blank"),
        }
    }

    #[test]
    fn box_fallback_tile_and_unmapped_codepoint() {
        let atlas = load();
        // The hollow-box tile must itself be rasterized, else every fallback renders nothing.
        let uv = atlas.box_tile_uv();
        let (x0, y0) = ((uv[0] * atlas.width as f32) as u32, (uv[1] * atlas.height as f32) as u32);
        let mut cov = 0;
        for y in y0..y0 + atlas.cell_h {
            for x in x0..x0 + atlas.cell_w {
                if atlas.pixels[(y * atlas.width + x) as usize] > 0 {
                    cov += 1;
                }
            }
        }
        assert!(cov > 0, "box fallback tile has no coverage");
        // U+FDD0 is a Unicode noncharacter — guaranteed absent from every font's cmap, so it must
        // fall back to the box tile (Ready, one cell).
        match atlas.glyph('\u{FDD0}', false) {
            GlyphLookup::Ready { uv: b, cells } => {
                assert_eq!(b, uv);
                assert_eq!(cells, 1);
            }
            _ => panic!("noncharacter must fall back to the box tile"),
        }
    }

    #[test]
    fn ascii_is_side_effect_free_ready() {
        // ASCII never touches the dynamic region: it returns Ready with the preloaded UV.
        let atlas = load();
        let GlyphLookup::Ready { uv, cells } = atlas.glyph('A', false) else {
            panic!("ASCII must be Ready");
        };
        assert_eq!(cells, 1);
        assert_eq!(uv, atlas.tile_uv('A').unwrap());
    }
}
