//! The thin-client windowed app: a winit window + wgpu surface that renders the **daemon's** panes
//! (fetched over the pipe each frame) and forwards input/control to the daemon. The daemon owns the
//! panes, so closing this window detaches — the agents keep running — and relaunching reattaches.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gmux_mux::{Attention, Cell, PaneSnapshot, Rect, Rgb};
use gmux_notify::{flash_window, Notifier, ProgressState as NProgress, Taskbar, ToastRequest, Urgency as NUrgency};
use gmux_proto::{Call, GridWire, LayoutWire, LinkWire, NotifyWire, PaneRectWire, ResultBody, CELL_BOLD, CELL_INVERSE, CELL_ITALIC, CELL_UNDERLINE, CELL_WIDE, MOUSE_DRAG, MOUSE_MOTION, MOUSE_SGR};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::config::{config_path, default_template, Action, Config, Keymap};
use crate::daemon_client::{spawn_output_subscriber, DaemonClient};
use crate::renderer::{
    GroupHeader, PaletteView, PaneView, SearchBar, SettingsRow, SettingsView, SidebarItem, SidebarRow,
    SPINNER_STEP_MS,
};
use crate::Renderer;

// The Windows clipboard helper lives in its own file but is declared here (a child of `app`) so the
// crate root (`lib.rs`, owned by the renderer task) needs no edit. ponytail: `#[path]` over a
// `lib.rs` mod line, to avoid touching a file another task is editing.
#[path = "clipboard.rs"]
mod clipboard;

/// Fallback font size when `config.font_px` is unset.
const DEFAULT_FONT_PX: f32 = 18.0;
/// Theme defaults (match the renderer's baked-in look): sidebar text and window background.
const DEFAULT_FG: [u8; 3] = [0xcc, 0xcc, 0xcc];
const DEFAULT_BG: [u8; 3] = [0x08, 0x08, 0x08]; // 0.03 * 255 ≈ 8
const TOAST_GROUP: &str = "gmux-session";
const TOAST_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const FRAME: Duration = Duration::from_millis(33); // ~30 fps poll of remote grids while active
/// How long after the last activity (input or a damage/notification wake) the loop keeps polling
/// at `FRAME` before dropping to the idle heartbeat.
const ACTIVE_WINDOW: Duration = Duration::from_millis(500);
/// Idle cadence: resend geometry, hot-reload config, and check the subscriber thread this often —
/// with no per-tick redraw, so an idle GUI stops fetching grids entirely.
const HEARTBEAT: Duration = Duration::from_secs(1);
const RESIZE_THROTTLE: Duration = Duration::from_millis(33); // ~30 resize sends/s while dragging
/// Below this surface dimension (px) the window is treated as minimized: skip reconfigure/render so
/// wgpu's scissor-rect validation can't panic on a degenerate (~1x1) render target.
const MIN_SURFACE: u32 = 16;
/// Consecutive same-cell clicks within this window escalate single → double (word) → triple (line).
const CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Gap (px) the renderer draws between split panes. ponytail: mirrored from renderer.rs's `GAP`
/// design token (owned by the other task) so the divider hit-test can grab that visible band — if
/// it changes there, mirror it here.
const GAP: f32 = 4.0;
// Pane-chrome design tokens, mirrored from renderer.rs (owned by the other task) so mouse
// selection maps pixels to the SAME cell grid the renderer draws. ponytail: duplicated constants,
// not a shared module — a handful of numbers; if they change in the renderer, mirror them here.
// The cell area origin is `rect + margin + BORDER + (INSET, TITLE_STRIP+INSET)`; see `pixel_to_cell`.
const MARGIN: f32 = 8.0; // outer margin at a content boundary (GAP/2 at an interior split edge)
const BORDER: f32 = 1.0; // pane border (the 2px attention ring is ignored — 1px error at most)
const INSET: f32 = 8.0; // cell-area inset inside the border
const TITLE_STRIP: f32 = 22.0; // title band inside the border, above the cells
const SEARCH_BAR: f32 = 22.0; // search band inside the border, covering the bottom of the cells
const SCROLLBAR_W: f32 = 8.0; // scrollback scrollbar strip at the cell-area right edge (mirrors renderer.rs)
// Font-zoom bounds. ponytail: mirror of renderer.rs's atlas clamp (8..=40) — the atlas won't
// rasterize outside this range, so the GUI clamps to the same window before calling set_font_px.
const FONT_MIN: f32 = 8.0;
const FONT_MAX: f32 = 40.0;

// Sidebar row hit-test metrics. ponytail: hardcoded here to mirror the renderer's design tokens
// (16px top padding, ~20px "WORKSPACES" section label, 48px rows, 4px gaps). The renderer (owned by
// the other task) is the source of truth for the visuals; this is a deliberate shared-constant
// divergence — if those tokens change there, mirror them here. The clickable sidebar *width* is
// still read live from the renderer via `areas()`, so only the vertical row math is duplicated.

pub struct App {
    mods: ModifiersState,
    state: Option<State>,
    /// Cloned into each subscriber thread to wake the loop (`send_event(())`); stashed here so
    /// `resumed` can hand a clone to the `State` it builds.
    proxy: EventLoopProxy<()>,
}

/// The daemon connection. `Connecting` holds the background connect thread's result channel so
/// startup never blocks the window paint; `Ready` is the live client. `call`/`control` degrade to
/// an error / no-op while connecting, so the render and input paths need no special-casing.
enum Client {
    Connecting(Receiver<io::Result<DaemonClient>>),
    Ready(DaemonClient),
}

impl Client {
    fn ready(&mut self) -> Option<&mut DaemonClient> {
        match self {
            Client::Ready(c) => Some(c),
            Client::Connecting(_) => None,
        }
    }
    fn call(&mut self, call: Call) -> Result<ResultBody, String> {
        match self.ready() {
            Some(c) => c.call(call),
            None => Err("daemon still connecting".to_string()),
        }
    }
    fn control(&mut self, call: Call) {
        if let Some(c) = self.ready() {
            c.control(call);
        }
    }
}

/// An in-progress divider drag: which pane to grow, along which axis, and the throttle/accum state.
struct Drag {
    /// Target pane (the top/left side of the dragged divider); grows as the divider moves.
    pane: u64,
    /// Vertical divider (drag along x, adjusts a horizontal split) vs horizontal (drag along y).
    vertical: bool,
    /// Combined pixel extent of the two adjacent panes along the drag axis, for px→ratio scaling.
    /// ponytail: approximate (uses the neighbour pair, not the exact split area) — good enough for
    /// an interactive drag the daemon re-lays-out every frame; exact area math isn't worth it.
    span: f32,
    /// Cursor position (physical px) at the last sent delta; the next delta measures from here.
    origin: (f64, f64),
    /// Last send time, for the ~30/s throttle.
    last_send: Instant,
}

/// A divider grabbed by the drag hit-test.
struct Divider {
    pane: u64,
    vertical: bool,
    span: f32,
}

/// Hit-test the gap bands between the (edge-to-edge) cached pane rects. `(cx, cy)` is in
/// content-area coords (sidebar offset already removed). Returns the divider within `tol` px of a
/// shared pane boundary — the top/left pane is the resize target — or `None`. Pure, so unit-tested.
fn divider_at(panes: &[PaneRectWire], cx: f32, cy: f32, tol: f32) -> Option<Divider> {
    for l in panes {
        for r in panes {
            if l.id == r.id {
                continue;
            }
            // Vertical divider: `l` directly left of `r` (shared edge), y-ranges overlap the cursor.
            if l.x + l.w == r.x {
                let bx = (l.x + l.w) as f32;
                let y0 = l.y.max(r.y) as f32;
                let y1 = (l.y + l.h).min(r.y + r.h) as f32;
                if (cx - bx).abs() <= tol && cy >= y0 && cy < y1 {
                    return Some(Divider { pane: l.id, vertical: true, span: (l.w + r.w) as f32 });
                }
            }
            // Horizontal divider: `l` directly above `r`.
            if l.y + l.h == r.y {
                let by = (l.y + l.h) as f32;
                let x0 = l.x.max(r.x) as f32;
                let x1 = (l.x + l.w).min(r.x + r.w) as f32;
                if (cy - by).abs() <= tol && cx >= x0 && cx < x1 {
                    return Some(Divider { pane: l.id, vertical: false, span: (l.h + r.h) as f32 });
                }
            }
        }
    }
    None
}

/// A text selection in one pane: an anchor cell and the dragged-to cell, both in viewport cell
/// coords. Kept un-normalized (start = the press cell); `normalize_selection` orders them for
/// rendering and copy.
struct Selection {
    pane: u64,
    start: (u16, u16),
    end: (u16, u16),
}

/// The settings overlay's state (Ctrl+,).
struct SettingsState {
    /// 0 = theme, 1 = keys, 2 = schemes.
    tab: usize,
    sel: usize,
    /// A colour scheme being tried on: its palette is live in every pane but nothing is written to
    /// disk yet. Enter keeps it, Escape (or leaving the tab) restores the config's own palette.
    preview: Option<String>,
    /// Rebinding: the next chord pressed becomes this action's binding.
    capturing: bool,
}

/// Accent choices the theme tab cycles through, in order. "default" is gmux's own accent and
/// "system" follows Windows; the rest are common terminal accents, so the cycle is useful without
/// a colour picker.
const ACCENT_CYCLE: &[&str] = &["default", "system", "#3b8ae6", "#8f7ae6", "#4bb58a", "#d98a4b", "#c9566d"];

/// The settings panel's sections, in tab-strip order (`SettingsState::tab` indexes this).
const SETTINGS_TABS: [&str; 3] = ["theme", "keys", "schemes"];

/// A colour scheme's preview ribbon, in the renderer's colour type.
fn swatch_rgb(name: &str) -> Vec<Rgb> {
    crate::config::preset_swatch(name).into_iter().map(|[r, g, b]| Rgb { r, g, b }).collect()
}

/// A press on a pane's title strip that may become a pane rearrange once the cursor moves past the
/// threshold. Dropping on another pane swaps the two.
struct PaneDrag {
    from: u64,
    start: (f64, f64),
    dragging: bool,
    /// Pane currently under the cursor (the swap partner), if it isn't the dragged one.
    over: Option<u64>,
}

/// A sidebar row press that may become a tab reorder once the cursor moves past the threshold.
struct SidebarDrag {
    from_row: usize,
    start_y: f64,
    reordering: bool,
    /// Sidebar item index the drop would land on (`item_meta.len()` = past the end), tracked while
    /// dragging so the renderer can show the indicator before the user commits.
    over: Option<usize>,
}

/// The pane id from a toast launch arg ("pane=5" -> 5); `None` for non-pane args ("welcome").
/// Pure/tested.
fn parse_activation_pane(arg: &str) -> Option<u64> {
    arg.strip_prefix("pane=").and_then(|n| n.parse().ok())
}

/// Keyboard copy mode: a cell cursor the user drives with arrows/hjkl, an optional selection
/// anchor (v), copy on y/Enter, exit on Escape. Coordinates are viewport cells of the active pane
/// at its current scroll offset; scrolling shifts both so the marked CONTENT stays selected.
struct CopyModeState {
    cursor: (u16, u16),
    anchor: Option<(u16, u16)>,
}

/// The open command palette: a typed filter and the highlighted row. The item list itself is
/// rebuilt from the query on every keystroke/render (stateless — tabs can change under it).
struct PaletteState {
    query: String,
    selected: usize,
}

/// One palette entry's payload.
#[derive(Clone)]
enum PaletteCmd {
    Act(Action),
    Tab(usize),
}

/// Case-insensitive subsequence match ("spl h" chars all appear in order in "split horizontal").
/// `_` and `-` are dropped alongside whitespace, so typing the config's action name (`split_h`,
/// which is what a user who has edited `gmux.json` knows) matches the palette's "split h" label
/// instead of silently filtering everything out. Pure/tested.
fn fuzzy_match(hay: &str, needle: &str) -> bool {
    let skip = |c: &char| c.is_whitespace() || *c == '_' || *c == '-';
    let mut h = hay.chars().flat_map(char::to_lowercase).filter(|c| !skip(c));
    needle
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !skip(c))
        .all(|n| h.any(|c| c == n))
}

/// What a rendered sidebar item stands for, so a click on item `i` knows what it hit. Parallel to
/// the `SidebarItem` list handed to the renderer.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ItemMeta {
    /// A group header; toggling it collapses/expands that group.
    Header(String),
    /// A workspace row, carrying its index among the VISIBLE rows (already windowed).
    Row(usize),
}

/// Format a pressed chord the way `gmux.json` writes it (`ctrl+shift+d`, `alt+left`), for the
/// settings panel's rebinding capture. Returns `None` for a chord the config parser would reject —
/// notably one with no modifier, which would swallow that key before it reached the pane. Mirrors
/// the token vocabulary of `config::parse_chord`. Pure/tested.
fn chord_string(mods: ModifiersState, key: &Key) -> Option<String> {
    let mut parts = Vec::new();
    if mods.control_key() {
        parts.push("ctrl");
    }
    if mods.shift_key() {
        parts.push("shift");
    }
    if mods.alt_key() {
        parts.push("alt");
    }
    if mods.super_key() {
        parts.push("super");
    }
    if parts.is_empty() {
        return None; // a bare key would eat normal typing
    }
    let name = match key {
        Key::Named(NamedKey::ArrowLeft) => "left".to_string(),
        Key::Named(NamedKey::ArrowRight) => "right".to_string(),
        Key::Named(NamedKey::ArrowUp) => "up".to_string(),
        Key::Named(NamedKey::ArrowDown) => "down".to_string(),
        Key::Named(NamedKey::PageUp) => "pageup".to_string(),
        Key::Named(NamedKey::PageDown) => "pagedown".to_string(),
        Key::Named(NamedKey::Home) => "home".to_string(),
        Key::Named(NamedKey::End) => "end".to_string(),
        Key::Character(c) => {
            let c = c.to_lowercase();
            // Only single characters are bindable; the parser reads one char per token.
            if c.chars().count() != 1 {
                return None;
            }
            c
        }
        _ => return None,
    };
    parts.push(&name);
    Some(parts.join("+"))
}

/// Whether a workspace row survives the sidebar filter. Matches the same fuzzy subsequence the
/// command palette uses, against the workspace NAME and its git BRANCH — filtering by branch is
/// the case that matters when several workspaces share a project name. Pure/tested.
fn row_matches_filter(name: &str, branch: Option<&str>, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    fuzzy_match(name, query) || branch.is_some_and(|b| fuzzy_match(b, query))
}

/// What a reorder drop onto sidebar item `over` means: `(visible row index to move to, the group
/// to file the dragged window under)`.
///
/// Grouping reorders rows visually, so "where it looks like it will land" and "the daemon's tab
/// order" are different things — the drop target's own group is what the gesture should mean.
/// Dropping on a header files the window into that group and lands it on the group's first member,
/// stopping at the next header so an EMPTY group can't borrow a neighbour's row. Past the end
/// appends, ungrouped. Pure/tested.
fn drop_decision(
    meta: &[ItemMeta],
    row_groups: &[Option<String>],
    row_count: usize,
    over: usize,
) -> (usize, Option<String>) {
    match meta.get(over) {
        Some(ItemMeta::Row(vi)) => (*vi, row_groups.get(*vi).cloned().flatten()),
        Some(ItemMeta::Header(g)) => {
            let first = meta[over + 1..]
                .iter()
                .take_while(|m| matches!(m, ItemMeta::Row(_)))
                .find_map(|m| match m {
                    ItemMeta::Row(vi) => Some(*vi),
                    ItemMeta::Header(_) => None,
                })
                .unwrap_or(row_count.saturating_sub(1));
            (first, Some(g.clone()))
        }
        None => (row_count.saturating_sub(1), None),
    }
}

/// Fold `rows` (with each row's group) into the renderer's item list plus the parallel meta list.
///
/// Ungrouped workspaces come first at the root, the way cmux lists them; each group then gets a
/// header followed by its members, and a collapsed group contributes its header alone (carrying the
/// member count and the group's summed unread, so a collapsed group can still shout). Groups appear
/// in first-seen order, so the sidebar doesn't reshuffle when a window is renamed. Pure/tested.
fn sidebar_items(
    rows: Vec<SidebarRow>,
    groups: &[Option<String>],
    collapsed: &HashSet<String>,
    hover_item: Option<usize>,
) -> (Vec<SidebarItem>, Vec<ItemMeta>) {
    // Group name -> the visible-row indices under it, in order; `None` collects the ungrouped.
    let mut order: Vec<String> = Vec::new();
    let mut members: HashMap<String, Vec<usize>> = HashMap::new();
    let mut ungrouped: Vec<usize> = Vec::new();
    for (i, _) in rows.iter().enumerate() {
        match groups.get(i).and_then(|g| g.clone()) {
            Some(g) => {
                if !members.contains_key(&g) {
                    order.push(g.clone());
                }
                members.entry(g).or_default().push(i);
            }
            None => ungrouped.push(i),
        }
    }

    // `rows` is consumed as we place each index exactly once, so take them out by index.
    let mut slots: Vec<Option<SidebarRow>> = rows.into_iter().map(Some).collect();
    let mut items = Vec::new();
    let mut meta = Vec::new();
    fn push_row(slots: &mut [Option<SidebarRow>], idx: usize, items: &mut Vec<SidebarItem>, meta: &mut Vec<ItemMeta>) {
        if let Some(row) = slots[idx].take() {
            items.push(SidebarItem::Row(row));
            meta.push(ItemMeta::Row(idx));
        }
    }
    for idx in ungrouped {
        push_row(&mut slots, idx, &mut items, &mut meta);
    }
    for name in order {
        let idxs = members.remove(&name).unwrap_or_default();
        let is_collapsed = collapsed.contains(&name);
        let unread = idxs.iter().filter_map(|i| slots[*i].as_ref()).map(|r| r.unread).sum();
        items.push(SidebarItem::Header(GroupHeader {
            name: name.clone(),
            collapsed: is_collapsed,
            members: idxs.len(),
            unread,
            hover: hover_item == Some(items.len()),
        }));
        meta.push(ItemMeta::Header(name));
        if !is_collapsed {
            for idx in idxs {
                push_row(&mut slots, idx, &mut items, &mut meta);
            }
        }
    }
    (items, meta)
}

/// Build the palette's filtered item list: recently-run actions first, then every "tab: NAME"
/// entry, then the remaining default-bound actions ("split h" style labels, chord hints).
/// Pure/tested.
fn palette_items(tab_names: &[String], query: &str, recents: &[String]) -> Vec<(String, String, PaletteCmd)> {
    let mut out: Vec<(String, String, PaletteCmd)> = Vec::new();
    for (i, name) in tab_names.iter().enumerate() {
        let label = format!("tab: {name}");
        if fuzzy_match(&label, query) {
            out.push((label, format!("alt+{}", i + 1), PaletteCmd::Tab(i)));
        }
    }
    for (name, chord, action) in crate::config::default_bindings() {
        // The palette itself is noise in its own list.
        if matches!(action, Action::CommandPalette) {
            continue;
        }
        let label = name.replace('_', " ");
        if fuzzy_match(&label, query) {
            out.push((label, chord.to_string(), PaletteCmd::Act(*action)));
        }
    }
    // Recency-first, stable: a label's position in `recents` (most-recent first) wins.
    out.sort_by_key(|(label, _, _)| recents.iter().position(|r| r == label).unwrap_or(usize::MAX));
    out
}

/// A close gesture waiting on confirmation because the target has running child processes
/// (a build, an agent). Enter proceeds; Escape or any other key cancels.
enum ConfirmClose {
    /// Close this specific pane (Ctrl+Shift+W, or a title-strip close button). Carries the id
    /// because the button can target a pane that is NOT the active one — confirming a bare
    /// "close the active pane" would then kill the wrong one.
    Pane(u64),
    /// Middle-click on the sidebar tab with this stable window id.
    Window(u64),
}

/// Active in-terminal search (Ctrl+Shift+F). `matches` are scroll offsets from `Call::SearchPane`
/// (nearest-to-bottom first, directly usable as `GetGrid.offset`); `current` indexes into them.
struct SearchState {
    query: String,
    matches: Vec<u32>,
    current: usize,
}

/// Active sidebar tab rename (started by double-clicking a row). `id` is the stable `WindowId`;
/// `buffer` is the edited name. While `Some`, all keyboard input builds the buffer (mutually
/// exclusive with search). Committed via `Call::RenameWindow` on Enter, dropped on Escape.
struct RenameState {
    id: u64,
    buffer: String,
}

/// Whether `row` was clicked twice within `CLICK_INTERVAL` (a sidebar double-click starts a
/// rename). Pure, so unit-tested.
fn sidebar_double_click(last: Option<(usize, Instant)>, row: usize, now: Instant) -> bool {
    matches!(last, Some((r, t)) if r == row && now.duration_since(t) < CLICK_INTERVAL)
}

/// A detected http/https URL in a pane's viewport: the inclusive cell column range on `row` and the
/// URL text. Rebuilt each render for Ctrl+click hit-testing; the cells are also underlined in-place.
#[derive(Clone)]
struct UrlSpan {
    row: u16,
    start: u16,
    end: u16,
    url: String,
}

/// Length (in chars) of an `http://` / `https://` scheme at the start of `s`, else 0.
fn url_scheme_len(s: &[char]) -> usize {
    const HTTPS: [char; 8] = ['h', 't', 't', 'p', 's', ':', '/', '/'];
    const HTTP: [char; 7] = ['h', 't', 't', 'p', ':', '/', '/'];
    if s.starts_with(&HTTPS) {
        8
    } else if s.starts_with(&HTTP) {
        7
    } else {
        0
    }
}

/// Find http/https URLs in a row of chars. Returns `(start, end_exclusive)` column spans: a scheme
/// followed by a run of non-space chars, with trailing `.,);]` trimmed (so a link at a sentence end
/// doesn't swallow the period). Column == char index because each cell is exactly one char (wide
/// spacers included). Pure/tested.
fn find_urls(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let scheme = url_scheme_len(&chars[i..]);
        if scheme == 0 {
            i += 1;
            continue;
        }
        let mut j = i + scheme;
        while j < chars.len() && !chars[j].is_whitespace() {
            j += 1;
        }
        while j > i + scheme && matches!(chars[j - 1], '.' | ',' | ';' | ')' | ']') {
            j -= 1;
        }
        // A bare scheme (a URL wrapped right after "https://") isn't clickable content.
        if j > i + scheme {
            spans.push((i, j));
        }
        i = j;
    }
    spans
}

/// Scan a snapshot for URLs: underline each URL's cells (always-on affordance) and return the spans
/// for Ctrl+click hit-testing. Mutates `snap` in place.
fn detect_urls(snap: &mut PaneSnapshot) -> Vec<UrlSpan> {
    let mut spans = Vec::new();
    for (r, row) in snap.cells.iter_mut().enumerate() {
        let chars: Vec<char> = row.iter().map(|c| c.ch).collect();
        for (s, e) in find_urls(&chars) {
            for cell in row[s..e].iter_mut() {
                cell.underline = true;
            }
            spans.push(UrlSpan {
                row: r as u16,
                start: s as u16,
                end: (e - 1) as u16, // inclusive end column (e > s always)
                url: chars[s..e].iter().collect(),
            });
        }
    }
    spans
}

/// The URL under cell `(col, row)` in a pane's span list, if any. Pure/tested.
fn url_at(spans: &[UrlSpan], col: u16, row: u16) -> Option<&str> {
    spans.iter().find(|s| s.row == row && col >= s.start && col <= s.end).map(|s| s.url.as_str())
}

/// Whether an OSC-8 hyperlink URI is safe to open on Ctrl+click: only http/https/mailto
/// (case-insensitive). A `file://` or custom-scheme URI from untrusted terminal content must NOT
/// reach explorer.exe, so anything else is dropped at merge time. Pure/tested.
fn link_scheme_ok(uri: &str) -> bool {
    let u = uri.to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:")
}

/// Convert wire OSC-8 hyperlink spans to `UrlSpan`s, dropping any whose scheme `link_scheme_ok`
/// rejects (the sanitize step of the cross-agent contract). `end` is inclusive on both sides.
fn links_to_spans(links: &[LinkWire]) -> Vec<UrlSpan> {
    links
        .iter()
        .filter(|l| link_scheme_ok(&l.uri))
        .map(|l| UrlSpan { row: l.row, start: l.start, end: l.end, url: l.uri.clone() })
        .collect()
}

/// Underline the cells covered by `spans` in `snap` (OSC-8 hyperlinks get the same always-on
/// underline affordance as detected URLs). Out-of-range rows/cols are skipped, so a stale or
/// oversized wire span can't panic the render. Pure/tested.
fn underline_spans(snap: &mut PaneSnapshot, spans: &[UrlSpan]) {
    for s in spans {
        let Some(row) = snap.cells.get_mut(s.row as usize) else { continue };
        let start = s.start as usize;
        if start >= row.len() {
            continue;
        }
        let end = (s.end as usize).min(row.len() - 1);
        for cell in &mut row[start..=end] {
            cell.underline = true;
        }
    }
}

/// Merge heuristic-detected URL spans with explicit OSC-8 hyperlink spans, OSC-8 winning on
/// overlap: any detected span that intersects an OSC-8 span (same row, overlapping columns) is
/// dropped, then the OSC-8 spans are appended. `url_at` returns the first match, so the surviving
/// detected spans followed by the OSC-8 spans give explicit links precedence. ponytail: a detected
/// span is dropped whole on any intersection (not split at the boundary) — simplest correct rule.
fn merge_link_spans(detected: Vec<UrlSpan>, osc8: Vec<UrlSpan>) -> Vec<UrlSpan> {
    let mut out: Vec<UrlSpan> = detected
        .into_iter()
        .filter(|d| !osc8.iter().any(|o| o.row == d.row && d.start <= o.end && o.start <= d.end))
        .collect();
    out.extend(osc8);
    out
}

/// Wrap-around index step: move `current` by `dir` within `0..len`. `len == 0` stays 0. Pure/tested.
fn step_index(current: usize, len: usize, dir: i64) -> usize {
    if len == 0 {
        return 0;
    }
    let n = len as i64;
    (((current as i64 + dir) % n + n) % n) as usize
}

/// Clamp a requested font size to the atlas's rasterizable range (mirrors renderer.rs). Pure/tested.
fn clamp_font_px(px: f32) -> f32 {
    px.clamp(FONT_MIN, FONT_MAX)
}

/// Map a scrollbar-thumb cursor y to a scrollback offset. The track's top maps to the deepest
/// history (`history`), its bottom to the live screen (0). A zero-height track or empty history
/// yields 0. Pure/tested.
fn scrollbar_offset_at(cursor_y: f32, track_top: f32, track_h: f32, history: usize) -> usize {
    if track_h <= 0.0 || history == 0 {
        return 0;
    }
    let frac = ((cursor_y - track_top) / track_h).clamp(0.0, 1.0);
    ((1.0 - frac) * history as f32).round() as usize
}

