//! gmux-proto — the wire protocol for the `\\.\pipe\gmux.<user>` automation API.
//!
//! Framing: **newline-delimited JSON** — one `{"id":N,"method":...,"params":...}` request per
//! line, one `{"id":N,...}` response per line (amended from LSP framing in D-005: strictly simpler
//! for PowerShell/Python/Node scripting clients; cmux precedent). Lines are capped at
//! [`MAX_LINE`] bytes; anything larger is rejected rather than buffered unbounded.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

/// Protocol version — bump on breaking changes; `hello` negotiates.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum accepted line length (1 MiB) — bounds memory against hostile/buggy clients.
pub const MAX_LINE: usize = 1024 * 1024;

/// A request envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

/// The method + params of a request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", rename_all = "kebab-case")]
pub enum Call {
    /// Version/capability handshake.
    Hello { client_version: String },
    /// List all panes across the session.
    ListPanes,
    /// Write text (and optionally a trailing Enter) to a pane.
    SendKeys { pane: u64, text: String, #[serde(default)] enter: bool },
    /// Read a pane's screen text. `scrollback` (the `-S` option) requests history above the
    /// viewport: `Some(0)` = all retained scrollback + screen, `Some(n)` = the most-recent `n`
    /// lines, `None` = the visible screen only.
    CapturePane { pane: u64, #[serde(default)] scrollback: Option<usize> },
    /// Search a pane's scrollback + visible screen for `query` (case-insensitive substring).
    /// Replies [`ResultBody::Matches`]: the scroll offsets (lines above the live bottom, usable
    /// directly as [`Call::GetGrid`]'s `offset`) of matching lines, nearest-to-bottom first, capped
    /// at 500. An empty `query` yields no matches; an unknown pane errors.
    SearchPane { pane: u64, #[serde(default)] query: String },
    /// Scroll offsets of a pane's recorded prompt starts (OSC 133;A / ConEmu 9;12), for
    /// prompt-jump navigation. Replies [`ResultBody::Matches`] with the same offset semantics as
    /// `search-pane` (nearest-to-bottom first). An unknown pane errors.
    PromptOffsets { pane: u64 },
    /// Whether a pane's shell has running children (close-confirmation guard). Replies
    /// [`ResultBody::Busy`]; an unknown pane errors. Remote-mirror panes report `false`.
    PaneBusy { pane: u64 },
    /// Whether ANY pane in the window with stable id `id` is busy (middle-click close guard).
    /// Replies [`ResultBody::Busy`]; a gone id reports `false` (nothing left to protect).
    WindowBusy {
        #[serde(default)]
        id: u64,
    },
    /// Split the active pane. `dir` is "h" (side-by-side) or "v" (stacked).
    SplitPane { dir: String, #[serde(default)] command: Option<String> },
    /// Open a new window (tab). `cwd` anchors it to a workspace directory: the first pane starts
    /// there and so does every later pane in that window (splits included).
    NewWindow {
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Anchor an existing window (by stable id) to a workspace directory; empty clears it.
    SetWorkspaceDir {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        dir: String,
    },
    /// Open one workspace per project folder directly inside `dir` — importing a projects
    /// directory in one gesture. By default only folders containing a `.git` are taken (`all`
    /// takes every subfolder). Folders already open as a workspace are skipped, so re-importing
    /// the same directory adds only what is new. Replies [`ResultBody::Imported`].
    ImportWorkspaces {
        #[serde(default)]
        dir: String,
        #[serde(default)]
        all: bool,
    },
    /// Raise a notification (as if the target pane emitted OSC 777).
    Notify { #[serde(default)] pane: Option<u64>, title: String, #[serde(default)] body: String },

    // --- rendering / control (used by the thin-client GUI, M6 stage 2) ---
    /// Get the active window's pane rectangles + tab list for a content area of `w`×`h` pixels.
    GetLayout { w: u32, h: u32 },
    /// Get a pane's grid for rendering. `offset` scrolls the viewport `offset` lines up into
    /// scrollback history (0 = the live screen); it is clamped server-side to available history.
    GetGrid { pane: u64, #[serde(default)] offset: usize },
    /// Report the GUI's content-area geometry so the daemon resizes the active window's panes.
    /// `pane_chrome` is the per-axis pixel overhead the GUI draws around each pane's cell area
    /// (margins/borders/insets, both sides summed); the daemon subtracts it before dividing a
    /// pane's rect into cells so grids match the *visible* area instead of being scissored.
    /// `pane_chrome` is the horizontal per-axis overhead (subtracted before dividing a pane's
    /// width into columns); `pane_chrome_y` the vertical overhead (adds the title strip), used for
    /// rows. A zero `pane_chrome_y` (old client) falls back to `pane_chrome`.
    ResizeView {
        w: u32,
        h: u32,
        cell_w: u32,
        cell_h: u32,
        #[serde(default)]
        pane_chrome: u32,
        #[serde(default)]
        pane_chrome_y: u32,
    },
    /// Move focus between panes: `dir` is "left" | "right" | "up" | "down".
    FocusPane { dir: String },
    /// Close the active pane.
    ClosePane,
    /// Swap two panes' positions in the active window's split tree (a pane drag-and-drop). The
    /// split shape is unchanged — only which pane occupies which slot. Unknown ids are a no-op.
    SwapPanes {
        #[serde(default)]
        a: u64,
        #[serde(default)]
        b: u64,
    },
    /// Close a SPECIFIC pane by id (a click on that pane's close button). Focuses it first, so the
    /// daemon's close path and the layout that follows are the same ones `ClosePane` produces.
    /// An unknown id is a harmless no-op.
    ClosePaneId {
        #[serde(default)]
        pane: u64,
    },
    /// Toggle zoom on the active pane.
    ToggleZoom,
    /// Drag-resize the split at a divider: grow `pane` (the top/left pane of the dragged divider)
    /// by a fractional split-ratio delta. `dx` moves a vertical divider (adjusts the pane's
    /// horizontal split), `dy` a horizontal divider; the idle axis is 0. Ignored if the pane is
    /// gone. Sent throttled while dragging.
    ResizeSplit {
        #[serde(default)]
        pane: u64,
        #[serde(default)]
        dx: f32,
        #[serde(default)]
        dy: f32,
    },
    /// Switch tabs: `next` true = next window, false = previous.
    SwitchWindow { next: bool },
    /// Close a window (tab) by its STABLE id (`TabWire::id`, a middle-click) — not by sidebar
    /// index, which goes stale when a window is removed daemon-side between the GUI's last
    /// render and the click (every later index shifts). A gone id is a harmless no-op.
    CloseWindow {
        #[serde(default)]
        id: u64,
    },
    /// Activate a window (tab) by its index in the sidebar (a sidebar click). Out-of-range
    /// indices are ignored server-side.
    SelectWindow {
        #[serde(default)]
        index: usize,
    },
    /// Set a window's custom name by its STABLE id (a sidebar double-click rename). An empty
    /// `name` clears the override back to the derived workspace name. Resolved like
    /// `CloseWindow`; a gone id is a harmless no-op.
    RenameWindow {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        name: String,
    },
    /// Put a window (by stable id) into a sidebar group, or take it out with an empty `group`.
    /// Resolved like `RenameWindow`; a gone id is a harmless no-op.
    GroupWindow {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        group: String,
    },
    /// Tag a window (by stable id) with a `#rrggbb` sidebar color; an empty `color` clears it.
    /// Resolved like `RenameWindow`; a gone id is a harmless no-op.
    ColorWindow {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        color: String,
    },
    /// Set a window's pull-request badge by stable id. `status` is `open`/`draft`/`merged`/`closed`;
    /// an empty `status` (or an unparseable one) clears the badge. A gone id is a harmless no-op.
    SetPr {
        #[serde(default)]
        id: u64,
        #[serde(default)]
        number: u32,
        #[serde(default)]
        status: String,
        /// The PR page, so a click on the chip can open it. Optional.
        #[serde(default)]
        url: Option<String>,
    },
    /// Focus a specific pane by id, activating its window too (a pane click). Unknown ids are
    /// ignored server-side.
    FocusPaneId {
        #[serde(default)]
        pane: u64,
    },
    /// Reorder tabs: move the window at index `from` to index `to` (a sidebar drag-drop). Both
    /// indices are clamped to the window count server-side; the active tab follows the moved window.
    MoveWindow {
        #[serde(default)]
        from: usize,
        #[serde(default)]
        to: usize,
    },
    /// Drain notifications raised since the last poll (for the GUI to toast).
    PollNotifications,
    /// Register this connection as a push subscriber: the daemon replies `ok(Done)`, then streams
    /// every subsequent event batch as an unsolicited `Response{id:0}` carrying
    /// [`ResultBody::Notifications`] — one line per tick that produced events. Pane exits arrive in
    /// the same stream as a [`NotifyWire`] with `title == "pane-exited"` and the pane id in `pane`.
    /// The connection stays usable for further requests. Replaces the poll-in-a-loop pattern.
    ///
    /// With `output: true` the push stream ALSO carries per-pane damage wires — a [`NotifyWire`]
    /// with `title == "pane-output"` and the damaged pane's id in `pane`, coalesced to at most one
    /// per pane per tick — so a rendering client redraws only what changed instead of polling. The
    /// default (`false`) keeps `gmux subscribe` CLI streams clean.
    Subscribe {
        #[serde(default)]
        output: bool,
    },
    /// Set the color palette the daemon's terminals resolve grid cells against (fg/bg + the 16
    /// system colors). The GUI sends this once after connecting and on config hot-reload; the
    /// daemon applies it to every existing and future pane. `ansi` shorter than 16 leaves the
    /// remaining indices at their defaults. Old daemons fail to parse the unknown method and
    /// drop the connection after the error response — the GUI's reconnect heals it (SetPalette
    /// is idempotent), at the cost of one connection churn per send.
    SetPalette {
        #[serde(default)]
        fg: [u8; 3],
        #[serde(default)]
        bg: [u8; 3],
        #[serde(default)]
        ansi: Vec<[u8; 3]>,
    },

    // --- browser pane (M12 stage 1, flag-gated in the GUI) ---
    /// Queue a browser request: open (or navigate the existing) browser pane to `url`. The daemon
    /// only queues it; the GUI drains it via [`Call::PollBrowse`] and drives the WebView2 window.
    Browse { url: String },
    /// Drain browser requests queued since the last poll (mirrors `PollNotifications`).
    PollBrowse,

    // --- remote (M9 stage 2c) ---
    /// Attach a remote tmux session and mirror its windows/panes into this session. `target` is
    /// an ssh destination (the daemon runs `ssh -tt <target> -- tmux -CC new -As gmux`);
    /// `command` overrides the entire transport command line (tests / power users can inject any
    /// process that speaks control mode on stdio).
    SshTmux { target: String, #[serde(default)] command: Option<String> },
}

/// A response envelope: exactly one of `result` / `error` is set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultBody>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(id: u64, result: ResultBody) -> Self {
        Response { id, result: Some(result), error: None }
    }
    pub fn err(id: u64, msg: impl Into<String>) -> Self {
        Response { id, result: None, error: Some(msg.into()) }
    }
}

/// Result payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ResultBody {
    Hello { server_version: String, protocol: u32 },
    Panes(Vec<PaneInfo>),
    Text(String),
    /// Scroll offsets of `search-pane` matches (see [`Call::SearchPane`]).
    Matches(Vec<u32>),
    PaneId(u64),
    Layout(LayoutWire),
    Grid(GridWire),
    Notifications(Vec<NotifyWire>),
    /// Browser requests drained by `PollBrowse` (M12): a list of urls to open/navigate to.
    Browses(Vec<String>),
    /// `pane-busy` / `window-busy` verdict (close-confirmation guard).
    Busy(bool),
    /// `import-workspaces` outcome: how many workspaces were opened, how many candidate folders
    /// were skipped because they are already open, and how many were left out by the cap.
    Imported { created: usize, already_open: usize, capped: usize },
    Done,
}

/// A notification raised by a pane (for the GUI to toast). `urgency`: 0 low, 1 normal, 2 critical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotifyWire {
    pub pane: u64,
    pub title: String,
    pub body: String,
    pub urgency: u8,
}

/// A cell on the wire (compact; `flags` bit0 bold, bit1 italic, bit2 underline, bit3 inverse,
/// bit4 wide).
///
/// Wire-size elision: `fg`/`bg`/`flags` are omitted when they equal the CANONICAL wire defaults
/// below and restored by serde on decode — lossless for every value (a themed cell whose colors
/// differ from the canonical constants simply serializes in full). On a default-palette text
/// grid this cuts the per-cell JSON from ~55 to ~10 bytes (~80% of the 30fps grid traffic).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CellWire {
    pub ch: char,
    #[serde(default = "wire_default_fg", skip_serializing_if = "is_wire_default_fg")]
    pub fg: [u8; 3],
    #[serde(default = "wire_default_bg", skip_serializing_if = "is_wire_default_bg")]
    pub bg: [u8; 3],
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub flags: u8,
}

/// Canonical wire default foreground (the built-in palette's fg) — the elision baseline.
pub fn wire_default_fg() -> [u8; 3] {
    [0xcc, 0xcc, 0xcc]
}
/// Canonical wire default background (the built-in palette's bg) — the elision baseline.
pub fn wire_default_bg() -> [u8; 3] {
    [0x11, 0x11, 0x11]
}
fn is_wire_default_fg(v: &[u8; 3]) -> bool {
    *v == wire_default_fg()
}
fn is_wire_default_bg(v: &[u8; 3]) -> bool {
    *v == wire_default_bg()
}
fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

pub const CELL_BOLD: u8 = 1;
pub const CELL_ITALIC: u8 = 2;
pub const CELL_UNDERLINE: u8 = 4;
pub const CELL_INVERSE: u8 = 8;
/// A double-width (CJK/emoji) char: it occupies this cell plus the next, which arrives as a blank
/// spacer (`ch == ' '`, no `CELL_WIDE`).
pub const CELL_WIDE: u8 = 16;

// [`GridWire::mouse_mode`] bits — the application's mouse-reporting mode (0 = wants no mouse).
/// Report button clicks (DECSET 1000).
pub const MOUSE_CLICKS: u8 = 1;
/// Report motion while a button is held (DECSET 1002).
pub const MOUSE_DRAG: u8 = 2;
/// Report any pointer motion (DECSET 1003).
pub const MOUSE_MOTION: u8 = 4;
/// Encode reports in SGR form (DECSET 1006) rather than the legacy byte encoding.
pub const MOUSE_SGR: u8 = 8;

/// A pane's visible grid for rendering (row-major `cells`, length `cols * rows`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GridWire {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cells: Vec<CellWire>,
    /// Scrollback lines available above the viewport (for scroll clamping / indicators).
    #[serde(default)]
    pub history: u32,
    /// The scroll offset actually rendered (the requested offset clamped to `history`).
    #[serde(default)]
    pub offset: u32,
    /// The pane's application enabled bracketed paste (DECSET 2004): pasted text should be
    /// wrapped in `ESC[200~` / `ESC[201~`.
    #[serde(default)]
    pub bracketed_paste: bool,
    /// The pane's mouse-reporting mode (bitfield: [`MOUSE_CLICKS`] | [`MOUSE_DRAG`] |
    /// [`MOUSE_MOTION`] | [`MOUSE_SGR`]). 0 = the app wants no mouse, so the GUI keeps its own
    /// selection/drag behavior; nonzero = forward mouse events to the pane.
    #[serde(default)]
    pub mouse_mode: u8,
    /// The pane's cursor shape as the RAW DECSCUSR Ps value: 0/1/2 block, 3/4 underline, 5/6 bar
    /// (see [`gmux_vt::Terminal::cursor_style`]). The renderer draws the shape; blink is ignored.
    #[serde(default)]
    pub cursor_style: u8,
    /// OSC 8 hyperlink spans visible in this grid (`end` inclusive), capped at 256 server-side.
    /// The GUI merges these into its plain-text-URL underline mechanism, OSC 8 winning on overlap.
    #[serde(default)]
    pub links: Vec<LinkWire>,
}

/// One OSC 8 hyperlink span in a grid: `row`, columns `start..=end` (**`end` inclusive**), and the
/// target `uri`. Mirrors the GUI's `UrlSpan` so it drops straight into the same underline path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkWire {
    pub row: u16,
    pub start: u16,
    pub end: u16,
    pub uri: String,
}

/// One pane's rectangle within the content area.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneRectWire {
    pub id: u64,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub active: bool,
    pub attention: bool,
    /// Short pane title for the GUI's per-pane title strip (daemon-filled: pane title, else the
    /// cwd's short name, else the pane id). `#[serde(default)]` so old daemons still parse.
    #[serde(default)]
    pub title: String,
}

