//! gmux-tmux — sans-io tmux control-mode (`tmux -CC`) stream parser.
//!
//! gmux's v1 remote story (M9) runs `ssh host tmux -CC new -A -s work` and mirrors the remote
//! tmux session as native gmux tabs/panes — the iTerm2 mechanism. This crate is the pure
//! protocol layer: bytes/lines in, typed [`Event`]s out. No ssh, no process spawning, no async;
//! the transport lands in a later milestone. Protocol facts come from
//! `docs/research/mux-architecture.md` (verified against the tmux Control-Mode wiki).
//!
//! Stream shape:
//! - Every command's reply is wrapped in guards: `%begin <ts> <num> <flags>` … body lines …
//!   `%end <same>` (or `%error <same>` on failure). Replies are strictly ordered; [`Parser`]
//!   correlates `%end`/`%error` to the open `%begin` by command number, and body lines — even
//!   ones that themselves start with `%` — are never misparsed as notifications.
//! - Pane output arrives as `%output %<pane-id> <data>` with bytes < 0x20 and `\` octal-escaped
//!   (`\` → `\134`, CR → `\015`); [`Notification::Output`] carries the *unescaped* bytes
//!   (see [`unescape`]). UTF-8 arrives as raw high bytes and is preserved.
//! - Asynchronous notifications (`%window-add`, `%layout-change`, `%exit`, …) map to
//!   [`Notification`] variants. Control mode adds notifications across tmux versions, so an
//!   unrecognized or malformed `%` line NEVER errors — it becomes [`Notification::Unknown`].
//!
//! The transport is responsible for stripping the `-CC` DCS wrapper (leading `\x1bP1000p`,
//! trailing ST `\x1b\\`) before feeding bytes here; a stray ST simply sits in the line buffer.
//!
//! ```
//! use gmux_tmux::{Event, Notification, Parser};
//!
//! let mut p = Parser::new();
//! let events = p.feed(b"%output %1 hi\\015\n");
//! assert_eq!(
//!     events,
//!     vec![Event::Notification(Notification::Output { pane: 1, data: b"hi\r".to_vec() })],
//! );
//! ```

mod layout;
mod unescape;

pub use layout::{parse_layout, Cell, Layout};
pub use unescape::unescape;

// ---------------------------------------------------------------------------
// Public contract (the M9 remote-tmux client depends on these EXACT shapes).
// ---------------------------------------------------------------------------

/// An asynchronous control-mode notification. Ids are the numeric part of tmux's
/// server-lifetime-unique ids with the sigil stripped: pane `%5` → `5`, window `@3` → `3`,
/// session `$1` → `1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// `%output %<pane> <data>` — pane produced output. `data` is unescaped raw bytes.
    Output { pane: u64, data: Vec<u8> },
    /// `%layout-change @<window> <layout> [...]` — window layout changed. Extra fields
    /// (visible layout, flags; tmux >= 3.2) are ignored.
    LayoutChange { window: u64, layout: Layout },
    /// `%window-add @<window>` — window created.
    WindowAdd { window: u64 },
    /// `%window-close @<window>` — window closed.
    WindowClose { window: u64 },
    /// `%window-renamed @<window> <name>` — window renamed (name may contain spaces).
    WindowRenamed { window: u64, name: String },
    /// `%session-changed $<session> <name>` — the client switched session.
    SessionChanged { session: u64, name: String },
    /// `%sessions-changed` — a session was created or destroyed.
    SessionsChanged,
    /// `%pane-mode-changed %<pane>` — pane entered or left a mode (e.g. copy mode).
    PaneModeChanged { pane: u64 },
    /// `%pause %<pane>` — flow control paused this pane's output (tmux >= 3.2).
    Pause { pane: u64 },
    /// `%continue %<pane>` — flow control resumed this pane's output (tmux >= 3.2).
    Continue { pane: u64 },
    /// `%exit [reason]` — control mode is ending; no further output follows (bar ST).
    Exit { reason: Option<String> },
    /// Any line this parser does not recognize, preserved verbatim (lossy UTF-8). Control mode
    /// grows new notifications across versions — never treat these as fatal.
    Unknown { line: String },
}