/// Ask the user for a folder with the standard Windows picker (`IFileOpenDialog` in
/// pick-folders mode), returning its path. `None` on cancel or any COM failure.
///
/// Modal, so it blocks the event loop while open — acceptable because it only runs from an
/// explicit gesture (clicking '+ open workspace'), never from the render or heartbeat path.
#[cfg(windows)]
fn pick_folder() -> Option<String> {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, IShellItem, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    unsafe {
        // winit already put this thread in an STA, so no CoInitialize here.
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog.SetOptions(opts | FOS_PICKFOLDERS).ok()?;
        // A cancelled dialog returns an error HRESULT; that is the common path, not a fault.
        dialog.Show(None).ok()?;
        let item: IShellItem = dialog.GetResult().ok()?;
        let path = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = path.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(path.0 as *const _));
        s
    }
}

#[cfg(not(windows))]
fn pick_folder() -> Option<String> {
    None
}

/// Open a detected URL in the default browser. ponytail: `explorer.exe <url>` rather than the
/// settings' `cmd /c start` pattern — the URL is untrusted terminal content, and `cmd` shell-parses
/// `&`/`|` and expands `%VAR%`, so `cmd /c start "" http://x&calc` would run calc. `explorer` hands
/// the single arg straight to the http protocol handler with no shell parsing.
fn open_url(url: &str) {
    if let Err(e) = std::process::Command::new("explorer").arg(url).spawn() {
        eprintln!("gmux: could not open url {url}: {e}");
    }
}

/// Map a physical pixel `(px, py)` (window coords) to a `(col, row)` cell in a pane whose `rect` is
/// in WINDOW coords (sidebar offset already applied). Mirrors the renderer's per-pane chrome layout
/// exactly (margin/gap edges + border + title strip + inset), then clamps to the visible cell grid.
/// Pure, so unit-tested. ponytail: assumes the 1px border, not the 2px attention ring — a 1px error
/// at most, and selection lives on the focused pane, which has attention cleared.
fn pixel_to_cell(
    px: f32,
    py: f32,
    rect: Rect,
    sidebar_w: u32,
    surf_w: u32,
    surf_h: u32,
    cell_w: u32,
    cell_h: u32,
) -> (u16, u16) {
    let (ox, oy, ow, oh) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    let l = if rect.x <= sidebar_w { MARGIN } else { GAP / 2.0 };
    let t = if rect.y == 0 { MARGIN } else { GAP / 2.0 };
    let rgt = if rect.x + rect.w >= surf_w { MARGIN } else { GAP / 2.0 };
    let bot = if rect.y + rect.h >= surf_h { MARGIN } else { GAP / 2.0 };
    let ix = ox + l + BORDER + INSET;
    let iy = oy + t + BORDER + TITLE_STRIP + INSET;
    let iw = (ow - l - rgt - 2.0 * BORDER - 2.0 * INSET).max(cell_w as f32);
    let ih = (oh - t - bot - 2.0 * BORDER - TITLE_STRIP - 2.0 * INSET).max(cell_h as f32);
    let cols = (iw / cell_w as f32).floor().max(1.0);
    let rows = (ih / cell_h as f32).floor().max(1.0);
    let col = ((px - ix) / cell_w as f32).floor().clamp(0.0, cols - 1.0);
    let row = ((py - iy) / cell_h as f32).floor().clamp(0.0, rows - 1.0);
    (col as u16, row as u16)
}

/// A "word char" for double-click selection: alphanumeric plus the terminal-ish punctuation that
/// keeps paths/urls/flags intact (`_ - . / \ ~ : @ % + = ? & #`). Space is deliberately excluded,
/// so a wide glyph's ' ' spacer ends the word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | '\\' | '~' | ':' | '@' | '%' | '+' | '=' | '?' | '&' | '#')
}

/// Inclusive column span of the word at `col`: the maximal run of `is_word_char` cells covering it.
/// A non-word (or out-of-range) cell spans just itself. Pure, so unit-tested.
fn word_span(chars: &[char], col: usize) -> (usize, usize) {
    if col >= chars.len() || !is_word_char(chars[col]) {
        return (col, col);
    }
    let mut s = col;
    while s > 0 && is_word_char(chars[s - 1]) {
        s -= 1;
    }
    let mut e = col;
    while e + 1 < chars.len() && is_word_char(chars[e + 1]) {
        e += 1;
    }
    (s, e)
}