/// One sidebar tab (window).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabWire {
    pub index: usize,
    /// Stable window id (`WindowId`), the target for `Call::CloseWindow` — survives reorders
    /// and removals that shift `index`. `#[serde(default)]` so old daemons still parse.
    #[serde(default)]
    pub id: u64,
    pub name: String,
    pub branch: Option<String>,
    pub attention: bool,
    /// Unread notifications in this window (badged in the sidebar). `#[serde(default)]` so an old
    /// daemon that doesn't send it just reads as zero.
    #[serde(default)]
    pub unread: u32,
    /// Sidebar group this window sits under; `None` = ungrouped (listed above every group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// User-chosen `#rrggbb` tag color for this workspace's row; `None` = untagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A pane in this window has running children (a build, an agent): the sidebar spins an
    /// activity indicator while true. `#[serde(default)]` so an old daemon reads as idle.
    #[serde(default)]
    pub busy: bool,
    /// A pull-request badge; `None` = no PR. `#[serde(default)]` so an old daemon reads as none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PrWire>,
    pub active: bool,
    /// Aggregate agent progress across the window's panes: `Some(pct)` = the least-done active
    /// agent's percentage, `None` = no pane reporting progress. Indeterminate/paused panes count
    /// as active but contribute no percentage.
    #[serde(default)]
    pub progress: Option<u8>,
    /// A pane in the window reported an OSC 9;4 error state (takes visual precedence over `progress`).
    #[serde(default)]
    pub progress_error: bool,
}

