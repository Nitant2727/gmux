//! wgpu renderer: draws a [`PaneSnapshot`] as background cell quads + glyph quads (from the
//! [`Atlas`]) + a block cursor + an attention ring. Two pipelines (opaque bg, alpha-blended
//! glyphs). Vertex buffers are rebuilt per frame (damage tracking is a later optimization).

use bytemuck::{Pod, Zeroable};
use gmux_mux::{Attention, Cell, PaneSnapshot, Rect, Rgb};
use wgpu::util::DeviceExt;

use crate::atlas::{Atlas, GlyphLookup};
use crate::config::AccentChoice;

// Design tokens (single source of truth).
// Fluent dark (WinUI layer/accent tokens): neutral gray layers, cyan accent, semantic status hues.
// The accent is a runtime value ([`accent`]) — WinUI apps follow the user's Windows accent color.
// Near-black neutrals: the chrome should sit UNDER the terminal, not beside it. Each layer is a
// small step up from the one behind it (app < sidebar < pane), which is enough separation on an
// OLED-ish dark surface without the mid-grey haze the Fluent defaults had.
const BG_APP: Rgb = Rgb { r: 0x0b, g: 0x0b, b: 0x0d }; // window / between-pane background
const BG_SIDEBAR: Rgb = Rgb { r: 0x11, g: 0x11, b: 0x14 };
const BG_PANE: Rgb = Rgb { r: 0x15, g: 0x15, b: 0x18 }; // pane fill + letterbox
const SIDEBAR_ROW_ACTIVE: Rgb = Rgb { r: 0x22, g: 0x22, b: 0x26 };
const SIDEBAR_ROW_HOVER: Rgb = Rgb { r: 0x1a, g: 0x1a, b: 0x1e }; // between BG_SIDEBAR and active
// cmux's accent (its `cmuxAccentNSColor` for the dark scheme: rgb 0,145,255). gmux follows it by
// default so the two apps look like the same product; `"accent": "system"` opts into the Windows
// accent instead, and any hex pins a custom one.
// Softened from cmux's rgb(0,145,255): against near-black neutrals the pure cyan-blue glares,
// especially as a solid row fill you look at all day. Same hue family, less saturation burn.
const ACCENT_FALLBACK: Rgb = Rgb { r: 0x3b, g: 0x8a, b: 0xe6 };
// Not pure white on near-black — 0xff on 0x0b is harsh at a terminal's text density.
const TEXT: Rgb = Rgb { r: 0xe8, g: 0xe8, b: 0xec };
const TEXT_DIM: Rgb = Rgb { r: 0x7e, g: 0x80, b: 0x88 };
// cmux rings a pane that wants you in systemBlue, not a warning color — attention there means
// "this agent has news", not "this is broken". Matching it (macOS dark systemBlue).
const ATTENTION: Rgb = Rgb { r: 0x4d, g: 0x9a, b: 0xf0 }; // attention dot / ring
const PEACH: Rgb = Rgb { r: 0xe3, g: 0xb3, b: 0x41 }; // search-match highlight
const PROGRESS: Rgb = Rgb { r: 0x5f, g: 0xb0, b: 0x71 };
const ERROR: Rgb = Rgb { r: 0xe0, g: 0x7b, b: 0x86 };
// Barely-there border: on near-black, a light-grey outline is the loudest thing on screen.
const PANE_BORDER_INACTIVE: Rgb = Rgb { r: 0x24, g: 0x24, b: 0x29 };
const CURSOR: Rgb = Rgb { r: 0xc8, g: 0xc8, b: 0xd0 };

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
// cmux's sidebar metrics (SidebarWorkspaceListMetrics + the row's own padding): 10px inside the
// row, 6px between the row and the panel edge, 8px above/below the text block.
const ROW_PAD_H: f32 = 10.0; // horizontal padding inside a sidebar row
const ROW_OUTER_PAD: f32 = 6.0; // gap between the row pill and the panel edges
const ROW_PAD_V: f32 = 8.0; // padding above the first text line / below the last
const ROW_STROKE: f32 = 1.5; // hairline around the selected row
// cmux's group header is a short strip above its members (`dropTargetHeight` bottoms out at 24).
const HEADER_H: f32 = 24.0;
const ATTN_DOT: f32 = 8.0;
// cmux's unread badge padding (`baseUnreadHorizontalPadding` 5 / `baseUnreadVerticalPadding` 1).
const BADGE_PAD_H: f32 = 5.0;
const BADGE_PAD_V: f32 = 1.0;
const RADIUS: f32 = 6.0; // rounded corner radius for sidebar rows + pane chrome
const GLOW_W: f32 = 4.0; // accent focus glow around the active pane (must stay < MARGIN)
const PROGRESS_RAIL: f32 = 3.0; // progress bar height along a sidebar row's bottom edge
const DROP_LINE: f32 = 2.0; // reorder drop indicator, drawn at the target item's top edge
const STATUS_DOT: f32 = 8.0; // leading activity dot: filled = busy, ring = idle
const STATUS_RING: f32 = 1.5; // ring thickness of the idle (hollow) dot
const STATUS_GAP: f32 = 6.0; // space between the dot and the workspace name
const DRAG_FADE: f32 = 0.45; // how much of the dragged row's ink survives while it is in flight
// cmux's leading rail for a color-tagged workspace: a 3px capsule inset 4px, 5px in from the ends.
const COLOR_RAIL_W: f32 = 3.0;
const COLOR_RAIL_INSET: f32 = 4.0;
// Activity spinner: cmux spins 8 spokes on a 0.8s cycle, i.e. one spoke step every 100ms.
const SPINNER_SPOKES: u32 = 8;
pub const SPINNER_STEP_MS: u64 = 100;
const SPINNER_R: f32 = 5.0; // ring radius
const SPINNER_DOT: f32 = 2.5; // spoke dot diameter
const BADGE_RADIUS: f32 = 4.0; // scroll badge chip
const TITLE_STRIP: f32 = 22.0; // pane title band inside the border, above the cells
const SEARCH_BAR: f32 = 22.0; // search band inside the border, below the cells (active pane only)
const SCROLLBAR_W: f32 = 8.0; // scrollback scrollbar strip at the cell-area right edge (active pane only)

const fn clear_of(c: Rgb) -> wgpu::Color {
    wgpu::Color { r: c.r as f64 / 255.0, g: c.g as f64 / 255.0, b: c.b as f64 / 255.0, a: 1.0 }
}
const DEFAULT_CLEAR: wgpu::Color = clear_of(BG_APP);

/// CPU-side alpha blend `fg` over `bg` (the bg pipeline is opaque, so the cursor is pre-mixed).
fn blend(fg: Rgb, bg: Rgb, a: f32) -> Rgb {
    let m = |f: u8, b: u8| ((f as f32 * a) + (b as f32 * (1.0 - a))).round() as u8;
    Rgb { r: m(fg.r, bg.r), g: m(fg.g, bg.g), b: m(fg.b, bg.b) }
}

/// Fluent surfaces are lit from above: every gradient keeps the token color at the TOP and falls
/// off toward the bottom (never brightens past the token, so contrast floors stay where they were).
const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
fn darker(c: Rgb, a: f32) -> Rgb {
    blend(BLACK, c, a)
}

/// The live accent color, packed `0x01_RR_GG_BB` (bit 24 marks "resolved"; 0 means "not yet").
/// Read on every frame build, so it stays an atomic rather than a lock.
static ACCENT_RGB: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The accent used for the selected tab fill, active borders, highlights and chips. Defaults to
/// [`ACCENT_FALLBACK`]; [`set_accent`] swaps in a pinned or system-derived color.
pub fn accent() -> Rgb {
    use std::sync::atomic::Ordering;
    let packed = ACCENT_RGB.load(Ordering::Relaxed);
    if packed == 0 {
        return ACCENT_FALLBACK;
    }
    unpack_accent(packed)
}

/// Apply the config's accent choice: the built-in default, the Windows accent (lifted if it is too
/// dark to read against), or a pinned color. Called at startup and on config hot-reload.
pub fn set_accent(choice: AccentChoice) {
    use std::sync::atomic::Ordering;
    let color = match choice {
        AccentChoice::Default => None,
        AccentChoice::Fixed([r, g, b]) => Some(Rgb { r, g, b }),
        AccentChoice::System => system_accent().map(ensure_legible),
    };
    let packed = match color {
        Some(c) => 1 << 24 | (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32,
        None => 0,
    };
    ACCENT_RGB.store(packed, Ordering::Relaxed);
}

/// The colour a choice would produce, without applying it — what the accent picker paints each
/// option's swatch with, including the legibility lift `"system"` gets.
pub fn resolve_accent(choice: AccentChoice) -> Rgb {
    match choice {
        AccentChoice::Default => ACCENT_FALLBACK,
        AccentChoice::Fixed([r, g, b]) => Rgb { r, g, b },
        AccentChoice::System => system_accent().map(ensure_legible).unwrap_or(ACCENT_FALLBACK),
    }
}

fn unpack_accent(p: u32) -> Rgb {
    Rgb { r: (p >> 16) as u8, g: (p >> 8) as u8, b: p as u8 }
}

/// Windows stores eight accent shades in `HKCU\...\Explorer\Accent\AccentPalette` as RGBA quads,
/// dark to light. Entry 4 is `SystemAccentColorLight2` — the shade WinUI uses on dark surfaces.
fn accent_from_palette(bytes: &[u8]) -> Option<Rgb> {
    let q = bytes.get(16..19)?;
    Some(Rgb { r: q[0], g: q[1], b: q[2] })
}

/// A user's accent can be near-black (some themes), which would erase every focus cue on our dark
/// chrome. Lift it toward white until it clears a relative-luminance floor.
fn ensure_legible(c: Rgb) -> Rgb {
    let lum = |c: Rgb| {
        (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0
    };
    let mut out = c;
    // Each step is a fixed blend toward white; 6 steps take even #000000 past the floor.
    for _ in 0..6 {
        if lum(out) >= 0.45 {
            break;
        }
        out = blend(TEXT, out, 0.2);
    }
    out
}

/// Read the accent palette out of the registry. `None` on any failure (no key, wrong type, short
/// buffer) — the caller falls back to the built-in accent.
#[cfg(windows)]
fn system_accent() -> Option<Rgb> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_BINARY,
    };
    let mut buf = [0u8; 32];
    let mut len = buf.len() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent"),
            w!("AccentPalette"),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
    };
    if rc.is_err() {
        return None;
    }
    accent_from_palette(&buf[..len as usize])
}

#[cfg(not(windows))]
fn system_accent() -> Option<Rgb> {
    None
}