/// One parsed control-mode event: a notification, or a fully assembled command reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Notification(Notification),
    /// A `%begin` … `%end`/`%error` block. `num` is the command number used to correlate the
    /// reply with the sent command (replies are strictly ordered). `body` holds the raw bytes of
    /// each line between the guards, verbatim — replies like `capture-pane` can carry non-UTF-8
    /// bytes, so conversion (`String::from_utf8_lossy`) is the consumer's call. `error: true`
    /// means the block closed with `%error`.
    Reply { num: u64, body: Vec<Vec<u8>>, error: bool },
}

// ---------------------------------------------------------------------------
// Parser.
// ---------------------------------------------------------------------------

/// Stateful, sans-io control-mode parser. Feed it byte chunks as they arrive from the
/// transport; it buffers partial lines across calls (lines end `\n`; `\r\n` is tolerated) and
/// tracks an open `%begin` block, so splitting the stream at any byte boundary yields
/// identical events.
#[derive(Debug, Default)]
pub struct Parser {
    /// Bytes of the current, not-yet-terminated line.
    buf: Vec<u8>,
    /// The open `%begin` block, if any. While open, every line (including `%`-prefixed ones
    /// other than the matching `%end`/`%error`) is reply body, not a notification.
    reply: Option<PendingReply>,
    /// True while an overlong unterminated line is being dropped (see [`MAX_LINE`]).
    discarding: bool,
}

/// Longest unterminated line buffered before the parser starts discarding. The peer controls
/// line lengths, so without a cap a newline-free stream grows [`Parser::buf`] without bound.
/// Real control-mode lines (chunked `%output`, one screen row per `capture-pane` body line)
/// stay far below this.
const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug)]
struct PendingReply {
    num: u64,
    body: Vec<Vec<u8>>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume a chunk of transport bytes, returning all events completed by this chunk.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let buf = std::mem::take(&mut self.buf);
        let mut events = Vec::new();
        let mut start = 0;
        while let Some(off) = buf[start..].iter().position(|&b| b == b'\n') {
            let nl = start + off;
            if self.discarding {
                // This newline ends an overlong line whose prefix was already dropped; surface
                // the loss instead of processing a truncated tail as a real line.
                self.discarding = false;
                const NOTE: &str = "<gmux-tmux: overlong line discarded>";
                match self.reply.as_mut() {
                    Some(open) => open.body.push(NOTE.as_bytes().to_vec()),
                    None => events
                        .push(Event::Notification(Notification::Unknown { line: NOTE.into() })),
                }
            } else {
                let end = if nl > start && buf[nl - 1] == b'\r' { nl - 1 } else { nl };
                if let Some(event) = self.process_line(&buf[start..end]) {
                    events.push(event);
                }
            }
            start = nl + 1;
        }
        let rest = &buf[start..];
        if self.discarding {
            // Still inside the overlong line: keep dropping.
        } else if rest.len() > MAX_LINE {
            self.discarding = true;
        } else {
            self.buf = rest.to_vec();
        }
        events
    }

    /// Handle one complete line (terminator already stripped). Returns at most one event;
    /// `%begin` and reply-body lines return `None` until the block closes.
    fn process_line(&mut self, line: &[u8]) -> Option<Event> {
        if let Some(open_num) = self.reply.as_ref().map(|r| r.num) {
            match parse_guard(line) {
                Some(Guard::End { num, error }) if num == open_num => {
                    let done = self.reply.take().expect("reply is open");
                    return Some(Event::Reply { num: done.num, body: done.body, error });
                }
                // Anything else — including `%output …`, a nested `%begin`, or an `%end` with
                // a different command number — is reply body.
                _ => {
                    let open = self.reply.as_mut().expect("reply is open");
                    open.body.push(line.to_vec());
                    return None;
                }
            }
        }
        if let Some(Guard::Begin { num }) = parse_guard(line) {
            self.reply = Some(PendingReply { num, body: Vec::new() });
            return None;
        }
        Some(Event::Notification(parse_notification(line)))
    }
}

// ---------------------------------------------------------------------------
// Guard lines (%begin / %end / %error).
// ---------------------------------------------------------------------------

