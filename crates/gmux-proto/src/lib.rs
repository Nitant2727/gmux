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
    /// Read a pane's visible screen text.
    CapturePane { pane: u64 },
    /// Split the active pane. `dir` is "h" (side-by-side) or "v" (stacked).
    SplitPane { dir: String, #[serde(default)] command: Option<String> },
    /// Open a new window (tab).
    NewWindow { #[serde(default)] command: Option<String> },
    /// Raise a notification (as if the target pane emitted OSC 777).
    Notify { #[serde(default)] pane: Option<u64>, title: String, #[serde(default)] body: String },

    // --- rendering / control (used by the thin-client GUI, M6 stage 2) ---
    /// Get the active window's pane rectangles + tab list for a content area of `w`×`h` pixels.
    GetLayout { w: u32, h: u32 },
    /// Get a pane's visible grid for rendering.
    GetGrid { pane: u64 },
    /// Report the GUI's content-area geometry so the daemon resizes the active window's panes.
    ResizeView { w: u32, h: u32, cell_w: u32, cell_h: u32 },
    /// Move focus between panes: `dir` is "left" | "right" | "up" | "down".
    FocusPane { dir: String },
    /// Close the active pane.
    ClosePane,
    /// Toggle zoom on the active pane.
    ToggleZoom,
    /// Switch tabs: `next` true = next window, false = previous.
    SwitchWindow { next: bool },
    /// Drain notifications raised since the last poll (for the GUI to toast).
    PollNotifications,
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
    PaneId(u64),
    Layout(LayoutWire),
    Grid(GridWire),
    Notifications(Vec<NotifyWire>),
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

/// A cell on the wire (compact; `flags` bit0 bold, bit1 italic, bit2 underline, bit3 inverse).
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

/// A pane's visible grid for rendering (row-major `cells`, length `cols * rows`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GridWire {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cells: Vec<CellWire>,
}

/// One pane's rectangle within the content area.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneRectWire {
    pub id: u64,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub active: bool,
    pub attention: bool,
}

/// One sidebar tab (window).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabWire {
    pub index: usize,
    pub name: String,
    pub branch: Option<String>,
    pub attention: bool,
    pub active: bool,
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
            Call::CapturePane { pane: 5 },
            Call::SplitPane { dir: "h".into(), command: None },
            Call::NewWindow { command: Some("cmd.exe".into()) },
            Call::Notify { pane: Some(5), title: "T".into(), body: "B".into() },
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
            Call::GetGrid { pane: 3 },
            Call::ResizeView { w: 800, h: 600, cell_w: 9, cell_h: 18 },
            Call::FocusPane { dir: "right".into() },
            Call::ClosePane,
            Call::ToggleZoom,
            Call::SwitchWindow { next: true },
        ] {
            let req = Request { id: 1, call };
            let mut buf = Vec::new();
            write_msg(&mut buf, &req).unwrap();
            let back: Request = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
            assert_eq!(back, req);
        }

        let grid = ResultBody::Grid(GridWire {
            cols: 2,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![
                CellWire { ch: 'h', fg: [255, 255, 255], bg: [0, 0, 0], flags: CELL_BOLD },
                CellWire { ch: 'i', fg: [10, 20, 30], bg: [1, 2, 3], flags: 0 },
            ],
        });
        let resp = Response::ok(2, grid.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &resp).unwrap();
        let back: Response = read_msg(&mut Cursor::new(buf)).unwrap().unwrap();
        assert_eq!(back.result, Some(grid));
    }

    #[test]
    fn scripting_friendly_hand_written_json_parses() {
        // What a PowerShell script would emit by hand.
        let line = r#"{"id":1,"method":"send-keys","params":{"pane":3,"text":"echo hi","enter":true}}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        assert_eq!(req.call, Call::SendKeys { pane: 3, text: "echo hi".into(), enter: true });
    }
}