/// Border width + color for a pane: attention ring overrides the active/inactive border.
fn border_style(active: bool, attention: Attention) -> (f32, Rgb) {
    if attention.is_pending() {
        (ATTN_BORDER, ATTENTION)
    } else if active {
        (BORDER, accent())
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
    /// Total scrollback depth available (lines above the live screen). With `scrolled` it sizes and
    /// positions the scrollbar thumb; 0 = no scrollback, so no scrollbar is drawn.
    pub history: u32,
    /// Title shown in the pane's title strip (daemon-provided; short cwd / pane name).
    pub title: String,
    /// Selected cell range `((start_col,start_row),(end_col,end_row))`, normalized start<=end in
    /// reading order, in viewport cell coords. Those cells get fg/bg swapped + an ACCENT tint.
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// Draw a close button in this pane's title strip (the active pane, or the hovered one) —
    /// always-on close buttons in every pane of a busy split are visual noise.
    pub show_close: bool,
    /// A pane rearrange is hovering this pane: it would receive the dragged pane on release.
    pub drop_target: bool,
    /// This pane is the one being dragged.
    pub dragging: bool,
}

/// One workspace (window/tab) row in the sidebar.
pub struct SidebarRow {
    pub name: String,
    pub branch: Option<String>,
    pub attention: bool,
    /// Unread notifications in this workspace; `> 0` renders a count badge in place of the dot.
    pub unread: u32,
    /// User tag color (`#rrggbb`): a leading rail on the row, brightened for the dark sidebar.
    pub color: Option<String>,
    /// A pane here has running children — spins the activity indicator.
    pub busy: bool,
    /// This row is the one being dragged: drawn faded, so the drop indicator reads as "where it
    /// will land" rather than "where a second copy appears".
    pub dragging: bool,
    /// A pull-request badge: `(number, status)` where status is `open`/`draft`/`merged`/`closed`.
    pub pr: Option<(u32, String)>,
    pub active: bool,
    /// Cursor is hovering this row: draws a subtle hover fill (ignored when `active`).
    pub hover: bool,
    /// Aggregate agent progress: `Some(pct)` renders " 42%" after the name.
    pub progress: Option<u8>,
    /// A pane reported a progress error: renders " !" after the name (takes precedence over pct).
    pub progress_error: bool,
}

/// One line in the sidebar list: a collapsible group header, or a workspace row. The app builds
/// this list (grouping + collapse state live there); the renderer just lays it out top to bottom,
/// which keeps hit-testing and drawing reading off the same sequence.
pub enum SidebarItem {
    Header(GroupHeader),
    Row(SidebarRow),
}

impl SidebarItem {
    /// Laid-out height of this item (headers are shorter than workspace rows).
    fn height(&self) -> f32 {
        match self {
            SidebarItem::Header(_) => HEADER_H,
            SidebarItem::Row(_) => ROW_H,
        }
    }
}

/// A collapsible group header above its member workspaces.
pub struct GroupHeader {
    pub name: String,
    /// Members are hidden; the chevron points right instead of down.
    pub collapsed: bool,
    /// How many workspaces are in the group (shown when collapsed, where the rows can't speak).
    pub members: usize,
    /// Unread notifications across the group — badged like a row, so a collapsed group still says
    /// that something inside it wants you.
    pub unread: u32,
    pub hover: bool,
}

/// The command palette overlay: a centered panel with a query line and filtered rows.
pub struct PaletteView {
    pub query: String,
    /// Visible rows: `(label, right-aligned hint)` — the app pre-filters and pre-truncates.
    pub items: Vec<(String, String)>,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
}

/// Inner padding of the settings card, and the gap between tab labels. Named because the
/// hit-tests below have to agree with the drawing to the pixel.
const SET_PAD: f32 = 14.0;
const SET_TAB_GAP: f32 = 22.0;
/// Side of one colour chip in a scheme ribbon.
const SET_CHIP: f32 = 11.0;
/// Width of the settings card's scrollbar, and the gutter the rows give up for it.
const SET_BAR: f32 = 4.0;
/// Where the settings card's top edge sits, as a fraction of the window height.
const SET_TOP: f32 = 0.15;

/// The settings card's laid-out rect plus its row metrics.
struct Card {
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    /// Height of one row.
    row_h: f32,
    /// Height of the tab strip above the rows.
    head: f32,
}

/// Lay the settings card out for `rows` rows in a `fw` x `fh` window. The single source of the
/// card's geometry: `render_frame` draws with it and the `settings_*_at` hit-tests read it, so a
/// click can't land on a row the panel drew somewhere else.
fn settings_card(rows: usize, ch_cell: f32, fw: f32, fh: f32) -> Card {
    const SET_W: f32 = 640.0;
    let row_h = ch_cell + 8.0;
    let pw = SET_W.min(fw - 24.0).max(160.0);
    let head = row_h + 10.0; // tab strip
    let ph = (SET_PAD * 2.0 + head + row_h * (rows as f32 + 1.0)).min(fh - 32.0);
    // The top edge is PINNED, not centred: the card's height follows its row count, so centring it
    // made the whole panel jump up and down as you switched between a five-row tab and a twelve-row
    // one. Held at a fixed fraction of the window, clamped so a tall card still fits.
    let top = (fh * SET_TOP).max(8.0);
    Card {
        px: ((fw - pw) / 2.0).max(0.0),
        py: top.min((fh - ph - 8.0).max(8.0)),
        pw,
        ph,
        row_h,
        head,
    }
}

/// The settings row under `(x, y)`, or `None` in the tab strip, the footer, or the margins.
fn settings_row_index(x: f32, y: f32, rows: usize, ch_cell: f32, fw: f32, fh: f32) -> Option<usize> {
    let c = settings_card(rows, ch_cell, fw, fh);
    if x < c.px || x >= c.px + c.pw {
        return None;
    }
    let rows_y = c.py + SET_PAD + c.head;
    let i = ((y - rows_y) / c.row_h).floor();
    if i < 0.0 {
        return None;
    }
    let i = i as usize;
    // The drawing stops a row short of the footer; a click past that hits nothing.
    let ry = rows_y + c.row_h * i as f32;
    (i < rows && ry + c.row_h <= c.py + c.ph - c.row_h).then_some(i)
}

/// The tab under `(x, y)`, walking the same pill/gap sequence the strip is drawn with.
#[allow(clippy::too_many_arguments)]
fn settings_tab_index(x: f32, y: f32, tabs: &[String], rows: usize, cw_cell: f32, ch_cell: f32, fw: f32, fh: f32) -> Option<usize> {
    let c = settings_card(rows, ch_cell, fw, fh);
    let (y0, y1) = (c.py + SET_PAD - 2.0, c.py + SET_PAD + ch_cell + 4.0);
    if y < y0 || y >= y1 {
        return None;
    }
    let mut tx = c.px + SET_PAD;
    for (i, name) in tabs.iter().enumerate() {
        let tw = name.chars().count() as f32 * cw_cell;
        if x >= tx - 6.0 && x < tx + tw + 6.0 {
            return Some(i);
        }
        tx += tw + SET_TAB_GAP;
    }
    None
}

/// Cut `text` to at most `max` characters, at a word boundary, marking the cut with `…`. A hint
/// line that stopped mid-word (`"esc can"`) reads as a bug rather than as "there is more". Text
/// that already fits is returned unchanged. Pure/tested.
fn clip_words(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return "…".repeat(max);
    }
    let head: String = text.chars().take(max - 1).collect();
    // Back up to the last space so the ellipsis follows a whole word; if the first word alone is
    // longer than the line, cut it rather than returning nothing at all.
    let cut = head.rfind(' ').map_or(head.as_str(), |i| head[..i].trim_end());
    format!("{}…", if cut.is_empty() { head.as_str() } else { cut })
}

/// A scrollbar thumb as `(offset from the track's top, height)`: the window's share of the list,
/// placed at the window's share of the way through it. The height is floored so the thumb stays
/// grabbable-looking on a long list, and the floor is taken out of the travel rather than allowed
/// to push the thumb past the track's end. Pure/tested.
fn scroll_thumb(total: usize, visible: usize, offset: usize, track: f32) -> (f32, f32) {
    if total <= visible || visible == 0 {
        return (0.0, track); // nothing hidden: a full-length thumb, no travel
    }
    let th = (track * visible as f32 / total as f32).max(SET_BAR * 3.0).min(track);
    let at = (offset as f32 / (total - visible) as f32).clamp(0.0, 1.0);
    ((track - th) * at, th)
}

/// One settings row: a label on the left, a value on the right, and an optional colour ribbon
/// previewing what the value means (a terminal scheme's background, six ANSI colours, foreground).
#[derive(Default)]
pub struct SettingsRow {
    pub label: String,
    pub value: String,
    /// Colours to draw as a ribbon at the row's right edge. Empty = a plain value row.
    pub swatch: Vec<Rgb>,
    /// Draw the value as a problem (a chord two actions both claim), not as a setting.
    pub warn: bool,
}

/// The settings overlay (Ctrl+,): a centered panel with a tab strip and a list of
/// [`SettingsRow`]s. The app owns what the rows mean and what Enter does to them; the
/// renderer only lays them out.
pub struct SettingsView {
    pub tabs: Vec<String>,
    /// Index into `tabs` of the open section.
    pub tab: usize,
    pub rows: Vec<SettingsRow>,
    /// Index into `rows` of the highlighted row.
    pub selected: usize,
    /// Hint line along the bottom (changes while capturing a chord).
    pub footer: String,
    /// The row filter, when open. Drawn at the tab strip's right end with a caret.
    pub query: Option<String>,
    /// How many rows the open tab has in total, and which one `rows[0]` is — the app windows the
    /// list, so the card alone can't tell how much of it is off-screen. `total > rows.len()` is
    /// what puts a scrollbar on the card.
    pub total: usize,
    pub offset: usize,
}

/// The active pane's search overlay: a band drawn at the pane bottom. `current`/`total` are shown
/// as-is (app.rs owns their semantics); `total == 0` with a non-empty `query` renders "no matches".
/// Also reused as a generic prompt band (close confirmation): a custom `label`, empty query, and
/// `total == 0` renders just the label with no caret/counter.
pub struct SearchBar {
    /// Dim prefix label — "find:" for search, a full sentence for confirmations.
    pub label: String,
    pub query: String,
    pub current: usize,
    pub total: usize,
    /// `true` draws the band OVER the bottom cell row without shrinking the viewport — for
    /// transient surfaces (hover tooltips, notices) where a per-frame reflow would jitter.
    /// Search keeps `false` so results are never hidden under the band.
    pub overlay_only: bool,
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

/// Same, with an explicit alpha — the rounded pipeline blends, so this is how the focus glow and
/// other translucent chrome are expressed.
fn rgba_a(c: Rgb, a: f32) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, a]
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
    push_rounded_grad(out, x0, y0, x1, y1, radius, color, color, fw, fh);
}

/// Same, with a vertical gradient: `top` at the rect's top edge, `bot` at its bottom. The rounded
/// pipeline interpolates vertex color, so this costs nothing beyond the two colors.
fn push_rounded_grad(out: &mut Vec<RoundedVertex>, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, top: [f32; 4], bot: [f32; 4], fw: f32, fh: f32) {
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
            color: if ly < 0.0 { top } else { bot },
        });
    }
}

/// The drawn rect of a pane's chrome, given the daemon's edge-to-edge tile.
///
/// Daemon rects tile the content area with no gaps; the GUI shrinks each edge — MARGIN at a
/// content boundary, GAP/2 at an interior split edge, so neighbours share one GAP. Shared by the
/// renderer and the close-button hit-test, because a hit-test that re-derived these insets would
/// drift from the drawing the moment either changed. Pure/tested.
fn pane_chrome_rect(rect: Rect, sidebar_w: u32, surf_w: u32, surf_h: u32) -> (f32, f32, f32, f32) {
    let (ox, oy, ow, oh) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    let l = if rect.x <= sidebar_w { MARGIN } else { GAP / 2.0 };
    let t = if rect.y == 0 { MARGIN } else { GAP / 2.0 };
    let rgt = if rect.x + rect.w >= surf_w { MARGIN } else { GAP / 2.0 };
    let bot = if rect.y + rect.h >= surf_h { MARGIN } else { GAP / 2.0 };
    (ox + l, oy + t, (ow - l - rgt).max(1.0), (oh - t - bot).max(1.0))
}

/// Stroke a rounded rect as a ring `w` px thick: the outer rounded quad, then the inner one punched
/// back to the surface color would need a second pass, so instead this pushes four thin rounded
/// bars along the edges — cheap, and at 1.5px the corner gap is invisible.
#[allow(clippy::too_many_arguments)]
fn stroke_rounded(out: &mut Vec<RoundedVertex>, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, w: f32, color: [f32; 4], fw: f32, fh: f32) {
    push_rounded(out, x0 + radius, y0, x1 - radius, y0 + w, 0.0, color, fw, fh); // top
    push_rounded(out, x0 + radius, y1 - w, x1 - radius, y1, 0.0, color, fw, fh); // bottom
    push_rounded(out, x0, y0 + radius, x0 + w, y1 - radius, 0.0, color, fw, fh); // left
    push_rounded(out, x1 - w, y0 + radius, x1, y1 - radius, 0.0, color, fw, fh); // right
}

/// Text color that reads on `bg`: white or black, whichever has more contrast. cmux computes its
/// selected-row foreground the same way, which is what lets an arbitrary accent stay legible.
fn on_accent(bg: Rgb) -> Rgb {
    let lum = (0.2126 * bg.r as f32 + 0.7152 * bg.g as f32 + 0.0722 * bg.b as f32) / 255.0;
    if lum > 0.55 {
        Rgb { r: 0, g: 0, b: 0 }
    } else {
        TEXT
    }
}