/// Quote a filesystem path for the shell if it contains spaces: wrap in double quotes with any
/// embedded double-quote doubled (PowerShell/cmd convention). Space-free paths pass through as-is.
/// Pure, so unit-tested.
fn quote_path(s: &str) -> String {
    if s.contains(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Order a selection's two endpoints into reading order (row-major), so `start <= end`. Matches the
/// renderer's `PaneView.selection` contract.
fn normalize_selection(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    // Compare by (row, col): a cell earlier in reading order sorts first.
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Rebuild the selected text from a grid. `start`/`end` are normalized (reading order) inclusive
/// viewport cells. Rows join with CRLF (the Windows clipboard convention); each row is trimmed of
/// trailing spaces. Indices are clamped to the grid so a stale selection can't panic. Pure/tested.
fn grid_selection_text(grid: &GridWire, start: (u16, u16), end: (u16, u16)) -> String {
    let cols = grid.cols as usize;
    let rows = grid.rows as usize;
    if cols == 0 || rows == 0 {
        return String::new();
    }
    let (r0, r1) = (start.1 as usize, (end.1 as usize).min(rows.saturating_sub(1)));
    let mut out: Vec<String> = Vec::new();
    for r in r0..=r1.min(rows - 1) {
        // First row starts at start.col; last row ends at end.col; middle rows span the full width.
        let c_start = if r == start.1 as usize { start.0 as usize } else { 0 };
        let c_end = if r == end.1 as usize { end.0 as usize } else { cols - 1 };
        let (c_start, c_end) = (c_start.min(cols - 1), c_end.min(cols - 1));
        let mut line = String::new();
        let mut skip_spacer = false;
        for c in c_start..=c_end {
            let cell = grid.cells.get(r * cols + c);
            if skip_spacer {
                // The blank cell after a wide (CJK) glyph is layout filler, not content.
                skip_spacer = false;
                continue;
            }
            skip_spacer = cell.map(|cell| cell.flags & gmux_proto::CELL_WIDE != 0).unwrap_or(false);
            line.push(cell.map(|cell| cell.ch).unwrap_or(' '));
        }
        line.truncate(line.trim_end_matches(' ').len());
        out.push(line);
    }
    out.join("\r\n")
}

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    client: Client,
    /// The `SetPalette` call to push once the daemon connection is live (computed from the startup
    /// config, so the daemon's pane colors match the renderer theme). Taken on first connect.
    init_palette: Option<Call>,
    active_pane: u64,
    focused: bool,
    /// True while the window is minimized (surface shrunk to a degenerate size): `render`
    /// early-returns so no frame is acquired at ~1x1. Cleared by the restore `Resized`.
    minimized: bool,
    /// Last window title pushed to the OS ("<pane> — gmux"); cached so we only `set_title` on a
    /// change (retitling re-enters the event loop on Windows).
    last_title: String,
    hwnd: isize,
    /// Last known cursor position (physical px), tracked from `CursorMoved` for click hit-testing.
    cursor: (f64, f64),
    /// In-progress divider drag (mouse-down on a split gap), or `None`.
    drag: Option<Drag>,
    /// Current pane text selection (rendered highlighted; copied on release / Ctrl+Shift+C).
    selection: Option<Selection>,
    /// True while a left-drag is actively building `selection` (press inside a pane's cell area).
    sel_dragging: bool,
    /// Consecutive-click tracking for double-click word / triple-click line select: the last
    /// press's `(pane, cell, time, count)`. A same-cell press within `CLICK_INTERVAL` escalates
    /// `count` 1 → 2 → 3 then wraps to 1.
    last_click: Option<(u64, (u16, u16), Instant, u8)>,
    /// Last sidebar row press `(row, time)`, for double-click (rename) detection — the pane-cell
    /// `last_click` above is keyed by pane/cell and doesn't apply to sidebar rows.
    last_sidebar_click: Option<(usize, Instant)>,
    /// A sidebar row press that may turn into a tab reorder, or `None`.
    sidebar_drag: Option<SidebarDrag>,
    /// A pane title-strip press that may turn into a pane rearrange, or `None`.
    pane_drag: Option<PaneDrag>,
    /// The active window's pane rectangles from the last rendered layout (content-area coords, i.e.
    /// before the sidebar-width offset), cached each frame for mouse hit-testing.
    last_panes: Vec<PaneRectWire>,
    /// Sidebar row count from the last layout, to bound a sidebar click's row index.
    tab_count: usize,
    /// Stable window ids per sidebar row from the last layout — middle-click closes by id, not
    /// index, so a window removed daemon-side since the last render can't shift the target.
    tab_ids: Vec<u64>,
    /// Tab names per sidebar row from the last layout, cached to seed a rename buffer with the
    /// current name (rename starts from a mouse gesture, outside the render that builds the rows).
    tab_names: Vec<String>,
    /// Index of the active tab from the last layout — what the keyboard rename/close act on.
    active_tab: usize,
    /// Each visible row's group from the last render, so a drop can tell which group it landed in.
    row_groups: Vec<Option<String>>,
    /// Visible sidebar row -> real tab index (see [`State::real_tab`]); rebuilt every render.
    row_tabs: Vec<usize>,
    /// The sidebar filter (Ctrl+Shift+K), or `None` when not filtering. While `Some`, keystrokes
    /// edit the query instead of reaching the pane — a modal like `search`/`rename`.
    sidebar_filter: Option<String>,
    /// What each rendered sidebar item was, from the last render — mouse handlers run between
    /// renders, so they hit-test against this rather than rebuilding the list.
    item_meta: Vec<ItemMeta>,
    /// Item heights from the last render, in the same order as `item_meta`; the hit-test walks
    /// these so a header (24px) and a row (48px) can't be confused for one another.
    item_heights: Vec<f32>,
    /// Group headers the user has collapsed (by name; a group that disappears just drops out).
    collapsed_groups: HashSet<String>,
    /// Per visible row: `(pr number, pr url, has a color rail)` from the last render — what a
    /// click on the PR chip needs, since the rows themselves aren't kept.
    row_pr: HashMap<usize, (u32, Option<String>, bool)>,
    /// Active sidebar tab rename (double-click a row), or `None`. While `Some`, all keyboard input
    /// edits the buffer instead of reaching the pane — mutually exclusive with `search`.
    rename: Option<RenameState>,
    notifier: Option<Notifier>,
    taskbar: Option<Taskbar>,
    last_toast: std::collections::HashMap<u64, Instant>,
    // Scrollback viewport per pane id (`pane_scroll[id]` = lines above the live tail; a missing
    // entry = 0 = live screen). Only scrolled panes have an entry, so the map doubles as the
    // "which panes are scrolled" set for the fetch gate; evicted alongside `snap_cache`.
    pane_scroll: HashMap<u64, usize>,
    /// Last-seen history depth per pane, to pin a scrolled viewport to CONTENT: when history grows
    /// under a scrolled pane, the offset is bumped by the growth (see the GetGrid accept block).
    last_history: HashMap<u64, usize>,
    // History depth + grid rows of the ACTIVE pane from the last GetGrid, for local wheel clamping
    // and page sizing. ponytail: active-only — the scrollbar/history chrome is active-pane-only, so
    // a non-active pane clamps at 0 locally and lets the server clamp its top on the next fetch.
    scroll_history: usize,
    /// True while the scrollback scrollbar thumb is being dragged (mouse-down on the thumb/track):
    /// cursor motion maps directly to the active pane's `pane_scroll` and other paths are skipped.
    scrollbar_drag: bool,
    /// Time of the last `DroppedFile`, to space-separate multiple files dropped in one drag (they
    /// arrive as separate events in quick succession).
    last_drop: Option<Instant>,
    active_rows: usize,
    /// Whether the active pane's app enabled bracketed paste (from the last GetGrid).
    active_bracketed: bool,
    /// The active pane's mouse-reporting mode (bitfield from the last GetGrid; 0 = no reporting,
    /// so the GUI keeps its own selection/drag). See `report_button`/`report_motion`/`report_wheel`.
    active_mouse_mode: u8,
    /// The button code (0/1/2) currently held for mouse reporting, so a drag reports it and a
    /// release matches its press even if the cursor left the pane. `None` when no reported press
    /// is outstanding.
    mouse_down: Option<u8>,
    /// Last cell a motion report was sent for, to suppress duplicate reports while the pointer
    /// stays in one cell (winit fires `CursorMoved` per pixel). Reset on press/release.
    mouse_last_cell: Option<(u16, u16)>,
    /// Wakes the loop from the subscriber thread (`send_event(())`).
    proxy: EventLoopProxy<()>,
    /// Pane ids that produced output since the last render (filled by the subscriber thread,
    /// taken/cleared each render to gate GetGrid fetches).
    damaged: Arc<Mutex<HashSet<u64>>>,
    /// Real (non-`pane-output`) notifications forwarded by the subscriber thread, drained to toasts.
    toast_rx: Receiver<NotifyWire>,
    /// Sender half, kept to clone into a respawned subscriber thread.
    toast_tx: Sender<NotifyWire>,
    /// Liveness flag of the current subscriber thread (`false` once it dies); `None` before the
    /// first spawn. Respawned from the heartbeat when dead.
    sub_alive: Option<Arc<AtomicBool>>,
    /// Cached last snapshot per pane id, reused for undamaged panes so a damage-gated render still
    /// hands the renderer a full views list. Evicted for panes gone from the layout.
    snap_cache: HashMap<u64, PaneSnapshot>,
    /// Last activity (input or a wake): within `ACTIVE_WINDOW` the loop keeps polling at `FRAME`.
    last_activity: Instant,
    /// Any workspace has a busy pane (from the last layout). While true — and ONLY while true —
    /// the loop wakes on the spinner cadence to animate it.
    any_busy: bool,
    /// When the spinner last stepped.
    last_spinner: Instant,
    /// Last idle-heartbeat time.
    last_heartbeat: Instant,
    /// Hash of the last layout's geometry; a change forces a full grid refetch (tab switch/split/resize).
    last_layout_hash: u64,
    /// Force a full refetch of every pane next render (first frame, resize, reconnect).
    force_full: bool,
    /// A Ready client that can no longer reach the daemon: the loop exits.
    fatal: bool,
    // Config-driven keybindings + the last config mtime we loaded, for hot-reload.
    keymap: Keymap,
    font_px: f32,
    /// The font size the config file asks for (last loaded). `font_px` may differ after a live
    /// Ctrl+wheel / zoom-action; `zoom_reset` snaps back to this, and a config hot-reload that
    /// changes it applies live.
    config_font_px: f32,
    config_mtime: Option<std::time::SystemTime>,
    /// Active in-terminal search (Ctrl+Shift+F), or `None`. While `Some`, keystrokes build the query
    /// instead of reaching the pane.
    search: Option<SearchState>,
    /// Current IME preedit (composition) string, drawn at the active pane's cursor; cleared on
    /// Commit/Disabled.
    preedit: Option<String>,
    /// A close gesture awaiting confirmation (the target has running children).
    confirm_close: Option<ConfirmClose>,
    /// The command palette overlay (Ctrl+Shift+P), or `None`.
    palette: Option<PaletteState>,
    /// The settings overlay (Ctrl+,), or `None`.
    settings: Option<SettingsState>,
    /// Keyboard copy mode (Ctrl+Shift+M), or `None`.
    copy_mode: Option<CopyModeState>,
    /// Last divider press `(pane, when)` — a second press within `CLICK_INTERVAL` equalizes.
    last_divider_click: Option<(u64, Instant)>,
    /// Recently-run palette action labels, most-recent first (cap 5) — ordered first in the list.
    palette_recent: Vec<String>,
    /// First visible sidebar row (tab overflow scrolling; wheel over the sidebar adjusts it).
    /// Clamped each render to the tab count and the rows that fit.
    sidebar_scroll: usize,
    /// A transient status line (e.g. "exported to ...") shown in the bottom band until it expires
    /// on a later redraw. Non-modal: it intercepts nothing.
    notice: Option<(String, Instant)>,
    /// The URL/hyperlink target under the cursor, tooltipped in the bottom band while hovering.
    hover_link: Option<String>,
    /// Cached `focus_follows_mouse` config flag (default off; hot-reloaded with the config).
    focus_follows_mouse: bool,
    /// Last pane the cursor hovered (focus-follows-mouse edge trigger).
    hover_pane: Option<u64>,
    /// Detected URL spans per pane (viewport cell coords), rebuilt each render for Ctrl+click.
    url_spans: HashMap<u64, Vec<UrlSpan>>,
    /// Cached OSC-8 hyperlink spans per pane (scheme-filtered), refreshed on each GetGrid fetch and
    /// reused for undamaged frames so the always-on underline doesn't flicker. Evicted with
    /// `snap_cache` (same live-pane set).
    link_cache: HashMap<u64, Vec<UrlSpan>>,
    /// M12 stage 2: the flag-gated WebView2 panel, hosted on THIS thread and parented to this
    /// window (see `gmux_browser::embedded`), shown in the right-hand dock.
    #[cfg(feature = "browser")]
    browser: Option<gmux_browser::EmbeddedBrowser>,
    /// Width of the browser panel in px; `0` = hidden. Subtracted from the content area in
    /// [`State::areas`], so the terminal panes reflow around it.
    dock_w: u32,
}

/// Run the gmux GUI. `_shell` is currently unused (the daemon picks its shell); kept for the CLI
/// signature and a future `--daemon <shell>` hand-off.
pub fn run(_shell: String) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App { mods: ModifiersState::empty(), state: None, proxy };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            el.create_window(Window::default_attributes().with_title("gmux")).expect("create window"),
        );
        // Let the OS route IME composition to this window: CJK/emoji-picker input then arrives as
        // `WindowEvent::Ime` (see the handler in `window_event`) instead of raw keystrokes.
        window.set_ime_allowed(true);
        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .expect("request adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gmux-gui"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface default config");
        config.format = format;
        surface.configure(&device, &config);

        // Load user config up front: font size feeds the atlas build, theme feeds the renderer.
        // Clamped like every later font path (zoom/reload) — an out-of-range config value would
        // otherwise render at e.g. 60px until the first zoom SHRINKS it to the 40px clamp.
        let user_config = Config::load();
        let font_px = clamp_font_px(user_config.font_px.unwrap_or(DEFAULT_FONT_PX));
        let keymap = Keymap::build(&user_config);
        let config_mtime = config_mtime();

        let mut renderer = Renderer::from_device(device, queue, format, font_px);
        apply_theme(&mut renderer, &user_config);

        // Attach to (or start) the daemon on a background thread: `connect_or_spawn` can block for
        // seconds (spawn + poll), which would freeze the window into a white "Not Responding" shell.
        // The window paints a cleared frame while `about_to_wait` polls this channel for the result.
        let (tx, connect_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(DaemonClient::connect_or_spawn("gmux"));
        });

        let hwnd = window_hwnd(&window).unwrap_or(0);
        // Fluent window chrome: dark titlebar, Mica backdrop, rounded Win11 corners, and a window
        // border tinted with the same accent the chrome uses. All best-effort — pre-Win11 DWM
        // rejects the attributes it doesn't know and the window just looks like it did before.
        if hwnd != 0 {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::Graphics::Dwm::{
                DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_SYSTEMBACKDROP_TYPE,
                DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
            };
            let set = |attr, value: u32| unsafe {
                let _ = DwmSetWindowAttribute(
                    HWND(hwnd as *mut _),
                    attr,
                    &value as *const _ as *const _,
                    std::mem::size_of::<u32>() as u32,
                );
            };
            set(DWMWA_USE_IMMERSIVE_DARK_MODE, 1);
            set(DWMWA_SYSTEMBACKDROP_TYPE, 2); // DWMSBT_MAINWINDOW (Mica)
            set(DWMWA_WINDOW_CORNER_PREFERENCE, 2); // DWMWCP_ROUND
            let a = crate::renderer::accent();
            set(DWMWA_BORDER_COLOR, (a.b as u32) << 16 | (a.g as u32) << 8 | a.r as u32); // COLORREF
        }
        let notifier = Notifier::new("com.gmux.app", "gmux").ok();
        let taskbar = if hwnd != 0 { Taskbar::new(hwnd) } else { None };

        // First launch ever: one welcome toast pointing at the two setup commands.
        if first_run(&state_dir()) {
            if let Some(nf) = &notifier {
                let _ = nf.show(&ToastRequest {
                    tag: "welcome".to_string(),
                    group: TOAST_GROUP.to_string(),
                    title: "gmux".to_string(),
                    body: "Run 'gmux hooks setup all' to get agent notifications, and 'gmux shell-integration --install' for prompt/cwd tracking.".to_string(),
                    urgency: NUrgency::Normal,
                    launch_arg: "welcome".to_string(),
                });
            }
        }

        // Real notifications flow from the subscriber thread to the main loop over this channel.
        let (toast_tx, toast_rx) = std::sync::mpsc::channel();
        let now = Instant::now();
        let st = State {
            window,
            surface,
            config,
            renderer,
            client: Client::Connecting(connect_rx),
            init_palette: Some(palette_call(&user_config)), // pushed once the connection is live
            active_pane: 0,
            focused: true,
            minimized: false,
            last_title: "gmux".to_string(), // the window is created with this title
            hwnd,
            cursor: (0.0, 0.0),
            drag: None,
            selection: None,
            sel_dragging: false,
            last_click: None,
            last_sidebar_click: None,
            sidebar_drag: None,
            pane_drag: None,
            last_panes: Vec::new(),
            tab_count: 0,
            tab_ids: Vec::new(),
            tab_names: Vec::new(),
            active_tab: 0,
            row_groups: Vec::new(),
            row_tabs: Vec::new(),
            sidebar_filter: None,
            item_meta: Vec::new(),
            item_heights: Vec::new(),
            collapsed_groups: HashSet::new(),
            row_pr: HashMap::new(),
            rename: None,
            notifier,
            taskbar,
            last_toast: std::collections::HashMap::new(),
            pane_scroll: HashMap::new(),
            last_history: HashMap::new(),
            scroll_history: 0,
            scrollbar_drag: false,
            last_drop: None,
            active_rows: 0,
            active_bracketed: false,
            active_mouse_mode: 0,
            mouse_down: None,
            mouse_last_cell: None,
            proxy: self.proxy.clone(),
            damaged: Arc::new(Mutex::new(HashSet::new())),
            toast_rx,
            toast_tx,
            sub_alive: None,
            snap_cache: HashMap::new(),
            last_activity: now,
            any_busy: false,
            last_spinner: now,
            last_heartbeat: now,
            last_layout_hash: 0,
            force_full: true,
            fatal: false,
            keymap,
            font_px,
            config_font_px: font_px,
            config_mtime,
            search: None,
            preedit: None,
            confirm_close: None,
            palette: None,
            settings: None,
            copy_mode: None,
            last_divider_click: None,
            palette_recent: Vec::new(),
            sidebar_scroll: 0,
            notice: None,
            hover_link: None,
            focus_follows_mouse: user_config.focus_follows_mouse.unwrap_or(false),
            hover_pane: None,
            url_spans: HashMap::new(),
            link_cache: HashMap::new(),
            #[cfg(feature = "browser")]
            browser: None,
            dock_w: 0,
        };
        // sync_size + palette are sent from `poll_connect` once the daemon answers.
        self.state = Some(st);
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(f) => {
                if let Some(st) = self.state.as_mut() {
                    st.focused = f;
                    if f {
                        st.mark_activity();
                        st.clear_active_toast();
                        flash_window(st.hwnd, false);
                        st.window.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(sz) => {
                if let Some(st) = self.state.as_mut() {
                    // Minimizing shrinks the surface to ~1x1; configuring + rendering at that size
                    // trips a wgpu scissor-rect validation panic. Skip the reconfigure/redraw while
                    // degenerate and flag it (render() also early-returns). The restore Resized
                    // (real size) clears the flag and forces a full refetch + redraw below.
                    if sz.width < MIN_SURFACE || sz.height < MIN_SURFACE {
                        st.minimized = true;
                        return;
                    }
                    st.minimized = false;
                    st.config.width = sz.width;
                    st.config.height = sz.height;
                    st.surface.configure(&st.renderer.device, &st.config);
                    st.sync_size();
                    st.sync_dock_bounds(); // the browser panel rides the window's right edge
                    st.force_full = true; // panes reflow at the new size: refetch every grid
                    st.mark_activity();
                    st.window.request_redraw();
                }
            }
            WindowEvent::Ime(ime) => {
                if let Some(st) = self.state.as_mut() {
                    match ime {
                        // Committed composition (a finished CJK/emoji sequence). While renaming it
                        // appends to the rename buffer, while searching to the query — never the
                        // pane; otherwise send it as text to the active pane, like typing — snap
                        // back to live and drop any selection.
                        Ime::Commit(text) => {
                            st.mark_activity();
                            st.preedit = None;
                            if let Some(p) = st.palette.as_mut() {
                                p.query.push_str(&text);
                                p.selected = 0;
                                st.window.request_redraw();
                            } else if let Some(r) = st.rename.as_mut() {
                                r.buffer.push_str(&text);
                                st.window.request_redraw();
                            } else if st.search.is_some() {
                                if let Some(s) = st.search.as_mut() {
                                    s.query.push_str(&text);
                                }
                                st.refresh_search();
                                st.window.request_redraw();
                            } else {
                                st.clear_selection();
                                st.set_scroll(st.active_pane, 0);
                                st.client.control(Call::SendKeys { pane: st.active_pane, text, enter: false });
                            }
                        }
                        // Composition in progress: stash the preedit string for the renderer to draw
                        // at the active pane's cursor. Empty clears it (composition cancelled).
                        Ime::Preedit(text, _cursor) => {
                            st.preedit = if text.is_empty() { None } else { Some(text) };
                            st.window.request_redraw();
                        }
                        Ime::Enabled => {}
                        Ime::Disabled => {
                            st.preedit = None;
                            st.window.request_redraw();
                        }
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(st) = self.state.as_mut() {
                    // Park the cursor off-window so hover highlights clear, and end any drag —
                    // stale hover otherwise stays lit until the cursor re-enters. The selection
                    // highlight itself is kept (still copyable via Ctrl+Shift+C).
                    st.cursor = (-1.0, -1.0);
                    st.drag = None;
                    st.sel_dragging = false;
                    st.sidebar_drag = None;
                    st.scrollbar_drag = false;
                    st.mark_activity(); // one more frame to clear the parked hover highlight
                    st.window.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let shift = self.mods.shift_key();
                if let Some(st) = self.state.as_mut() {
                    st.cursor = (position.x, position.y);
                    st.mark_activity(); // keep polling so hover/selection tracks the cursor
                    // In-progress local drags (divider/selection/reorder) always keep the motion;
                    // otherwise forward it to a mouse-reporting pane (drag / any-motion), and only
                    // when the app doesn't take it run the local hover/selection logic.
                    let local_drag_live = st.drag.is_some()
                        || st.sel_dragging
                        || st.sidebar_drag.is_some()
                        || st.scrollbar_drag;
                    if local_drag_live || !st.report_motion(shift) {
                        st.on_cursor_moved();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let shift = self.mods.shift_key();
                let ctrl = self.mods.control_key();
                if let Some(st) = self.state.as_mut() {
                    st.mark_activity();
                    let pressed = state == ElementState::Pressed;
                    // A press on the active pane's search band hits chrome, not cells — the cell
                    // row under the band is hidden, so selection/URL/reporting there would act on
                    // something the user can't see. Swallow it.
                    if pressed && st.search.is_some() && st.cursor_in_search_band() {
                        return;
                    }
                    // The settings panel is modal: a press on its card acts on the panel and never
                    // reaches the sidebar or a pane behind it.
                    if pressed && st.settings.is_some() && st.settings_click(button) {
                        return;
                    }
                    // Ctrl+left-click on a detected URL opens it and consumes the click — checked
                    // before mouse reporting and before selection/focus.
                    if pressed && ctrl && button == MouseButton::Left && st.open_url_at_cursor() {
                        return;
                    }
                    // Middle-click on a sidebar tab closes that window and consumes the click.
                    if pressed && button == MouseButton::Middle && st.close_tab_under_cursor() {
                        return;
                    }
                    // Local surfaces win first: an in-progress divider/selection/reorder/scrollbar
                    // drag, or a fresh press on a divider band, must not be swallowed by reporting.
                    let local_drag_live = st.drag.is_some()
                        || st.sel_dragging
                        || st.sidebar_drag.is_some()
                        || st.scrollbar_drag;
                    let grabs_divider = pressed
                        && button == MouseButton::Left
                        && st.divider_under_cursor();
                    // A left press on the scrollbar thumb/track starts a scrollbar drag — after the
                    // divider check, before app reporting. Consumes the click when it lands on the bar.
                    if pressed && button == MouseButton::Left && !grabs_divider && st.grab_scrollbar() {
                        return;
                    }
                    // App mouse reporting: if the active pane wants mouse events and this one is
                    // inside its cell area without Shift, forward it and suppress local behavior.
                    if !local_drag_live && !grabs_divider {
                        if let Some(b) = mouse_button_code(button) {
                            if st.report_button(b, pressed, shift) {
                                return;
                            }
                        }
                    }
                    // Right-click pastes into the active pane (Windows Terminal convention) when
                    // mouse reporting above didn't claim it. Gated on the cursor being inside the
                    // ACTIVE pane — paste always targets the active shell, so a spatial gate any
                    // wider would misdirect input (right-click a non-active split pane -> the
                    // clipboard runs in the pane the user was NOT pointing at). While searching it
                    // feeds the query instead, matching the keyboard Paste chord.
                    if pressed
                        && button == MouseButton::Right
                        && matches!(st.active_pane_rect(), Some((_, true)))
                    {
                        if st.search.is_some() {
                            st.paste_into_query();
                        } else {
                            st.set_scroll(st.active_pane, 0); // pasting sends input; snap back to live
                            st.paste_clipboard();
                        }
                        return;
                    }
                    // Only the left button drives local selection / focus / divider-resize.
                    if button == MouseButton::Left {
                        if pressed {
                            st.on_click();
                        } else {
                            st.on_release();
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let shift = self.mods.shift_key();
                let ctrl = self.mods.control_key();
                if let Some(st) = self.state.as_mut() {
                    st.mark_activity();
                    // Ctrl+wheel zooms the font (wheel up = larger) and consumes the event, before
                    // any scrollback / app-reporting handling.
                    if ctrl {
                        let dir = match delta {
                            MouseScrollDelta::LineDelta(_, y) => y,
                            MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        };
                        if dir != 0.0 {
                            st.zoom(if dir > 0.0 { 2.0 } else { -2.0 });
                        }
                        return;
                    }
                    // Shift+wheel always drives gmux scrollback; an unmodified wheel over a
                    // mouse-reporting pane is forwarded to its app instead.
                    if !shift && st.report_wheel(delta) {
                        return;
                    }
                    // Wheel up (positive y) scrolls deeper into history, in the pane under the
                    // cursor. ponytail: report_wheel above only forwards for the ACTIVE pane, so a
                    // wheel over a NON-active reporting pane just scrolls it locally rather than
                    // forwarding to its app — good enough; forwarding to a non-focused app is rare.
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i64,
                        MouseScrollDelta::PixelDelta(p) => (p.y / st.cell_dims().1 as f64).round() as i64,
                    };
                    // Wheel over the sidebar scrolls the tab list (overflow), not a pane.
                    let (cx, _) = st.cursor;
                    let (sidebar_w, _, _) = st.areas();
                    if cx >= 0.0 && (cx as u32) < sidebar_w {
                        let step = lines.unsigned_abs() as usize;
                        st.sidebar_scroll = if lines > 0 {
                            st.sidebar_scroll.saturating_sub(step)
                        } else {
                            st.sidebar_scroll.saturating_add(step) // clamped next render
                        };
                        st.window.request_redraw();
                        return;
                    }
                    let target = st.pane_under_cursor();
                    st.scroll_by(target, lines);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(st) = self.state.as_mut() {
                    st.mark_activity();
                }
                // The command palette intercepts every key while open: edit the filter, move the
                // selection (Up/Down), run (Enter), or close (Escape).
                if let Some(st) = self.state.as_mut() {
                    if st.palette.is_some() {
                        st.palette_key(&event, self.mods);
                        return;
                    }
                }
                // Copy mode intercepts every key: arrows/hjkl move, v marks, y/Enter copies,
                // Escape exits.
                if let Some(st) = self.state.as_mut() {
                    if st.copy_mode.is_some() {
                        st.copy_mode_key(&event);
                        return;
                    }
                }
                // A pending close confirmation intercepts first: Enter proceeds with the close,
                // ANY other key (incl. Escape) cancels — safe-by-default for a destructive act.
                if let Some(st) = self.state.as_mut() {
                    if st.confirm_close.is_some() {
                        // Bare modifier presses (the Ctrl of a chord) neither confirm nor cancel.
                        if !matches!(
                            &event.logical_key,
                            Key::Named(
                                NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super
                            )
                        ) {
                            st.confirm_close_key(matches!(
                                &event.logical_key,
                                Key::Named(NamedKey::Enter)
                            ));
                        }
                        return;
                    }
                }
                // Rename mode intercepts every key first (mutually exclusive with search): build
                // the tab name, commit (Enter), or cancel (Escape).
                if let Some(st) = self.state.as_mut() {
                    if st.rename.is_some() {
                        st.rename_key(&event, self.mods);
                        return;
                    }
                }
                // The settings panel intercepts every key while open (including plain letters,
                // which pick rows and start captures).
                if let Some(st) = self.state.as_mut() {
                    if st.settings.is_some() {
                        st.settings_key(&event, self.mods);
                        return;
                    }
                }
                // The sidebar filter is the same kind of modal: while it is open, keys narrow the
                // list instead of reaching the pane.
                if let Some(st) = self.state.as_mut() {
                    if st.sidebar_filter.is_some() {
                        st.filter_key(&event, self.mods);
                        return;
                    }
                }
                // Search mode intercepts every key before the keymap/SendKeys: build the query,
                // navigate matches (Enter/Shift+Enter), edit (Backspace), or exit (Escape).
                if let Some(st) = self.state.as_mut() {
                    if st.search.is_some() {
                        st.search_key(&event, self.mods);
                        return;
                    }
                }
                if !self.try_shortcut(&event) {
                    if let Some(bytes) = key_to_bytes(&event, self.mods) {
                        if let Some(st) = self.state.as_mut() {
                            st.set_scroll(st.active_pane, 0); // typing snaps back to the live screen
                            let text = String::from_utf8_lossy(&bytes).into_owned();
                            st.client.control(Call::SendKeys { pane: st.active_pane, text, enter: false });
                        }
                    }
                }
            }
            WindowEvent::DroppedFile(path) => {
                if let Some(st) = self.state.as_mut() {
                    st.mark_activity();
                    // While searching, a drop feeds the query like every other paste-class input
                    // — drop_file's SendKeys + snap-to-live would silently type into the shell
                    // behind the overlay and desync the parked match viewport.
                    if st.search.is_some() {
                        let text: String = path
                            .to_string_lossy()
                            .chars()
                            .filter(|c| !c.is_control())
                            .collect();
                        if let Some(s) = st.search.as_mut() {
                            s.query.push_str(&text);
                        }
                        st.refresh_search();
                        st.window.request_redraw();
                    } else {
                        st.drop_file(&path);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(st) = self.state.as_mut() {
                    st.render();
                }
            }
            _ => {}
        }
    }

    /// The subscriber thread woke us: fresh output (damage) and/or a real notification arrived.
    /// Mark activity so the loop polls at `FRAME` briefly (catching a burst of output), and redraw
    /// — `render` drains the damaged set and refetches only those panes. Toasts are drained in
    /// `about_to_wait`, which runs right after this in the same loop iteration.
    fn user_event(&mut self, _el: &ActiveEventLoop, _event: ()) {
        if let Some(st) = self.state.as_mut() {
            st.mark_activity();
            st.window.request_redraw();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };

        // Still bringing up the daemon connection: keep painting (a cleared frame) each tick and
        // poll the connect thread. Skip all the client-dependent polling below until it's live.
        if matches!(st.client, Client::Connecting(_)) {
            st.poll_connect(el);
            st.window.request_redraw();
            el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
            return;
        }

        // A Ready client that lost the daemon irrecoverably (render's GetLayout errored): give up.
        if st.fatal {
            el.exit();
            return;
        }

        // Toasts now ride the subscriber channel; fire the ones seen while unfocused.
        st.drain_toasts();

        // A clicked toast focuses the window AND lands on the pane that notified (its launch arg
        // carries "pane=N"; FocusPaneId activates that pane's tab too). Most recent click wins.
        let acts = st.notifier.as_ref().map(|nf| nf.poll_activations()).unwrap_or_default();
        if !acts.is_empty() {
            st.window.focus_window();
            st.clear_active_toast();
            flash_window(st.hwnd, false);
            if let Some(pane) = acts.iter().rev().find_map(|a| parse_activation_pane(a)) {
                st.client.control(Call::FocusPaneId { pane });
                st.sync_size();
                st.force_full = true;
                st.window.request_redraw();
            }
        }

        // Idle housekeeping (~1s), with NO redraw: resend geometry so a restarted daemon relearns
        // pane sizes, hot-reload config, keep the subscriber thread alive, drain browser requests.
        let now = Instant::now();
        if now.duration_since(st.last_heartbeat) >= HEARTBEAT {
            st.last_heartbeat = now;
            st.sync_size();
            st.maybe_reload_config();
            st.ensure_subscriber();
            // render() — the only place that flags a dead daemon — never runs while minimized,
            // so probe here too; an unrecoverable daemon death would otherwise leave a zombie
            // taskbar entry until restore. ListPanes is read-only (no layout resize side effect).
            if st.minimized
                && matches!(st.client, Client::Ready(_))
                && st.client.call(Call::ListPanes).is_err()
            {
                st.fatal = true;
            }
            #[cfg(feature = "browser")]
            st.poll_browse();
        }

        // Active (recent input / a wake / an in-progress drag): keep polling at FRAME and redraw.
        // Idle: sleep until the next heartbeat with no redraw — a wake (input or subscriber event)
        // pops us out early.
        if st.is_active(now) {
            st.window.request_redraw();
            el.set_control_flow(ControlFlow::WaitUntil(now + FRAME));
        } else if st.any_busy {
            // Something is running (a build, an agent): step the spinner at its own slow cadence
            // and redraw just for that. Nothing else animates, and once the last busy pane goes
            // quiet this branch stops firing — an idle gmux still never wakes.
            let step = Duration::from_millis(SPINNER_STEP_MS);
            if now.duration_since(st.last_spinner) >= step {
                st.last_spinner = now;
                st.renderer.advance_spinner();
                st.window.request_redraw();
            }
            el.set_control_flow(ControlFlow::WaitUntil(now + step));
        } else {
            el.set_control_flow(ControlFlow::WaitUntil(now + HEARTBEAT));
        }
    }
}

impl App {
    /// Handle a gmux keyboard chord by dispatching the configured [`Action`] to the daemon.
    fn try_shortcut(&mut self, event: &KeyEvent) -> bool {
        let mods = self.mods;
        let Some(st) = self.state.as_mut() else { return false };

        // A bare modifier press is a chord prefix, not a key: letting it fall through to the
        // "any key clears the selection" step below would kill the selection on the Ctrl of
        // Ctrl+Shift+C before the C ever arrives — copy would never see a selection.
        if matches!(
            &event.logical_key,
            Key::Named(NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super)
        ) {
            return false;
        }

        // Ctrl+Shift+C copies the active selection (hardcoded — not a rebindable action, and it
        // must run before the "any key clears the selection" step below).
        if mods == (ModifiersState::CONTROL | ModifiersState::SHIFT)
            && matches!(&event.logical_key, Key::Character(s) if s.eq_ignore_ascii_case("c"))
        {
            st.copy_selection();
            st.clear_selection();
            return true;
        }

        // Any other key press dismisses a pending selection before the key is handled.
        let had_selection = st.selection.is_some();
        st.clear_selection();

        if let Some(action) = st.keymap.action(mods, &event.logical_key) {
            st.dispatch(action);
            return true;
        }

        // Escape while scrolled snaps back to live (not a rebindable action; consumed here so the
        // pane never sees it). Escape that only dismissed a selection is likewise consumed, so it
        // isn't also forwarded to the pane.
        if let Key::Named(NamedKey::Escape) = &event.logical_key {
            if st.scroll_of(st.active_pane) > 0 {
                st.set_scroll(st.active_pane, 0);
                st.window.request_redraw();
                return true;
            }
            if had_selection {
                return true;
            }
        }
        false
    }
}

impl State {
    /// Run a keybinding [`Action`] with the same side effects the old hardcoded matches had.
    fn dispatch(&mut self, action: Action) {
        // Input-ish actions snap the active pane back to the live screen. Scroll actions must NOT
        // (they move the viewport), and neither must focus/tab navigation — per-pane offsets mean
        // leaving a pane keeps its place, matching mouse focus (which never snapped).
        if !matches!(
            action,
            Action::ScrollPageUp
                | Action::ScrollPageDown
                | Action::FocusLeft
                | Action::FocusRight
                | Action::FocusUp
                | Action::FocusDown
                | Action::NextWindow
                | Action::PrevWindow
                | Action::SelectTab(_)
                | Action::PrevPrompt
                | Action::NextPrompt
                | Action::ExportScrollback
                | Action::ResizeLeft
                | Action::ResizeRight
                | Action::ResizeUp
                | Action::ResizeDown
        ) {
            self.set_scroll(self.active_pane, 0);
        }
        match action {
            Action::CommandPalette => {
                // Opening the palette closes every other modal surface.
                self.search = None;
                self.rename = None;
                self.confirm_close = None;
                self.copy_mode = None;
                self.palette = Some(PaletteState { query: String::new(), selected: 0 });
                self.window.request_redraw();
            }
            Action::ToggleBrowser => self.toggle_browser_dock(),
            Action::CopyMode => {
                self.search = None;
                self.rename = None;
                self.confirm_close = None;
                self.palette = None;
                // Start at the active pane's live cursor (fall back to origin).
                let cur = self
                    .snap_cache
                    .get(&self.active_pane)
                    .map(|s| (s.cursor.0.min(s.cols.saturating_sub(1)), s.cursor.1.min(s.rows.saturating_sub(1))))
                    .unwrap_or((0, 0));
                self.copy_mode = Some(CopyModeState { cursor: cur, anchor: None });
                // The existing selection highlight doubles as the mode's cell cursor.
                self.selection =
                    Some(Selection { pane: self.active_pane, start: cur, end: cur });
                self.window.request_redraw();
            }
            Action::PrevPrompt => self.prompt_jump(true),
            Action::NextPrompt => self.prompt_jump(false),
            Action::ExportScrollback => self.export_scrollback(),
            // Keyboard split nudges: grow/shrink the active pane's divider fraction. ResizeSplit
            // adjusts the split whose top/left pane is `pane` — from the keyboard the active pane
            // stands in for "the divider I mean"; edge panes without that divider no-op.
            Action::ResizeLeft => self.nudge_split(-0.03, 0.0),
            Action::ResizeRight => self.nudge_split(0.03, 0.0),
            Action::ResizeUp => self.nudge_split(0.0, -0.03),
            Action::ResizeDown => self.nudge_split(0.0, 0.03),
            Action::SelectTab(n) => {
                // Alt+1..9: activate sidebar tab N-1 (out-of-range indices are ignored server-side).
                self.client.control(Call::SelectWindow { index: n.saturating_sub(1) as usize });
                self.sync_size();
                self.force_full = true;
                self.window.request_redraw();
            }
            Action::FocusLeft => self.client.control(Call::FocusPane { dir: "left".into() }),
            Action::FocusRight => self.client.control(Call::FocusPane { dir: "right".into() }),
            Action::FocusUp => self.client.control(Call::FocusPane { dir: "up".into() }),
            Action::FocusDown => self.client.control(Call::FocusPane { dir: "down".into() }),
            Action::SplitH => {
                self.client.control(Call::SplitPane { dir: "h".into(), command: None });
                self.sync_size();
            }
            Action::SplitV => {
                self.client.control(Call::SplitPane { dir: "v".into(), command: None });
                self.sync_size();
            }
            Action::ClosePane => {
                // Guard: a pane whose shell has running children (a build, an agent) asks for
                // confirmation instead of dying to one stray chord. Query failure = not busy.
                let busy = matches!(
                    self.client.call(Call::PaneBusy { pane: self.active_pane }),
                    Ok(ResultBody::Busy(true))
                );
                if busy {
                    self.confirm_close = Some(ConfirmClose::Pane(self.active_pane));
                    self.window.request_redraw();
                } else {
                    self.client.control(Call::ClosePane);
                    self.sync_size();
                }
            }
            Action::ToggleZoom => {
                self.client.control(Call::ToggleZoom);
                self.sync_size();
                self.force_full = true; // the zoomed pane fills the window; refetch every grid
            }
            Action::ZoomIn => self.zoom(2.0),
            Action::ZoomOut => self.zoom(-2.0),
            Action::ZoomReset => self.apply_font_px(self.config_font_px),
            Action::NewWindow => {
                self.client.control(Call::NewWindow { command: None, cwd: None });
                self.sync_size();
            }
            Action::OpenWorkspace => self.open_workspace_dir(),
            Action::ImportWorkspaces => self.import_workspaces(),
            Action::FilterWorkspaces => {
                // Mutually exclusive with the other text modals, like search and rename are.
                self.search = None;
                self.rename = None;
                self.palette = None;
                self.sidebar_filter = Some(String::new());
                self.window.request_redraw();
            }
            Action::RenameWorkspace => {
                // The active tab's row, so the keyboard reaches the same inline editor a
                // double-click opens.
                let idx = self.active_tab;
                self.start_rename(idx);
            }
            Action::CloseWorkspace => {
                let idx = self.active_tab;
                self.close_tab(idx);
            }
            Action::NextWindow => {
                self.client.control(Call::SwitchWindow { next: true });
                self.sync_size();
            }
            Action::PrevWindow => {
                self.client.control(Call::SwitchWindow { next: false });
                self.sync_size();
            }
            Action::ScrollPageUp => self.scroll_page(1),
            Action::ScrollPageDown => self.scroll_page(-1),
            Action::Paste => self.paste_clipboard(),
            Action::OpenSettings => {
                // The panel covers accent, font size and every keybinding; 'e' inside it still
                // hands the raw gmux.json to an editor for what it does not cover.
                self.search = None;
                self.rename = None;
                self.palette = None;
                self.cancel_preview(); // reopening mid-preview must not strand the try-on palette
                self.settings =
                    Some(SettingsState { tab: 0, sel: 0, capturing: false, preview: None });
                self.window.request_redraw();
            }
            Action::Search => self.enter_search(),
        }
        self.window.request_redraw();
    }

    /// If the config file's mtime changed since we last loaded, reload it: keys and theme apply
    /// live; a font-size change needs a renderer rebuild we don't do here, so it's logged and
    /// deferred to the next launch.
    fn maybe_reload_config(&mut self) {
        let now = config_mtime();
        if now == self.config_mtime {
            return;
        }
        self.config_mtime = now;
        let config = Config::load();
        self.keymap = Keymap::build(&config);
        self.focus_follows_mouse = config.focus_follows_mouse.unwrap_or(false);
        apply_theme(&mut self.renderer, &config);
        self.send_palette(&config); // re-theme the daemon's panes on hot-reload
        // The daemon re-resolves every grid's colors but emits no damage wires for it, so the
        // snapshot cache is stale until the next output — force a full refetch.
        self.force_full = true;
        // A font-size change in the config applies live (rebuilds the atlas + resends geometry).
        // Compared against `config_font_px` (not `font_px`) so a live Ctrl+wheel zoom isn't undone
        // by an unrelated reload — only an actual change to the file's value re-applies.
        let new_font = clamp_font_px(config.font_px.unwrap_or(DEFAULT_FONT_PX));
        if (new_font - self.config_font_px).abs() > f32::EPSILON {
            self.config_font_px = new_font;
            self.apply_font_px(new_font);
        }
        self.window.request_redraw();
    }

    /// Set the font size live: clamp to the atlas range, rebuild the glyph atlas, then resend
    /// geometry so the daemon re-cells every pane at the new cell size and refetch every grid.
    fn apply_font_px(&mut self, px: f32) {
        let px = clamp_font_px(px);
        if (px - self.font_px).abs() < f32::EPSILON {
            // Already at this size (wheel spun past a clamp bound, or reset at the config size):
            // skip the disk font reload, atlas/texture rebuild, geometry resend, and full refetch.
            return;
        }
        self.renderer.set_font_px(px);
        self.font_px = px;
        self.sync_size(); // new cell_w/cell_h -> daemon re-divides panes into cells
        self.force_full = true;
        self.window.request_redraw();
    }

    /// Nudge the font size by `delta` px (Ctrl+wheel / zoom-in/out actions).
    fn zoom(&mut self, delta: f32) {
        self.apply_font_px(self.font_px + delta);
    }

    /// Poll the background connect thread. Once the daemon answers, promote the connection to
    /// `Ready` and do the post-connect setup the old blocking startup did (report geometry + push
    /// the palette). A connect failure (or a dead connect thread) exits the app.
    fn poll_connect(&mut self, el: &ActiveEventLoop) {
        let recv = match &self.client {
            Client::Connecting(rx) => rx.try_recv(),
            Client::Ready(_) => return,
        };
        match recv {
            Ok(Ok(dc)) => {
                self.client = Client::Ready(dc);
                self.sync_size();
                if let Some(p) = self.init_palette.take() {
                    self.client.control(p); // theme the daemon's panes to match config
                }
                self.ensure_subscriber(); // start streaming output/notifications now that it's live
                self.mark_activity(); // poll a few frames so the first output settles
                self.window.request_redraw();
            }
            Ok(Err(e)) => {
                eprintln!("gmux: cannot reach the daemon: {e}");
                el.exit();
            }
            Err(TryRecvError::Empty) => {} // still connecting
            Err(TryRecvError::Disconnected) => {
                eprintln!("gmux: daemon connect thread ended without a result");
                el.exit();
            }
        }
    }

    /// Note user/wake activity; within `ACTIVE_WINDOW` of this the loop polls at `FRAME`.
    fn mark_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Whether the loop should keep polling at `FRAME`: a drag/selection is in progress, or the
    /// last activity was recent.
    fn is_active(&self, now: Instant) -> bool {
        self.drag.is_some()
            || self.sel_dragging
            || self.sidebar_drag.is_some()
            || now.duration_since(self.last_activity) < ACTIVE_WINDOW
    }

    /// Drain the subscriber channel, toasting the notifications that arrived while unfocused.
    fn drain_toasts(&mut self) {
        while let Ok(n) = self.toast_rx.try_recv() {
            // Clipboard-set wires (an OSC 52 copy from a pane) apply to the Windows clipboard
            // regardless of focus and are never toasted (reserved title; cross-agent contract).
            if n.title == "clipboard-set" {
                clipboard::set_text(self.hwnd, &n.body);
                continue;
            }
            if !self.focused {
                self.fire_toast(&n);
            }
        }
    }

    /// Spawn (or respawn) the output subscriber thread if it isn't running. Called on connect and
    /// from the heartbeat, so a thread that died on pipe EOF (daemon restart) is replaced within ~1s.
    fn ensure_subscriber(&mut self) {
        let alive = self.sub_alive.as_ref().is_some_and(|a| a.load(Ordering::Relaxed));
        if alive {
            return;
        }
        // A respawn means the daemon may have restarted (the old subscriber saw EOF); refetch every
        // pane next render so a rebuilt session doesn't render from stale cached snapshots.
        self.force_full = true;
        self.sub_alive =
            Some(spawn_output_subscriber(self.proxy.clone(), self.damaged.clone(), self.toast_tx.clone()));
    }

    /// M12 (feature "browser"): drain queued Browse requests into the browser panel, opening the
    /// dock if it was closed — `gmux browse --pane <url>` should show you the page, not silently
    /// load it behind a hidden panel.
    #[cfg(feature = "browser")]
    fn poll_browse(&mut self) {
        if let Ok(ResultBody::Browses(urls)) = self.client.call(Call::PollBrowse) {
            for url in urls {
                if self.dock_w == 0 {
                    self.open_dock();
                }
                match &self.browser {
                    Some(b) => {
                        b.navigate(&url);
                        b.set_visible(true);
                    }
                    None => self.embed_browser(&url),
                }
                self.sync_size();
                self.window.request_redraw();
            }
        }
    }

    /// Open a workspace: ask for a directory, then create a tab anchored to it. Every pane in that
    /// window — the first shell, splits, and panes restored from a snapshot — opens there.
    /// Cancelling the picker does nothing at all (no stray empty tab).
    fn open_workspace_dir(&mut self) {
        let Some(dir) = pick_folder() else { return };
        self.client.control(Call::NewWindow { command: None, cwd: Some(dir) });
        self.sync_size();
        self.window.request_redraw();
    }

    /// Import a projects directory: pick a parent folder, then open one workspace per git project
    /// inside it. Folders already open are skipped, so re-importing only adds what is new. The
    /// notice band reports the outcome — this can create several tabs at once, and silence would
    /// leave you guessing whether anything happened.
    fn import_workspaces(&mut self) {
        let Some(dir) = pick_folder() else { return };
        let msg = match self.client.call(Call::ImportWorkspaces { dir, all: false }) {
            Ok(ResultBody::Imported { created, already_open, capped }) => {
                let mut m = format!("imported {created} workspace(s)");
                if already_open > 0 {
                    m.push_str(&format!(", {already_open} already open"));
                }
                if capped > 0 {
                    m.push_str(&format!(", {capped} over the limit"));
                }
                if created == 0 && already_open == 0 {
                    m = "no git projects found in that folder".to_string();
                }
                m
            }
            _ => "import failed".to_string(),
        };
        self.notice = Some((msg, Instant::now()));
        self.sync_size();
        self.window.request_redraw();
    }

    /// Default dock width: 40% of the window, floored at 320px and never more than half — a panel
    /// you can't read is useless, and one that swallows the terminal defeats the point.
    #[cfg(feature = "browser")]
    fn default_dock_w(&self) -> u32 {
        (self.config.width * 2 / 5).clamp(320, (self.config.width / 2).max(320))
    }

    /// Reserve the dock column (does not create the WebView2 — see `embed_browser`).
    #[cfg(feature = "browser")]
    fn open_dock(&mut self) {
        self.dock_w = self.default_dock_w().min(self.config.width.saturating_sub(200));
    }

    /// Create the embedded panel at the current dock rect and point it at `url`.
    #[cfg(feature = "browser")]
    fn embed_browser(&mut self, url: &str) {
        let hwnd = window_hwnd(&self.window).unwrap_or(0);
        if hwnd == 0 {
            return;
        }
        let (x, y, w, h) = self.dock_rect();
        // Hosted on THIS (the winit) thread — WebView2 requires the controller to live on the
        // thread owning its parent window, which is what round 44's child-window attempt got wrong.
        match gmux_browser::EmbeddedBrowser::new(hwnd, x, y, w, h, url) {
            Ok(b) => self.browser = Some(b),
            Err(e) => eprintln!("gmux: browser panel failed: {e}"),
        }
        // The panes just lost the dock's width; tell the daemon so it re-cells them.
        self.sync_size();
        self.window.request_redraw();
    }

    /// Ctrl+Shift+B: show/hide the browser panel. Hiding keeps the WebView2 (and its page + login
    /// session) alive, so toggling back is instant. The first toggle with no panel yet opens a
    /// blank page rather than nothing at all.
    ///
    #[cfg(feature = "browser")]
    fn toggle_browser_dock(&mut self) {
        if self.dock_w > 0 {
            self.dock_w = 0;
            if let Some(b) = &self.browser {
                b.set_visible(false);
            }
        } else {
            self.open_dock();
            match &self.browser {
                Some(b) => {
                    let (x, y, w, h) = self.dock_rect();
                    b.set_bounds(x, y, w, h);
                    b.set_visible(true);
                }
                None => self.embed_browser("about:blank"),
            }
        }
        self.sync_size();
        self.window.request_redraw();
    }

    /// Keep the panel glued to its column after a window resize.
    #[cfg(feature = "browser")]
    fn sync_dock_bounds(&self) {
        if self.dock_w == 0 {
            return;
        }
        if let Some(b) = &self.browser {
            let (x, y, w, h) = self.dock_rect();
            b.set_bounds(x, y, w, h);
        }
    }

    /// Without the `browser` feature the dock never opens, so these are no-ops that keep the call
    /// sites free of `#[cfg]`.
    #[cfg(not(feature = "browser"))]
    fn toggle_browser_dock(&mut self) {
        self.notice =
            Some(("browser panel needs a --features browser build".into(), Instant::now()));
        self.window.request_redraw();
    }
    #[cfg(not(feature = "browser"))]
    fn sync_dock_bounds(&self) {}

    /// Handle a left press: in the sidebar, the '+' row opens a new tab and a workspace row selects
    /// that window (and arms a possible tab reorder); in the content area, a split-gap starts a
    /// divider drag and a press inside a pane's cell area starts a text selection (a release without
    /// movement becomes a plain focus, see [`State::on_release`]). Uses the renderer's row metrics
    /// for the sidebar and the last rendered layout (cached in `render`) for pane / divider tests.
    /// Whether the cursor currently sits on a split-divider band (content-area coords) — used to
    /// give local drag-resize precedence over app mouse reporting on a fresh press.
    fn divider_under_cursor(&self) -> bool {
        let (cx, cy) = self.cursor;
        if cx < 0.0 || cy < 0.0 {
            return false;
        }
        let (sidebar_w, _, _) = self.areas();
        if (cx as u32) < sidebar_w {
            return false;
        }
        let content_x = cx - sidebar_w as f64;
        divider_at(&self.last_panes, content_x as f32, cy as f32, GAP + 2.0).is_some()
    }

    fn on_click(&mut self) {
        let (cx, cy) = self.cursor;
        if cx < 0.0 || cy < 0.0 {
            return;
        }
        let (sidebar_w, _, _) = self.areas();
        let (px, py) = (cx as u32, cy as u32);
        if px < sidebar_w {
            self.clear_selection();
            // The '+' new-tab row sits after the last item, so test it first.
            if self.renderer.sidebar_new_tab_at(cy as f32, &self.item_heights) {
                // Clicking '+ open workspace' asks for a directory: the new workspace is anchored
                // to it, and every pane opened in it starts there. Ctrl+Shift+T stays the plain
                // "new tab wherever" path for when you don't want to pick anything.
                self.set_scroll(self.active_pane, 0);
                self.open_workspace_dir();
            } else if let Some(vi) = self.hit_row(cy) {
                // A click on the PR chip opens the pull request instead of selecting the tab.
                if self.open_pr_at(cx, cy, vi) {
                    return;
                }
                // The hover close button: same busy-guarded path as a middle-click.
                if self.close_button_at(cx, cy, vi) {
                    return;
                }
                let idx = self.real_tab(vi);
                // A second click on the same row within CLICK_INTERVAL starts renaming that tab.
                let now = Instant::now();
                let dbl = sidebar_double_click(self.last_sidebar_click, idx, now);
                self.last_sidebar_click = Some((idx, now));
                if dbl {
                    self.start_rename(idx);
                    return;
                }
                self.set_scroll(self.active_pane, 0);
                self.client.control(Call::SelectWindow { index: idx });
                // Arm a tab reorder: a >8px vertical drag before release turns into a MoveWindow.
                self.sidebar_drag =
                    Some(SidebarDrag { from_row: idx, start_y: cy, reordering: false, over: None });
                self.window.request_redraw();
            } else if let Some(group) = self.hit_header(cy) {
                // A click on a group header folds/unfolds it. Collapse state is GUI-local: the
                // daemon owns which group a window is in, not whether you have it open.
                if !self.collapsed_groups.remove(&group) {
                    self.collapsed_groups.insert(group);
                }
                self.window.request_redraw();
            }
            return;
        }
        // A pane's close button wins over everything else in the content area (it sits in the
        // title strip, where a plain click would otherwise just focus the pane).
        if self.close_pane_button_at(cx, cy) {
            return;
        }
        // A press on the title strip itself arms a pane rearrange: dragging it onto another pane
        // swaps the two. Selection must not start here — the strip is chrome, not cells.
        if let Some(pane) = self.title_strip_at(cx, cy) {
            self.clear_selection();
            self.pane_drag = Some(PaneDrag { from: pane, start: (cx, cy), dragging: false, over: None });
            self.client.control(Call::FocusPaneId { pane });
            self.window.request_redraw();
            return;
        }
        // Content area: cached rects are in content-area coords, so shift the click by the sidebar.
        let content_x = cx - sidebar_w as f64;
        // A split gap starts a drag-resize instead of a selection/focus. A DOUBLE-click on the
        // same divider equalizes it to 50/50: clamp(r+10)=0.9 then -0.4 lands exactly 0.5 from
        // any starting ratio — two existing resize-split calls, no new protocol (ponytail; the
        // intermediate 0.9 is never rendered, both apply before the next layout fetch).
        if let Some(d) = divider_at(&self.last_panes, content_x as f32, cy as f32, GAP + 2.0) {
            self.clear_selection();
            let now = Instant::now();
            if matches!(self.last_divider_click, Some((p, at)) if p == d.pane && now.duration_since(at) < CLICK_INTERVAL)
            {
                self.last_divider_click = None;
                self.drag = None;
                let (bx, sx) = if d.vertical { (10.0, -0.4) } else { (0.0, 0.0) };
                let (by, sy) = if d.vertical { (0.0, 0.0) } else { (10.0, -0.4) };
                self.client.control(Call::ResizeSplit { pane: d.pane, dx: bx, dy: by });
                self.client.control(Call::ResizeSplit { pane: d.pane, dx: sx, dy: sy });
                self.sync_size();
                self.force_full = true;
                self.window.request_redraw();
                return;
            }
            self.last_divider_click = Some((d.pane, now));
            self.drag = Some(Drag {
                pane: d.pane,
                vertical: d.vertical,
                span: d.span,
                origin: self.cursor,
                // Backdate so the first CursorMoved sends immediately.
                last_send: Instant::now().checked_sub(RESIZE_THROTTLE).unwrap_or_else(Instant::now),
            });
            return;
        }
        // Inside a pane: anchor a selection at the pressed cell. Focus is deferred to release.
        let cxp = content_x as u32;
        let hit = self
            .last_panes
            .iter()
            .find(|p| cxp >= p.x && cxp < p.x + p.w && py >= p.y && py < p.y + p.h)
            .cloned();
        if let Some(pr) = hit {
            let (cw, ch) = self.cell_dims();
            let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
            let cell = pixel_to_cell(cx as f32, cy as f32, rect, sidebar_w, self.config.width, self.config.height, cw, ch);
            // Escalate consecutive same-cell clicks: single → double (word) → triple (line) → single.
            let now = Instant::now();
            let count = match self.last_click {
                Some((p, c, t, n)) if p == pr.id && c == cell && now.duration_since(t) < CLICK_INTERVAL => n % 3 + 1,
                _ => 1,
            };
            self.last_click = Some((pr.id, cell, now, count));
            match count {
                2 => self.select_word(pr.id, cell),
                3 => self.select_line(pr.id, cell.1),
                // Single click: anchor a selection; a subsequent drag extends it (existing behavior).
                _ => {
                    self.selection = Some(Selection { pane: pr.id, start: cell, end: cell });
                    self.sel_dragging = true;
                    self.window.request_redraw();
                }
            }
        }
    }

    /// Double-click: select the word at `cell` in `pane`, using that pane's cached snapshot row for
    /// the char run. A blank (space) cell selects nothing (clears any selection), matching a plain
    /// click on empty space. ponytail: a drag after this behaves as a fresh single-cell drag (no
    /// word-wise extension) — `sel_dragging` stays false, so `on_release` won't re-copy either.
    fn select_word(&mut self, pane: u64, cell: (u16, u16)) {
        let row: Vec<char> = self
            .snap_cache
            .get(&pane)
            .and_then(|s| s.cells.get(cell.1 as usize))
            .map(|r| r.iter().map(|c| c.ch).collect())
            .unwrap_or_default();
        if row.get(cell.0 as usize).map_or(true, |c| *c == ' ') {
            self.clear_selection();
            return;
        }
        let (s, e) = word_span(&row, cell.0 as usize);
        self.selection = Some(Selection { pane, start: (s as u16, cell.1), end: (e as u16, cell.1) });
        self.window.request_redraw();
    }

    /// Triple-click: select the whole `row` of `pane` (full grid width; `grid_selection_text` trims
    /// trailing blanks on copy). Ctrl+Shift+C copies it via the existing path.
    fn select_line(&mut self, pane: u64, row: u16) {
        match self.snap_cache.get(&pane).map(|s| s.cols) {
            Some(cols) if cols > 0 => {
                self.selection = Some(Selection { pane, start: (0, row), end: (cols - 1, row) });
                self.window.request_redraw();
            }
            _ => self.clear_selection(),
        }
    }

    /// Route cursor motion to the active interaction: extend a text selection, promote a sidebar
    /// press to a tab reorder past the drag threshold, or (a divider drag) throttle a `ResizeSplit`.
    fn on_cursor_moved(&mut self) {
        if self.scrollbar_drag {
            self.drag_scrollbar_to_cursor();
            return;
        }
        if self.sel_dragging {
            self.extend_selection_to_cursor();
            return;
        }
        self.update_hover();
        let cy = self.cursor.1;
        // A pane rearrange in flight: past the threshold, track which pane would receive the swap.
        if self.pane_drag.is_some() {
            let (cx, cy) = self.cursor;
            let over = self.pane_under_cursor();
            if let Some(pd) = self.pane_drag.as_mut() {
                if !pd.dragging
                    && ((cx - pd.start.0).abs() > 8.0 || (cy - pd.start.1).abs() > 8.0)
                {
                    pd.dragging = true;
                }
                let target = (pd.dragging && over != pd.from).then_some(over);
                if pd.over != target {
                    pd.over = target;
                    self.window.request_redraw();
                }
            }
            return;
        }
        if self.sidebar_drag.is_some() {
            // Resolve the drop target on the way through so the indicator tracks the cursor; the
            // move itself still fires on release.
            let over = self.drop_target(cy);
            if let Some(sd) = self.sidebar_drag.as_mut() {
                if !sd.reordering && (cy - sd.start_y).abs() > 8.0 {
                    sd.reordering = true;
                }
                if sd.reordering && sd.over != over {
                    sd.over = over;
                    self.window.request_redraw();
                }
            }
            return;
        }
        let now = Instant::now();
        let (cx, cy) = self.cursor;
        let sent = {
            let Some(drag) = self.drag.as_mut() else { return };
            if now.duration_since(drag.last_send) < RESIZE_THROTTLE {
                return;
            }
            let ratio = |delta: f64, span: f32| delta as f32 / span.max(1.0);
            let (dx, dy) = if drag.vertical {
                (ratio(cx - drag.origin.0, drag.span), 0.0)
            } else {
                (0.0, ratio(cy - drag.origin.1, drag.span))
            };
            if dx == 0.0 && dy == 0.0 {
                return;
            }
            drag.origin = (cx, cy);
            drag.last_send = now;
            (drag.pane, dx, dy)
        };
        let (pane, dx, dy) = sent;
        self.client.control(Call::ResizeSplit { pane, dx, dy });
        self.window.request_redraw();
    }

    /// Extend the in-progress selection to the cursor's cell within the selection's pane, clamped
    /// to that pane's grid. Requests a redraw when the end cell changed.
    fn extend_selection_to_cursor(&mut self) {
        let (cx, cy) = self.cursor;
        let (sidebar_w, _, _) = self.areas();
        let (cw, ch) = self.cell_dims();
        let (surf_w, surf_h) = (self.config.width, self.config.height);
        let Some(pane) = self.selection.as_ref().map(|s| s.pane) else { return };
        // Copy the pane rect out before mutating `selection` (both borrow `self`).
        let Some(pr) = self.last_panes.iter().find(|p| p.id == pane).cloned() else { return };
        let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
        let cell = pixel_to_cell(cx as f32, cy as f32, rect, sidebar_w, surf_w, surf_h, cw, ch);
        if let Some(sel) = self.selection.as_mut() {
            if sel.end != cell {
                sel.end = cell;
                self.window.request_redraw();
            }
        }
    }

    /// Finish whatever left-drag was in progress: commit a tab reorder, finalize a selection (copy
    /// it, or treat a zero-movement press as a plain focus click), and clear any divider drag.
    fn on_release(&mut self) {
        self.drag = None;
        self.scrollbar_drag = false;
        // A pane dropped on another pane swaps the two; dropped anywhere else it just stays put
        // (the press already focused it, which is the sensible no-op).
        if let Some(pd) = self.pane_drag.take() {
            if let Some(b) = pd.over.filter(|b| *b != pd.from) {
                self.client.control(Call::SwapPanes { a: pd.from, b });
                self.sync_size();
                self.force_full = true;
                self.window.request_redraw();
            }
        }
        if let Some(sd) = self.sidebar_drag.take() {
            if sd.reordering {
                // `over` is what the indicator was showing; fall back to the cursor for a drag
                // that never produced a move event after crossing the threshold.
                let target = sd.over.or_else(|| self.drop_target(self.cursor.1));
                self.finish_reorder(sd.from_row, target);
            }
        }
        if self.sel_dragging {
            self.sel_dragging = false;
            let is_click = self.selection.as_ref().map_or(true, |s| s.start == s.end);
            if is_click {
                // No movement: a plain focus click (the old press-to-focus behavior). ponytail:
                // per-pane scroll means each pane owns its offset, so focusing one no longer snaps
                // anything to live (the old global reset compensated for a single shared offset).
                if let Some(pane) = self.selection.take().map(|s| s.pane) {
                    self.client.control(Call::FocusPaneId { pane });
                }
                self.window.request_redraw();
            } else {
                self.copy_selection();
            }
        }
    }

    /// Copy the current selection's text to the clipboard, rebuilt from the pane's freshly fetched
    /// grid at the same offset it was rendered. ponytail: refetch rather than cache every pane's
    /// grid each frame — copy is rare; this reflects what's currently on screen at those cells.
    fn copy_selection(&mut self) {
        let Some((pane, start, end)) = self.selection.as_ref().map(|s| (s.pane, s.start, s.end)) else {
            return;
        };
        let (start, end) = normalize_selection(start, end);
        let offset = self.scroll_of(pane);
        if let Ok(ResultBody::Grid(g)) = self.client.call(Call::GetGrid { pane, offset }) {
            let text = grid_selection_text(&g, start, end);
            if !text.is_empty() {
                clipboard::set_text(self.hwnd, &text);
            }
        }
    }

    /// Drop any selection (and its highlight), requesting a redraw if one was showing.
    fn clear_selection(&mut self) {
        self.sel_dragging = false;
        if self.selection.take().is_some() {
            self.window.request_redraw();
        }
    }

    /// Enter search mode: an empty query, no matches yet. Keystrokes now build the query.
    fn enter_search(&mut self) {
        self.rename = None; // search and rename are mutually exclusive modal input
        self.search = Some(SearchState { query: String::new(), matches: Vec::new(), current: 0 });
        self.window.request_redraw();
    }

    /// Start renaming sidebar row `idx`: seed the buffer with the tab's current name (from the last
    /// layout) and intercept all keyboard input until Enter/Escape. Exits search (mutually
    /// exclusive). No-op if the row has no stable id yet.
    fn start_rename(&mut self, idx: usize) {
        let Some(&id) = self.tab_ids.get(idx) else { return };
        self.search = None;
        let buffer = self.tab_names.get(idx).cloned().unwrap_or_default();
        self.rename = Some(RenameState { id, buffer });
        self.window.request_redraw();
    }

    /// A key press while rename mode is active: edit the buffer (chars incl. space, Backspace pops),
    /// commit (Enter -> `RenameWindow` with the trimmed buffer; empty clears back to the derived
    /// name), or cancel (Escape). Mirrors `search_key`'s modal text editing.
    fn rename_key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.rename = None,
            Key::Named(NamedKey::Enter) => {
                if let Some(r) = self.rename.take() {
                    self.client.control(Call::RenameWindow { id: r.id, name: r.buffer.trim().to_string() });
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(r) = self.rename.as_mut() {
                    r.buffer.pop();
                }
            }
            Key::Named(NamedKey::Space) => {
                if let Some(r) = self.rename.as_mut() {
                    r.buffer.push(' ');
                }
            }
            // Printable input; reject pure-Ctrl / Super chords like search_key (AltGr = Ctrl+Alt ok).
            Key::Character(text) if !mods.super_key() && !(mods.control_key() && !mods.alt_key()) => {
                if let Some(r) = self.rename.as_mut() {
                    r.buffer.push_str(text.as_str());
                }
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    /// The settings panel's rows for the open tab: `(label, value)`.
    ///
    /// Theme values come from the config file rather than the live renderer, because the file is
    /// what the panel edits — showing the resolved colour would make "default" and an explicit hex
    /// look identical.
    fn settings_rows(&self, tab: usize) -> Vec<SettingsRow> {
        let plain = |label: &str, value: String| SettingsRow {
            label: label.to_string(),
            value,
            swatch: Vec::new(),
        };
        let cfg = Config::load();
        if tab == 2 {
            // Every scheme, each drawn in its own colours: the list IS the preview.
            let current = self.settings.as_ref().and_then(|s| s.preview.clone()).unwrap_or_else(|| {
                cfg.theme
                    .as_ref()
                    .and_then(|t| t.preset.clone())
                    .unwrap_or_else(|| "default".to_string())
            });
            return crate::config::preset_names()
                .into_iter()
                .map(|name| SettingsRow {
                    swatch: swatch_rgb(name),
                    label: name.to_string(),
                    value: if name.eq_ignore_ascii_case(&current) { "in use" } else { "" }.to_string(),
                })
                .collect();
        }
        if tab == 0 {
            let accent = cfg
                .theme
                .as_ref()
                .and_then(|t| t.accent.clone())
                .unwrap_or_else(|| "default".to_string());
            let preset = cfg
                .theme
                .as_ref()
                .and_then(|t| t.preset.clone())
                .unwrap_or_else(|| "default".to_string());
            vec![
                SettingsRow {
                    swatch: swatch_rgb(&preset),
                    label: "color scheme".to_string(),
                    value: preset,
                },
                plain("accent", accent),
                plain("font size", format!("{:.0} px", self.font_px)),
                plain(
                    "follow mouse focus",
                    if self.focus_follows_mouse { "on" } else { "off" }.to_string(),
                ),
            ]
        } else {
            // Every bindable action with its CURRENT chord (config override, else the default).
            let overrides = cfg.keys.unwrap_or_default();
            crate::config::default_bindings()
                .iter()
                .map(|(name, chord, _)| {
                    let cur = overrides.get(*name).cloned().unwrap_or_else(|| chord.to_string());
                    plain(name, cur)
                })
                .collect()
        }
    }

    /// A key press while the settings panel is open. Arrows move, Tab switches section, Enter acts
    /// on the row (cycling a theme value, or starting a chord capture), Escape backs out.
    fn settings_key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        let Some(st) = self.settings.as_ref() else { return };
        // Capturing: the very next chord becomes the binding. Escape aborts; a bare modifier is
        // the prefix of the chord being pressed, not the chord itself.
        if st.capturing {
            if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                if let Some(st) = self.settings.as_mut() {
                    st.capturing = false;
                }
                self.window.request_redraw();
                return;
            }
            if matches!(
                &event.logical_key,
                Key::Named(NamedKey::Control | NamedKey::Shift | NamedKey::Alt | NamedKey::Super)
            ) {
                return;
            }
            let (tab, sel) = (st.tab, st.sel);
            if let Some(chord) = chord_string(mods, &event.logical_key) {
                let rows = self.settings_rows(tab);
                if let Some(row) = rows.get(sel) {
                    let action = row.label.clone();
                    self.write_config(|cfg| {
                        let keys = cfg
                            .as_object_mut()
                            .unwrap()
                            .entry("keys")
                            .or_insert_with(|| serde_json::json!({}));
                        if let Some(map) = keys.as_object_mut() {
                            map.insert(action, serde_json::Value::String(chord));
                        }
                    });
                }
            }
            if let Some(st) = self.settings.as_mut() {
                st.capturing = false;
            }
            self.window.request_redraw();
            return;
        }

        let tab = st.tab;
        let rows = self.settings_rows(tab).len().max(1);
        let Some(st) = self.settings.as_mut() else { return };
        match &event.logical_key {
            // On the schemes tab, Escape drops the scheme you were trying on and puts the config's
            // own palette back before it closes — a preview must never survive a cancel.
            Key::Named(NamedKey::Escape) => {
                self.cancel_preview();
                self.settings = None;
            }
            Key::Named(NamedKey::ArrowDown) => {
                st.sel = (st.sel + 1) % rows;
                self.preview_selected_scheme();
            }
            Key::Named(NamedKey::ArrowUp) => {
                st.sel = st.sel.checked_sub(1).unwrap_or(rows - 1);
                self.preview_selected_scheme();
            }
            Key::Named(NamedKey::Tab) | Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::ArrowLeft) => {
                let back = matches!(&event.logical_key, Key::Named(NamedKey::ArrowLeft));
                let n = SETTINGS_TABS.len();
                st.tab = if back { (st.tab + n - 1) % n } else { (st.tab + 1) % n };
                st.sel = 0;
                self.cancel_preview(); // leaving the schemes tab abandons the try-on
            }
            Key::Named(NamedKey::Enter) => {
                let sel = st.sel;
                if tab == 2 {
                    self.commit_preview();
                } else if tab == 1 {
                    st.capturing = true;
                } else {
                    self.apply_theme_row(sel);
                }
            }
            // 'e' hands the raw file to the OS editor — the panel covers the common cases, not
            // colour schemes or per-pane settings.
            Key::Character(c) if c.as_str() == "e" => self.open_config_file(),
            _ => {}
        }
        self.window.request_redraw();
    }

    /// The rows the panel actually draws — windowed around the selection, because the keys tab
    /// lists every action and is taller than any card — plus the selection's index *within that
    /// window* and where the window starts. The render builder and the click hit-test both go
    /// through here, so a click can only ever resolve to a row that was drawn under it.
    fn settings_window(&self) -> (Vec<SettingsRow>, usize, usize) {
        let Some(s) = self.settings.as_ref() else { return (Vec::new(), 0, 0) };
        let all = self.settings_rows(s.tab);
        let sel = s.sel.min(all.len().saturating_sub(1));
        let start = sel.saturating_sub(11);
        (all.into_iter().skip(start).take(12).collect(), sel - start, start)
    }

    /// A mouse press while the settings panel is open. Returns whether the panel consumed it —
    /// `true` for anything on the card (including its empty margins, which is what makes it
    /// modal), `false` for a click outside, which falls through to the app underneath.
    ///
    /// On the schemes tab a click selects and previews the scheme under the cursor, and a click on
    /// a row that's already previewing keeps it — so clicking a swatch twice is "try it, keep it"
    /// with no keyboard at all.
    fn settings_click(&mut self, button: MouseButton) -> bool {
        let (x, y) = (self.cursor.0 as f32, self.cursor.1 as f32);
        let (sw, sh) = (self.config.width, self.config.height);
        let Some(st) = self.settings.as_ref() else { return false };
        let (tab, sel) = (st.tab, st.sel);
        let (rows, _, start) = self.settings_window();
        if !self.renderer.settings_hit(x, y, rows.len(), sw, sh) {
            return false;
        }
        if button != MouseButton::Left {
            return true; // consumed: the card is modal, but only the left button acts
        }
        let tabs: Vec<String> = SETTINGS_TABS.iter().map(|s| (*s).to_string()).collect();
        if let Some(i) = self.renderer.settings_tab_at(x, y, &tabs, rows.len(), sw, sh) {
            if i != tab {
                self.cancel_preview();
                if let Some(st) = self.settings.as_mut() {
                    st.tab = i;
                    st.sel = 0;
                }
                self.window.request_redraw();
            }
            return true;
        }
        if let Some(w) = self.renderer.settings_row_at(x, y, rows.len(), sw, sh) {
            let chips = rows[w].swatch.len();
            let on_swatch = self.renderer.settings_swatch_hit(x, chips, rows.len(), sw, sh);
            let i = start + w; // window index -> the row's index in the full list
            // Same row twice = keep it. Checked before moving the selection so the second click
            // commits rather than re-previewing what is already live.
            let repeat = tab == 2 && i == sel;
            if let Some(st) = self.settings.as_mut() {
                st.sel = i;
                st.capturing = false; // a click abandons a half-finished chord capture
            }
            match tab {
                2 if repeat => self.commit_preview(),
                2 => self.preview_selected_scheme(),
                // The theme tab's ribbon is a shortcut into the schemes tab: click the colours to
                // get to where colours can be tried on. Elsewhere on the row a click only selects,
                // so a stray click can't silently cycle a setting.
                0 if on_swatch => self.apply_theme_row(i),
                _ => {}
            }
            self.window.request_redraw();
        }
        true
    }

    /// Try the selected scheme on: push its palette to the daemon so every pane repaints in it,
    /// without touching `gmux.json`. No-op off the schemes tab, and when the scheme is already the
    /// one being previewed (arrowing back onto a row shouldn't re-push a whole palette).
    fn preview_selected_scheme(&mut self) {
        let Some(st) = self.settings.as_ref() else { return };
        if st.tab != 2 {
            return;
        }
        let names = crate::config::preset_names();
        let Some(name) = names.get(st.sel).copied() else { return };
        if self.settings.as_ref().and_then(|s| s.preview.as_deref()) == Some(name) {
            return;
        }
        // The preview must show what COMMITTING would look like, so it resolves through the real
        // config — a hand-set `theme.fg` still wins over the scheme after you keep it, too.
        let p = Config::load().palette_with_preset(name);
        self.client.control(Call::SetPalette { fg: p.fg, bg: p.bg, ansi: p.ansi.to_vec() });
        self.force_full = true; // the daemon re-resolves colors but emits no damage for it
        if let Some(st) = self.settings.as_mut() {
            st.preview = Some(name.to_string());
        }
        self.window.request_redraw();
    }

    /// Put the config's own palette back, dropping whatever was being tried on. Safe to call when
    /// nothing is previewing.
    fn cancel_preview(&mut self) {
        let previewing = self.settings.as_mut().and_then(|s| s.preview.take()).is_some();
        if !previewing {
            return;
        }
        let cfg = Config::load();
        self.send_palette(&cfg);
        self.force_full = true;
        self.window.request_redraw();
    }

    /// Keep the previewed scheme: write it to `gmux.json` (the palette is already live) and go
    /// back to the theme tab, where the colour-scheme row now shows it.
    fn commit_preview(&mut self) {
        let Some(st) = self.settings.as_ref() else { return };
        let names = crate::config::preset_names();
        let Some(name) = names.get(st.sel).copied() else { return };
        self.write_config(|cfg| {
            let theme =
                cfg.as_object_mut().unwrap().entry("theme").or_insert_with(|| serde_json::json!({}));
            if let Some(map) = theme.as_object_mut() {
                // "default" means "no preset key at all", so the built-in palette wins.
                if name == "default" {
                    map.remove("preset");
                } else {
                    map.insert("preset".into(), serde_json::Value::String(name.into()));
                }
                // fg/bg are the LAST layer, so leaving them set would keep overriding the scheme
                // just chosen — a scheme supplies both, so they go with it.
                map.remove("fg");
                map.remove("bg");
            }
        });
        if let Some(st) = self.settings.as_mut() {
            st.preview = None; // committed, not pending: nothing left to restore
            st.tab = 0;
            st.sel = 0;
        }
        self.window.request_redraw();
    }

    /// Enter on a theme row: open the schemes tab, cycle the accent, step the font size, or flip
    /// the boolean.
    fn apply_theme_row(&mut self, sel: usize) {
        match sel {
            // The colour-scheme row is a doorway to the schemes tab, where each scheme is drawn in
            // its own colours and previews live. One place changes the scheme, not two.
            0 => {
                let cur = Config::load().theme.and_then(|t| t.preset);
                let sel = cur
                    .as_deref()
                    .and_then(|c| crate::config::preset_names().iter().position(|n| n.eq_ignore_ascii_case(c)))
                    .unwrap_or(0);
                if let Some(st) = self.settings.as_mut() {
                    st.tab = 2;
                    st.sel = sel;
                }
            }
            1 => {
                let cur = Config::load().theme.and_then(|t| t.accent);
                let idx = cur
                    .as_deref()
                    .and_then(|c| ACCENT_CYCLE.iter().position(|p| p.eq_ignore_ascii_case(c)))
                    .unwrap_or(0);
                let next = ACCENT_CYCLE[(idx + 1) % ACCENT_CYCLE.len()];
                self.write_config(|cfg| {
                    let theme = cfg
                        .as_object_mut()
                        .unwrap()
                        .entry("theme")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(map) = theme.as_object_mut() {
                        // "default" means "no accent key at all", so the built-in wins.
                        if next == "default" {
                            map.remove("accent");
                        } else {
                            map.insert("accent".into(), serde_json::Value::String(next.into()));
                        }
                    }
                });
            }
            2 => {
                // Wrap at the clamp so one key can walk the whole range.
                let next = if self.font_px >= 28.0 { 10.0 } else { self.font_px + 2.0 };
                self.write_config(|cfg| {
                    cfg.as_object_mut()
                        .unwrap()
                        .insert("font_px".into(), serde_json::json!(next));
                });
                self.config_font_px = next;
                self.apply_font_px(next);
            }
            3 => {
                let next = !self.focus_follows_mouse;
                self.write_config(|cfg| {
                    cfg.as_object_mut()
                        .unwrap()
                        .insert("focus_follows_mouse".into(), serde_json::json!(next));
                });
            }
            _ => {}
        }
    }

    /// Read `gmux.json`, apply `edit`, write it back. Everything the panel doesn't touch is
    /// preserved (it edits the parsed JSON, not a template), and the config's own mtime watcher
    /// picks the change up and re-applies it live.
    fn write_config(&mut self, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = config_path();
        let mut cfg: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(t.strip_prefix('\u{feff}').unwrap_or(&t)).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !cfg.is_object() {
            cfg = serde_json::json!({});
        }
        edit(&mut cfg);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&cfg) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    eprintln!("gmux: could not write settings: {e}");
                }
            }
            Err(e) => eprintln!("gmux: could not serialize settings: {e}"),
        }
    }

    /// Hand `gmux.json` to the OS's editor (the panel's 'e' key, and the old Ctrl+, behaviour).
    fn open_config_file(&mut self) {
        let path = config_path();
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, default_template()) {
                eprintln!("gmux: could not write config template {}: {e}", path.display());
            }
        }
        if let Err(e) = std::process::Command::new("cmd")
            .args(["/c", "start", "", &path.to_string_lossy()])
            .spawn()
        {
            eprintln!("gmux: could not open settings {}: {e}", path.display());
        }
    }

    /// A key press while the sidebar filter is open. Escape closes it (restoring the full list),
    /// Enter selects the first surviving workspace and closes it, everything printable edits the
    /// query. Modelled on the rename editor above, which has the same shape.
    fn filter_key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.sidebar_filter = None,
            Key::Named(NamedKey::Enter) => {
                // The first row still visible is what the user is aiming at.
                if let Some(&idx) = self.row_tabs.first() {
                    self.client.control(Call::SelectWindow { index: idx });
                    self.sync_size();
                }
                self.sidebar_filter = None;
                self.force_full = true;
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(q) = self.sidebar_filter.as_mut() {
                    q.pop();
                }
            }
            Key::Named(NamedKey::Space) => {
                if let Some(q) = self.sidebar_filter.as_mut() {
                    q.push(' ');
                }
            }
            // Printable input; same guard as the rename editor (AltGr = Ctrl+Alt still types).
            Key::Character(text) if !mods.super_key() && !(mods.control_key() && !mods.alt_key()) => {
                if let Some(q) = self.sidebar_filter.as_mut() {
                    q.push_str(text.as_str());
                }
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    /// Leave search mode and snap the active pane back to its live screen.
    fn exit_search(&mut self) {
        self.search = None;
        self.set_scroll(self.active_pane, 0);
        self.force_full = true; // refetch the active pane at the live tail
        self.window.request_redraw();
    }

    /// A key press in copy mode. Movement clamps to the active pane's grid; PageUp/PageDown
    /// scroll the viewport while shifting the marks so the selected CONTENT stays put.
    fn copy_mode_key(&mut self, event: &KeyEvent) {
        let (cols, rows) = self
            .snap_cache
            .get(&self.active_pane)
            .map(|s| (s.cols.max(1), s.rows.max(1)))
            .unwrap_or((1, 1));
        let Some(cm) = self.copy_mode.as_mut() else { return };
        let step = |c: &mut (u16, u16), dx: i32, dy: i32| {
            c.0 = (c.0 as i32 + dx).clamp(0, cols as i32 - 1) as u16;
            c.1 = (c.1 as i32 + dy).clamp(0, rows as i32 - 1) as u16;
        };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.copy_mode = None;
                self.clear_selection();
                return;
            }
            Key::Named(NamedKey::Enter) => {
                self.copy_selection();
                self.copy_mode = None;
                self.clear_selection();
                self.notice = Some(("copied".into(), Instant::now()));
                self.window.request_redraw();
                return;
            }
            Key::Named(NamedKey::ArrowLeft) => step(&mut cm.cursor, -1, 0),
            Key::Named(NamedKey::ArrowRight) => step(&mut cm.cursor, 1, 0),
            Key::Named(NamedKey::ArrowUp) => step(&mut cm.cursor, 0, -1),
            Key::Named(NamedKey::ArrowDown) => step(&mut cm.cursor, 0, 1),
            Key::Named(NamedKey::Home) => cm.cursor.0 = 0,
            Key::Named(NamedKey::End) => cm.cursor.0 = cols - 1,
            Key::Named(NamedKey::PageUp | NamedKey::PageDown) => {
                // Scroll a page and shift the marks by the ACTUAL offset delta so the marked
                // content stays selected (viewport-relative coords, content-stable semantics).
                let up = matches!(&event.logical_key, Key::Named(NamedKey::PageUp));
                let page = rows.saturating_sub(1) as i64;
                let before = self.scroll_of(self.active_pane) as i64;
                let pane = self.active_pane;
                self.scroll_by(pane, if up { page } else { -page });
                let delta = (self.scroll_of(pane) as i64 - before) as i32; // + = scrolled up
                let Some(cm) = self.copy_mode.as_mut() else { return };
                let shift = |c: &mut (u16, u16)| {
                    c.1 = (c.1 as i32 + delta).clamp(0, rows as i32 - 1) as u16;
                };
                shift(&mut cm.cursor);
                if let Some(a) = cm.anchor.as_mut() {
                    shift(a);
                }
            }
            Key::Character(t) => match t.as_str() {
                "h" => step(&mut cm.cursor, -1, 0),
                "l" => step(&mut cm.cursor, 1, 0),
                "k" => step(&mut cm.cursor, 0, -1),
                "j" => step(&mut cm.cursor, 0, 1),
                "v" => cm.anchor = if cm.anchor.is_some() { None } else { Some(cm.cursor) },
                "y" => {
                    self.copy_selection();
                    self.copy_mode = None;
                    self.clear_selection();
                    self.notice = Some(("copied".into(), Instant::now()));
                    self.window.request_redraw();
                    return;
                }
                _ => {}
            },
            _ => return, // bare modifiers etc.: no state change, no redraw
        }
        // Mirror the mode's cursor/anchor into the selection highlight (single cell = the cursor).
        let (cursor, anchor) = {
            let cm = self.copy_mode.as_ref().unwrap();
            (cm.cursor, cm.anchor)
        };
        self.selection = Some(Selection {
            pane: self.active_pane,
            start: anchor.unwrap_or(cursor),
            end: cursor,
        });
        self.window.request_redraw();
    }

    /// Cursor-motion hover effects: the link tooltip (band shows the REAL target under the
    /// cursor) and, when enabled, focus-follows-mouse (edge-triggered on pane change; never
    /// during drags — callers gate that).
    fn update_hover(&mut self) {
        let (cx, cy) = self.cursor;
        let (sidebar_w, _, _) = self.areas();
        let over = if cx >= 0.0 && cy >= 0.0 && (cx as u32) >= sidebar_w {
            let cxp = (cx - sidebar_w as f64) as u32;
            let py = cy as u32;
            self.last_panes
                .iter()
                .find(|p| cxp >= p.x && cxp < p.x + p.w && py >= p.y && py < p.y + p.h)
                .cloned()
        } else {
            None
        };
        // Link tooltip: what's under the cursor in the hovered pane's span list.
        let link = over.as_ref().and_then(|pr| {
            let (cw, ch) = self.cell_dims();
            let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
            let (col, row) = pixel_to_cell(
                cx as f32, cy as f32, rect, sidebar_w, self.config.width, self.config.height, cw, ch,
            );
            self.url_spans.get(&pr.id).and_then(|spans| url_at(spans, col, row)).map(str::to_string)
        });
        if link != self.hover_link {
            self.hover_link = link;
            self.window.request_redraw();
        }
        // Focus follows mouse (opt-in): edge-triggered when the hovered pane changes.
        let hover_id = over.map(|p| p.id);
        if self.focus_follows_mouse {
            if let Some(id) = hover_id {
                if hover_id != self.hover_pane && id != self.active_pane {
                    self.client.control(Call::FocusPaneId { pane: id });
                    self.window.request_redraw();
                }
            }
        }
        self.hover_pane = hover_id;
    }

    /// Write the active pane's full scrollback to `Downloads\gmux-<pane>-<stamp>.txt` and flash
    /// the result in the bottom notice band. Failures land in the band too — no silent drops.
    fn export_scrollback(&mut self) {
        let text = match self.client.call(Call::CapturePane { pane: self.active_pane, scrollback: Some(0) }) {
            Ok(ResultBody::Text(t)) => t,
            _ => {
                self.notice = Some(("export failed: could not capture pane".into(), Instant::now()));
                self.window.request_redraw();
                return;
            }
        };
        let dir = std::env::var("USERPROFILE")
            .map(|u| std::path::PathBuf::from(u).join("Downloads"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let stamp = {
            // Seconds since epoch — good enough for a unique, sortable name without a time dep.
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            d
        };
        let path = dir.join(format!("gmux-pane{}-{stamp}.txt", self.active_pane));
        let msg = match std::fs::write(&path, text) {
            Ok(()) => format!("scrollback exported to {}", path.display()),
            Err(e) => format!("export failed: {e}"),
        };
        self.notice = Some((msg, Instant::now()));
        self.window.request_redraw();
    }

    /// Nudge the active pane's split divider by a fractional delta (keyboard resize).
    fn nudge_split(&mut self, dx: f32, dy: f32) {
        self.client.control(Call::ResizeSplit { pane: self.active_pane, dx, dy });
        self.sync_size();
        self.force_full = true;
        self.window.request_redraw();
    }

    /// A key press while the command palette is open: edit the filter, navigate, run, or close.
    fn palette_key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        // Current filtered length (ArrowDown clamp), computed before the mutable borrow below.
        let filtered_len = self
            .palette
            .as_ref()
            .map(|p| palette_items(&self.tab_names, &p.query, &self.palette_recent).len())
            .unwrap_or(0);
        let Some(p) = self.palette.as_mut() else { return };
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.palette = None,
            Key::Named(NamedKey::ArrowUp) => p.selected = p.selected.saturating_sub(1),
            Key::Named(NamedKey::ArrowDown) => {
                p.selected = (p.selected + 1).min(filtered_len.saturating_sub(1));
            }
            Key::Named(NamedKey::Backspace) => {
                p.query.pop();
                p.selected = 0;
            }
            Key::Named(NamedKey::Space) => {
                p.query.push(' ');
                p.selected = 0;
            }
            Key::Named(NamedKey::Enter) => {
                let p = self.palette.take().unwrap();
                // `selected` is an index into the FULL filtered list; the renderer windows the
                // same list around it, so Enter always runs the highlighted row.
                let items = palette_items(&self.tab_names, &p.query, &self.palette_recent);
                if let Some((label, _, cmd)) =
                    items.get(p.selected.min(items.len().saturating_sub(1)))
                {
                    let (label, cmd) = (label.clone(), cmd.clone());
                    match cmd {
                        PaletteCmd::Act(a) => {
                            // Remember action runs (not tabs — those churn) for recency ordering.
                            self.palette_recent.retain(|r| *r != label);
                            self.palette_recent.insert(0, label);
                            self.palette_recent.truncate(5);
                            self.dispatch(a);
                        }
                        PaletteCmd::Tab(i) => {
                            self.client.control(Call::SelectWindow { index: i });
                            self.sync_size();
                            self.force_full = true;
                        }
                    }
                }
            }
            Key::Character(text) if !mods.super_key() && !(mods.control_key() && !mods.alt_key()) => {
                p.query.push_str(text.as_str());
                p.selected = 0;
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    /// Resolve a pending close confirmation: `confirm` (Enter) executes the guarded close, any
    /// other key cancels it. Either way the band disappears.
    fn confirm_close_key(&mut self, confirm: bool) {
        let Some(target) = self.confirm_close.take() else { return };
        if confirm {
            match target {
                ConfirmClose::Pane(pane) => self.client.control(Call::ClosePaneId { pane }),
                ConfirmClose::Window(id) => self.client.control(Call::CloseWindow { id }),
            }
            self.sync_size();
            self.force_full = true;
        }
        self.window.request_redraw();
    }

    /// Jump the active pane's viewport to the previous (`up`) / next command prompt (OSC 133
    /// marks), anchoring the prompt line at the TOP of the view so the command and its output
    /// read downward. Stateless: each press derives the destination from the current offset —
    /// prompts already fully visible near the live tail top-anchor to 0 and are skipped upward.
    fn prompt_jump(&mut self, up: bool) {
        let Ok(ResultBody::Matches(offs)) =
            self.client.call(Call::PromptOffsets { pane: self.active_pane })
        else {
            return;
        };
        let lift = self.active_rows.saturating_sub(1) as u32;
        let cur = self.scroll_of(self.active_pane) as u32;
        let targets: Vec<u32> = offs.iter().map(|&o| o.saturating_sub(lift)).collect();
        let dest = if up {
            targets.iter().copied().filter(|&t| t > cur).min()
        } else {
            // Below the lowest remaining prompt: snap back to the live screen.
            targets.iter().copied().filter(|&t| t < cur).max().or(Some(0))
        };
        if let Some(d) = dest {
            if d != cur {
                self.set_scroll(self.active_pane, d as usize);
                self.force_full = true;
                self.window.request_redraw();
            }
        }
    }

    /// Append the clipboard to the search query (the keyboard Paste chord and right-click both
    /// land here while searching). Control chars (a pasted newline) would corrupt the single-line
    /// query — printable text only.
    fn paste_into_query(&mut self) {
        if let Some(text) = clipboard::get_text(self.hwnd) {
            if let Some(s) = self.search.as_mut() {
                s.query.extend(text.chars().filter(|c| !c.is_control()));
            }
            self.refresh_search();
        }
        self.window.request_redraw();
    }

    /// A key press while search mode is active: edit the query, navigate matches, or exit.
    fn search_key(&mut self, event: &KeyEvent, mods: ModifiersState) {
        // Paste appends to the query (honors the configured Paste chord).
        if self.keymap.action(mods, &event.logical_key) == Some(Action::Paste) {
            self.paste_into_query();
            return;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.exit_search(),
            Key::Named(NamedKey::Enter) => {
                self.search_step(if mods.shift_key() { -1 } else { 1 });
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(s) = self.search.as_mut() {
                    s.query.pop();
                }
                self.refresh_search();
            }
            Key::Named(NamedKey::Space) => {
                if let Some(s) = self.search.as_mut() {
                    s.query.push(' ');
                }
                self.refresh_search();
            }
            // Printable input. AltGr arrives as Ctrl+Alt on Windows, so accept Ctrl+Alt text
            // (@ { } € on non-US layouts); reject pure-Ctrl and Super chords as non-text.
            Key::Character(text) if !mods.super_key() && !(mods.control_key() && !mods.alt_key()) => {
                if let Some(s) = self.search.as_mut() {
                    s.query.push_str(text.as_str());
                }
                self.refresh_search();
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    /// Re-run the search for the current query and jump to the first match. An empty query clears
    /// the matches (and leaves the viewport where it is).
    fn refresh_search(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else { return };
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            match self.client.call(Call::SearchPane { pane: self.active_pane, query }) {
                Ok(ResultBody::Matches(m)) => m,
                _ => Vec::new(),
            }
        };
        if let Some(s) = self.search.as_mut() {
            s.matches = matches;
            s.current = 0;
        }
        self.jump_to_current_match();
    }

    /// Move to the next/previous match (wrapping) and jump the viewport to it.
    fn search_step(&mut self, dir: i64) {
        let Some(s) = self.search.as_mut() else { return };
        if s.matches.is_empty() {
            return;
        }
        s.current = step_index(s.current, s.matches.len(), dir);
        self.jump_to_current_match();
    }

    /// Scroll the active pane to the current match's offset (a `search-pane` result is a `GetGrid`
    /// offset). Forces a refetch so the server-side clamp applies via the normal render path.
    ///
    /// The match's natural offset puts it on the BOTTOM visible row — exactly the row the search
    /// band covers (the renderer shrinks the viewport by `SEARCH_BAR` without resizing the pty).
    /// Scroll back a little less so the match lands above the band. Matches within a band-row of
    /// the live bottom can't be lifted (can't scroll below live); they stay partially covered.
    fn jump_to_current_match(&mut self) {
        if let Some(off) = self.search.as_ref().and_then(|s| s.matches.get(s.current).copied()) {
            let ch = self.cell_dims().1;
            let band_rows = (SEARCH_BAR as u32).div_ceil(ch) as usize;
            self.set_scroll(self.active_pane, (off as usize).saturating_sub(band_rows));
            self.force_full = true;
            self.window.request_redraw();
        }
    }

    /// Ctrl+click handler: open the URL under the cursor, if any. Returns true when one was opened
    /// (so the caller suppresses reporting/selection). Mirrors `on_click`'s pane hit-test.
    fn open_url_at_cursor(&mut self) -> bool {
        let (cx, cy) = self.cursor;
        if cx < 0.0 || cy < 0.0 {
            return false;
        }
        let (sidebar_w, _, _) = self.areas();
        if (cx as u32) < sidebar_w {
            return false;
        }
        let cxp = (cx - sidebar_w as f64) as u32;
        let py = cy as u32;
        let Some(pr) = self
            .last_panes
            .iter()
            .find(|p| cxp >= p.x && cxp < p.x + p.w && py >= p.y && py < p.y + p.h)
            .cloned()
        else {
            return false;
        };
        let (cw, ch) = self.cell_dims();
        let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
        let (col, row) = pixel_to_cell(cx as f32, cy as f32, rect, sidebar_w, self.config.width, self.config.height, cw, ch);
        let Some(spans) = self.url_spans.get(&pr.id) else { return false };
        match url_at(spans, col, row) {
            Some(url) => {
                open_url(url);
                true
            }
            None => false,
        }
    }


    /// Paste the clipboard into the active pane: CRLF/LF normalized to a bare CR, wrapped in
    /// bracketed-paste markers when the app enabled DECSET 2004, and chunked so a huge paste
    /// doesn't exceed the pipe's per-line cap. Shared by the Paste action and right-click paste.
    fn paste_clipboard(&mut self) {
        let Some(text) = clipboard::get_text(self.hwnd) else { return };
        self.paste_text(&text);
    }

    /// Send `text` to the active pane as a paste: newline-normalized, bracketed-paste wrapped when
    /// the app asked for it (DECSET 2004), and chunked. The single funnel for clipboard pastes and
    /// file drops — raw `SendKeys` of pasted text into a full-screen TUI (vim in normal mode)
    /// would otherwise execute it as commands.
    fn paste_text(&mut self, text: &str) {
        // Terminals expect a bare CR for Enter; normalize CRLF (Windows) then any remaining lone
        // LF (Unix clipboards).
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        // When the app enabled bracketed paste (DECSET 2004), wrap the text so the shell treats it
        // as one literal paste instead of executing each line.
        let text = if self.paste_is_bracketed() {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        // The pipe rejects lines over MAX_LINE (1 MiB) and JSON escaping inflates further -- chunk
        // huge pastes into multiple SendKeys instead of losing them.
        const CHUNK: usize = 64 * 1024;
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let mut cut = rest.len().min(CHUNK);
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let (piece, tail) = rest.split_at(cut);
            self.client.control(Call::SendKeys {
                pane: self.active_pane,
                text: piece.to_string(),
                enter: false,
            });
            rest = tail;
        }
    }

    /// Handle a file dropped onto the window: send its (space-quoted) path to the active pane as
    /// text, like a paste. Multiple files from one drag arrive as separate events in quick
    /// succession — space-separate them.
    fn drop_file(&mut self, path: &std::path::Path) {
        let mut text = quote_path(&path.to_string_lossy());
        let now = Instant::now();
        // ponytail: a 1s window treats back-to-back drops as one gesture (winit has no per-gesture
        // batching), prefixing the second+ file with a space so paths don't run together.
        if matches!(self.last_drop, Some(t) if now.duration_since(t) < Duration::from_secs(1)) {
            text.insert(0, ' ');
        }
        self.last_drop = Some(now);
        self.set_scroll(self.active_pane, 0); // dropping sends input; snap the active pane to live
        // Through the paste funnel, not raw SendKeys: a bracketed-paste-aware TUI (vim) must see
        // the path as a literal insert, not as normal-mode commands.
        self.paste_text(&text);
        self.window.request_redraw();
    }

    fn cell_dims(&self) -> (u32, u32) {
        (self.renderer.cell_w().max(1), self.renderer.cell_h().max(1))
    }

    /// Sidebar overflow window: `(first_visible_row, visible_count)`, clamped to the tab count
    /// and the rows that fit. ponytail: mirrors the renderer's sidebar metrics (16px pad +
    /// label row, 48px rows + 4px gaps = 52 stride, one slot reserved for '+ new tab') — the
    /// same deliberate constant duplication the row hit-test already documents.
    fn sidebar_window(&self) -> (usize, usize) {
        let stride = 52.0_f32;
        let top = 24.0 + self.renderer.cell_h() as f32;
        let usable = (self.config.height as f32 - top - stride).max(stride);
        let cap = ((usable / stride) as usize).max(1);
        let max_off = self.tab_count.saturating_sub(cap);
        (self.sidebar_scroll.min(max_off), cap.min(self.tab_count.saturating_sub(self.sidebar_scroll.min(max_off))))
    }

    /// Map a VISIBLE sidebar row index (renderer hit-test) to the real tab index. With a filter
    /// active the visible rows are not a contiguous slice of the tabs, so this is a lookup rather
    /// than `index + scroll offset`; the fallback covers the first frame, before any render.
    fn real_tab(&self, visible_idx: usize) -> usize {
        self.row_tabs
            .get(visible_idx)
            .copied()
            .unwrap_or_else(|| visible_idx + self.sidebar_window().0)
    }

    /// The visible row index under `y`, or `None` when the cursor is on a group header, in a gap,
    /// or past the list. Headers and rows share one hit-test walk, so they can't be confused.
    fn hit_row(&self, y: f64) -> Option<usize> {
        match self.renderer.sidebar_item_at(y as f32, &self.item_heights).and_then(|i| self.item_meta.get(i)) {
            Some(ItemMeta::Row(vi)) => Some(*vi),
            _ => None,
        }
    }

    /// If `(x, y)` landed on visible row `vi`'s PR chip and that badge carries a URL, open it and
    /// report `true` (the caller then skips tab selection). A chip with no URL — a hand-set badge
    /// — falls through to the normal row click rather than doing nothing surprising.
    fn open_pr_at(&mut self, x: f64, y: f64, vi: usize) -> bool {
        let Some((number, url, has_color)) = self.row_pr.get(&vi).cloned() else { return false };
        let Some(url) = url else { return false };
        let Some(item) = self
            .item_meta
            .iter()
            .position(|m| matches!(m, ItemMeta::Row(i) if *i == vi))
        else {
            return false;
        };
        let top = self.renderer.sidebar_item_top(item, &self.item_heights);
        if !self.renderer.pr_chip_hit(x as f32, y as f32, top, has_color, number) {
            return false;
        }
        // Same scheme guard as OSC-8 links: only http(s) reaches explorer.exe.
        if link_scheme_ok(&url) {
            open_url(&url);
        }
        true
    }

    /// The pane whose title strip contains `(x, y)`, if any. The strip is the band between the
    /// pane's top border and its cell area — pressing it grabs the pane rather than its text.
    fn title_strip_at(&self, x: f64, y: f64) -> Option<u64> {
        let (sidebar_w, _, surf_h) = self.areas();
        let surf_w = self.config.width;
        self.last_panes
            .iter()
            .find(|p| {
                let rect = Rect { x: p.x + sidebar_w, y: p.y, w: p.w, h: p.h };
                self.renderer.title_strip_hit(x as f32, y as f32, rect, sidebar_w, surf_w, surf_h)
            })
            .map(|p| p.id)
    }

    /// If `(x, y)` landed on a pane's title-strip close button, close THAT pane and report `true`.
    /// Only the panes that draw the button (active, or hovered) are considered, so an invisible
    /// hit-box can't swallow a click meant to focus a pane.
    fn close_pane_button_at(&mut self, x: f64, y: f64) -> bool {
        let (sidebar_w, _, surf_h) = self.areas();
        let surf_w = self.config.width;
        let hovered = self.pane_under_cursor();
        let target = self.last_panes.iter().find(|p| {
            let shown = p.id == self.active_pane || p.id == hovered;
            if !shown {
                return false;
            }
            // last_panes are content-area coords; shift into window coords for the hit-test.
            let rect = Rect { x: p.x + sidebar_w, y: p.y, w: p.w, h: p.h };
            let att = if p.attention { Attention::Pending } else { Attention::Quiet };
            self.renderer.pane_close_hit(
                x as f32,
                y as f32,
                rect,
                sidebar_w,
                surf_w,
                surf_h,
                p.id == self.active_pane,
                att,
            )
        });
        let Some(pane) = target.map(|p| p.id) else { return false };
        // A pane running something asks first, exactly like Ctrl+Shift+W does.
        let busy = matches!(self.client.call(Call::PaneBusy { pane }), Ok(ResultBody::Busy(true)));
        if busy {
            self.confirm_close = Some(ConfirmClose::Pane(pane));
        } else {
            self.client.control(Call::ClosePaneId { pane });
            self.sync_size();
            self.force_full = true;
        }
        self.window.request_redraw();
        true
    }

    /// If `(x, y)` landed on visible row `vi`'s close button, close that workspace (with the same
    /// busy confirmation a middle-click gets) and report `true` so the caller skips selection.
    fn close_button_at(&mut self, x: f64, y: f64, vi: usize) -> bool {
        let (sidebar_w, _, _) = self.areas();
        let Some(item) = self
            .item_meta
            .iter()
            .position(|m| matches!(m, ItemMeta::Row(i) if *i == vi))
        else {
            return false;
        };
        let top = self.renderer.sidebar_item_top(item, &self.item_heights);
        if !self.renderer.close_button_hit(x as f32, y as f32, top, sidebar_w) {
            return false;
        }
        self.close_tab(self.real_tab(vi))
    }

    /// Close the workspace at tab index `idx` by its STABLE id, asking first when it has running
    /// children. Shared by the middle-click, the hover close button, and the close action.
    fn close_tab(&mut self, idx: usize) -> bool {
        let Some(&win) = self.tab_ids.get(idx) else { return false };
        let busy =
            matches!(self.client.call(Call::WindowBusy { id: win }), Ok(ResultBody::Busy(true)));
        if busy {
            self.confirm_close = Some(ConfirmClose::Window(win));
        } else {
            self.client.control(Call::CloseWindow { id: win });
            self.sync_size();
            self.force_full = true;
        }
        self.window.request_redraw();
        true
    }

    /// Which sidebar item a drop at `y` would land on: an item index, `item_meta.len()` for a drop
    /// past the last item, or `None` above the list. Gaps between items resolve to the item below,
    /// so the indicator never blinks out while the cursor crosses a boundary.
    fn drop_target(&self, y: f64) -> Option<usize> {
        if self.item_meta.is_empty() {
            return None;
        }
        if let Some(i) = self.renderer.sidebar_item_at(y as f32, &self.item_heights) {
            return Some(i);
        }
        let top = self.renderer.sidebar_item_top(0, &self.item_heights);
        if (y as f32) < top {
            return None; // above the first row (the "WORKSPACES" label)
        }
        let end = self.renderer.sidebar_item_top(self.item_meta.len(), &self.item_heights);
        if (y as f32) >= end {
            return Some(self.item_meta.len()); // past the last item: append
        }
        // In a gap: the item whose top edge is the next one down.
        (0..self.item_meta.len())
            .find(|i| self.renderer.sidebar_item_top(*i, &self.item_heights) > y as f32)
            .or(Some(self.item_meta.len()))
    }

    /// Apply a finished reorder drag: move the window and, if it was dropped into (or out of) a
    /// group, re-file it. Grouping reorders rows visually, so "where it looks like it will land"
    /// and "the daemon's tab index" are different things — the target's own group is what the drop
    /// should mean.
    fn finish_reorder(&mut self, from_row: usize, over: Option<usize>) {
        let Some(over) = over else { return };
        let Some(&win) = self.tab_ids.get(from_row) else { return };
        let from_group = self.row_groups.get(from_row).cloned().flatten();

        let (to_vi, to_group) =
            drop_decision(&self.item_meta, &self.row_groups, self.row_groups.len(), over);
        let to_row = self.real_tab(to_vi);

        if to_group != from_group {
            self.client.control(Call::GroupWindow {
                id: win,
                group: to_group.unwrap_or_default(),
            });
        }
        if to_row != from_row {
            self.client.control(Call::MoveWindow { from: from_row, to: to_row });
        }
        self.sync_size();
        self.window.request_redraw();
    }

    /// The group name whose header sits under `y`, if any.
    fn hit_header(&self, y: f64) -> Option<String> {
        match self.renderer.sidebar_item_at(y as f32, &self.item_heights).and_then(|i| self.item_meta.get(i)) {
            Some(ItemMeta::Header(g)) => Some(g.clone()),
            _ => None,
        }
    }

    /// Whether the cursor sits on the active pane's search band (the bottom `SEARCH_BAR` strip the
    /// renderer draws while searching, plus the margin/border sliver below it — clicks there hit
    /// nothing anyway).
    fn cursor_in_search_band(&self) -> bool {
        let Some((rect, inside)) = self.active_pane_rect() else { return false };
        inside
            && self.cursor.1 >= (rect.y + rect.h) as f64 - (SEARCH_BAR + MARGIN + BORDER) as f64
    }

    /// Whether the active pane's app asked for SGR (1006) mouse encoding.
    fn sgr_mouse(&self) -> bool {
        self.active_mouse_mode & MOUSE_SGR != 0
    }

    /// The active pane's rectangle in window coords (sidebar offset applied), plus whether the
    /// cursor currently sits inside it. `None` if the active pane isn't in the last layout yet.
    fn active_pane_rect(&self) -> Option<(Rect, bool)> {
        let (sidebar_w, _, _) = self.areas();
        let pr = self.last_panes.iter().find(|p| p.id == self.active_pane)?;
        let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
        let (cx, cy) = self.cursor;
        let inside = cx >= rect.x as f64
            && cx < (rect.x + rect.w) as f64
            && cy >= rect.y as f64
            && cy < (rect.y + rect.h) as f64;
        Some((rect, inside))
    }

    /// The active pane's scrollback scrollbar track rectangle `(x0, y0, x1, y1)` in window coords:
    /// an 8px strip at the cell-area right edge, spanning the cell area's height. Mirrors the
    /// renderer's cell-area geometry (MARGIN/GAP/BORDER/INSET/TITLE_STRIP) so the hit-test matches
    /// the drawn bar. `rect` is the pane rect in window coords (sidebar offset applied).
    fn scrollbar_track(&self, rect: Rect) -> (f32, f32, f32, f32) {
        // The scrollbar sits at the cell-area right edge, so only the top/right/bottom insets matter
        // (the left margin is irrelevant here). Mirrors renderer.rs's per-pane edge shrink.
        let (surf_w, surf_h) = (self.config.width, self.config.height);
        let (ox, oy, ow, oh) = (rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
        let t = if rect.y == 0 { MARGIN } else { GAP / 2.0 };
        let rgt = if rect.x + rect.w >= surf_w { MARGIN } else { GAP / 2.0 };
        let bot = if rect.y + rect.h >= surf_h { MARGIN } else { GAP / 2.0 };
        let x1 = ox + ow - rgt - BORDER - INSET; // cell-area right edge
        let x0 = x1 - SCROLLBAR_W;
        let y0 = oy + t + BORDER + TITLE_STRIP + INSET; // cell-area top
        // While searching the renderer shrinks the visible cell area by SEARCH_BAR — mirror it,
        // or the thumb is miscalibrated by that band and lags the cursor during a drag.
        let sb = if self.search.is_some() { SEARCH_BAR } else { 0.0 };
        let y1 = (y0 + (oh - t - bot - 2.0 * BORDER - TITLE_STRIP - 2.0 * INSET - sb)).max(y0);
        (x0, y0, x1, y1)
    }

    /// A left press on the scrollbar thumb/track begins a scrollbar drag and jumps the viewport to
    /// the cursor. Returns true (consuming the click) only when the press lands on the bar and there
    /// is scrollback to scroll (the active pane's offset > 0 and history present).
    fn grab_scrollbar(&mut self) -> bool {
        if self.scroll_of(self.active_pane) == 0 || self.scroll_history == 0 {
            return false;
        }
        let (cx, cy) = self.cursor;
        if cx < 0.0 || cy < 0.0 {
            return false;
        }
        let Some((rect, _)) = self.active_pane_rect() else { return false };
        let (x0, y0, x1, y1) = self.scrollbar_track(rect);
        let (fx, fy) = (cx as f32, cy as f32);
        if fx < x0 || fx > x1 || fy < y0 || fy > y1 {
            return false;
        }
        self.scrollbar_drag = true;
        self.drag_scrollbar_to_cursor();
        true
    }

    /// Map the current cursor y to a scrollback offset within the active pane's scrollbar track and
    /// jump the viewport there. Called while `scrollbar_drag` is live.
    fn drag_scrollbar_to_cursor(&mut self) {
        let Some((rect, _)) = self.active_pane_rect() else { return };
        let (_, y0, _, y1) = self.scrollbar_track(rect);
        let off = scrollbar_offset_at(self.cursor.1 as f32, y0, y1 - y0, self.scroll_history);
        if off != self.scroll_of(self.active_pane) {
            self.set_scroll(self.active_pane, off);
            self.selection = None;
            self.force_full = true;
            self.window.request_redraw();
        }
    }

    /// Close the sidebar tab under the cursor (middle-click). Returns true (consuming the click)
    /// when the cursor is over a sidebar row; a click in the sidebar's empty area or outside it
    /// no-ops.
    fn close_tab_under_cursor(&mut self) -> bool {
        let (cx, cy) = self.cursor;
        if cx < 0.0 || cy < 0.0 {
            return false;
        }
        let (sidebar_w, _, _) = self.areas();
        if (cx as u32) >= sidebar_w {
            return false;
        }
        // By stable id (inside `close_tab`): a window removed daemon-side since the last render
        // shifts the indices, but never re-targets an id — ids are never reused, so a stale one
        // no-ops instead of closing someone else's workspace.
        match self.hit_row(cy) {
            Some(vi) => self.close_tab(self.real_tab(vi)),
            None => false,
        }
    }

    /// The cursor's 1-based cell within `rect` (mouse reports are 1-based; `pixel_to_cell` clamps
    /// pixels in the chrome/outside to the pane's visible grid).
    fn active_cell(&self, rect: Rect) -> (u16, u16) {
        let (sidebar_w, _, _) = self.areas();
        let (cw, ch) = self.cell_dims();
        let (col, row) = pixel_to_cell(
            self.cursor.0 as f32,
            self.cursor.1 as f32,
            rect,
            sidebar_w,
            self.config.width,
            self.config.height,
            cw,
            ch,
        );
        (col + 1, row + 1)
    }

    /// Send an encoded mouse sequence to the active pane. ponytail: `SendKeys` carries a `String`,
    /// and SGR reports are pure ASCII so they round-trip exactly; the legacy X10 fallback packs
    /// bytes >127 for cols/rows past 95, which a UTF-8 string can't carry — `from_utf8_lossy` is
    /// exact for the ASCII/SGR case and best-effort otherwise. Upgrade path if pure-X10 apps in
    /// wide panes ever matter: a bytes-carrying wire call. Modern apps enable SGR, sidestepping it.
    fn send_mouse(&mut self, seq: Vec<u8>) {
        let text = String::from_utf8_lossy(&seq).into_owned();
        self.client.control(Call::SendKeys { pane: self.active_pane, text, enter: false });
    }

    /// Forward a mouse button press/release to the active pane's app when it wants mouse events,
    /// Shift isn't held, and (press) the cursor is in its cell area or (release) the matching press
    /// was reported. Returns true when reported, so the caller suppresses the local selection/focus.
    fn report_button(&mut self, b: u8, pressed: bool, shift: bool) -> bool {
        if self.active_mouse_mode == 0 {
            // The app turned reporting off while a reported button was held: clear the tracked
            // press so a later re-enable can't hallucinate a drag from the stale state.
            if !pressed && self.mouse_down == Some(b) {
                self.mouse_down = None;
                self.mouse_last_cell = None;
            }
            return false;
        }
        if pressed {
            // Shift bypasses reporting so gmux's own selection/focus still works over a mouse app.
            if shift {
                return false;
            }
            let Some((rect, true)) = self.active_pane_rect() else { return false };
            let (col, row) = self.active_cell(rect);
            self.mouse_down = Some(b);
            self.mouse_last_cell = Some((col, row));
            let seq = encode_mouse(self.sgr_mouse(), b, true, col, row);
            self.send_mouse(seq);
            true
        } else {
            // Release only if we reported this button's press (keeps press/release matched even if
            // the cursor left the pane or Shift changed mid-drag).
            if self.mouse_down != Some(b) {
                return false;
            }
            self.mouse_down = None;
            self.mouse_last_cell = None;
            let (rect, _) = match self.active_pane_rect() {
                Some(r) => r,
                None => return true, // pane gone; still consume it (its press was reported)
            };
            let (col, row) = self.active_cell(rect);
            let seq = encode_mouse(self.sgr_mouse(), b, false, col, row);
            self.send_mouse(seq);
            true
        }
    }

    /// Forward pointer motion to the active pane's app: a held-button drag (mode has `MOUSE_DRAG`
    /// or `MOUSE_MOTION`) reports the held button + 32; buttonless motion (mode has `MOUSE_MOTION`)
    /// reports button 35. Deduped per cell. Returns true when the motion was consumed by reporting.
    fn report_motion(&mut self, shift: bool) -> bool {
        if self.active_mouse_mode == 0 || shift {
            return false;
        }
        let drag = self.active_mouse_mode & MOUSE_DRAG != 0;
        let any = self.active_mouse_mode & MOUSE_MOTION != 0;
        let button = match self.mouse_down {
            Some(b) if drag || any => b + 32,
            None if any => 35,
            _ => return false, // no motion reporting for this mode/button state
        };
        let Some((rect, true)) = self.active_pane_rect() else { return false };
        let (col, row) = self.active_cell(rect);
        if self.mouse_last_cell == Some((col, row)) {
            return true; // same cell: consume the motion but don't spam a duplicate report
        }
        self.mouse_last_cell = Some((col, row));
        let seq = encode_mouse(self.sgr_mouse(), button, true, col, row);
        self.send_mouse(seq);
        true
    }

    /// Forward a wheel notch to the active pane's app (button 64 up / 65 down) when it wants mouse
    /// events and the cursor is over its cell area. Returns true when reported (caller skips
    /// gmux scrollback).
    fn report_wheel(&mut self, delta: MouseScrollDelta) -> bool {
        if self.active_mouse_mode == 0 {
            return false;
        }
        let ch = self.cell_dims().1 as f64;
        let (y, notches) = match delta {
            MouseScrollDelta::LineDelta(_, y) => (y as f64, (y.abs().round() as u32).max(1)),
            MouseScrollDelta::PixelDelta(p) => (p.y, ((p.y.abs() / ch).round() as u32).max(1)),
        };
        if y == 0.0 {
            return false; // a purely horizontal wheel event: leave it to the fallthrough (no-op)
        }
        let Some((rect, true)) = self.active_pane_rect() else { return false };
        let button = if y > 0.0 { 64 } else { 65 };
        let (col, row) = self.active_cell(rect);
        let sgr = self.sgr_mouse();
        for _ in 0..notches.min(10) {
            let seq = encode_mouse(sgr, button, true, col, row);
            self.send_mouse(seq);
        }
        true
    }

    fn areas(&self) -> (u32, u32, u32) {
        let sidebar_w = self.renderer.sidebar_width().min(self.config.width / 3);
        // The browser panel eats from the right of the content area, so the terminal panes reflow
        // through the SAME ResizeView path a window resize uses — the daemon needs no new concept.
        let content_w =
            self.config.width.saturating_sub(sidebar_w).saturating_sub(self.dock_w).max(1);
        (sidebar_w, content_w, self.config.height)
    }

    /// The browser panel's rect in window coords: the column to the right of the panes.
    #[cfg(feature = "browser")]
    fn dock_rect(&self) -> (i32, i32, i32, i32) {
        let (sidebar_w, content_w, h) = self.areas();
        ((sidebar_w + content_w) as i32, 0, self.dock_w as i32, h as i32)
    }

    /// Scroll the active pane's viewport by `lines` (positive = deeper into history), clamped
    /// locally to the last-seen history depth; the daemon clamps again server-side.
    /// Whether pasted text should be wrapped in bracketed-paste markers (the active pane's
    /// application turned on DECSET 2004).
    fn paste_is_bracketed(&self) -> bool {
        self.active_bracketed
    }

    /// The scrollback offset of pane `id` (0 = live tail; a missing map entry is 0).
    fn scroll_of(&self, id: u64) -> usize {
        self.pane_scroll.get(&id).copied().unwrap_or(0)
    }

    /// Set pane `id`'s scrollback offset, dropping the entry at 0 so the map holds only scrolled
    /// panes (keeps the fetch gate and eviction cheap).
    fn set_scroll(&mut self, id: u64, off: usize) {
        if off == 0 {
            self.pane_scroll.remove(&id);
        } else {
            self.pane_scroll.insert(id, off);
        }
    }

    /// The pane id under the cursor (content-area hit-test), falling back to the active pane when
    /// the cursor is over the sidebar or empty space. Targets wheel scroll at the pointed-at pane.
    fn pane_under_cursor(&self) -> u64 {
        let (cx, cy) = self.cursor;
        let (sidebar_w, _, _) = self.areas();
        if cx < 0.0 || cy < 0.0 || (cx as u32) < sidebar_w {
            return self.active_pane;
        }
        let cxp = (cx - sidebar_w as f64) as u32;
        let py = cy as u32;
        self.last_panes
            .iter()
            .find(|p| cxp >= p.x && cxp < p.x + p.w && py >= p.y && py < p.y + p.h)
            .map(|p| p.id)
            .unwrap_or(self.active_pane)
    }

    /// Scroll `pane`'s viewport by `lines` (positive = deeper into history). The lower bound is 0;
    /// the upper bound is clamped locally only for the active pane (whose history depth we track).
    /// ponytail: a non-active pane isn't top-clamped here — the next GetGrid clamps to that pane's
    /// history server-side and we accept the clamped offset back into `pane_scroll`.
    fn scroll_by(&mut self, pane: u64, lines: i64) {
        let cur = self.scroll_of(pane) as i64;
        let max = if pane == self.active_pane { self.scroll_history as i64 } else { i64::MAX };
        let next = (cur + lines).clamp(0, max) as usize;
        if next != cur as usize {
            self.set_scroll(pane, next);
            // Scrolling moves content under a viewport-anchored selection; clear it rather than
            // let the highlight drift onto unintended text.
            self.selection = None;
            self.window.request_redraw();
        }
    }

    /// Scroll the active pane by one page (`dir` = +1 up into history, -1 back toward live).
    fn scroll_page(&mut self, dir: i64) {
        let page = if self.active_rows > 1 { self.active_rows - 1 } else { 24 };
        self.scroll_by(self.active_pane, dir * page as i64);
    }

    /// Tell the daemon our content geometry so it resizes its panes.
    fn sync_size(&mut self) {
        let (_, content_w, h) = self.areas();
        let (cw, ch) = self.cell_dims();
        let pane_chrome = self.renderer.pane_chrome_px();
        let pane_chrome_y = self.renderer.pane_chrome_y_px();
        self.client.control(Call::ResizeView {
            w: content_w,
            h,
            cell_w: cw,
            cell_h: ch,
            pane_chrome,
            pane_chrome_y,
        });
    }

    /// Push `config`'s full terminal palette to the daemon (fg/bg + 16 system colors), which
    /// applies it to every pane. Sent once after connecting and on each config hot-reload. A
    /// pre-palette daemon rejects the unknown method; `control` discards the error, so old daemons
    /// simply keep their built-in colors.
    fn send_palette(&mut self, config: &Config) {
        self.client.control(palette_call(config));
    }

    fn clear_active_toast(&self) {
        if let Some(nf) = &self.notifier {
            nf.clear(&format!("pane-{}", self.active_pane), TOAST_GROUP);
        }
    }

    fn fire_toast(&mut self, n: &NotifyWire) {
        let now = Instant::now();
        if let Some(prev) = self.last_toast.get(&n.pane) {
            if now.duration_since(*prev) < TOAST_MIN_INTERVAL {
                return;
            }
        }
        self.last_toast.insert(n.pane, now);
        let title = if n.title.is_empty() { "gmux".to_string() } else { n.title.clone() };
        let req = ToastRequest {
            tag: format!("pane-{}", n.pane),
            group: TOAST_GROUP.to_string(),
            title,
            body: n.body.clone(),
            urgency: match n.urgency {
                0 => NUrgency::Low,
                2 => NUrgency::Critical,
                _ => NUrgency::Normal,
            },
            launch_arg: format!("pane={}", n.pane),
        };
        if let Some(nf) = &self.notifier {
            let _ = nf.show(&req);
        }
        flash_window(self.hwnd, true);
    }

    /// Sync the window title to the active pane: "<pane title> — gmux", or plain "gmux" when there
    /// are no panes / the active pane's title is empty. Only calls `set_title` on an actual change —
    /// retitling re-enters the event loop on Windows, so spamming it every frame stalls input.
    fn sync_title(&mut self) {
        let title = self
            .last_panes
            .iter()
            .find(|p| p.id == self.active_pane)
            .map(|p| p.title.trim())
            .filter(|t| !t.is_empty())
            .map(|t| format!("{t} \u{2014} gmux"))
            .unwrap_or_else(|| "gmux".to_string());
        if title != self.last_title {
            self.window.set_title(&title);
            self.last_title = title;
        }
    }

    fn render(&mut self) {
        // Minimized / degenerate surface: don't acquire a frame at all (a ~1x1 render target trips
        // a wgpu scissor-rect validation panic). Nothing is acquired here, so the
        // every-acquired-frame-must-present invariant is untouched. Restore re-arms via `Resized`.
        if self.minimized || self.config.width < MIN_SURFACE || self.config.height < MIN_SURFACE {
            return;
        }
        use wgpu::CurrentSurfaceTexture::{Suboptimal, Success};
        let frame = match self.surface.get_current_texture() {
            Success(t) | Suboptimal(t) => t,
            _ => {
                self.surface.configure(&self.renderer.device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let (w, h) = (self.config.width, self.config.height);
        let (sidebar_w, content_w, _) = self.areas();

        // Build this frame's draw data. It stays empty while the daemon is still connecting or if a
        // layout/grid fetch fails — but we ALWAYS fall through to the present below. Dropping an
        // acquired SurfaceTexture unpresented exhausts the swapchain and wedges the window white, so
        // every path presents (a cleared frame when there's nothing to draw).
        let mut rows: Vec<SidebarRow> = Vec::new();
        // Each visible row's group (parallel to `rows`), used to fold them under headers below.
        let mut groups: Vec<Option<String>> = Vec::new();
        // Per pane: snapshot, attention, active, rect (window coords), scroll offset, id, title.
        let mut snaps: Vec<(PaneSnapshot, Attention, bool, Rect, u32, u64, String)> = Vec::new();
        // URL spans detected this frame, keyed by pane id (rebuilt every render).
        let mut url_spans: HashMap<u64, Vec<UrlSpan>> = HashMap::new();
        if let Ok(ResultBody::Layout(layout)) = self.client.call(Call::GetLayout { w: content_w, h }) {
            if layout.active_pane != self.active_pane {
                // The active pane changed daemon-side (e.g. the old one exited). ponytail: with
                // per-pane scroll the new active pane keeps its own offset (a gone pane's entry is
                // evicted below), so no global snap here. Any open search's matches are offsets into
                // the OLD pane's scrollback — drop them so Enter can't jump the new pane to a
                // meaningless line (typing re-searches the new pane).
                if let Some(s) = self.search.as_mut() {
                    s.matches.clear();
                    s.current = 0;
                }
            }
            self.active_pane = layout.active_pane;
            // Cache for mouse hit-testing (content-area coords; the sidebar offset is applied below).
            self.last_panes = layout.panes.clone();
            self.tab_count = layout.tabs.len();
            self.tab_ids = layout.tabs.iter().map(|t| t.id).collect();
            self.tab_names = layout.tabs.iter().map(|t| t.name.clone()).collect();
            self.active_tab = layout.tabs.iter().position(|t| t.active).unwrap_or(0);
            // A tab renamed out of existence (closed elsewhere) can't keep swallowing keystrokes.
            if let Some(r) = self.rename.as_ref() {
                if !self.tab_ids.contains(&r.id) {
                    self.rename = None;
                }
            }

            rows = layout
                .tabs
                .iter()
                .map(|t| {
                    // While renaming this tab, show the live buffer + '_' caret instead of its name.
                    let name = match self.rename.as_ref() {
                        Some(r) if r.id == t.id => format!("{}_", r.buffer),
                        _ => t.name.clone(),
                    };
                    SidebarRow { name, branch: t.branch.clone(), attention: t.attention, unread: t.unread, color: t.color.clone(), busy: t.busy, dragging: false, pr: t.pr.as_ref().map(|p| (p.number, p.status.clone())), active: t.active, progress: t.progress, progress_error: t.progress_error, hover: false }
                })
                .collect();
            // Tab overflow: window the rows to what fits (wheel over the sidebar scrolls). The
            // clamp also heals a stale offset after tabs close.
            let (off, cap) = self.sidebar_window();
            self.sidebar_scroll = off;
            if off > 0 || rows.len() > cap {
                rows = rows.into_iter().skip(off).take(cap).collect();
            }
            groups = layout.tabs.iter().skip(off).take(cap).map(|t| t.group.clone()).collect();
            // The filter narrows the visible rows (and their parallel group list) before grouping,
            // so a group whose members all filter out disappears with them.
            // Visible row -> real tab index. Without a filter this is just `i + off`, but a filter
            // punches holes in it, and every gesture (select, close, rename, drag) resolves through
            // it — so it is built here rather than recomputed as arithmetic at each call site.
            self.row_tabs = (off..off + rows.len()).collect();
            if let Some(q) = self.sidebar_filter.as_deref().filter(|q| !q.is_empty()) {
                let keep: Vec<bool> =
                    rows.iter().map(|r| row_matches_filter(&r.name, r.branch.as_deref(), q)).collect();
                let mut it = keep.iter();
                rows.retain(|_| *it.next().unwrap_or(&false));
                let mut it = keep.iter();
                groups.retain(|_| *it.next().unwrap_or(&false));
                let mut it = keep.iter();
                self.row_tabs.retain(|_| *it.next().unwrap_or(&false));
            }
            self.row_groups = groups.clone();
            // Drives the spinner's wake cadence in `about_to_wait` — see `any_busy`.
            self.any_busy = layout.tabs.iter().any(|t| t.busy);
            // What a click on a PR chip needs, keyed by VISIBLE row index (see `open_pr_at`).
            self.row_pr = layout
                .tabs
                .iter()
                .skip(off)
                .take(cap)
                .enumerate()
                .filter_map(|(i, t)| {
                    t.pr.as_ref().map(|p| (i, (p.number, p.url.clone(), t.color.is_some())))
                })
                .collect();

            // Update the taskbar attention badge / progress based on overall attention.
            if let Some(tb) = &self.taskbar {
                let any = layout.panes.iter().any(|p| p.attention);
                tb.set_progress(if any { NProgress::Paused } else { NProgress::None }, None);
            }

            // Damage-gate the (expensive) per-pane GetGrid: fetch a pane only when it has fresh
            // output, on a layout change (geometry differs), for the scrolled/selected active pane,
            // or when it isn't cached yet. Undamaged panes reuse their cached snapshot, so the
            // renderer always gets a full views list.
            let hash = layout_fetch_hash(&layout);
            let force_full = self.force_full || hash != self.last_layout_hash;
            self.force_full = false;
            self.last_layout_hash = hash;
            let damaged = take_damaged(&self.damaged);
            let live: HashSet<u64> = layout.panes.iter().map(|p| p.id).collect();
            evict_stale(&mut self.snap_cache, &live);
            evict_stale(&mut self.pane_scroll, &live);
            evict_stale(&mut self.last_history, &live);
            evict_stale(&mut self.link_cache, &live);

            for pr in &layout.panes {
                // Per-pane scrollback: each pane fetches at its own offset (missing = 0 = live).
                let offset = self.scroll_of(pr.id);
                // Refetch a pane whose viewport is dynamic: scrolled off the live tail, or owning
                // the selection. Generalized from active-only to any pane with a nonzero offset.
                let active_dyn = offset > 0
                    || self.selection.as_ref().is_some_and(|s| s.pane == pr.id);
                let need = needs_fetch(pr.id, force_full, &damaged, active_dyn, self.snap_cache.contains_key(&pr.id));
                let snap = if need {
                    match self.client.call(Call::GetGrid { pane: pr.id, offset }) {
                        Ok(ResultBody::Grid(g)) => {
                            // Accept the server's clamp for every fetched pane (it clamps the offset
                            // to that pane's history). A SCROLLED pane pins to CONTENT, not to the
                            // tail: offsets count lines above the live bottom, so new output would
                            // otherwise slide the viewport toward live (the error being read scrolls
                            // away). When history grew since the last fetch, bump the offset by the
                            // growth and refetch — one frame of drift, corrected on the next paint.
                            let hist = g.history as usize;
                            let last = self.last_history.insert(pr.id, hist).unwrap_or(hist);
                            let grown = hist.saturating_sub(last);
                            if g.offset > 0 && grown > 0 {
                                self.set_scroll(pr.id, (g.offset as usize + grown).min(hist));
                                if let Ok(mut d) = self.damaged.lock() {
                                    d.insert(pr.id); // re-fetch at the pinned offset next frame
                                }
                                self.window.request_redraw();
                            } else {
                                self.set_scroll(pr.id, g.offset as usize);
                            }
                            if pr.active {
                                self.scroll_history = hist;
                                self.active_rows = g.rows as usize;
                                self.active_bracketed = g.bracketed_paste;
                                self.active_mouse_mode = g.mouse_mode;
                            }
                            let mut s = grid_to_snapshot(&g);
                            if g.offset > 0 {
                                // Scrolled into history: park the cursor off-grid so no cell draws it.
                                s.cursor = (g.cols, g.rows);
                            }
                            self.snap_cache.insert(pr.id, s.clone());
                            // Refresh the pane's OSC-8 spans (scheme-filtered) alongside its
                            // snapshot; undamaged frames reuse both from cache together.
                            self.link_cache.insert(pr.id, links_to_spans(&g.links));
                            Some(s)
                        }
                        // Fetch failed this frame: fall back to the cached snapshot if we have one.
                        _ => self.snap_cache.get(&pr.id).cloned(),
                    }
                } else {
                    self.snap_cache.get(&pr.id).cloned()
                };
                let Some(mut snap) = snap else { continue };
                // Underline detected URLs in-place and stash their spans for Ctrl+click hit-testing.
                let detected = detect_urls(&mut snap);
                // Merge cached OSC-8 hyperlink spans (refreshed on fetch, reused for undamaged
                // frames): explicit beats heuristic — a detected span that intersects a hyperlink
                // is dropped so Ctrl+click opens the link's real URI, not the visible text.
                let spans = match self.link_cache.get(&pr.id) {
                    Some(links) if !links.is_empty() => {
                        underline_spans(&mut snap, links);
                        merge_link_spans(detected, links.clone())
                    }
                    _ => detected,
                };
                if !spans.is_empty() {
                    url_spans.insert(pr.id, spans);
                }
                let att = if pr.attention { Attention::Pending } else { Attention::Quiet };
                let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
                // Per-pane offset (post server-clamp): the renderer draws a '+n' badge for any pane
                // with scrolled>0 and a scrollbar only for the active one (history below).
                let scrolled = self.scroll_of(pr.id) as u32;
                // Zoom badge: a zoomed window shows one pane where several live — say so in the
                // title strip, or the layout reads as "my other panes vanished".
                let title = if layout.zoomed && pr.active {
                    format!("{} · zoomed (ctrl+shift+z restores)", pr.title)
                } else {
                    pr.title.clone()
                };
                snaps.push((snap, att, pr.active, rect, scrolled, pr.id, title));
            }
        } else if matches!(self.client, Client::Ready(_)) {
            // A Ready client that can't answer GetLayout has lost the daemon (reconnect+respawn
            // already failed inside `call`): flag it so the loop exits instead of hanging white.
            self.fatal = true;
        }
        // Persist this frame's spans for Ctrl+click hit-testing (and clear them when the layout
        // fetch failed, so a stale span can't open a URL that is no longer under the cursor).
        self.url_spans = url_spans;
        self.sync_title(); // window title follows the active pane (last_panes/active_pane now current)
        // Which pane's title strip shows a close button (besides the active one's).
        let hovered_pane = {
            let (cx, cy) = self.cursor;
            (cx >= 0.0 && cy >= 0.0 && (cx as u32) >= sidebar_w).then(|| self.pane_under_cursor())
        };
        let views: Vec<PaneView> = snaps
            .iter()
            .map(|(s, a, active, rect, scrolled, id, title)| {
                // Highlight the selection only on the pane that owns it, normalized to reading order.
                let selection = self
                    .selection
                    .as_ref()
                    .filter(|sel| sel.pane == *id)
                    .map(|sel| normalize_selection(sel.start, sel.end));
                PaneView {
                    snap: s,
                    attention: *a,
                    active: *active,
                    rect: *rect,
                    scrolled: *scrolled,
                    history: if *active { self.scroll_history as u32 } else { 0 },
                    title: title.clone(),
                    selection,
                    // The active pane always offers its close button; others only while hovered,
                    // so a four-way split isn't four x's competing for attention.
                    show_close: *active || hovered_pane == Some(*id),
                    drop_target: self.pane_drag.as_ref().and_then(|d| d.over) == Some(*id),
                    dragging: self.pane_drag.as_ref().is_some_and(|d| d.dragging && d.from == *id),
                }
            })
            .collect();
        let empty_msg = if matches!(self.client, Client::Connecting(_)) {
            "starting daemon..."
        } else {
            "no panes - Ctrl+Shift+T for a new tab"
        };
        // Hover: mark the sidebar row / '+' row under the cursor (only when the cursor is over the
        // sidebar). The renderer draws the hover fill (and ignores it on the active row).
        let (cx, cy) = self.cursor;
        let mut plus_hover = false;
        let over_sidebar = cx >= 0.0 && (cx as u32) < sidebar_w;
        // Hover is resolved against the PREVIOUS render's item list (the new one isn't folded yet);
        // it only tints a fill, so a one-frame lag on a fast pointer costs nothing.
        let hover_item = if over_sidebar {
            self.renderer.sidebar_item_at(cy as f32, &self.item_heights)
        } else {
            None
        };
        if let Some(ItemMeta::Row(vi)) = hover_item.and_then(|i| self.item_meta.get(i)).cloned() {
            if let Some(row) = rows.get_mut(vi) {
                row.hover = true;
            }
        }
        // Fade the row being dragged so the drop indicator reads as its destination. `from_row` is
        // a real tab index; the rows here are the scrolled window of them.
        if let Some(from) = self.sidebar_drag.as_ref().filter(|d| d.reordering).map(|d| d.from_row) {
            let off = self.sidebar_window().0;
            if let Some(row) = from.checked_sub(off).and_then(|vi| rows.get_mut(vi)) {
                row.dragging = true;
            }
        }
        // Fold the rows under their group headers, then cache what was drawn so the mouse handlers
        // (which run between renders) hit-test the same list.
        let (items, meta) = sidebar_items(rows, &groups, &self.collapsed_groups, hover_item);
        self.item_meta = meta;
        self.item_heights = Renderer::sidebar_item_heights(&items);
        if over_sidebar {
            plus_hover = self.renderer.sidebar_new_tab_at(cy as f32, &self.item_heights);
        }
        // Search bar shows a 1-based "current/total" (renderer renders the numbers as-is). A
        // pending close confirmation reuses the same band as a pure prompt (label only) — search
        // wins if both are somehow active.
        let search_bar = self
            .search
            .as_ref()
            .map(|s| SearchBar {
                label: "find:".into(),
                query: s.query.clone(),
                current: if s.matches.is_empty() { 0 } else { s.current + 1 },
                total: s.matches.len(),
                overlay_only: false,
            })
            .or_else(|| {
                self.confirm_close.as_ref().map(|c| SearchBar {
                    label: match c {
                        ConfirmClose::Pane(_) => "pane is busy — Enter closes it, Esc keeps it".into(),
                        ConfirmClose::Window(_) => {
                            "tab has busy panes — Enter closes it, Esc keeps it".into()
                        }
                    },
                    query: String::new(),
                    current: 0,
                    total: 0,
                    overlay_only: true,
                })
            })
            .or_else(|| {
                self.copy_mode.as_ref().map(|cm| SearchBar {
                    label: if cm.anchor.is_some() {
                        "copy mode — y/Enter copies the selection · Esc exits".into()
                    } else {
                        "copy mode — arrows/hjkl move · v marks · Esc exits".into()
                    },
                    query: String::new(),
                    current: 0,
                    total: 0,
                    overlay_only: true,
                })
            })
            .or_else(|| {
                // Link hover tooltip: the REAL target under the cursor, before any Ctrl+click —
                // OSC-8 links carry hidden URIs, and this is the only place they're visible.
                self.hover_link.as_ref().map(|u| SearchBar {
                    label: format!("link: {u}"),
                    query: String::new(),
                    current: 0,
                    total: 0,
                    overlay_only: true,
                })
            })
            .or_else(|| {
                // Transient notice ("exported to ..."), expiring lazily on the first redraw after
                // 4s — non-modal, lowest band priority.
                match &self.notice {
                    Some((msg, at)) if at.elapsed() < Duration::from_secs(4) => Some(SearchBar {
                        label: msg.clone(),
                        query: String::new(),
                        current: 0,
                        total: 0,
                        overlay_only: true,
                    }),
                    Some(_) => {
                        self.notice = None;
                        None
                    }
                    None => None,
                }
            });
        // Palette overlay: rebuild the filtered rows from the live query and window 10 rows
        // around the selection (the list scrolls with ArrowDown past the bottom).
        let palette_view = self.palette.as_ref().map(|p| {
            let items = palette_items(&self.tab_names, &p.query, &self.palette_recent);
            let sel = p.selected.min(items.len().saturating_sub(1));
            let start = sel.saturating_sub(9);
            let visible: Vec<(String, String)> =
                items.into_iter().skip(start).take(10).map(|(l, h, _)| (l, h)).collect();
            PaletteView {
                query: p.query.clone(),
                selected: sel - start,
                items: visible,
            }
        });
        // Settings overlay: rows for the open tab, windowed around the selection like the palette
        // (the keys tab lists every action, which is taller than any window).
        let settings_view = self.settings.as_ref().map(|s| {
            let (rows, sel, _) = self.settings_window();
            SettingsView {
                tabs: SETTINGS_TABS.iter().map(|s| (*s).to_string()).collect(),
                tab: s.tab,
                rows,
                selected: sel,
                footer: if s.capturing {
                    "press the new chord  ·  esc cancels".into()
                } else if s.tab == 2 {
                    "click or arrow to try one on  ·  enter keeps it  ·  esc restores".into()
                } else if s.tab == 1 {
                    "enter rebinds  ·  tab switches  ·  e opens gmux.json  ·  esc closes".into()
                } else {
                    "enter changes  ·  tab switches  ·  e opens gmux.json  ·  esc closes".into()
                },
            }
        });
        self.renderer.render_frame(
            &view,
            &items,
            sidebar_w,
            &views,
            w,
            h,
            empty_msg,
            plus_hover,
            self.sidebar_drag.as_ref().filter(|d| d.reordering).and_then(|d| d.over),
            self.sidebar_filter.as_deref(),
            search_bar.as_ref(),
            self.preedit.as_deref(),
            palette_view.as_ref(),
            settings_view.as_ref(),
        );
        // Present explicitly: dropping a SurfaceTexture does NOT present it — unpresented frames
        // exhaust the swapchain and every later acquire times out (window stays white/stale).
        self.renderer.queue.present(frame);
    }
}

/// Reconstruct a [`PaneSnapshot`] from a wire grid. `pub` for the CLI's `gmux screenshot`
/// (grid fetch -> offscreen render -> image file).
pub fn grid_to_snapshot(g: &GridWire) -> PaneSnapshot {
    let cols = g.cols as usize;
    let rows = g.rows as usize;
    let blank = Cell {
        ch: ' ',
        fg: Rgb { r: 0xcc, g: 0xcc, b: 0xcc },
        bg: Rgb { r: 0x11, g: 0x11, b: 0x11 },
        bold: false,
        italic: false,
        underline: false,
        inverse: false,
        wide: false,
    };
    let mut cells = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let idx = r * cols + c;
            row.push(match g.cells.get(idx) {
                Some(cw) => Cell {
                    ch: cw.ch,
                    fg: Rgb { r: cw.fg[0], g: cw.fg[1], b: cw.fg[2] },
                    bg: Rgb { r: cw.bg[0], g: cw.bg[1], b: cw.bg[2] },
                    bold: cw.flags & CELL_BOLD != 0,
                    italic: cw.flags & CELL_ITALIC != 0,
                    underline: cw.flags & CELL_UNDERLINE != 0,
                    inverse: cw.flags & CELL_INVERSE != 0,
                    wide: cw.flags & CELL_WIDE != 0,
                },
                None => blank,
            });
        }
        cells.push(row);
    }
    // cursor_style: the raw DECSCUSR Ps value straight through (agent C adds GridWire.cursor_style,
    // agent B adds PaneSnapshot.cursor_style). Until both land, gmux-gui won't build — this line is
    // the only coupling on their fields; nothing to stub, it's correct once they're in.
    PaneSnapshot { cells, cursor: (g.cursor_col, g.cursor_row), cols: g.cols, rows: g.rows, cursor_style: g.cursor_style }
}

/// A hash of the layout's geometry (active pane + each pane's id/rect). A change means a tab
/// switch, split, close, or resize happened, so every visible pane's grid must be refetched.
/// Deliberately excludes tab metadata (name/attention/progress) — those are sidebar-only and don't
/// affect which grids to fetch.
fn layout_fetch_hash(layout: &LayoutWire) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    layout.active_pane.hash(&mut h);
    for p in &layout.panes {
        (p.id, p.x, p.y, p.w, p.h).hash(&mut h);
    }
    h.finish()
}

/// Take and clear the shared damaged-pane set (empty if the lock is poisoned).
fn take_damaged(set: &Mutex<HashSet<u64>>) -> HashSet<u64> {
    set.lock().map(|mut d| std::mem::take(&mut *d)).unwrap_or_default()
}

/// Drop map entries for panes no longer present in the layout (used for both the snapshot cache and
/// the per-pane scroll map).
fn evict_stale<V>(cache: &mut HashMap<u64, V>, live: &HashSet<u64>) {
    cache.retain(|id, _| live.contains(id));
}

/// Whether pane `id` needs a fresh GetGrid this frame: a full refetch was forced (layout change),
/// it produced output (in the damaged set), it's the active pane while scrolled/selected, or it
/// isn't cached yet.
fn needs_fetch(id: u64, force_full: bool, damaged: &HashSet<u64>, active_dyn: bool, in_cache: bool) -> bool {
    force_full || active_dyn || !in_cache || damaged.contains(&id)
}

/// Last-modified time of the config file, or `None` if it doesn't exist / can't be stat'd.
fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

/// Build the `SetPalette` call from the config's resolved palette (defaults when no theme).
fn palette_call(config: &Config) -> Call {
    let p = config.palette();
    Call::SetPalette { fg: p.fg, bg: p.bg, ansi: p.ansi.to_vec() }
}

/// Push the config's theme (fg/bg, with the built-in defaults as fallback) into the renderer.
fn apply_theme(renderer: &mut Renderer, config: &Config) {
    let [fr, fg, fb] = config.fg(DEFAULT_FG);
    let [br, bg, bb] = config.bg(DEFAULT_BG);
    renderer.set_theme(Rgb { r: fr, g: fg, b: fb }, Rgb { r: br, g: bg, b: bb });
    // `theme.accent`: unset = the built-in cmux blue, "system" = the Windows accent, hex = pinned.
    crate::renderer::set_accent(config.accent());
}

/// Where the first-run marker lives: `%LOCALAPPDATA%\gmux\state`.
fn state_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base).join("gmux").join("state")
}

/// True exactly once per install: reports whether `dir/first-run` is absent and drops the marker.
/// Io errors are ignored — a failed marker just means the welcome toast may repeat next launch.
fn first_run(dir: &std::path::Path) -> bool {
    let marker = dir.join("first-run");
    if marker.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&marker, "");
    true
}

fn window_hwnd(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Terminal button code for a physical mouse button (left 0, middle 1, right 2). Back/forward and
/// other buttons aren't reported.
fn mouse_button_code(b: MouseButton) -> Option<u8> {
    Some(match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        _ => return None,
    })
}

/// Encode a mouse event as a terminal report. `button` is the full button code the caller already
/// computed: 0/1/2 for left/middle/right, `base + 32` for a held-button drag, 35 for buttonless
/// motion, 64/65 for wheel up/down. `col`/`row` are 1-based cells. SGR (1006) uses the textual
/// `ESC[<b;col;row` form with a trailing `M` (press) / `m` (release), keeping the button number on
/// release. The legacy X10 fallback packs `ESC[M` + three bytes `32+b`, `32+col`, `32+row`, with a
/// release always reported as button 3 and each coord clamped to 223 so `32+coord` fits a byte.
/// Pure, so unit-tested.
fn encode_mouse(sgr: bool, button: u8, pressed: bool, col: u16, row: u16) -> Vec<u8> {
    if sgr {
        let f = if pressed { 'M' } else { 'm' };
        format!("\x1b[<{button};{col};{row}{f}").into_bytes()
    } else {
        // X10 has no per-button release: a release reports button 3 (all buttons up).
        let b = if pressed { button } else { 3 };
        // Clamp to 94 (not the spec's 223): the report travels as a JSON String over the pipe,
        // and coordinate bytes above 127 are not standalone UTF-8 — they'd be mangled to U+FFFD
        // in transit. 32+94 = 126 keeps every byte ASCII; panes wider than 94 cells need SGR
        // (DECSET 1006), which every modern mouse app requests anyway.
        let coord = |v: u16| 32u8.saturating_add(v.min(94) as u8);
        vec![0x1b, b'[', b'M', 32u8.saturating_add(b), coord(col), coord(row)]
    }
}

/// Translate a key press into bytes for the PTY (full win32-input-mode fidelity comes later).
fn key_to_bytes(event: &KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
    use NamedKey::*;
    match &event.logical_key {
        Key::Named(named) => Some(match named {
            Enter => vec![b'\r'],
            Backspace => vec![0x7f],
            Tab => vec![b'\t'],
            Escape => vec![0x1b],
            Space => vec![b' '],
            ArrowUp => b"\x1b[A".to_vec(),
            ArrowDown => b"\x1b[B".to_vec(),
            ArrowRight => b"\x1b[C".to_vec(),
            ArrowLeft => b"\x1b[D".to_vec(),
            Home => b"\x1b[H".to_vec(),
            End => b"\x1b[F".to_vec(),
            Delete => b"\x1b[3~".to_vec(),
            _ => return None,
        }),
        Key::Character(s) => {
            if mods.control_key() && !mods.shift_key() {
                let c = s.chars().next()?.to_ascii_lowercase();
                if c.is_ascii_lowercase() {
                    return Some(vec![(c as u8 - b'a') + 1]);
                }
            }
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: u64, x: u32, y: u32, w: u32, h: u32) -> PaneRectWire {
        PaneRectWire { id, x, y, w, h, active: false, attention: false, title: String::new() }
    }

    /// Divider hit-test: a vertical split boundary grabs the left pane (drag along x); a horizontal
    /// one grabs the top pane (drag along y); a click away from any boundary misses.
    #[test]
    fn divider_at_grabs_the_right_boundary_and_pane() {
        // 1 | 2 side by side (edge-to-edge at x=50).
        let side = [rect(1, 0, 0, 50, 40), rect(2, 50, 0, 50, 40)];
        let d = divider_at(&side, 51.0, 20.0, GAP + 2.0).expect("on the vertical divider");
        assert_eq!(d.pane, 1, "left pane is the target");
        assert!(d.vertical);
        assert_eq!(d.span, 100.0, "span is both pane widths");
        // Away from the boundary and inside a pane: no divider (caller falls through to focus).
        assert!(divider_at(&side, 10.0, 20.0, GAP + 2.0).is_none());
        // Just outside the tolerance band still misses.
        assert!(divider_at(&side, 58.0, 20.0, GAP + 2.0).is_none());

        // 1 over 2 stacked (edge-to-edge at y=20).
        let stack = [rect(1, 0, 0, 80, 20), rect(2, 0, 20, 80, 20)];
        let d = divider_at(&stack, 40.0, 21.0, GAP + 2.0).expect("on the horizontal divider");
        assert_eq!(d.pane, 1, "top pane is the target");
        assert!(!d.vertical);
        assert_eq!(d.span, 40.0, "span is both pane heights");
    }

    /// A vertical divider only registers within the panes' shared y-range, not past a pane's end
    /// (T-junction): dragging outside the overlap misses.
    #[test]
    fn divider_at_requires_axis_overlap() {
        // 1 fills the left; 2 and 3 stack on the right. The 1|2 divider only spans y 0..20.
        let panes = [rect(1, 0, 0, 50, 40), rect(2, 50, 0, 50, 20), rect(3, 50, 20, 50, 20)];
        // Within 1|2's overlap: grabs 1, span = 50 + 50.
        let d = divider_at(&panes, 50.0, 10.0, GAP + 2.0).unwrap();
        assert_eq!((d.pane, d.vertical, d.span), (1, true, 100.0));
        // Lower down the same edge falls in 1|3's overlap: still grabs 1.
        let d = divider_at(&panes, 50.0, 30.0, GAP + 2.0).unwrap();
        assert_eq!(d.pane, 1);
    }

    fn cw(ch: char) -> gmux_proto::CellWire {
        gmux_proto::CellWire { ch, fg: [0; 3], bg: [0; 3], flags: 0 }
    }

    fn grid(cols: u16, rows: u16, chars: &[char]) -> GridWire {
        GridWire {
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            cells: chars.iter().map(|c| cw(*c)).collect(),
            history: 0,
            offset: 0,
            bracketed_paste: false,
            mouse_mode: 0,
            links: Vec::new(),
            cursor_style: 0,
        }
    }

    /// Pixel→cell mirrors the renderer chrome: cells start at margin+border+inset (x) and
    /// margin+border+title-strip+inset (y), and out-of-area pixels clamp to the grid.
    #[test]
    fn pixel_to_cell_maps_and_clamps() {
        // One pane filling the content area right of a 200px sidebar in a 1000x600 window.
        let rect = Rect { x: 200, y: 0, w: 800, h: 600 };
        let (sw, surf_w, surf_h, cwid, chgt) = (200, 1000, 600, 10, 20);
        // Cell-area origin: ix = 200+8+1+8 = 217, iy = 0+8+1+22+8 = 39.
        assert_eq!(pixel_to_cell(217.0, 39.0, rect, sw, surf_w, surf_h, cwid, chgt), (0, 0));
        // 3 cells right, 2 down from the origin (plus a few px into the cell).
        assert_eq!(pixel_to_cell(252.0, 84.0, rect, sw, surf_w, surf_h, cwid, chgt), (3, 2));
        // Above/left of the cell area clamps to (0,0).
        assert_eq!(pixel_to_cell(0.0, 0.0, rect, sw, surf_w, surf_h, cwid, chgt), (0, 0));
        // Far bottom-right clamps to the last visible cell (cols=76 -> 75, rows=27 -> 26).
        assert_eq!(pixel_to_cell(1e6, 1e6, rect, sw, surf_w, surf_h, cwid, chgt), (75, 26));
    }

    /// Double-click word span: expands over word chars (incl. `-`, `/`, `.` etc.), stops at spaces,
    /// and a non-word or out-of-range cell spans only itself.
    #[test]
    fn word_span_expands_over_word_chars() {
        // 0:f 1:o 2:o 3:· 4:b 5:a 6:r 7:- 8:b 9:a 10:z 11:· 12:q 13:u 14:x
        let line: Vec<char> = "foo bar-baz qux".chars().collect();
        assert_eq!(word_span(&line, 6), (4, 10), "'-' is a word char: the whole bar-baz spans");
        assert_eq!(word_span(&line, 0), (0, 2), "word at the start");
        assert_eq!(word_span(&line, 14), (12, 14), "word at the end");
        assert_eq!(word_span(&line, 3), (3, 3), "a space spans only itself");
    }

    /// Word span with wide glyphs, their ' ' spacer, and all-space rows: the spacer ends the word,
    /// a space cell is just itself, and an out-of-range column clamps to a single cell.
    #[test]
    fn word_span_wide_spacer_and_all_spaces() {
        // Wide '中' at col 0 with a ' ' spacer at col 1, then '文' + spacer.
        let wide: Vec<char> = vec!['中', ' ', '文', ' '];
        assert_eq!(word_span(&wide, 0), (0, 0), "the wide lead is a word; its spacer ends it");
        assert_eq!(word_span(&wide, 1), (1, 1), "the spacer (a space) spans only itself");
        let spaces: Vec<char> = vec![' ', ' ', ' '];
        assert_eq!(word_span(&spaces, 1), (1, 1), "an all-space row: no expansion");
        assert_eq!(word_span(&spaces, 9), (9, 9), "out-of-range column clamps to itself");
    }

    /// Selection endpoints normalize into reading order (row-major), regardless of drag direction.
    #[test]
    fn normalize_selection_orders_reading_order() {
        // Backwards across rows: swapped so start precedes end.
        assert_eq!(normalize_selection((5, 2), (1, 0)), ((1, 0), (5, 2)));
        // Already ordered on one row: untouched.
        assert_eq!(normalize_selection((0, 0), (3, 0)), ((0, 0), (3, 0)));
        // Backwards on the same row: swapped by column.
        assert_eq!(normalize_selection((4, 1), (2, 1)), ((2, 1), (4, 1)));
    }

    /// Text assembly: first/last rows are partial, middle rows full width, trailing spaces trimmed,
    /// rows joined with CRLF, and out-of-range endpoints clamp instead of panicking.
    #[test]
    fn grid_selection_text_assembles_rows() {
        // 4x3: "ab  " / "cdef" / "gh  ".
        let g = grid(4, 3, &['a', 'b', ' ', ' ', 'c', 'd', 'e', 'f', 'g', 'h', ' ', ' ']);
        // Single row, trailing spaces trimmed.
        assert_eq!(grid_selection_text(&g, (0, 0), (3, 0)), "ab");
        // Multi-row: first row from col 1, middle full, last row to col 1.
        assert_eq!(grid_selection_text(&g, (1, 0), (1, 2)), "b\r\ncdef\r\ngh");
        // A stale selection past the grid clamps (no panic) to the last row/col.
        assert_eq!(grid_selection_text(&g, (0, 0), (10, 10)), "ab\r\ncdef\r\ngh");
    }

    #[test]
    fn first_run_reports_once_then_sees_the_marker() {
        let dir = std::env::temp_dir().join(format!("gmux-first-run-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(first_run(&dir), "fresh dir should be a first run");
        assert!(dir.join("first-run").exists(), "marker file should be created");
        assert!(!first_run(&dir), "second call should see the marker");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Damage-set take: the shared set is drained into the caller and cleared, so a second take is
    /// empty — the render's "fetch these, then forget them" contract.
    #[test]
    fn take_damaged_drains_and_clears() {
        let set = Mutex::new(HashSet::new());
        set.lock().unwrap().extend([1u64, 2, 3]);
        assert_eq!(take_damaged(&set), HashSet::from([1, 2, 3]));
        assert!(take_damaged(&set).is_empty(), "second take is empty — the set was cleared");
        assert!(set.lock().unwrap().is_empty());
    }

    /// Cache eviction keeps only panes still in the layout (a closed pane's snapshot is dropped).
    #[test]
    fn evict_stale_keeps_only_live_panes() {
        let mut cache: HashMap<u64, PaneSnapshot> = HashMap::new();
        for id in [1u64, 2, 3] {
            cache.insert(id, grid_to_snapshot(&grid(1, 1, &[' '])));
        }
        evict_stale(&mut cache, &HashSet::from([1u64, 3]));
        let mut kept: Vec<u64> = cache.keys().copied().collect();
        kept.sort();
        assert_eq!(kept, vec![1, 3]);
    }

    /// Fetch gate: forced full, damage, an active scrolled/selected pane, or a cache miss each
    /// force a GetGrid; an idle cached undamaged pane is skipped (reuses its snapshot).
    #[test]
    fn needs_fetch_gates_on_damage_force_and_cache() {
        let dmg = HashSet::from([7u64]);
        assert!(!needs_fetch(1, false, &dmg, false, true), "cached + undamaged + idle: skip");
        assert!(needs_fetch(7, false, &dmg, false, true), "damaged: fetch");
        assert!(needs_fetch(1, false, &dmg, false, false), "cache miss: fetch");
        assert!(needs_fetch(1, true, &dmg, false, true), "forced full: fetch");
        assert!(needs_fetch(1, false, &dmg, true, true), "active scrolled/selected: fetch");
    }

    /// The fetch hash tracks geometry (active pane + pane rects) and ignores tab metadata, so a
    /// mere attention/progress change doesn't trigger a full refetch but a resize does.
    #[test]
    fn layout_fetch_hash_tracks_geometry_only() {
        let panes = || vec![rect(1, 0, 0, 80, 40), rect(2, 80, 0, 80, 40)];
        let base = LayoutWire { zoomed: false, active_pane: 1, tabs: vec![], panes: panes() };
        let tab = gmux_proto::TabWire {
            group: None,
            color: None,
            busy: false,
            pr: None,
            index: 0, id: 7, name: "changed".into(), branch: None, attention: true, unread: 0, active: true,
            progress: Some(50), progress_error: false,
        };
        let same = LayoutWire { zoomed: false, active_pane: 1, tabs: vec![tab], panes: panes() };
        assert_eq!(layout_fetch_hash(&base), layout_fetch_hash(&same), "tab metadata is ignored");
        let resized = LayoutWire { zoomed: false, active_pane: 1, tabs: vec![], panes: vec![rect(1, 0, 0, 100, 40), rect(2, 100, 0, 60, 40)] };
        assert_ne!(layout_fetch_hash(&base), layout_fetch_hash(&resized), "a resize changes the hash");
        let refocused = LayoutWire { zoomed: false, active_pane: 2, tabs: vec![], panes: panes() };
        assert_ne!(layout_fetch_hash(&base), layout_fetch_hash(&refocused), "a focus change changes the hash");
    }

    /// grid_to_snapshot maps the wire's CELL_WIDE flag (bit 4) onto Cell.wide; the spacer isn't wide.
    #[test]
    fn grid_to_snapshot_maps_wide_flag() {
        let g = GridWire {
            cols: 2,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![
                gmux_proto::CellWire { ch: '中', fg: [0; 3], bg: [0; 3], flags: CELL_WIDE },
                gmux_proto::CellWire { ch: ' ', fg: [0; 3], bg: [0; 3], flags: 0 },
            ],
            history: 0,
            offset: 0,
            bracketed_paste: false,
            mouse_mode: 0,
            links: Vec::new(),
            cursor_style: 5, // bar; must map straight through to the snapshot
        };
        let snap = grid_to_snapshot(&g);
        assert!(snap.cells[0][0].wide, "wide char maps to Cell.wide");
        assert!(!snap.cells[0][1].wide, "the spacer cell is not wide");
        assert_eq!(snap.cursor_style, 5, "cursor_style maps straight through");
    }

    /// SGR (1006) mouse reports: press uses `M`, release `m` (keeping the button number), wheel and
    /// drag carry their pre-computed button codes, all with 1-based coords.
    #[test]
    fn encode_mouse_sgr_forms() {
        assert_eq!(encode_mouse(true, 0, true, 1, 1), b"\x1b[<0;1;1M");
        assert_eq!(encode_mouse(true, 0, false, 1, 1), b"\x1b[<0;1;1m"); // release keeps the button
        assert_eq!(encode_mouse(true, 2, true, 10, 5), b"\x1b[<2;10;5M"); // right press
        assert_eq!(encode_mouse(true, 64, true, 3, 4), b"\x1b[<64;3;4M"); // wheel up
        assert_eq!(encode_mouse(true, 65, true, 3, 4), b"\x1b[<65;3;4M"); // wheel down
        assert_eq!(encode_mouse(true, 32, true, 7, 8), b"\x1b[<32;7;8M"); // left drag (0 + 32)
        assert_eq!(encode_mouse(true, 35, true, 2, 2), b"\x1b[<35;2;2M"); // buttonless any-motion
    }

    /// X10 fallback: `ESC[M` + three offset bytes; a release is always button 3; coords clamp to
    /// 94 so every byte stays ASCII (the report rides a JSON String — bytes > 127 would be
    /// mangled to U+FFFD in transit; wider panes need SGR).
    #[test]
    fn encode_mouse_x10_fallback_and_clamp() {
        // Left press at (1,1): 32+0, 32+1, 32+1.
        assert_eq!(encode_mouse(false, 0, true, 1, 1), vec![0x1b, b'[', b'M', 32, 33, 33]);
        // Release reports button 3 regardless of which button (32+3 = 35).
        assert_eq!(encode_mouse(false, 2, false, 1, 1), vec![0x1b, b'[', b'M', 35, 33, 33]);
        // Wheel up code 64 -> 32+64 = 96.
        assert_eq!(encode_mouse(false, 64, true, 1, 1), vec![0x1b, b'[', b'M', 96, 33, 33]);
        // Coords past 94 clamp so the byte tops out at ASCII 126 (32 + 94).
        assert_eq!(encode_mouse(false, 0, true, 300, 300), vec![0x1b, b'[', b'M', 32, 126, 126]);
    }

    /// Only left/middle/right map to report codes; other buttons don't report.
    #[test]
    fn mouse_button_code_maps_the_three_buttons() {
        assert_eq!(mouse_button_code(MouseButton::Left), Some(0));
        assert_eq!(mouse_button_code(MouseButton::Middle), Some(1));
        assert_eq!(mouse_button_code(MouseButton::Right), Some(2));
        assert_eq!(mouse_button_code(MouseButton::Back), None);
    }

    /// Toast launch args: "pane=N" parses, anything else (the welcome toast) is None.
    #[test]
    fn activation_pane_parses() {
        assert_eq!(parse_activation_pane("pane=5"), Some(5));
        assert_eq!(parse_activation_pane("pane=x"), None);
        assert_eq!(parse_activation_pane("welcome"), None);
    }

    /// Rows for grouping tests: named, otherwise inert.
    fn grow(name: &str, unread: u32) -> SidebarRow {
        SidebarRow {
            name: name.into(),
            branch: None,
            attention: false,
            unread,
            color: None,
            busy: false,
            dragging: false,
            pr: None,
            active: false,
            hover: false,
            progress: None,
            progress_error: false,
        }
    }

    fn item_names(items: &[SidebarItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                SidebarItem::Header(h) => format!("#{}", h.name),
                SidebarItem::Row(r) => r.name.clone(),
            })
            .collect()
    }

    #[test]
    fn captured_chords_round_trip_through_the_config_parser() {
        use winit::keyboard::{Key, NamedKey};
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(
            chord_string(ctrl_shift, &Key::Character("D".into())).as_deref(),
            Some("ctrl+shift+d"),
            "captured chords are lowercased, like the config writes them"
        );
        assert_eq!(
            chord_string(ModifiersState::ALT, &Key::Named(NamedKey::ArrowLeft)).as_deref(),
            Some("alt+left")
        );
        // A bare key is refused: binding it would swallow that key before the pane ever saw it.
        assert_eq!(chord_string(ModifiersState::empty(), &Key::Character("q".into())), None);
        // Keys the config parser has no token for are refused rather than written unparseably.
        assert_eq!(chord_string(ctrl_shift, &Key::Named(NamedKey::F7)), None);

        // The real contract: anything captured must parse back to the same binding.
        for (mods, key) in [
            (ctrl_shift, Key::Character("k".into())),
            (ModifiersState::ALT | ModifiersState::SHIFT, Key::Named(NamedKey::ArrowUp)),
            (ModifiersState::CONTROL, Key::Named(NamedKey::PageDown)),
        ] {
            let chord = chord_string(mods, &key).expect("captured");
            let km = crate::config::Keymap::build(&crate::config::Config {
                keys: Some([("split_h".to_string(), chord.clone())].into_iter().collect()),
                ..Default::default()
            });
            assert_eq!(
                km.action(mods, &key),
                Some(crate::config::Action::SplitH),
                "{chord} must bind the action it was captured for"
            );
        }
    }

    #[test]
    fn sidebar_filter_matches_name_or_branch() {
        // Name match, fuzzy like the palette (subsequence, case-insensitive).
        assert!(row_matches_filter("billing-service", None, "bill"));
        assert!(row_matches_filter("billing-service", None, "BLS"));
        assert!(!row_matches_filter("billing-service", None, "zzz"));
        // Branch match matters when several workspaces share a project name.
        assert!(row_matches_filter("api", Some("feat/checkout"), "checkout"));
        assert!(!row_matches_filter("api", Some("main"), "checkout"));
        // An empty query keeps everything (the filter is open but nothing typed yet).
        assert!(row_matches_filter("anything", None, ""));
    }

    #[test]
    fn drop_target_carries_the_group_it_landed_in() {
        // Sidebar: [a] [#api] [b] — a is ungrouped, b is in "api".
        let meta = vec![ItemMeta::Row(0), ItemMeta::Header("api".into()), ItemMeta::Row(1)];
        let groups = vec![None, Some("api".to_string())];

        // Dropping on a grouped row files the dragged window into that group...
        assert_eq!(drop_decision(&meta, &groups, 2, 2), (1, Some("api".into())));
        // ...dropping on the header does too, landing on its first member.
        assert_eq!(drop_decision(&meta, &groups, 2, 1), (1, Some("api".into())));
        // ...and dropping on an ungrouped row takes it back out of the group.
        assert_eq!(drop_decision(&meta, &groups, 2, 0), (0, None));
        // Past the end appends, ungrouped.
        assert_eq!(drop_decision(&meta, &groups, 2, 3), (1, None));
    }

    #[test]
    fn drop_on_an_empty_group_header_does_not_borrow_a_foreign_row() {
        // [#api (empty)] [#web] [row in web]: the header's "first member" lookup must stop at the
        // next header instead of walking into web's row.
        let meta = vec![
            ItemMeta::Header("api".into()),
            ItemMeta::Header("web".into()),
            ItemMeta::Row(0),
        ];
        let groups = vec![Some("web".to_string())];
        let (to, group) = drop_decision(&meta, &groups, 1, 0);
        assert_eq!(group, Some("api".into()), "the drop still files it under the empty group");
        assert_eq!(to, 0, "and falls back to the last row rather than web's member by accident");
    }

    #[test]
    fn grouping_puts_ungrouped_first_then_headers_in_first_seen_order() {
        let rows = vec![grow("a", 0), grow("b", 0), grow("c", 0), grow("d", 0)];
        // b + d are in "api", c is in "web", a is ungrouped. "api" is seen first.
        let groups = vec![None, Some("api".into()), Some("web".into()), Some("api".into())];
        let (items, meta) = sidebar_items(rows, &groups, &HashSet::new(), None);
        assert_eq!(item_names(&items), ["a", "#api", "b", "d", "#web", "c"]);
        // Meta rows carry the ORIGINAL visible-row index, so a click still selects the right tab.
        assert_eq!(
            meta,
            vec![
                ItemMeta::Row(0),
                ItemMeta::Header("api".into()),
                ItemMeta::Row(1),
                ItemMeta::Row(3),
                ItemMeta::Header("web".into()),
                ItemMeta::Row(2),
            ]
        );
    }

    #[test]
    fn collapsed_group_hides_members_and_summarizes_them() {
        let rows = vec![grow("a", 2), grow("b", 5)];
        let groups = vec![Some("api".into()), Some("api".into())];
        let collapsed: HashSet<String> = ["api".to_string()].into_iter().collect();
        let (items, meta) = sidebar_items(rows, &groups, &collapsed, None);
        assert_eq!(item_names(&items), ["#api"]);
        assert_eq!(meta, vec![ItemMeta::Header("api".into())]);
        match &items[0] {
            SidebarItem::Header(h) => {
                assert_eq!(h.members, 2, "collapsed header reports how many are hidden");
                assert_eq!(h.unread, 7, "and sums their unread so it can still shout");
                assert!(h.collapsed);
            }
            _ => panic!("expected a header"),
        }
    }

    #[test]
    fn ungrouped_rows_alone_produce_no_headers() {
        // The common case (nobody has grouped anything) must render exactly as it did before.
        let rows = vec![grow("a", 0), grow("b", 0)];
        let (items, meta) = sidebar_items(rows, &[None, None], &HashSet::new(), None);
        assert_eq!(item_names(&items), ["a", "b"]);
        assert_eq!(meta, vec![ItemMeta::Row(0), ItemMeta::Row(1)]);
    }

    #[test]
    fn palette_fuzzy_filter_and_items() {
        assert!(fuzzy_match("split horizontal", "spl h"));
        assert!(fuzzy_match("Split Horizontal", "SPLIT"));
        assert!(!fuzzy_match("split", "splx"));
        assert!(fuzzy_match("anything", ""));
        // The config's action names (underscored) must find their palette labels — a user who has
        // edited gmux.json types `split_h`, and that used to match nothing at all.
        assert!(fuzzy_match("split h", "split_h"));
        assert!(fuzzy_match("new window", "new_window"));
        assert!(fuzzy_match("export scrollback", "export-scrollback"));
        // Separators in the label are ignored too, so "tab: my_project" is reachable either way.
        assert!(fuzzy_match("tab: my_project", "myproject"));
        // A genuine mismatch still fails — dropping separators must not make everything match.
        assert!(!fuzzy_match("new window", "new_windowz"));

        let tabs = vec!["backend".to_string(), "web".to_string()];
        let all = palette_items(&tabs, "", &[]);
        assert!(all.iter().any(|(l, h, _)| l == "tab: backend" && h == "alt+1"));
        assert!(all.iter().any(|(l, _, c)| l == "split h" && matches!(c, PaletteCmd::Act(Action::SplitH))));
        assert!(!all.iter().any(|(l, _, _)| l == "command palette"), "palette excludes itself");

        // "back" also subsequence-matches "export scrollBACK" — tabs list first regardless.
        let filtered = palette_items(&tabs, "backe", &[]);
        assert_eq!(filtered.len(), 1, "only the backend tab matches 'backe': {:?}", filtered.iter().map(|(l, _, _)| l).collect::<Vec<_>>());
        assert_eq!(filtered[0].0, "tab: backend");
    }

    /// OSC-8 merge: only http/https/mailto survive the scheme filter; a detected span that
    /// intersects an explicit hyperlink is dropped (explicit beats heuristic); underlining a
    /// stale/oversized wire span clamps instead of panicking.
    #[test]
    fn osc8_spans_filter_merge_and_clamp() {
        let links = vec![
            LinkWire { row: 0, start: 2, end: 5, uri: "https://a.test".into() },
            LinkWire { row: 1, start: 0, end: 3, uri: "file:///etc/passwd".into() },
            LinkWire { row: 1, start: 5, end: 6, uri: "MAILTO:x@y.z".into() },
        ];
        let spans = links_to_spans(&links);
        let uris: Vec<&str> = spans.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(uris, vec!["https://a.test", "MAILTO:x@y.z"], "file:// dropped, case-insensitive schemes kept");

        // Overlap: detected (0, 4..=8) intersects the OSC-8 (0, 2..=5) and is dropped whole;
        // detected (0, 7..=9) does not intersect after the first is gone... it overlaps nothing.
        let detected = vec![
            UrlSpan { row: 0, start: 4, end: 8, url: "https://visible.text".into() },
            UrlSpan { row: 2, start: 0, end: 3, url: "https://keep.me".into() },
        ];
        let merged = merge_link_spans(detected, spans);
        let urls: Vec<&str> = merged.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec!["https://keep.me", "https://a.test", "MAILTO:x@y.z"]);

        // Bounds clamp: a span past the row/col limits underlines what exists, no panic.
        let mut snap = PaneSnapshot {
            cells: vec![vec![
                Cell { ch: 'x', fg: Rgb { r: 0, g: 0, b: 0 }, bg: Rgb { r: 0, g: 0, b: 0 }, bold: false, italic: false, underline: false, inverse: false, wide: false };
                3
            ]],
            cursor: (0, 0),
            cursor_style: 0,
            cols: 3,
            rows: 1,
        };
        underline_spans(
            &mut snap,
            &[
                UrlSpan { row: 0, start: 1, end: 99, url: "u".into() },
                UrlSpan { row: 9, start: 0, end: 1, url: "u".into() },
            ],
        );
        assert!(!snap.cells[0][0].underline);
        assert!(snap.cells[0][1].underline && snap.cells[0][2].underline);
    }

    /// URL spans: scheme + non-space run, trailing sentence punctuation trimmed, and a bare
    /// scheme (a URL wrapped right at "https://") skipped entirely.
    #[test]
    fn find_urls_detects_trims_and_skips_bare_scheme() {
        let line: Vec<char> =
            "see https://example.com/a. or (http://x.io/p) https:// end".chars().collect();
        let texts: Vec<String> = find_urls(&line)
            .iter()
            .map(|&(s, e)| line[s..e].iter().collect())
            .collect();
        assert_eq!(texts, vec!["https://example.com/a", "http://x.io/p"]);
    }

    /// `url_at` hit-tests the inclusive column span on the right row; `step_index` wraps both
    /// directions and stays 0 for an empty list.
    #[test]
    fn url_at_is_inclusive_and_step_index_wraps() {
        let spans = vec![UrlSpan { row: 2, start: 5, end: 9, url: "u".into() }];
        assert_eq!(url_at(&spans, 5, 2), Some("u"));
        assert_eq!(url_at(&spans, 9, 2), Some("u"));
        assert_eq!(url_at(&spans, 10, 2), None);
        assert_eq!(url_at(&spans, 5, 1), None);
        assert_eq!(step_index(0, 3, -1), 2);
        assert_eq!(step_index(2, 3, 1), 0);
        assert_eq!(step_index(0, 0, 1), 0);
    }

    /// The scrollbar y->offset map: the track top is the deepest history, the bottom the live
    /// screen, and the cursor clamps to the track. A zero-height track or empty history yields 0.
    #[test]
    fn scrollbar_offset_maps_top_to_history_bottom_to_zero() {
        // track_top = 0, track_h = 100, history = 50.
        assert_eq!(scrollbar_offset_at(0.0, 0.0, 100.0, 50), 50); // top -> deepest history
        assert_eq!(scrollbar_offset_at(100.0, 0.0, 100.0, 50), 0); // bottom -> live screen
        assert_eq!(scrollbar_offset_at(50.0, 0.0, 100.0, 50), 25); // mid -> half
        // Cursor past either end clamps to the track's ends.
        assert_eq!(scrollbar_offset_at(-20.0, 0.0, 100.0, 50), 50);
        assert_eq!(scrollbar_offset_at(200.0, 0.0, 100.0, 50), 0);
        // Degenerate track / empty history -> 0 (no divide-by-zero).
        assert_eq!(scrollbar_offset_at(10.0, 0.0, 0.0, 50), 0);
        assert_eq!(scrollbar_offset_at(10.0, 0.0, 100.0, 0), 0);
    }

    /// Font-size clamp keeps requests inside the atlas's rasterizable range (8..=40).
    #[test]
    fn clamp_font_px_bounds_the_atlas_range() {
        assert_eq!(clamp_font_px(4.0), 8.0);
        assert_eq!(clamp_font_px(100.0), 40.0);
        assert_eq!(clamp_font_px(18.0), 18.0);
    }

    /// Path quoting: a space triggers double-quoting with any embedded quote doubled; a plain path
    /// is untouched.
    #[test]
    fn quote_path_quotes_only_when_spaced() {
        assert_eq!(quote_path(r"C:\tmp\file.txt"), r"C:\tmp\file.txt");
        assert_eq!(quote_path(r"C:\my docs\a.txt"), "\"C:\\my docs\\a.txt\"");
        // A spaced path with an embedded quote doubles the quote inside the wrapper.
        assert_eq!(quote_path(r#"a "b" c"#), r#""a ""b"" c""#);
    }

    /// Per-pane scroll eviction: the generic `evict_stale` drops offsets for panes gone from the
    /// layout and keeps the live ones (same helper the snapshot cache uses).
    #[test]
    fn evict_stale_prunes_pane_scroll() {
        let mut scroll: HashMap<u64, usize> = HashMap::from([(1, 5), (2, 3), (3, 9)]);
        evict_stale(&mut scroll, &HashSet::from([1u64, 3]));
        let mut kept: Vec<(u64, usize)> = scroll.into_iter().collect();
        kept.sort();
        assert_eq!(kept, vec![(1, 5), (3, 9)]);
    }

    /// Sidebar double-click (rename trigger): the same row within CLICK_INTERVAL counts; a
    /// different row, no prior click, or too-slow second click don't. `t0 + Duration` avoids the
    /// Instant-underflow of subtracting from `now`.
    #[test]
    fn sidebar_double_click_same_row_within_interval() {
        let t0 = Instant::now();
        let quick = t0 + Duration::from_millis(100);
        let slow = t0 + CLICK_INTERVAL * 2;
        assert!(sidebar_double_click(Some((2, t0)), 2, quick), "same row, quick: double");
        assert!(!sidebar_double_click(Some((2, t0)), 3, quick), "different row: single");
        assert!(!sidebar_double_click(None, 2, quick), "no prior click: single");
        assert!(!sidebar_double_click(Some((2, t0)), 2, slow), "too slow: single");
    }
}