enum Guard {
    Begin { num: u64 },
    End { num: u64, error: bool },
}

/// Parse `%begin|%end|%error <timestamp> <command-number> [<flags>]`. Returns `None` for
/// anything else (including malformed guards, which then fall through to `Unknown`/body).
/// Guards are only recognized at column 0 — tmux emits them at line start, and an indented
/// look-alike inside a reply body (e.g. captured pane text) must not desync the framing.
fn parse_guard(line: &[u8]) -> Option<Guard> {
    let s = std::str::from_utf8(line).ok()?;
    if !s.starts_with('%') {
        return None;
    }
    let mut words = s.split_ascii_whitespace();
    let kind = words.next()?;
    if !matches!(kind, "%begin" | "%end" | "%error") {
        return None;
    }
    let _timestamp = words.next()?;
    let num: u64 = words.next()?.parse().ok()?;
    match kind {
        "%begin" => Some(Guard::Begin { num }),
        "%end" => Some(Guard::End { num, error: false }),
        _ => Some(Guard::End { num, error: true }),
    }
}

// ---------------------------------------------------------------------------
// Notifications.
// ---------------------------------------------------------------------------

/// Parse a line outside any reply block. Unrecognized/malformed lines become `Unknown`.
fn parse_notification(line: &[u8]) -> Notification {
    parse_known_notification(line).unwrap_or_else(|| Notification::Unknown {
        line: String::from_utf8_lossy(line).into_owned(),
    })
}

fn parse_known_notification(line: &[u8]) -> Option<Notification> {
    // %output data can be arbitrary bytes (raw UTF-8 high bytes, or invalid UTF-8 from a
    // binary-spewing pane), so it is handled at the byte level before any UTF-8 conversion.
    if let Some(rest) = line.strip_prefix(b"%output ") {
        let (id, data) = match rest.iter().position(|&b| b == b' ') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &[][..]),
        };
        let pane = parse_id(std::str::from_utf8(id).ok()?, '%')?;
        return Some(Notification::Output { pane, data: unescape(data) });
    }
    let s = std::str::from_utf8(line).ok()?;
    let (word, rest) = s.split_once(' ').unwrap_or((s, ""));
    match word {
        "%layout-change" => {
            // `@<window> <layout>`; tmux >= 3.2 appends visible-layout and flags tokens.
            let mut tokens = rest.split(' ');
            let window = parse_id(tokens.next()?, '@')?;
            let layout = tokens.next()?.parse().ok()?;
            Some(Notification::LayoutChange { window, layout })
        }
        "%window-add" => Some(Notification::WindowAdd { window: parse_id(first_token(rest)?, '@')? }),
        "%window-close" => {
            Some(Notification::WindowClose { window: parse_id(first_token(rest)?, '@')? })
        }
        "%window-renamed" => {
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            Some(Notification::WindowRenamed { window: parse_id(id, '@')?, name: name.to_owned() })
        }
        "%session-changed" => {
            let (id, name) = rest.split_once(' ').unwrap_or((rest, ""));
            Some(Notification::SessionChanged {
                session: parse_id(id, '$')?,
                name: name.to_owned(),
            })
        }
        "%sessions-changed" => Some(Notification::SessionsChanged),
        "%pane-mode-changed" => {
            Some(Notification::PaneModeChanged { pane: parse_id(first_token(rest)?, '%')? })
        }
        "%pause" => Some(Notification::Pause { pane: parse_id(first_token(rest)?, '%')? }),
        "%continue" => Some(Notification::Continue { pane: parse_id(first_token(rest)?, '%')? }),
        "%exit" => {
            let reason = if rest.is_empty() { None } else { Some(rest.to_owned()) };
            Some(Notification::Exit { reason })
        }
        _ => None,
    }
}

fn first_token(rest: &str) -> Option<&str> {
    rest.split_ascii_whitespace().next()
}

/// Strip the tmux id sigil (`%` pane / `@` window / `$` session) and parse the number.
/// A missing or wrong sigil is a parse failure (the caller falls back to `Unknown`).
fn parse_id(token: &str, sigil: char) -> Option<u64> {
    token.strip_prefix(sigil)?.parse().ok()
}

#[cfg(test)]
mod tests;