/// Lift a user-chosen tag color for a dark sidebar, porting cmux's `brightenedForDarkAppearance`:
/// in HSV, value becomes `min(1, max(v, 0.62) + (1 - v) * 0.28)` and saturation `s + (1 - s) * 0.12`
/// — except for near-grays (`s <= 0.08`), which keep their saturation so brightening introduces no
/// hue. Without this a dark red tag is indistinguishable from the panel. Pure/tested.
fn brighten_for_dark(c: Rgb) -> Rgb {
    let (r, g, b) = (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let v = max;
    let s = if max <= 0.0 { 0.0 } else { (max - min) / max };
    let v2 = (v.max(0.62) + (1.0 - v) * 0.28).min(1.0);
    let s2 = if s <= 0.08 { s } else { (s + (1.0 - s) * 0.12).min(1.0) };
    // Hue is preserved by rebuilding from the original hue sector.
    let h = if max <= min {
        0.0
    } else if max == r {
        60.0 * (((g - b) / (max - min)) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / (max - min) + 2.0)
    } else {
        60.0 * ((r - g) / (max - min) + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    let cc = v2 * s2;
    let x = cc * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v2 - cc;
    let (r2, g2, b2) = match (h / 60.0) as u32 {
        0 => (cc, x, 0.0),
        1 => (x, cc, 0.0),
        2 => (0.0, cc, x),
        3 => (0.0, x, cc),
        4 => (x, 0.0, cc),
        _ => (cc, 0.0, x),
    };
    let to8 = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Rgb { r: to8(r2), g: to8(g2), b: to8(b2) }
}

/// `#rrggbb` -> `Rgb`, or `None` if it isn't six hex digits (a bad value just renders untagged).
fn parse_hex_color(s: &str) -> Option<Rgb> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some(Rgb { r: byte(0)?, g: byte(2)?, b: byte(4)? })
}

/// GitHub's PR-state color (dark-theme values): open green, draft gray, merged purple, closed red.
/// `None` for an unrecognized status token, so a bad value just renders no PR badge. Pure/tested.
fn pr_color(status: &str) -> Option<Rgb> {
    Some(match status {
        "open" => Rgb { r: 0x3f, g: 0xb9, b: 0x50 },
        "draft" => Rgb { r: 0x8b, g: 0x94, b: 0x9e },
        "merged" => Rgb { r: 0xa3, g: 0x71, b: 0xf7 },
        "closed" => Rgb { r: 0xf8, g: 0x51, b: 0x49 },
        _ => return None,
    })
}

/// Which of the 8 spinner spokes is the bright one at `frame`, and how lit each spoke is: the spoke
/// under the head is fully lit and the rest fade around the ring, cmux's 8-spoke rotation. Pure.
fn spoke_alpha(frame: u32, spoke: u32) -> f32 {
    let behind = (frame + SPINNER_SPOKES - spoke) % SPINNER_SPOKES;
    // Head at 1.0 down to 0.25 for the spoke just ahead of it.
    1.0 - 0.75 * (behind as f32 / (SPINNER_SPOKES - 1) as f32)
}

/// The unread badge's text: the count, capped at "99+" so a runaway agent can't widen the row past
/// its name. Pure/tested.
fn unread_label(n: u32) -> String {
    if n > 99 {
        "99+".to_string()
    } else {
        n.to_string()
    }
}

/// Filled width of a progress rail: `pct` of `track`, clamped to 0..=100 so an out-of-range agent
/// report can't draw past the row. A nonzero percentage keeps a minimum nub so "1%" is visible.
/// Pure/tested.
fn progress_rail_w(track: f32, pct: u8) -> f32 {
    if pct == 0 {
        return 0.0;
    }
    let w = track * pct.min(100) as f32 / 100.0;
    w.max(PROGRESS_RAIL).min(track)
}

/// The scrollback scrollbar thumb `(top, bottom)` in px within a `track_h`-tall track. The thumb
/// height is proportional to the visible fraction `rows / (rows + history)`, floored at 24px and
/// capped at the track. Position is linear in `scrolled` over `[0, history]`: `scrolled == 0` pins
/// the thumb to the bottom (live screen), `scrolled == history` to the top (deepest history). A
/// zero `history` degenerates to a full-height thumb pinned at the bottom (no divide-by-zero).
/// Pure/tested.
fn scrollbar_thumb(track_h: f32, rows: u32, history: u32, scrolled: u32) -> (f32, f32) {
    let denom = (rows + history).max(1) as f32;
    let thumb_h = (track_h * rows as f32 / denom).max(24.0).min(track_h);
    let frac = if history == 0 { 0.0 } else { scrolled as f32 / history as f32 };
    let top = (track_h - thumb_h) * (1.0 - frac);
    (top, top + thumb_h)
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
    /// The atlas texture+sampler bind-group layout, kept so `set_font_px` can rebuild the bind
    /// group (new texture) against the same layout the glyph pipeline was created with.
    atlas_bind_layout: wgpu::BindGroupLayout,
    // Theme knobs (see `set_theme`). Cell fg/bg still come from the daemon's grid; these only drive
    // the window clear color and the sidebar text color.
    clear: wgpu::Color,
    text: Rgb,
    /// Which spoke of the activity spinner is lit. The app advances it (only while something is
    /// busy), so an idle gmux never redraws and idle CPU stays at zero.
    spinner_frame: u32,
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
        // Atlas GPU setup (texture + bind group against a shared layout). Extracted into
        // `atlas_bind_layout` / `atlas_texture` so `set_font_px` can rebuild them on a live zoom.
        let atlas = load_atlas(font_px);
        let bind_layout = Self::atlas_bind_layout(&device);
        let (tex, atlas_bind_group) = Self::atlas_texture(&device, &queue, &atlas, &bind_layout);

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
            atlas_bind_layout: bind_layout,
            clear: DEFAULT_CLEAR,
            spinner_frame: 0,
            text: TEXT,
        }
    }

    /// The atlas texture + sampler bind-group layout (shared by the glyph pipeline and every
    /// rebuilt atlas bind group). Extracted so `set_font_px` rebuilds the bind group against the
    /// exact layout the pipeline was created with.
    fn atlas_bind_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        })
    }

    /// Build the full-size R8 coverage texture for `atlas` (uploading only its initial ASCII+box
    /// region; the rest zero-inits and dynamic glyph tiles are written later via `glyph_uv`) plus a
    /// bind group pointing at it, using the shared `layout`. Returns `(texture, bind_group)`.
    fn atlas_texture(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &Atlas,
        layout: &wgpu::BindGroupLayout,
    ) -> (wgpu::Texture, wgpu::BindGroup) {
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("gmux-atlas-bg"),
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&tex_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        (tex, bind_group)
    }

    /// Change the font size live (Ctrl+wheel / zoom actions / config reload). Rebuilds the glyph
    /// atlas at `px` (new cell metrics; the dynamic glyph map re-rasterizes lazily) and its GPU
    /// texture + bind group. Clamped to the atlas's rasterizable range. Callers resend geometry so
    /// the daemon re-cells panes at the new `cell_w`/`cell_h`.
    pub fn set_font_px(&mut self, px: f32) {
        let px = px.clamp(8.0, 40.0);
        // ponytail: reloads font bytes from disk each call (rare, user-driven); cache if it shows.
        let atlas = load_atlas(px);
        let (tex, bind_group) =
            Self::atlas_texture(&self.device, &self.queue, &atlas, &self.atlas_bind_layout);
        self.atlas = atlas;
        self.atlas_tex = tex;
        self.atlas_bind_group = bind_group;
    }

    /// Apply theme colors: `bg` becomes the window clear color, `fg` the sidebar text color.
    /// (Terminal cell colors are owned by the daemon's grid, so they are unaffected.)
    /// Advance the activity spinner one spoke. Called only while a workspace is busy — an idle
    /// gmux never ticks it, so the no-timers/0%-idle-CPU invariant holds.
    pub fn advance_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_SPOKES;
    }

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
        // Active-pane search needle (query pre-folded to lowercase chars); highlights every visible
        // occurrence. `None` for inactive panes / no search.
        search: Option<&[char]>,
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
            // Per-row search-match mask (one folded scan per row); `None` when not searching.
            let row_matches = search.map(|needle| mark_matches(row, needle));
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
                // Search match: same wide-glyph spacer-inherit rule as selection/cursor.
                let matched = row_matches.as_ref().is_some_and(|h| {
                    let at = |col: usize| h.get(col).copied().unwrap_or(false);
                    at(c) || (lead_wide && at(c - 1)) || (cell.wide && at(c + 1))
                });
                // Selection: swap fg/bg and tint the (swapped) bg 30% toward ACCENT so the
                // highlight reads over any content (blank cells, dark-on-dark, etc.). A search match
                // does the same toward PEACH (stronger blend); selection wins on overlap.
                let (mut cell_bg, mut cell_fg) = (cell.bg, cell.fg);
                if selected {
                    std::mem::swap(&mut cell_bg, &mut cell_fg);
                    cell_bg = blend(accent(), cell_bg, 0.3);
                } else if matched {
                    std::mem::swap(&mut cell_bg, &mut cell_fg);
                    cell_bg = blend(PEACH, cell_bg, 0.6);
                }
                // Cursor shape (raw DECSCUSR Ps in `cursor_style`). Block (0,1,2 + any unknown) keeps
                // the old behavior: pre-blend the whole cell bg ~70% toward CURSOR (the opaque bg
                // pipeline can't alpha-blend). Underline (3,4) / bar (5,6) leave the bg alone and draw
                // a CURSOR strip after it (below). Blink bits ignored — no timers, idle CPU stays 0.
                let shape = is_cursor.then(|| cursor_shape(snap.cursor_style));
                let block_cursor = shape == Some(CursorShape::Block);
                let bg_color = if block_cursor { blend(CURSOR, cell_bg, 0.7) } else { cell_bg };
                push_quad(&mut bg, x0, y0, x0 + cw, y0 + ch, rgba(bg_color));

                // Inactive panes dim their text so the focused pane pops.
                let mut fg = rgba(cell_fg);
                if !active {
                    for ch in fg.iter_mut().take(3) {
                        *ch *= 0.8;
                    }
                }

                // Underline (SGR 4 / detected URLs): a 1px fg strip at the cell bottom, per cell so
                // it stays continuous across wide-glyph spacers. Same pipeline as the bg quads —
                // pushed after them, so it paints over the cell bg and under the glyphs.
                if cell.underline {
                    push_quad(&mut bg, x0, y0 + ch - 2.0, x0 + cw, y0 + ch - 1.0, fg);
                }

                // Underline/bar cursor: a 2px CURSOR-colored strip over the cell bg (pushed after it,
                // like the SGR underline). Bar draws only on the cursor's own column (`hits(c)`) so a
                // wide-glyph cursor's bar sits at the lead's left edge, not the spacer's; underline
                // draws on every is_cursor cell, so it spans the full wide-glyph width.
                // ponytail: cursor reported at a wide glyph's spacer column (rare) puts a bar mid-glyph.
                if is_cursor && !block_cursor && (hits(c) || shape != Some(CursorShape::Bar)) {
                    for q in cursor_quads(snap.cursor_style, x0, y0, cw, ch) {
                        push_quad(&mut bg, q.x0, q.y0, q.x1, q.y1, rgba(CURSOR));
                    }
                }

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
                show_close: false,
                drop_target: false,
                dragging: false,
                snap,
                attention,
                active: true,
                rect: Rect { x: 0, y: 0, w: px_w, h: px_h },
                scrolled: 0,
                history: 0,
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
                None,
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
    /// Hit-test a sidebar item (header or workspace row) at window `y`. Walks the same
    /// variable-height sequence `build_sidebar` draws, so a click can never land on a different
    /// item than the one under the cursor. `None` in the gaps between items.
    pub fn sidebar_item_at(&self, y: f32, heights: &[f32]) -> Option<usize> {
        let mut top = SIDEBAR_PAD_TOP + self.cell_h() as f32 + 8.0;
        if y < top {
            return None;
        }
        for (i, h) in heights.iter().enumerate() {
            if y < top + h {
                return Some(i);
            }
            top += h + ROW_GAP;
            if y < top {
                return None; // in the gap below this item
            }
        }
        None
    }

    /// Hit-test the '+ new tab' row drawn immediately after the last item (same walk, so the two
    /// never overlap).
    pub fn sidebar_new_tab_at(&self, y: f32, heights: &[f32]) -> bool {
        let mut top = SIDEBAR_PAD_TOP + self.cell_h() as f32 + 8.0;
        for h in heights {
            top += h + ROW_GAP;
        }
        y >= top && y < top + ROW_H
    }

    /// The laid-out height of each item, for the app to cache alongside its own item metadata (the
    /// hit-tests above walk exactly this).
    pub fn sidebar_item_heights(items: &[SidebarItem]) -> Vec<f32> {
        items.iter().map(SidebarItem::height).collect()
    }

    /// The top edge of item `index`, walking the same sequence the hit-tests do.
    pub fn sidebar_item_top(&self, index: usize, heights: &[f32]) -> f32 {
        let mut top = SIDEBAR_PAD_TOP + self.cell_h() as f32 + 8.0;
        for h in heights.iter().take(index) {
            top += h + ROW_GAP;
        }
        top
    }

    /// Whether `(x, y)` is anywhere on the settings card. The panel is modal, so a click that
    /// lands on it must never also reach the sidebar or a pane behind it.
    pub fn settings_hit(&self, x: f32, y: f32, rows: usize, surf_w: u32, surf_h: u32) -> bool {
        let c = settings_card(rows, self.cell_h() as f32, surf_w as f32, surf_h as f32);
        x >= c.px && x < c.px + c.pw && y >= c.py && y < c.py + c.ph
    }

    /// The settings row under `(x, y)`, or `None` in the tab strip / footer / margins. Walks the
    /// same rows `render_frame` lays out, including its clip at the card's bottom.
    pub fn settings_row_at(&self, x: f32, y: f32, rows: usize, surf_w: u32, surf_h: u32) -> Option<usize> {
        settings_row_index(x, y, rows, self.cell_h() as f32, surf_w as f32, surf_h as f32)
    }

    /// The tab under `(x, y)`, walking the same pill/gap sequence the strip is drawn with.
    pub fn settings_tab_at(&self, x: f32, y: f32, tabs: &[String], rows: usize, surf_w: u32, surf_h: u32) -> Option<usize> {
        let (cw, ch) = (self.cell_w() as f32, self.cell_h() as f32);
        settings_tab_index(x, y, tabs, rows, cw, ch, surf_w as f32, surf_h as f32)
    }

    /// Whether `(x, y)` lands on the given settings row (as returned by [`Self::settings_row_at`])
    /// *inside its colour ribbon*, rather than on its label or value.
    pub fn settings_swatch_hit(&self, x: f32, chips: usize, rows: usize, scrollable: bool, surf_w: u32, surf_h: u32) -> bool {
        if chips == 0 {
            return false;
        }
        let c = settings_card(rows, self.cell_h() as f32, surf_w as f32, surf_h as f32);
        // Mirrors the row's own right edge, which gives up a gutter when the bar is drawn.
        let right = c.px + c.pw - SET_PAD - if scrollable { SET_BAR + 6.0 } else { 0.0 };
        x >= right - SET_CHIP * chips as f32 && x < right
    }

    /// Whether `(x, y)` (window coords) lands on the close button in pane `rect`'s title strip.
    /// `rect` is the pane's OUTER rect as the app caches it, already shifted by the sidebar; the
    /// insets here mirror what `render_frame` draws.
    /// Whether `(x, y)` (window coords) is inside pane `rect`'s title strip — the grab handle for
    /// a pane rearrange. Uses the same chrome rect the strip is drawn into.
    pub fn title_strip_hit(&self, x: f32, y: f32, rect: Rect, sidebar_w: u32, surf_w: u32, surf_h: u32) -> bool {
        let (cx, cy, cw_, _) = pane_chrome_rect(rect, sidebar_w, surf_w, surf_h);
        x >= cx && x < cx + cw_ && y >= cy && y < cy + TITLE_STRIP + BORDER
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pane_close_hit(&self, x: f32, y: f32, rect: Rect, sidebar_w: u32, surf_w: u32, surf_h: u32, active: bool, attention: Attention) -> bool {
        let cw = self.cell_w() as f32;
        let ch = self.cell_h() as f32;
        let (bw, _) = border_style(active, attention);
        // The SAME chrome rect render_frame draws into — a split pane's edges are half-gaps, not
        // margins, so re-deriving them here would put the hit-box beside the glyph.
        let (cx, cy, cw_, _) = pane_chrome_rect(rect, sidebar_w, surf_w, surf_h);
        let (sx1, sy0) = (cx + cw_ - bw, cy + bw);
        let ty = (sy0 + (TITLE_STRIP - ch) / 2.0).max(sy0);
        let bx = sx1 - 8.0 - cw;
        x >= bx - 4.0 && x <= bx + cw + 4.0 && y >= ty - 3.0 && y < ty + ch + 3.0
    }

    /// Whether `(x, y)` lands on the close button of a hovered row whose top edge is `row_top`.
    /// Mirrors exactly where `build_sidebar` draws the 'x', with a small padding so the target is
    /// comfortable rather than one glyph wide. Only meaningful while the row is hovered — that is
    /// the only time the button is drawn.
    pub fn close_button_hit(&self, x: f32, y: f32, row_top: f32, sidebar_w: u32) -> bool {
        let ch = self.cell_h() as f32;
        let cw = self.cell_w() as f32;
        let pad_v = ROW_PAD_V.min(((ROW_H - 2.0 * ch) / 2.0).max(2.0));
        let line1 = row_top + pad_v;
        let right = sidebar_w as f32 - ROW_OUTER_PAD - ROW_PAD_H;
        // A 4px cushion on each side of the glyph; still inside the row's own padding.
        x >= right - cw - 4.0 && x <= right + 4.0 && y >= line1 - 2.0 && y < line1 + ch + 2.0
    }

    /// Whether `(x, y)` lands on the PR chip of a row whose top edge is `row_top`. `has_color` is
    /// whether the row also draws a tag rail (which shifts the chip right). Mirrors exactly what
    /// `build_sidebar` lays out, so the clickable area is the drawn area.
    pub fn pr_chip_hit(&self, x: f32, y: f32, row_top: f32, has_color: bool, number: u32) -> bool {
        let ch = self.cell_h() as f32;
        let cw = self.cell_w() as f32;
        let pad_v = ROW_PAD_V.min(((ROW_H - 2.0 * ch) / 2.0).max(2.0));
        let line2 = row_top + pad_v + ch;
        let mut x0 = ROW_OUTER_PAD + ROW_PAD_H;
        if has_color {
            x0 += COLOR_RAIL_W + COLOR_RAIL_INSET;
        }
        let w = (format!("#{number}").chars().count() as f32) * cw + 2.0 * BADGE_PAD_H;
        let (y0, y1) = (line2 - BADGE_PAD_V, line2 + ch + BADGE_PAD_V);
        x >= x0 && x < x0 + w && y >= y0 && y < y1
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

    /// One collapsible group header: chevron, name, and (collapsed) member count + unread badge.
    /// cmux uses SF Symbols chevrons; the atlas is ASCII-only, so `v`/`>` stand in.
    #[allow(clippy::too_many_arguments)]
    fn draw_group_header(&self, h: &GroupHeader, top: f32, sw: f32, cw: f32, ch: f32, fw: f32, fh: f32, rd: &mut Vec<RoundedVertex>, gl: &mut Vec<GlyphVertex>) {
        let (x0, x1) = (ROW_OUTER_PAD, sw - ROW_OUTER_PAD);
        if h.hover {
            push_rounded(rd, x0, top, x1, top + HEADER_H, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
        }
        let ty = top + (HEADER_H - ch) / 2.0;
        let tx = x0 + ROW_PAD_H;
        self.text_run(if h.collapsed { ">" } else { "v" }, tx, ty, rgba(TEXT_DIM), fw, fh, gl);
        let name_x = tx + 2.0 * cw;
        self.text_run(&h.name, name_x, ty, rgba(TEXT), fw, fh, gl);

        let mut right = x1 - ROW_PAD_H;
        if h.unread > 0 {
            let label = unread_label(h.unread);
            let bw_ = label.chars().count() as f32 * cw + 2.0 * BADGE_PAD_H;
            let bh = ch + 2.0 * BADGE_PAD_V;
            let by = ty - BADGE_PAD_V;
            push_rounded(rd, right - bw_, by, right, by + bh, bh / 2.0, rgba(accent()), fw, fh);
            self.text_run(&label, right - bw_ + BADGE_PAD_H, ty, rgba(TEXT), fw, fh, gl);
            right -= bw_ + 6.0;
        }
        // Collapsed, the member rows can't speak for themselves — say how many are hidden.
        if h.collapsed {
            let count = h.members.to_string();
            let w = count.chars().count() as f32 * cw;
            self.text_run(&count, right - w, ty, rgba(TEXT_DIM), fw, fh, gl);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_sidebar(&self, items: &[SidebarItem], sidebar_w: u32, plus_hover: bool, drop_at: Option<usize>, filter: Option<&str>, fw: f32, fh: f32) -> (Vec<BgVertex>, Vec<RoundedVertex>, Vec<GlyphVertex>) {
        let mut bg = Vec::new();
        let mut rd = Vec::new(); // rounded chrome (row fills, accent bar, attention dot)
        let mut gl = Vec::new();
        let sw = sidebar_w as f32;
        let ch = self.atlas.cell_h as f32;
        let to_ndc = |x: f32, y: f32| [x / fw * 2.0 - 1.0, 1.0 - y / fh * 2.0];
        // Vertical-gradient quad: `top` at y0, `bot` at y1 (the bg pipeline interpolates color).
        let quad_grad = |bg: &mut Vec<BgVertex>, x0: f32, y0: f32, x1: f32, y1: f32, top: [f32; 4], bot: [f32; 4]| {
            let (a, b, cc, d) = (to_ndc(x0, y0), to_ndc(x1, y0), to_ndc(x1, y1), to_ndc(x0, y1));
            for (p, c) in [(a, top), (b, top), (cc, bot), (a, top), (cc, bot), (d, bot)] {
                bg.push(BgVertex { pos: p, color: c });
            }
        };
        let cw = self.atlas.cell_w as f32;
        // App background: the clear color is flat, so paint the whole surface with a top-lit
        // gradient first; the panes draw over their own viewports afterwards.
        quad_grad(&mut bg, 0.0, 0.0, fw, fh, rgba(BG_APP), rgba(darker(BG_APP, 0.22)));
        quad_grad(&mut bg, 0.0, 0.0, sw, fh, rgba(BG_SIDEBAR), rgba(darker(BG_SIDEBAR, 0.28)));

        // Section header: "WORKSPACES", or the live filter query while filtering. The filter
        // replaces the label rather than pushing the list down, so rows don't shift under the
        // cursor as you type.
        let text_x = ROW_OUTER_PAD + ROW_PAD_H;
        match filter {
            Some(q) => {
                self.text_run("/", text_x, SIDEBAR_PAD_TOP, rgba(accent()), fw, fh, &mut gl);
                let shown = format!("{q}_");
                self.text_run(&shown, text_x + 2.0 * cw, SIDEBAR_PAD_TOP, rgba(TEXT), fw, fh, &mut gl);
            }
            None => {
                self.text_run("WORKSPACES", text_x, SIDEBAR_PAD_TOP, rgba(TEXT_DIM), fw, fh, &mut gl);
            }
        }
        let rows_y0 = SIDEBAR_PAD_TOP + ch + 8.0;
        // Text sits ROW_PAD_V from the row's top edge (cmux pads the block, it does not centre it);
        // clamped so a large font can't push the second line out of the row.
        let pad_v = ROW_PAD_V.min(((ROW_H - 2.0 * ch) / 2.0).max(2.0));
        let right_edge = sw - ROW_OUTER_PAD - ROW_PAD_H;

        // Items are laid out top to bottom with per-item heights (a header is shorter than a row),
        // so a running cursor replaces the old index * stride — `sidebar_item_at` walks the same way.
        let mut top = rows_y0;
        for (i, item) in items.iter().enumerate() {
            // Drop indicator: an accent line at the top edge of the item the drag would land on,
            // drawn before the item so a row fill can't cover it.
            if drop_at == Some(i) {
                push_rounded(&mut rd, ROW_OUTER_PAD, top - DROP_LINE, sw - ROW_OUTER_PAD, top, DROP_LINE / 2.0, rgba(accent()), fw, fh);
            }
            let r = match item {
                SidebarItem::Header(h) => {
                    self.draw_group_header(h, top, sw, cw, ch, fw, fh, &mut rd, &mut gl);
                    top += HEADER_H + ROW_GAP;
                    continue;
                }
                SidebarItem::Row(r) => r,
            };
            let line1 = top + pad_v;
            let line2 = line1 + ch;
            // cmux fills the selected workspace row SOLID with the accent (not a tint) and strokes
            // it with a 1.5px hairline; text flips to whatever reads on that fill. Everything else
            // in the row follows from which of those two worlds it is in.
            let (row_x0, row_x1) = (ROW_OUTER_PAD, sw - ROW_OUTER_PAD);
            if r.active {
                let fill = accent();
                push_rounded(&mut rd, row_x0, top, row_x1, top + ROW_H, RADIUS, rgba(fill), fw, fh);
                stroke_rounded(&mut rd, row_x0, top, row_x1, top + ROW_H, RADIUS, ROW_STROKE, rgba_a(TEXT, 0.5), fw, fh);
            } else if r.attention {
                // An unfocused workspace waiting on you: a whole-row wash, so a sidebar of ten tabs
                // still answers "who needs me" at a glance.
                push_rounded(&mut rd, row_x0, top, row_x1, top + ROW_H, RADIUS, rgba_a(ATTENTION, 0.25), fw, fh);
            } else if r.hover {
                push_rounded(&mut rd, row_x0, top, row_x1, top + ROW_H, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
            }
            // Leading activity dot: FILLED while something is running in the workspace, a hollow
            // ring when it is idle. This is the at-a-glance "which agents are still working"
            // signal — the spinner says the same thing but only survives a glance at one row.
            // The ring is drawn as an outer disc with the row's own background punched back into
            // it (the SDF pipeline fills shapes; it has no stroke mode).
            let row_bg = if r.active {
                accent()
            } else if r.attention {
                blend(ATTENTION, BG_SIDEBAR, 0.25)
            } else if r.hover {
                SIDEBAR_ROW_HOVER
            } else {
                BG_SIDEBAR
            };
            let dot_ink = if r.active { on_accent(accent()) } else { TEXT_DIM };
            let dot_cx = ROW_OUTER_PAD + ROW_PAD_H + STATUS_DOT / 2.0;
            let dot_cy = line1 + ch / 2.0;
            let disc = |rd: &mut Vec<RoundedVertex>, radius: f32, color: [f32; 4]| {
                push_rounded(rd, dot_cx - radius, dot_cy - radius, dot_cx + radius, dot_cy + radius, radius, color, fw, fh);
            };
            disc(&mut rd, STATUS_DOT / 2.0, rgba(dot_ink));
            if !r.busy {
                disc(&mut rd, STATUS_DOT / 2.0 - STATUS_RING, rgba(row_bg));
            }

            // A tagged workspace carries cmux's leading rail: a brightened capsule at the row's
            // left edge. It sits inside the pill, so it reads on the accent fill too.
            let tag = r.color.as_deref().and_then(parse_hex_color).map(brighten_for_dark);
            // Text starts after the status dot.
            let mut text_x = text_x + STATUS_DOT + STATUS_GAP;
            if let Some(tag) = tag {
                let rx = row_x0 + COLOR_RAIL_INSET;
                push_rounded(&mut rd, rx, top + 5.0, rx + COLOR_RAIL_W, top + ROW_H - 5.0, COLOR_RAIL_W / 2.0, rgba(tag), fw, fh);
                text_x += COLOR_RAIL_W + COLOR_RAIL_INSET; // keep the label clear of the rail
            }
            // On the accent fill the label is the readable-contrast color and the secondary line is
            // the same color at reduced alpha — cmux's `selectedWorkspaceForegroundNSColor`.
            let (label_col, sub_col) = if r.active {
                let on = on_accent(accent());
                (rgba(on), rgba_a(on, 0.72))
            } else {
                (rgba(self.text), rgba(TEXT_DIM))
            };
            // The row in flight fades, so the drop line reads as its destination.
            let (label_col, sub_col) = if r.dragging {
                ([label_col[0], label_col[1], label_col[2], DRAG_FADE], [sub_col[0], sub_col[1], sub_col[2], DRAG_FADE])
            } else {
                (label_col, sub_col)
            };
            self.text_run(&r.name, text_x, line1, label_col, fw, fh, &mut gl);
            // PR badge on the second line, before the branch: a small "#42" chip in the state
            // color (open green / draft gray / merged purple / closed red). The branch text starts
            // after it. On the accent-filled active row the chip text flips for contrast.
            let mut sub_x = text_x;
            if let Some((num, status)) = &r.pr {
                if let Some(col) = pr_color(status) {
                    let label = format!("#{num}");
                    let tw = label.chars().count() as f32 * cw;
                    let (bw_, bh) = (tw + 2.0 * BADGE_PAD_H, ch + 2.0 * BADGE_PAD_V);
                    let y0 = line2 - BADGE_PAD_V;
                    let ink = if r.active { on_accent(accent()) } else { TEXT };
                    push_rounded(&mut rd, sub_x, y0, sub_x + bw_, y0 + bh, bh / 2.0, rgba(col), fw, fh);
                    self.text_run(&label, sub_x + BADGE_PAD_H, line2, rgba(ink), fw, fh, &mut gl);
                    sub_x += bw_ + 5.0;
                }
            }
            if let Some(b) = &r.branch {
                self.text_run(&format!("git:{b}"), sub_x, line2, sub_col, fw, fh, &mut gl);
            }

            // Hovering a row reveals a close button at its right edge — closing a workspace was
            // previously middle-click only, which nothing on screen suggested. It takes the
            // indicators' place while hovering rather than squeezing them further left.
            let mut cursor_right = right_edge;
            if r.hover {
                let ink = if r.active { on_accent(accent()) } else { TEXT_DIM };
                self.text_run("x", cursor_right - cw, line1, rgba(ink), fw, fh, &mut gl);
                cursor_right -= cw + 6.0;
            }
            if r.progress_error || r.progress.is_some() {
                let (txt, col) = if r.progress_error {
                    ("!".to_string(), ERROR)
                } else {
                    (format!("{}%", r.progress.unwrap()), PROGRESS)
                };
                // On the accent fill, PROGRESS/ERROR green and red both muddy; the readable
                // foreground carries the number and the rail below carries the color.
                let col = if r.active { on_accent(accent()) } else { col };
                let w = txt.chars().count() as f32 * cw;
                self.text_run(&txt, cursor_right - w, line1, rgba(col), fw, fh, &mut gl);
                cursor_right -= w + 4.0;
            }
            // Unread count: cmux's capsule badge (accent fill, white semibold count, 5px/1px
            // padding). With no count to show — a bell with no notification, or an old daemon that
            // doesn't send one — it degrades to the plain attention dot.
            if r.unread > 0 {
                let label = unread_label(r.unread);
                let tw = label.chars().count() as f32 * cw;
                let (bw_, bh) = (tw + 2.0 * BADGE_PAD_H, ch + 2.0 * BADGE_PAD_V);
                let x0 = cursor_right - bw_;
                let y0 = line1 - BADGE_PAD_V;
                // On the accent-filled active row the badge inverts, or it would vanish into it.
                let (fill, ink) =
                    if r.active { (on_accent(accent()), accent()) } else { (accent(), TEXT) };
                push_rounded(&mut rd, x0, y0, cursor_right, y0 + bh, bh / 2.0, rgba(fill), fw, fh);
                self.text_run(&label, x0 + BADGE_PAD_H, line1, rgba(ink), fw, fh, &mut gl);
                cursor_right -= bw_ + 4.0;
            } else if r.attention {
                let x1 = cursor_right;
                let y0 = line1 + (ch - ATTN_DOT) / 2.0;
                let dot = if r.active { on_accent(accent()) } else { ATTENTION };
                push_rounded(&mut rd, x1 - ATTN_DOT, y0, x1, y0 + ATTN_DOT, ATTN_DOT / 2.0, rgba(dot), fw, fh);
                cursor_right -= ATTN_DOT + 4.0;
            }
            // Activity spinner: 8 spokes around a small ring, the lit one advancing per frame.
            // Drawn on the SECOND line so it never competes with the badge for space.
            if r.busy {
                let ink = if r.active { on_accent(accent()) } else { TEXT_DIM };
                let (cx_s, cy_s) = (right_edge - SPINNER_R, line2 + ch / 2.0);
                for spoke in 0..SPINNER_SPOKES {
                    let a = std::f32::consts::TAU * spoke as f32 / SPINNER_SPOKES as f32;
                    let (px, py) = (cx_s + SPINNER_R * a.cos(), cy_s + SPINNER_R * a.sin());
                    let h = SPINNER_DOT / 2.0;
                    push_rounded(&mut rd, px - h, py - h, px + h, py + h, h, rgba_a(ink, spoke_alpha(self.spinner_frame, spoke)), fw, fh);
                }
            }
            let _ = cursor_right; // (kept assigned so a later indicator can chain leftwards)

            // Progress rail along the row's bottom edge: the percentage as a bar, not just digits.
            // An error fills the whole rail in ERROR (there is no meaningful fraction to show).
            if r.progress_error || r.progress.is_some() {
                // Along the row pill's bottom edge (not inset to the text column): the two text
                // lines fill the 48px row, so an inset rail reads as an underline of the branch
                // name instead of a rail.
                let (x0, x1) = (row_x0, row_x1);
                let (y0, y1) = (top + ROW_H - PROGRESS_RAIL, top + ROW_H);
                push_rounded(&mut rd, x0, y0, x1, y1, 1.0, rgba_a(TEXT, 0.10), fw, fh);
                let (w, col) = if r.progress_error {
                    (x1 - x0, ERROR)
                } else {
                    (progress_rail_w(x1 - x0, r.progress.unwrap()), PROGRESS)
                };
                if w > 0.0 {
                    push_rounded(&mut rd, x0, y0, x0 + w, y1, 1.0, rgba(col), fw, fh);
                }
            }
            top += ROW_H + ROW_GAP;
        }

        // A drop past the last item lands at the end of the list; the indicator goes there.
        if drop_at == Some(items.len()) {
            push_rounded(&mut rd, ROW_OUTER_PAD, top - DROP_LINE, sw - ROW_OUTER_PAD, top, DROP_LINE / 2.0, rgba(accent()), fw, fh);
        }

        // '+ new tab' row, immediately after the last item (matches sidebar_new_tab_at).
        let plus_top = top;
        if plus_hover {
            push_rounded(&mut rd, ROW_OUTER_PAD, plus_top, sw - ROW_OUTER_PAD, plus_top + ROW_H, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
        }
        // Says what it does: clicking it asks for a directory and opens it as a workspace.
        self.text_run("+ open workspace", text_x, plus_top + (ROW_H - ch) / 2.0, rgba(TEXT_DIM), fw, fh, &mut gl);

        (bg, rd, gl)
    }

    /// Render a full frame: the sidebar (left column) plus the panes. `search`/`preedit` overlay the
    /// active pane (search band at the bottom; IME preedit at the cursor).
    pub fn render_frame(
        &self,
        view: &wgpu::TextureView,
        sidebar: &[SidebarItem],
        sidebar_w: u32,
        panes: &[PaneView],
        surf_w: u32,
        surf_h: u32,
        empty_msg: &str,
        plus_hover: bool,
        // `drop_at`: index of the sidebar item a reorder drag would land on (`items.len()` = the
        // end of the list); `None` when nothing is being dragged.
        drop_at: Option<usize>,
        // `filter`: the live sidebar filter query, shown in place of the WORKSPACES label.
        filter: Option<&str>,
        search: Option<&SearchBar>,
        preedit: Option<&str>,
        palette: Option<&PaletteView>,
        settings: Option<&SettingsView>,
    ) {
        let (fw, fh) = (surf_w.max(1) as f32, surf_h.max(1) as f32);
        // `sbg` is the opaque sidebar panel; `srd` is the rounded chrome (sidebar rows + pane
        // fills/borders); `sgl` is the sidebar text plus any empty-state message. `obg`/`ogl` are
        // the scroll-badge overlay, drawn last so they sit above the pane cells.
        let (sbg, mut srd, mut sgl) =
            self.build_sidebar(sidebar, sidebar_w, plus_hover, drop_at, filter, fw, fh);
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
            let (cx, cy, cw_, ch_) = pane_chrome_rect(pv.rect, sidebar_w, surf_w, surf_h);

            // Pane chrome: a rounded border ring (outer) with the BG_PANE fill (inner, inset by the
            // border width) drawn on top. The fill also letterboxes the cell-grid remainder.
            let (bw, bc) = border_style(pv.active, pv.attention);
            // Focus glow: two faint accent-tinted rings just outside the active pane's border,
            // fading with distance. Costs 12 vertices and sits inside the MARGIN, so no layout
            // moves; inactive panes draw nothing (the glow IS the focus cue).
            if pv.active && !pv.attention.is_pending() {
                for (i, alpha) in [(GLOW_W, 0.10f32), (GLOW_W / 2.0, 0.16)] {
                    push_rounded(
                        &mut srd,
                        cx - i,
                        cy - i,
                        cx + cw_ + i,
                        cy + ch_ + i,
                        RADIUS + i,
                        rgba_a(bc, alpha),
                        fw,
                        fh,
                    );
                }
            }
            // The inactive stroke fades toward the bottom (Fluent's lit-from-above control stroke);
            // active/attention rings stay flat so focus reads as one uniform color.
            let bc_bot = if pv.active { bc } else { darker(bc, 0.35) };
            push_rounded_grad(&mut srd, cx, cy, cx + cw_, cy + ch_, RADIUS, rgba(bc), rgba(bc_bot), fw, fh);
            push_rounded_grad(&mut srd, cx + bw, cy + bw, cx + cw_ - bw, cy + ch_ - bw, (RADIUS - bw).max(0.0), rgba(BG_PANE), rgba(darker(BG_PANE, 0.12)), fw, fh);

            // Title strip: a TITLE_STRIP-tall BG_SIDEBAR band inside the border. First quad rounds
            // the top corners (radius RADIUS-bw); the second (radius 0) squares off the bottom edge
            // where it meets the cell area. Active pane gets an ACCENT dot before the title text.
            let (sx0, sx1) = (cx + bw, cx + cw_ - bw);
            let (sy0, sy1) = (cy + bw, cy + bw + TITLE_STRIP);
            let sr = (RADIUS - bw).max(0.0);
            // Title band: lit at the top, settling into BG_PANE where it meets the cells, so the
            // strip reads as a raised surface instead of a flat stripe.
            // The ACTIVE pane's strip is tinted toward the accent and its title is full-strength;
            // inactive strips stay neutral and dim. Focus was previously carried only by the 1px
            // border and the glow, which is easy to lose in a four-way split.
            // A rearrange in flight: the receiving pane's strip goes accent, the dragged one's
            // dims — so the swap reads as "these two trade places" before you let go.
            let base = if pv.drop_target {
                blend(accent(), BG_SIDEBAR, 0.55)
            } else if pv.dragging {
                blend(TEXT, BG_SIDEBAR, 0.02)
            } else if pv.active {
                blend(accent(), BG_SIDEBAR, 0.16)
            } else {
                BG_SIDEBAR
            };
            let (t_top, t_bot) = (rgba(blend(TEXT, base, 0.05)), rgba(base));
            push_rounded_grad(&mut srd, sx0, sy0, sx1, sy1, sr, t_top, t_bot, fw, fh);
            push_rounded_grad(&mut srd, sx0, sy0 + sr, sx1, sy1, 0.0, t_top, t_bot, fw, fh);
            // Hairline where the strip meets the cells, so the title reads as its own surface.
            push_rounded(&mut srd, sx0, sy1 - 1.0, sx1, sy1, 0.0, rgba_a(TEXT, 0.07), fw, fh);
            let ty = (sy0 + (TITLE_STRIP - ch_cell) / 2.0).max(sy0);
            let mut tx = sx0 + 12.0;
            if pv.active {
                let dot = 6.0;
                let dy = sy0 + (TITLE_STRIP - dot) / 2.0;
                push_rounded(&mut srd, tx, dy, tx + dot, dy + dot, dot / 2.0, rgba(accent()), fw, fh);
                tx += dot + 5.0;
            }
            // Close button at the strip's right end (active pane, or whichever one is hovered).
            let mut title_right = sx1 - 8.0;
            if pv.show_close {
                let bx = title_right - cw_cell;
                self.text_run("x", bx, ty, rgba_a(TEXT, if pv.active { 0.8 } else { 0.5 }), fw, fh, &mut sgl);
                title_right = bx - 6.0;
            }
            let max_chars = ((title_right - tx).max(0.0) / cw_cell) as usize;
            let title = truncate_ellipsis(&pv.title, max_chars);
            if !title.is_empty() {
                let ink = if pv.active { TEXT } else { TEXT_DIM };
                self.text_run(&title, tx, ty, rgba(ink), fw, fh, &mut sgl);
            }

            // Cell-area geometry (hoisted so the scrollbar, scroll badge, and search band share this
            // rect). Inset INSET on the sides and bottom, below the title strip on top; the search
            // band (active pane) covers the bottom SEARCH_BAR of the visible height.
            // Overlay-only bands (tooltips/notices) draw over the bottom row without reflow.
            let search_here = pv.active && search.is_some_and(|s| !s.overlay_only);
            let pad = bw + INSET;
            let (ix, iy) = (cx + pad, cy + bw + TITLE_STRIP + INSET);
            let iw = (cw_ - 2.0 * pad).max(1.0);
            // ponytail: search band shrinks the visible cell area by SEARCH_BAR, but the daemon isn't
            // told — so the bottom cell row is covered (not resized away) while searching. Acceptable.
            let ih = (ch_ - bw - TITLE_STRIP - INSET - pad - if search_here { SEARCH_BAR } else { 0.0 }).max(1.0);

            // Scrollback scrollbar: an 8px strip at the cell-area right edge (BG_SIDEBAR track +
            // ACCENT thumb), pushed into the overlay BEFORE the scroll badge so the badge sits on
            // top where they overlap top-right. Active pane with scrollback only.
            if pv.active && pv.scrolled > 0 {
                let (t0, t1) = scrollbar_thumb(ih, pv.snap.rows as u32, pv.history, pv.scrolled);
                let sbx1 = ix + iw;
                let sbx0 = sbx1 - SCROLLBAR_W;
                push_rounded(&mut obg, sbx0, iy, sbx1, iy + ih, 0.0, rgba(BG_SIDEBAR), fw, fh); // track
                push_rounded(&mut obg, sbx0, iy + t0, sbx1, iy + t1, 0.0, rgba(accent()), fw, fh); // thumb
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
                self.text_run(&label, br - bw_chip + bpx, bt + bpy, rgba(accent()), fw, fh, &mut ogl);
            }

            // Search band: a SEARCH_BAR-tall BG_SIDEBAR band at the active pane's bottom, inside the
            // border (title strip owns the top). Round the bottom corners; square the top where it
            // meets the cells. Content: dim "find:" label, the query in TEXT with a '_' caret, and a
            // right-aligned "current/total" in ACCENT (or "no matches" in ERROR).
            if let (true, Some(sb)) = (pv.active, search) {
                let (bx0, bx1) = (cx + bw, cx + cw_ - bw);
                let by1 = cy + ch_ - bw;
                let by0 = by1 - SEARCH_BAR;
                let sr = (RADIUS - bw).max(0.0);
                let (b_top, b_bot) = (rgba(blend(TEXT, BG_SIDEBAR, 0.05)), rgba(BG_SIDEBAR));
                push_rounded_grad(&mut srd, bx0, by0, bx1, by1, sr, b_top, b_bot, fw, fh);
                push_rounded_grad(&mut srd, bx0, by0, bx1, by1 - sr, 0.0, b_top, b_bot, fw, fh);
                let ty = (by0 + (SEARCH_BAR - ch_cell) / 2.0).max(by0);
                let lx = bx0 + 12.0;
                self.text_run(&sb.label, lx, ty, rgba(TEXT_DIM), fw, fh, &mut sgl);
                let label_cells = sb.label.chars().count() as f32 + 1.0; // +1 cell = space
                // A pure prompt band (empty query, no matches) shows just the label: no caret,
                // no counter — the close-confirmation reuse.
                if !(sb.query.is_empty() && sb.total == 0) {
                    let q = format!("{}_", sb.query);
                    self.text_run(&q, lx + label_cells * cw_cell, ty, rgba(TEXT), fw, fh, &mut sgl);
                    let (counter, col) = if sb.total == 0 && !sb.query.is_empty() {
                        ("no matches".to_string(), ERROR)
                    } else {
                        (format!("{}/{}", sb.current, sb.total), accent())
                    };
                    let cwn = counter.chars().count() as f32 * cw_cell;
                    self.text_run(&counter, (bx1 - 12.0 - cwn).max(lx), ty, rgba(col), fw, fh, &mut sgl);
                }
            }

            // Cells draw at fixed size from the cell-area top-left (`ix`,`iy`; computed above); the
            // viewport clips overflow and the BG_PANE fill shows through any remainder.
            // Search-match highlight on the active pane only, when the query is non-empty AND the
            // daemon reported matches — gating on total keeps the highlight and the "no matches"
            // label from contradicting (a pane switch keeps the query but drops the matches).
            let needle: Option<Vec<char>> = if pv.active {
                search
                    .filter(|sb| !sb.query.is_empty() && sb.total > 0)
                    .map(|sb| sb.query.chars().map(fold).collect())
            } else {
                None
            };
            let (bg, glyphs) = self.build_vertices(
                pv.snap,
                pv.attention,
                pv.active,
                iw as u32,
                ih as u32,
                false,
                pv.selection,
                needle.as_deref(),
            );
            draws.push(Draw {
                rect: Rect { x: ix as u32, y: iy as u32, w: iw as u32, h: ih as u32 },
                bg_n: bg.len() as u32,
                glyph_n: glyphs.len() as u32,
                bg: vb(bytemuck::cast_slice(&bg)),
                glyph: vb(bytemuck::cast_slice(&glyphs)),
            });

            // IME preedit: at the active pane's cursor cell, an overlay (drawn last) — a filled rect
            // sized to the text, the text in TEXT, and a 1px underline beneath. Clamped inside cells.
            if let (true, Some(pe)) = (pv.active, preedit.filter(|p| !p.is_empty())) {
                let (col, row) = pv.snap.cursor;
                let pw = pe.chars().count() as f32 * cw_cell;
                let px = (ix + col as f32 * cw_cell).min(ix + iw - pw).max(ix);
                let py = (iy + row as f32 * ch_cell).min(iy + ih - ch_cell).max(iy);
                push_rounded(&mut obg, px, py, px + pw, py + ch_cell, 0.0, rgba(SIDEBAR_ROW_ACTIVE), fw, fh);
                push_rounded(&mut obg, px, py + ch_cell - 1.0, px + pw, py + ch_cell, 0.0, rgba(TEXT), fw, fh);
                self.text_run(pe, px, py, rgba(TEXT), fw, fh, &mut ogl);
            }
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

        // Command palette: a centered top panel in the overlay buffers (above everything). Query
        // line first ("> query_"), then the pre-filtered rows — selected row gets the accent fill,
        // hints (chords / "tab") sit right-aligned and dim. Geometry clamps to the surface via the
        // overlay pass's full-surface viewport, so a tiny window just crops it.
        if let Some(pal) = palette {
            const PAL_W: f32 = 520.0;
            const PAD: f32 = 12.0;
            let row_h = ch_cell + 8.0;
            let pw = PAL_W.min(fw - 16.0).max(120.0);
            let ph = PAD * 2.0 + row_h * (pal.items.len() as f32 + 1.0);
            let px = ((fw - pw) / 2.0).max(0.0);
            let py = 48.0_f32.min(fh * 0.1);
            push_rounded(&mut obg, px, py, px + pw, py + ph, RADIUS, rgba(BG_SIDEBAR), fw, fh);
            let q = format!("> {}_", pal.query);
            self.text_run(&q, px + PAD, py + PAD + 4.0, rgba(TEXT), fw, fh, &mut ogl);
            for (i, (label, hint)) in pal.items.iter().enumerate() {
                let ry = py + PAD + row_h * (i as f32 + 1.0);
                if i == pal.selected {
                    push_rounded(&mut obg, px + 4.0, ry, px + pw - 4.0, ry + row_h, RADIUS - 2.0, rgba(SIDEBAR_ROW_ACTIVE), fw, fh);
                }
                self.text_run(label, px + PAD, ry + 4.0, rgba(TEXT), fw, fh, &mut ogl);
                let hw = hint.chars().count() as f32 * cw_cell;
                self.text_run(hint, (px + pw - PAD - hw).max(px + PAD), ry + 4.0, rgba(TEXT_DIM), fw, fh, &mut ogl);
            }
        }

        // Settings panel: a centered card with a tab strip, `label ......... value` rows, and a
        // footer of key hints. Taller than the palette because it lists every keybinding, so it is
        // capped to the window and the app scrolls its row window.
        if let Some(sv) = settings {
            let Card { px, py, pw, ph, row_h, head } = settings_card(sv.rows.len(), ch_cell, fw, fh);
            // A hairline-ringed card, one step above the sidebar so it reads as "on top".
            push_rounded(&mut obg, px, py, px + pw, py + ph, RADIUS, rgba(SIDEBAR_ROW_HOVER), fw, fh);
            stroke_rounded(&mut obg, px, py, px + pw, py + ph, RADIUS, 1.0, rgba_a(TEXT, 0.10), fw, fh);

            // Tab strip: the open section is an accent pill, the others dim text.
            let mut tx = px + SET_PAD;
            for (i, name) in sv.tabs.iter().enumerate() {
                let tw = name.chars().count() as f32 * cw_cell;
                if i == sv.tab {
                    push_rounded(&mut obg, tx - 6.0, py + SET_PAD - 2.0, tx + tw + 6.0, py + SET_PAD + ch_cell + 4.0, BADGE_RADIUS, rgba(accent()), fw, fh);
                }
                let ink = if i == sv.tab { on_accent(accent()) } else { TEXT_DIM };
                self.text_run(name, tx, py + SET_PAD, rgba(ink), fw, fh, &mut ogl);
                tx += tw + SET_TAB_GAP;
            }
            // The filter shares the tab strip's line, right-aligned: it belongs to the whole list
            // below it, and the footer is already carrying the hints.
            if let Some(q) = &sv.query {
                let text = format!("/{q}_");
                let qw = text.chars().count() as f32 * cw_cell;
                let qx = (px + pw - SET_PAD - qw).max(tx);
                self.text_run(&text, qx, py + SET_PAD, rgba(accent()), fw, fh, &mut ogl);
            }

            let rows_y = py + SET_PAD + head;
            // A scrollbar down the card's right edge whenever the list is taller than the card:
            // without it, a windowed list looks like the whole list, and "my binding isn't here"
            // is indistinguishable from "scroll down".
            let scrollable = sv.total > sv.rows.len() && !sv.rows.is_empty();
            if scrollable {
                let (x0, x1) = (px + pw - SET_PAD - SET_BAR, px + pw - SET_PAD);
                let (y0, y1) = (rows_y, rows_y + row_h * sv.rows.len() as f32);
                push_rounded(&mut obg, x0, y0, x1, y1, SET_BAR / 2.0, rgba_a(TEXT, 0.06), fw, fh);
                let (dy, th) = scroll_thumb(sv.total, sv.rows.len(), sv.offset, y1 - y0);
                push_rounded(&mut obg, x0, y0 + dy, x1, y0 + dy + th, SET_BAR / 2.0, rgba_a(TEXT, 0.24), fw, fh);
            }
            for (i, row) in sv.rows.iter().enumerate() {
                let ry = rows_y + row_h * i as f32;
                if ry + row_h > py + ph - row_h {
                    break; // clipped by the card; the app windows the rows
                }
                if i == sv.selected {
                    push_rounded(&mut obg, px + 4.0, ry, px + pw - 4.0, ry + row_h, RADIUS - 2.0, rgba(SIDEBAR_ROW_ACTIVE), fw, fh);
                }
                self.text_run(&row.label, px + SET_PAD, ry + 4.0, rgba(TEXT), fw, fh, &mut ogl);
                // The ribbon sits at the card's right edge and the value text to its left, so the
                // swatch holds one x while cycling — it's the thing you watch, not the name.
                let mut right = px + pw - SET_PAD - if scrollable { SET_BAR + 6.0 } else { 0.0 };
                if !row.swatch.is_empty() {
                    const CHIP: f32 = SET_CHIP;
                    let sw = CHIP * row.swatch.len() as f32;
                    let (x0, y0) = (right - sw, ry + (row_h - CHIP) / 2.0);
                    for (n, c) in row.swatch.iter().enumerate() {
                        let cx = x0 + CHIP * n as f32;
                        push_rounded(&mut obg, cx, y0, cx + CHIP, y0 + CHIP, 1.0, rgba(*c), fw, fh);
                    }
                    // One ring around the whole ribbon: a scheme's background chip is near-black
                    // on a near-black card, and unringed it reads as a gap rather than a colour.
                    stroke_rounded(&mut obg, x0, y0, right, y0 + CHIP, 2.0, 1.0, rgba_a(TEXT, 0.18), fw, fh);
                    right = x0 - 10.0;
                }
                let vw = row.value.chars().count() as f32 * cw_cell;
                // A conflicting chord stays red even under the selection: it is a problem with the
                // row, not a property of which row you happen to be on.
                let ink = if row.warn {
                    ERROR
                } else if i == sv.selected {
                    accent()
                } else {
                    TEXT_DIM
                };
                self.text_run(&row.value, (right - vw).max(px + SET_PAD), ry + 4.0, rgba(ink), fw, fh, &mut ogl);
            }
            // Footer hints, pinned to the card's bottom edge. Clipped to the card: hints grow with
            // the tab you're on and the card doesn't, so an over-long line would spill onto the
            // pane behind it at a narrow window or a large font.
            let fit = ((pw - SET_PAD * 2.0) / cw_cell).max(0.0) as usize;
            let footer = clip_words(&sv.footer, fit);
            self.text_run(&footer, px + SET_PAD, py + ph - SET_PAD - ch_cell, rgba(TEXT_DIM), fw, fh, &mut ogl);
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
            // Sidebar (full-surface viewport). Clamp guards a degenerate surface (minimize → 1×1);
            // an unset viewport/scissor defaults to the full (valid) target, so the draws below stay
            // safe even when the clamp skips the set.
            if let Some((x, y, w, h)) = clamp_rect(0, 0, surf_w, surf_h, surf_w, surf_h) {
                pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                pass.set_scissor_rect(x, y, w, h);
            }
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
                // Clamp the inset rect to the surface; skip the pane when it clamps to empty
                // (off-surface origin on an absurdly small / minimized window).
                let Some((x, y, w, h)) = clamp_rect(d.rect.x, d.rect.y, d.rect.w, d.rect.h, surf_w, surf_h)
                else {
                    continue;
                };
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
            if let Some((x, y, w, h)) =
                clamp_rect(0, 0, surf_w, surf_h, surf_w, surf_h).filter(|_| !obg.is_empty() || !ogl.is_empty())
            {
                pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                pass.set_scissor_rect(x, y, w, h);
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

/// Approximate case-fold for per-cell match comparison: the first char of the Unicode lowercase
/// mapping. ponytail: rare multi-char folds (e.g. 'İ' → "i̇") collapse to their first char; the
/// daemon folds whole lines, so those cases may differ from the count in the search bar — accepted
/// for a terminal highlight.
fn fold(ch: char) -> char {
    ch.to_lowercase().next().unwrap_or(ch)
}

/// Mark every cell of `row` covered by a case-insensitive occurrence of `needle` (the query already
/// folded to lowercase chars). Both sides fold with [`fold`], so comparison is per-cell. A wide
/// glyph's ' ' spacer participates as an ordinary cell here; the caller extends a match onto the
/// spacer via the same lead_wide rule it uses for selection/cursor.
fn mark_matches(row: &[Cell], needle: &[char]) -> Vec<bool> {
    let n = row.len();
    let mut hit = vec![false; n];
    if needle.is_empty() || needle.len() > n {
        return hit;
    }
    let folded: Vec<char> = row.iter().map(|c| fold(c.ch)).collect();
    for start in 0..=(n - needle.len()) {
        if folded[start..start + needle.len()] == *needle {
            hit[start..start + needle.len()].fill(true);
        }
    }
    hit
}

/// A pixel-space rectangle (`x0,y0`..`x1,y1`), used for cursor-shape strips. Pure geometry so the
/// shape logic is unit-testable without a GPU.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Quad {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

/// Cursor shape category for a raw DECSCUSR Ps (`cursor_style`): 0,1,2 (and any unknown >6) → block,
/// 3,4 → underline, 5,6 → bar. The blink bit is folded away (odd = blink, even = steady share a
/// shape) — gmux never blinks, so idle CPU stays 0 with no timers. ponytail: fixed table, not math.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CursorShape {
    Block,
    Underline,
    Bar,
}
fn cursor_shape(style: u8) -> CursorShape {
    match style {
        3 | 4 => CursorShape::Underline,
        5 | 6 => CursorShape::Bar,
        _ => CursorShape::Block, // 0,1,2 and anything >6 clamp to block
    }
}

/// Cursor strip quad(s) for a cell at pixel `(x0,y0)` sized `cw × ch`, drawn in CURSOR color. A block
/// cursor emits none (it is rendered by pre-blending the cell bg instead); underline is a 2px strip
/// at the cell bottom (full width); bar is a 2px vertical strip at the cell's left edge. Pure/tested.
fn cursor_quads(style: u8, x0: f32, y0: f32, cw: f32, ch: f32) -> Vec<Quad> {
    match cursor_shape(style) {
        CursorShape::Block => Vec::new(),
        CursorShape::Underline => vec![Quad { x0, y0: y0 + ch - 2.0, x1: x0 + cw, y1: y0 + ch }],
        CursorShape::Bar => vec![Quad { x0, y0, x1: x0 + 2.0, y1: y0 + ch }],
    }
}

/// Clamp a pixel rect to a `tw × th` render target, returning `None` when it clamps to empty (origin
/// off-target or zero-sized). wgpu validates `set_scissor_rect`/`set_viewport` against the
/// attachment and panics on any rect not contained in it — defense-in-depth for a minimize that
/// collapses the surface (seen as a 1×1 target).
fn clamp_rect(x: u32, y: u32, w: u32, h: u32, tw: u32, th: u32) -> Option<(u32, u32, u32, u32)> {
    if x >= tw || y >= th {
        return None;
    }
    let w = w.min(tw - x);
    let h = h.min(th - y);
    if w == 0 || h == 0 {
        return None;
    }
    Some((x, y, w, h))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_card_holds_its_top_edge_across_tabs() {
        // The panel's height follows its row count. Its top must not: switching from a five-row
        // tab to a twelve-row one used to move the whole card up the screen.
        let (ch, fw, fh) = (20.0_f32, 1280.0_f32, 800.0_f32);
        let small = settings_card(5, ch, fw, fh);
        let large = settings_card(12, ch, fw, fh);
        assert_eq!(small.py, large.py, "the top edge is pinned");
        assert_eq!(small.px, large.px, "and so is the left one");
        assert!(large.ph > small.ph, "only the height follows the rows");
        // A card too tall to sit at the pin is pushed up rather than off the bottom of the window.
        let tall = settings_card(60, ch, 640.0, 400.0);
        assert!(tall.py >= 8.0 && tall.py + tall.ph <= 400.0, "{tall:?} escapes the window", tall = (tall.py, tall.ph));
    }

    #[test]
    fn footer_hints_are_cut_at_a_word() {
        // Text that fits is untouched.
        assert_eq!(clip_words("enter confirms", 20), "enter confirms");
        assert_eq!(clip_words("enter confirms", 14), "enter confirms");
        // Too long: cut at a word boundary, marked — never a severed word like "esc can".
        let cut = clip_words("reset every setting here? · enter confirms · esc cancels", 30);
        assert!(cut.ends_with('…') && cut.chars().count() <= 30);
        assert!(!cut.contains("canc"), "no half word survives: {cut}");
        assert!(cut.trim_end_matches('…').ends_with(|c: char| c != ' '), "no space before the ellipsis");
        // A single word longer than the line is cut rather than vanishing entirely.
        let long = clip_words("supercalifragilistic", 8);
        assert_eq!(long.chars().count(), 8);
        assert!(long.ends_with('…'));
        // Degenerate widths don't panic and don't overflow.
        assert_eq!(clip_words("anything", 0), "");
        assert_eq!(clip_words("anything", 1), "…");
    }

    #[test]
    fn the_settings_thumb_spans_its_window_and_stays_inside_the_track() {
        const TRACK: f32 = 240.0;
        // Twelve of twenty-five rows visible: the thumb is that share, at the top when unscrolled
        // and flush with the track's end at the last window — a thumb that stopped short would say
        // "there is more below" when there isn't.
        let (top, h) = scroll_thumb(25, 12, 0, TRACK);
        assert_eq!(top, 0.0);
        assert!((h - TRACK * 12.0 / 25.0).abs() < 0.01);
        let (bottom, h2) = scroll_thumb(25, 12, 13, TRACK);
        assert_eq!(h2, h, "the thumb doesn't resize as it travels");
        assert!((bottom + h2 - TRACK).abs() < 0.01, "the last window ends flush");
        // Halfway is halfway.
        let (mid, _) = scroll_thumb(25, 12, 6, TRACK);
        assert!(mid > 0.0 && mid < bottom);

        // A very long list floors the thumb's height, and the floor comes out of the travel rather
        // than pushing the thumb through the end of the track.
        let (far, tiny) = scroll_thumb(4000, 12, 3988, TRACK);
        assert!(tiny >= 12.0, "floored, not a hairline");
        assert!((far + tiny - TRACK).abs() < 0.01, "still ends flush");

        // Nothing hidden (or nothing shown) has no travel and no thumb to place.
        assert_eq!(scroll_thumb(12, 12, 0, TRACK), (0.0, TRACK));
        assert_eq!(scroll_thumb(3, 12, 0, TRACK), (0.0, TRACK));
        assert_eq!(scroll_thumb(25, 0, 0, TRACK), (0.0, TRACK), "empty window, no division by zero");
        // An offset past the end clamps instead of running off.
        let (over, oh) = scroll_thumb(25, 12, 999, TRACK);
        assert!((over + oh - TRACK).abs() < 0.01);
    }

    #[test]
    fn settings_clicks_land_on_the_row_that_was_drawn_there() {
        // The card, the tab strip and the row walk all come from settings_card, so this pins the
        // hit-test's agreement with the drawing: no overlap, no off-by-one, nothing outside.
        const ROWS: usize = 8;
        let (cw, ch, fw, fh) = (9.0_f32, 20.0_f32, 1280.0_f32, 800.0_f32);
        let c = settings_card(ROWS, ch, fw, fh);
        let rows_y = c.py + SET_PAD + c.head;
        let mid = c.px + c.pw / 2.0;

        // Every row's own band resolves to that row, top edge included and bottom edge excluded.
        for i in 0..ROWS {
            let ry = rows_y + c.row_h * i as f32;
            assert_eq!(settings_row_index(mid, ry, ROWS, ch, fw, fh), Some(i));
            assert_eq!(settings_row_index(mid, ry + c.row_h - 0.5, ROWS, ch, fw, fh), Some(i));
        }
        // The tab strip and the space above the rows belong to no row...
        assert_eq!(settings_row_index(mid, c.py + SET_PAD, ROWS, ch, fw, fh), None);
        assert_eq!(settings_row_index(mid, rows_y - 0.5, ROWS, ch, fw, fh), None);
        // ...nor does the footer band, or anything outside the card's sides.
        assert_eq!(settings_row_index(mid, c.py + c.ph - 1.0, ROWS, ch, fw, fh), None);
        assert_eq!(settings_row_index(c.px - 1.0, rows_y + 2.0, ROWS, ch, fw, fh), None);
        assert_eq!(settings_row_index(c.px + c.pw, rows_y + 2.0, ROWS, ch, fw, fh), None);

        // The tab strip maps each label to its own tab, and the gaps between them to none.
        let tabs: Vec<String> = ["theme", "keys", "schemes"].iter().map(|s| s.to_string()).collect();
        let ty = c.py + SET_PAD + 1.0;
        let mut tx = c.px + SET_PAD;
        for (i, name) in tabs.iter().enumerate() {
            let tw = name.chars().count() as f32 * cw;
            assert_eq!(settings_tab_index(tx + tw / 2.0, ty, &tabs, ROWS, cw, ch, fw, fh), Some(i));
            tx += tw + SET_TAB_GAP;
            if i + 1 < tabs.len() {
                // Between two pills (each padded 6px) there is dead space that hits neither.
                assert_eq!(settings_tab_index(tx - SET_TAB_GAP / 2.0, ty, &tabs, ROWS, cw, ch, fw, fh), None);
            }
        }
        // A row-band y is never read as a tab, however far left the click is.
        assert_eq!(settings_tab_index(mid, rows_y + 2.0, &tabs, ROWS, cw, ch, fw, fh), None);
    }

    fn cell(ch: char) -> Cell {
        let c = Rgb { r: 0, g: 0, b: 0 };
        Cell { ch, fg: c, bg: c, bold: false, italic: false, underline: false, inverse: false, wide: false }
    }

    #[test]
    fn dark_brightening_lifts_dim_tags_and_keeps_grays_neutral() {
        let lum = |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
        // A dark red would disappear into the panel; brightening lifts it while keeping it red.
        let dark_red = Rgb { r: 0x66, g: 0x00, b: 0x00 };
        let lifted = brighten_for_dark(dark_red);
        assert!(lum(lifted) > lum(dark_red), "dim tag must get brighter: {lifted:?}");
        assert!(lifted.r > lifted.g && lifted.r > lifted.b, "and stay red: {lifted:?}");
        // A neutral gray stays neutral — no hue introduced by the saturation boost.
        let gray = brighten_for_dark(Rgb { r: 0x40, g: 0x40, b: 0x40 });
        assert!(
            gray.r.abs_diff(gray.g) <= 1 && gray.g.abs_diff(gray.b) <= 1,
            "gray must not gain a hue: {gray:?}"
        );
        // An already-bright color is not pushed past white.
        let bright = brighten_for_dark(Rgb { r: 0xff, g: 0xff, b: 0xff });
        assert_eq!(bright, Rgb { r: 0xff, g: 0xff, b: 0xff });
    }

    #[test]
    fn pane_chrome_insets_margin_at_edges_half_gap_inside() {
        // A single pane filling the content area gets MARGIN on every side.
        let full = Rect { x: 100, y: 0, w: 400, h: 300 };
        let (x, y, w, h) = pane_chrome_rect(full, 100, 500, 300);
        assert_eq!((x, y), (100.0 + MARGIN, MARGIN));
        assert_eq!((w, h), (400.0 - 2.0 * MARGIN, 300.0 - 2.0 * MARGIN));

        // The LEFT half of a vertical split: margin on the outer edges, half a gap on the split.
        let left = Rect { x: 100, y: 0, w: 200, h: 300 };
        let (_, _, lw, _) = pane_chrome_rect(left, 100, 500, 300);
        assert_eq!(lw, 200.0 - MARGIN - GAP / 2.0);
        // And its neighbour mirrors it, so the two share exactly one GAP.
        let right = Rect { x: 300, y: 0, w: 200, h: 300 };
        let (rx, _, rw, _) = pane_chrome_rect(right, 100, 500, 300);
        assert_eq!(rx, 300.0 + GAP / 2.0);
        assert_eq!(rx - (100.0 + MARGIN + lw), GAP, "neighbours share one gap");
        assert_eq!(rw, 200.0 - MARGIN - GAP / 2.0);
    }

    #[test]
    fn pr_status_colors_are_distinct_and_junk_is_none() {
        // The four states must be visually distinct (GitHub's green/gray/purple/red).
        let open = pr_color("open").unwrap();
        let merged = pr_color("merged").unwrap();
        let closed = pr_color("closed").unwrap();
        assert!(open.g > open.r && open.g > open.b, "open is green: {open:?}");
        assert!(merged.r > merged.g && merged.b > merged.g, "merged is purple: {merged:?}");
        assert!(closed.r > closed.g && closed.r > closed.b, "closed is red: {closed:?}");
        assert!(pr_color("draft").is_some());
        // An unknown status renders no badge rather than a wrong color.
        assert_eq!(pr_color("bogus"), None);
        assert_eq!(pr_color(""), None);
    }

    #[test]
    fn parses_tag_hex_and_rejects_junk() {
        assert_eq!(parse_hex_color("#ff8800"), Some(Rgb { r: 0xff, g: 0x88, b: 0x00 }));
        assert_eq!(parse_hex_color("ff8800"), Some(Rgb { r: 0xff, g: 0x88, b: 0x00 }));
        assert_eq!(parse_hex_color("#fff"), None);
        assert_eq!(parse_hex_color("#gggggg"), None);
        assert_eq!(parse_hex_color(""), None);
    }

    #[test]
    fn spinner_head_is_brightest_and_wraps() {
        // The lit spoke follows the frame, and every other spoke trails off behind it.
        for frame in 0..SPINNER_SPOKES {
            let head = spoke_alpha(frame, frame);
            assert!((head - 1.0).abs() < f32::EPSILON, "frame {frame} head should be full");
            for spoke in 0..SPINNER_SPOKES {
                assert!(spoke_alpha(frame, spoke) <= head + f32::EPSILON);
                assert!(spoke_alpha(frame, spoke) >= 0.24, "no spoke fully vanishes");
            }
        }
        // Frame 0's dimmest spoke is the one just ahead of the head (it wrapped all the way round).
        assert!(spoke_alpha(0, 1) < spoke_alpha(0, 7));
    }

    #[test]
    fn unread_label_caps_at_99_plus() {
        assert_eq!(unread_label(1), "1");
        assert_eq!(unread_label(42), "42");
        assert_eq!(unread_label(99), "99");
        // Past 99 the badge stops growing, so a runaway agent can't squeeze out the tab name.
        assert_eq!(unread_label(100), "99+");
        assert_eq!(unread_label(u32::MAX), "99+");
    }

    #[test]
    fn progress_rail_width_is_clamped() {
        assert_eq!(progress_rail_w(100.0, 0), 0.0); // 0% draws nothing at all
        assert_eq!(progress_rail_w(100.0, 50), 50.0);
        assert_eq!(progress_rail_w(100.0, 100), 100.0);
        assert_eq!(progress_rail_w(100.0, 255), 100.0, "over-100 reports clamp to the track");
        // A tiny percentage still shows a visible nub instead of a sub-pixel sliver.
        assert_eq!(progress_rail_w(100.0, 1), PROGRESS_RAIL);
        // A track narrower than the minimum nub never overflows it.
        assert_eq!(progress_rail_w(2.0, 1), 2.0);
    }

    #[test]
    fn accent_palette_picks_the_dark_surface_shade() {
        // 8 RGBA quads, dark to light; entry 4 (bytes 16..20) is SystemAccentColorLight2.
        let mut bytes = [0u8; 32];
        bytes[16..20].copy_from_slice(&[0x99, 0xeb, 0xff, 0xff]);
        assert_eq!(accent_from_palette(&bytes), Some(Rgb { r: 0x99, g: 0xeb, b: 0xff }));
        // A truncated / empty buffer yields None rather than reading past the end.
        assert_eq!(accent_from_palette(&bytes[..8]), None);
        assert_eq!(accent_from_palette(&[]), None);
    }

    #[test]
    fn near_black_accent_is_lifted_but_bright_one_is_kept() {
        let lum = |c: Rgb| (0.2126 * c.r as f32 + 0.7152 * c.g as f32 + 0.0722 * c.b as f32) / 255.0;
        let black = ensure_legible(Rgb { r: 0, g: 0, b: 0 });
        assert!(lum(black) >= 0.45, "black accent must be lifted, got {black:?}");
        let bright = Rgb { r: 0x60, g: 0xcd, b: 0xff };
        assert_eq!(ensure_legible(bright), bright, "a legible accent is untouched");
    }

    #[test]
    fn accent_choice_applies_and_resets() {
        let pinned = [0x12u8, 0xc4, 0x9a];
        set_accent(AccentChoice::Fixed(pinned));
        assert_eq!(accent(), Rgb { r: pinned[0], g: pinned[1], b: pinned[2] });
        // System resolves to a legible color (or falls back when the registry read fails).
        set_accent(AccentChoice::System);
        let sys = accent();
        assert!(sys.r as u16 + sys.g as u16 + sys.b as u16 > 0);
        // Default is cmux blue, and resetting must actually go back to it.
        set_accent(AccentChoice::Default);
        assert_eq!(accent(), ACCENT_FALLBACK);
    }

    #[test]
    fn on_accent_flips_with_fill_brightness() {
        // White text on the cmux blue; black on a bright accent a user might pin.
        assert_eq!(on_accent(ACCENT_FALLBACK), TEXT);
        assert_eq!(on_accent(Rgb { r: 0xff, g: 0xd3, b: 0x3a }), Rgb { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn mark_matches_ascii_case_insensitive() {
        let row: Vec<Cell> = "Hello".chars().map(cell).collect();
        let fold_q = |s: &str| s.chars().map(fold).collect::<Vec<char>>();
        // "lo" hits indices 3,4.
        assert_eq!(mark_matches(&row, &fold_q("lo")), vec![false, false, false, true, true]);
        // Uppercase query still matches the mixed-case cells (case-insensitive both ways).
        assert_eq!(mark_matches(&row, &fold_q("HELLO")), vec![true; 5]);
        // No occurrence, and a needle longer than the row: all false, no panic.
        assert_eq!(mark_matches(&row, &fold_q("xyz")), vec![false; 5]);
        assert_eq!(mark_matches(&row, &fold_q("helloo")), vec![false; 5]);
    }

    #[test]
    fn mark_matches_wide_char_row() {
        // The wide lead holds the char; the spacer holds ' '. A single-wide-char needle marks only
        // the lead — the renderer extends the highlight onto the spacer via the cell.wide rule.
        let mut lead = cell('中');
        lead.wide = true;
        let row = vec![lead, cell(' '), cell('x')];
        assert_eq!(mark_matches(&row, &[fold('中')]), vec![true, false, false]);
    }

    #[test]
    fn cursor_quads_by_shape() {
        let (x0, y0, cw, ch) = (10.0, 20.0, 8.0, 16.0);
        // Block (0,1,2) and any unknown >6: no strip — the block is drawn via the bg blend instead.
        for s in [0u8, 1, 2, 7, 255] {
            assert!(cursor_quads(s, x0, y0, cw, ch).is_empty(), "style {s} should be block (no strip)");
        }
        // Underline (3,4): one 2px strip pinned to the cell bottom, spanning the full cell width.
        for s in [3u8, 4] {
            assert_eq!(
                cursor_quads(s, x0, y0, cw, ch),
                vec![Quad { x0: 10.0, y0: 34.0, x1: 18.0, y1: 36.0 }],
                "style {s} underline strip"
            );
        }
        // Bar (5,6): one 2px vertical strip at the cell's left edge, spanning the full cell height.
        for s in [5u8, 6] {
            assert_eq!(
                cursor_quads(s, x0, y0, cw, ch),
                vec![Quad { x0: 10.0, y0: 20.0, x1: 12.0, y1: 36.0 }],
                "style {s} bar strip"
            );
        }
    }

    #[test]
    fn clamp_rect_guards_degenerate() {
        assert_eq!(clamp_rect(0, 0, 10, 10, 20, 20), Some((0, 0, 10, 10))); // fits
        assert_eq!(clamp_rect(5, 5, 100, 100, 20, 20), Some((5, 5, 15, 15))); // overhang clamps
        assert_eq!(clamp_rect(20, 0, 5, 5, 20, 20), None); // origin at edge
        assert_eq!(clamp_rect(0, 20, 5, 5, 20, 20), None);
        assert_eq!(clamp_rect(0, 0, 0, 5, 20, 20), None); // zero-sized
        assert_eq!(clamp_rect(0, 0, 1, 1, 1, 1), Some((0, 0, 1, 1))); // 1×1 target fits
        assert_eq!(clamp_rect(1, 0, 1, 1, 1, 1), None); // offset on a 1×1 target
    }

    #[test]
    fn scrollbar_thumb_positions() {
        // rows == history: the thumb is half the track and slides between bottom and top.
        assert_eq!(scrollbar_thumb(100.0, 10, 10, 0), (50.0, 100.0)); // live screen -> bottom
        assert_eq!(scrollbar_thumb(100.0, 10, 10, 10), (0.0, 50.0)); // deepest history -> top
        assert_eq!(scrollbar_thumb(100.0, 10, 10, 5), (25.0, 75.0)); // midway

        // A tiny visible fraction floors the thumb height at 24px, still pinned to the bottom at 0.
        let (t, b) = scrollbar_thumb(100.0, 1, 100, 0);
        assert_eq!(b - t, 24.0);
        assert_eq!(b, 100.0);

        // history == 0 degenerates to a full-height thumb pinned to the bottom (no divide-by-zero).
        assert_eq!(scrollbar_thumb(100.0, 10, 0, 0), (0.0, 100.0));

        // A track shorter than the 24px floor caps the thumb at the track height (stays in bounds).
        let (t, b) = scrollbar_thumb(10.0, 10, 10, 0);
        assert!(t >= 0.0 && b <= 10.0 && b > t, "thumb {t}..{b} within a 10px track");
    }

    #[test]
    fn render_frame_into_1x1_does_not_panic() {
        // Minimize regression: a full frame (pane + search bar) into a 1×1 surface must not trip
        // the wgpu "Scissor Rect not contained in the render target" panic.
        let Some(r) = Renderer::new_headless(wgpu::TextureFormat::Rgba8Unorm, 18.0) else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("gmux-1x1"),
            size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let snap = PaneSnapshot { cells: vec![vec![cell('h'), cell('i')]], cursor: (0, 0), cols: 2, rows: 1, cursor_style: 0 };
        let pv = PaneView {
            snap: &snap,
            attention: Attention::default(),
            active: true,
            rect: Rect { x: 0, y: 0, w: 1, h: 1 },
            scrolled: 0,
            history: 0,
            title: "t".into(),
            selection: None,
            show_close: false,
            drop_target: false,
            dragging: false,
        };
        let sb = SearchBar { label: "find:".into(), query: "hi".into(), current: 1, total: 1, overlay_only: false };
        r.render_frame(&view, &[], 0, &[pv], 1, 1, "", false, None, None, Some(&sb), None, None, None);
        let _ = r.device.poll(wgpu::PollType::wait_indefinitely());
    }
}