/// A workspace's pull-request badge on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrWire {
    pub number: u32,
    /// `open` / `draft` / `merged` / `closed`.
    pub status: String,
    /// The PR page; `None` for a hand-set badge with no URL (the chip then isn't clickable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The active window's layout + the tab list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutWire {
    pub active_pane: u64,
    pub tabs: Vec<TabWire>,
    pub panes: Vec<PaneRectWire>,
    /// The active window is zoomed (one pane temporarily maximized) — the GUI badges it so a
    /// "missing panes" layout reads as deliberate. `#[serde(default)]` for old daemons.
    #[serde(default)]
    pub zoomed: bool,
}

/// One pane's metadata (for `list-panes`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneInfo {
    pub id: u64,
    pub window: usize,
    pub active: bool,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub attention: bool,
}

/// Write one message as a JSON line.
pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()
}

/// Read one JSON-line message. `Ok(None)` on clean EOF; errors on oversized/invalid lines.
pub fn read_msg<T: for<'de> Deserialize<'de>>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    let mut chunk = Vec::new();
    // A bounded take() (on the &mut, which is itself BufRead) enforces MAX_LINE.
    let mut limited = io::Read::take(io::Read::by_ref(r), MAX_LINE as u64 + 1);
    let n = limited.read_until(b'\n', &mut chunk)?;
    if n == 0 {
        return Ok(None); // EOF
    }
    if n > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line exceeds MAX_LINE"));
    }
    line.push_str(&String::from_utf8_lossy(&chunk));
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(trimmed)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_roundtrip_all_methods() {
        let calls = vec![
            Call::Hello { client_version: "0.0.0".into() },
            Call::ListPanes,
            Call::SendKeys { pane: 5, text: "ls".into(), enter: true },
            Call::CapturePane { pane: 5, scrollback: Some(100) },
            Call::SearchPane { pane: 5, query: "TODO".into() },
            Call::PromptOffsets { pane: 5 },
            Call::PaneBusy { pane: 5 },
            Call::WindowBusy { id: 3 },
            Call::SplitPane { dir: "h".into(), command: None },
            Call::NewWindow { command: Some("cmd.exe".into()), cwd: Some(r"C:\\proj".into()) },
            Call::SetWorkspaceDir { id: 5, dir: r"C:\\proj".into() },
            Call::ImportWorkspaces { dir: r"C:\\projects".into(), all: true },
            Call::Notify { pane: Some(5), title: "T".into(), body: "B".into() },
            Call::SshTmux { target: "dev@build-box".into(), command: None },
            Call::SshTmux { target: String::new(), command: Some("cmd.exe /c type canned.bin".into()) },
        ];
        for (i, call) in calls.into_iter().enumerate() {
            let req = Request { id: i as u64, call };
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let mut cur = Cursor::new(buf);
            let back: Request = read_msg(&mut cur).unwrap().unwrap();
            assert_eq!(back, req);
        }
    }

    #[test]
    fn response_roundtrip_and_wire_shape() {
        let resp = Response::ok(7, ResultBody::Text("hello".into()));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let s = String::from_utf8(buf.clone()).unwrap();
        assert!(s.ends_with('\n'), "must be newline-terminated");
        assert!(!s.contains("\"error\""), "ok responses omit error: {s}");
        let mut cur = Cursor::new(buf);
        let back: Response = read_msg(&mut cur).unwrap().unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn method_names_are_kebab_case() {
        let req = Request { id: 1, call: Call::SendKeys { pane: 2, text: "x".into(), enter: false } };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"send-keys\""), "{s}");
        let req = Request { id: 1, call: Call::ListPanes };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"list-panes\""), "{s}");
        let req = Request { id: 1, call: Call::CloseWindow { id: 0 } };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"close-window\""), "{s}");
    }

    #[test]
    fn eof_returns_none_and_oversize_errors() {
        let mut empty = Cursor::new(Vec::<u8>::new());
        assert!(read_msg::<Request>(&mut empty).unwrap().is_none());

        let big = vec![b'x'; MAX_LINE + 10];
        let mut cur = Cursor::new(big);
        assert!(read_msg::<Request>(&mut cur).is_err());
    }

    #[test]
    fn render_methods_and_results_roundtrip() {
        for call in [
            Call::GetLayout { w: 800, h: 600 },
            Call::GetGrid { pane: 3, offset: 25 },
            Call::ResizeView { w: 800, h: 600, cell_w: 9, cell_h: 18, pane_chrome: 34, pane_chrome_y: 56 },
            Call::FocusPane { dir: "right".into() },
            Call::ClosePane,
            Call::ClosePaneId { pane: 9 },
            Call::SwapPanes { a: 3, b: 4 },
            Call::ToggleZoom,
            Call::ResizeSplit { pane: 4, dx: 0.5, dy: -0.25 },
            Call::SwitchWindow { next: true },
            Call::CloseWindow { id: 2 },
            Call::SelectWindow { index: 2 },
            Call::RenameWindow { id: 5, name: "backend".into() },
            Call::GroupWindow { id: 5, group: "api".into() },
            Call::ColorWindow { id: 5, color: "#ff8800".into() },
            Call::SetPr { id: 5, number: 42, status: "open".into(), url: Some("https://x.test/pull/42".into()) },
            Call::FocusPaneId { pane: 7 },
            Call::MoveWindow { from: 3, to: 1 },
            Call::SetPalette {
                fg: [0xcc, 0xcc, 0xcc],
                bg: [0x11, 0x11, 0x11],
                ansi: vec![[0, 0, 0], [0xde, 0xad, 0xbe]],
            },
        ] {
            let req = Request { id: 1, call };
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let back: Request = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
            assert_eq!(back, req);
        }

        let grid = ResultBody::Grid(GridWire {
            cols: 3,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![
                CellWire { ch: 'h', fg: [255, 255, 255], bg: [0, 0, 0], flags: CELL_BOLD },
                CellWire { ch: '中', fg: [10, 20, 30], bg: [1, 2, 3], flags: CELL_WIDE },
                CellWire { ch: ' ', fg: [10, 20, 30], bg: [1, 2, 3], flags: 0 }, // wide spacer
            ],
            history: 120,
            offset: 25,
            bracketed_paste: true,
            mouse_mode: MOUSE_CLICKS | MOUSE_SGR,
            cursor_style: 4,
            links: vec![LinkWire { row: 0, start: 0, end: 2, uri: "https://example.com".into() }],
        });
        let resp = Response::ok(2, grid.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(grid));
    }

    #[test]
    fn tab_wire_progress_roundtrips_and_defaults() {
        let layout = LayoutWire { zoomed: false,
            active_pane: 1,
            tabs: vec![
                TabWire { index: 0, id: 10, name: "a".into(), branch: Some("main".into()), attention: false, unread: 0, group: None, color: None, busy: false, pr: None, active: true, progress: Some(42), progress_error: false },
                TabWire { index: 1, id: 11, name: "b".into(), branch: None, attention: true, unread: 7, group: Some("api".into()), color: Some("#ff8800".into()), busy: true, pr: Some(PrWire { number: 42, status: "open".into(), url: Some("https://x.test/pull/42".into()) }), active: false, progress: None, progress_error: true },
            ],
            panes: Vec::new(),
        };
        let resp = Response::ok(1, ResultBody::Layout(layout.clone()));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(ResultBody::Layout(layout)));

        // Old clients / hand-written JSON omitting the new fields still parse (serde default).
        let line = r#"{"index":2,"name":"c","branch":null,"attention":false,"active":false}"#;
        let tab: TabWire = serde_json::from_str(line).unwrap();
        assert_eq!(tab.progress, None);
        assert!(!tab.progress_error);
        assert_eq!(tab.unread, 0, "an old daemon's tab reads as zero unread, not a bad badge");
        assert_eq!(tab.group, None, "and as ungrouped, so the sidebar renders it at the root");
        assert_eq!(tab.color, None, "untagged, so no color rail");
        assert!(!tab.busy, "and idle, so no spinner");
        assert_eq!(tab.pr, None, "and no PR badge");
    }

    #[test]
    fn ssh_tmux_is_kebab_case_and_command_defaults_to_none() {
        // Hand-written client JSON may omit the optional command override entirely.
        let line = r#"{"id":4,"method":"ssh-tmux","params":{"target":"dev@build-box"}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.call, Call::SshTmux { target: "dev@build-box".into(), command: None });
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"method\":\"ssh-tmux\""), "{s}");
    }

    #[test]
    fn browse_calls_and_result_roundtrip_kebab_case() {
        for call in [Call::Browse { url: "https://example.com".into() }, Call::PollBrowse] {
            let req = Request { id: 1, call };
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let back: Request = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
            assert_eq!(back, req);
        }
        // Method names are kebab-case like the rest of the protocol.
        let s = serde_json::to_string(&Request { id: 1, call: Call::PollBrowse }).unwrap();
        assert!(s.contains("\"method\":\"poll-browse\""), "{s}");
        let s = serde_json::to_string(&Request { id: 1, call: Call::Browse { url: "u".into() } }).unwrap();
        assert!(s.contains("\"method\":\"browse\""), "{s}");

        let resp = Response::ok(2, ResultBody::Browses(vec!["https://a.test".into(), "https://b.test".into()]));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn subscribe_roundtrips_and_is_kebab_case() {
        for output in [false, true] {
            let req = Request { id: 1, call: Call::Subscribe { output } };
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let s = String::from_utf8(buf.clone()).unwrap();
            assert!(s.contains("\"method\":\"subscribe\""), "{s}");
            let back: Request = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
            assert_eq!(back, req);
        }
        // Hand-written / old-client JSON: an absent `output` (empty or missing params) defaults false.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"subscribe","params":{}}"#).unwrap();
        assert_eq!(req.call, Call::Subscribe { output: false });

        // A pushed event batch is an id:0 Response carrying Notifications.
        let push = Response::ok(0, ResultBody::Notifications(vec![NotifyWire {
            pane: 7, title: "pane-exited".into(), body: String::new(), urgency: 1,
        }]));
        let mut buf = Vec::new();
        write_msg(&mut buf, &push).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, push);
    }

    #[test]
    fn set_palette_is_kebab_case_and_fields_default() {
        let s = serde_json::to_string(&Request {
            id: 1,
            call: Call::SetPalette { fg: [1, 2, 3], bg: [4, 5, 6], ansi: vec![] },
        })
        .unwrap();
        assert!(s.contains("\"method\":\"set-palette\""), "{s}");
        // Hand-written / partial JSON: omitted fields fall back to serde defaults.
        let line = r#"{"id":2,"method":"set-palette","params":{"fg":[10,20,30]}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(
            req.call,
            Call::SetPalette { fg: [10, 20, 30], bg: [0, 0, 0], ansi: vec![] }
        );
    }

    #[test]
    fn select_window_and_focus_pane_id_kebab_case_and_default() {
        // Method names are kebab-case like the rest of the protocol.
        let s = serde_json::to_string(&Request { id: 1, call: Call::SelectWindow { index: 3 } }).unwrap();
        assert!(s.contains("\"method\":\"select-window\""), "{s}");
        let s = serde_json::to_string(&Request { id: 1, call: Call::FocusPaneId { pane: 9 } }).unwrap();
        assert!(s.contains("\"method\":\"focus-pane-id\""), "{s}");
        // Hand-written JSON omitting the (defaulted) param still parses.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"select-window","params":{}}"#).unwrap();
        assert_eq!(req.call, Call::SelectWindow { index: 0 });
        let req: Request = serde_json::from_str(r#"{"id":3,"method":"focus-pane-id","params":{}}"#).unwrap();
        assert_eq!(req.call, Call::FocusPaneId { pane: 0 });
    }

    #[test]
    fn rename_window_is_kebab_case_and_fields_default() {
        let s = serde_json::to_string(&Request { id: 1, call: Call::RenameWindow { id: 5, name: "api".into() } }).unwrap();
        assert!(s.contains("\"method\":\"rename-window\""), "{s}");
        // Hand-written JSON omitting the (defaulted) name clears the override; omitting id -> 0.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"rename-window","params":{"id":7}}"#).unwrap();
        assert_eq!(req.call, Call::RenameWindow { id: 7, name: String::new() });
    }

    #[test]
    fn resize_split_is_kebab_case_and_fields_default() {
        let s = serde_json::to_string(&Request { id: 1, call: Call::ResizeSplit { pane: 4, dx: 0.5, dy: 0.0 } }).unwrap();
        assert!(s.contains("\"method\":\"resize-split\""), "{s}");
        // Hand-written JSON may omit the (defaulted) deltas / pane.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"resize-split","params":{"pane":7}}"#).unwrap();
        assert_eq!(req.call, Call::ResizeSplit { pane: 7, dx: 0.0, dy: 0.0 });
    }

    #[test]
    fn move_window_is_kebab_case_and_fields_default() {
        let req = Request { id: 1, call: Call::MoveWindow { from: 3, to: 1 } };
        let mut buf = Vec::new();
        write_msg(&mut buf, &req).unwrap();
        let s = String::from_utf8(buf.clone()).unwrap();
        assert!(s.contains("\"method\":\"move-window\""), "{s}");
        let back: Request = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, req);
        // Hand-written JSON may omit the (defaulted) indices.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"move-window","params":{"to":5}}"#).unwrap();
        assert_eq!(req.call, Call::MoveWindow { from: 0, to: 5 });
    }

    #[test]
    fn pane_rect_title_roundtrips_and_defaults() {
        let layout = LayoutWire { zoomed: false,
            active_pane: 1,
            tabs: Vec::new(),
            panes: vec![PaneRectWire {
                id: 1, x: 0, y: 0, w: 80, h: 24, active: true, attention: false, title: "gmux".into(),
            }],
        };
        let resp = Response::ok(1, ResultBody::Layout(layout.clone()));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(ResultBody::Layout(layout)));
        // Old daemons / hand-written JSON omitting `title` still parse (serde default -> "").
        let line = r#"{"id":9,"x":0,"y":0,"w":80,"h":24,"active":false,"attention":false}"#;
        let rect: PaneRectWire = serde_json::from_str(line).unwrap();
        assert_eq!(rect.title, "");
    }

    #[test]
    fn search_pane_is_kebab_case_and_matches_roundtrips() {
        // Method name is kebab-case like the rest of the protocol.
        let s = serde_json::to_string(&Request { id: 1, call: Call::SearchPane { pane: 2, query: "err".into() } }).unwrap();
        assert!(s.contains("\"method\":\"search-pane\""), "{s}");
        // Hand-written JSON omitting the (defaulted) query still parses -> empty query.
        let req: Request = serde_json::from_str(r#"{"id":2,"method":"search-pane","params":{"pane":7}}"#).unwrap();
        assert_eq!(req.call, Call::SearchPane { pane: 7, query: String::new() });
        // The Matches result round-trips (kebab-case "matches").
        let resp = Response::ok(3, ResultBody::Matches(vec![0, 5, 42]));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        assert!(String::from_utf8(buf.clone()).unwrap().contains("\"matches\""));
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back, resp);
    }

    /// Cell elision: default-palette cells shed fg/bg/flags on the wire and restore losslessly;
    /// non-default cells roundtrip in full; the default-cell encoding stays under 15 bytes
    /// (~4x smaller than the un-elided ~55 — the 30fps grid traffic win).
    #[test]
    fn cell_elision_is_lossless_and_small() {
        let plain = CellWire { ch: 'x', fg: wire_default_fg(), bg: wire_default_bg(), flags: 0 };
        let s = serde_json::to_string(&plain).unwrap();
        assert!(s.len() <= 15, "default cell should elide fg/bg/flags: {s}");
        assert_eq!(serde_json::from_str::<CellWire>(&s).unwrap(), plain, "lossless restore");

        let themed = CellWire { ch: 'y', fg: [1, 2, 3], bg: [4, 5, 6], flags: CELL_BOLD };
        let s = serde_json::to_string(&themed).unwrap();
        assert!(s.contains("fg") && s.contains("bg") && s.contains("flags"), "non-default serializes fully: {s}");
        assert_eq!(serde_json::from_str::<CellWire>(&s).unwrap(), themed);
    }

    #[test]
    fn grid_links_roundtrip_and_default() {
        // A GridWire with OSC 8 links round-trips on the wire.
        let grid = GridWire {
            cols: 1,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![CellWire { ch: 'x', fg: [0, 0, 0], bg: [0, 0, 0], flags: 0 }],
            history: 0,
            offset: 0,
            bracketed_paste: false,
            mouse_mode: 0,
            cursor_style: 0,
            links: vec![
                LinkWire { row: 0, start: 0, end: 0, uri: "https://a.test".into() },
                LinkWire { row: 1, start: 3, end: 7, uri: "mailto:x@y.z".into() },
            ],
        };
        let resp = Response::ok(1, ResultBody::Grid(grid.clone()));
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(ResultBody::Grid(grid)));

        // Old daemons / hand-written JSON omitting `links` still parse (serde default -> empty).
        let line = r#"{"cols":1,"rows":1,"cursor_col":0,"cursor_row":0,"cells":[]}"#;
        let g: GridWire = serde_json::from_str(line).unwrap();
        assert!(g.links.is_empty(), "absent links defaults to empty");
    }

    #[test]
    fn scripting_friendly_hand_written_json_parses() {
        // What a PowerShell script would emit by hand.
        let line = r#"{"id":1,"method":"send-keys","params":{"pane":3,"text":"echo hi","enter":true}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.call, Call::SendKeys { pane: 3, text: "echo hi".into(), enter: true });
    }
}
