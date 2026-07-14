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
    /// Split the active pane. `dir` is "h" (side-by-side) or "v" (stacked).
    SplitPane { dir: String, #[serde(default)] command: Option<String> },
    /// Open a new window (tab).
    NewWindow { #[serde(default)] command: Option<String> },
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
    /// Activate a window (tab) by its index in the sidebar (a sidebar click). Out-of-range
    /// indices are ignored server-side.
    SelectWindow {
        #[serde(default)]
        index: usize,
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CellWire {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub flags: u8,
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
    pub name: String,
    pub branch: Option<String>,
    pub attention: bool,
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

/// The active window's layout + the tab list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LayoutWire {
    pub active_pane: u64,
    pub tabs: Vec<TabWire>,
    pub panes: Vec<PaneRectWire>,
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
            Call::SplitPane { dir: "h".into(), command: None },
            Call::NewWindow { command: Some("cmd.exe".into()) },
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
            Call::ToggleZoom,
            Call::ResizeSplit { pane: 4, dx: 0.5, dy: -0.25 },
            Call::SwitchWindow { next: true },
            Call::SelectWindow { index: 2 },
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
        });
        let resp = Response::ok(2, grid.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(grid));
    }

    #[test]
    fn tab_wire_progress_roundtrips_and_defaults() {
        let layout = LayoutWire {
            active_pane: 1,
            tabs: vec![
                TabWire { index: 0, name: "a".into(), branch: Some("main".into()), attention: false, active: true, progress: Some(42), progress_error: false },
                TabWire { index: 1, name: "b".into(), branch: None, attention: true, active: false, progress: None, progress_error: true },
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
        let layout = LayoutWire {
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

    #[test]
    fn scripting_friendly_hand_written_json_parses() {
        // What a PowerShell script would emit by hand.
        let line = r#"{"id":1,"method":"send-keys","params":{"pane":3,"text":"echo hi","enter":true}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.call, Call::SendKeys { pane: 3, text: "echo hi".into(), enter: true });
    }
}
