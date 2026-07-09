//! Side OSC/BEL event extractor.
//!
//! Per gmux ADR-003: alacritty_terminal's ansi layer silently drops OSC 9/99/777 (it has no
//! unknown-OSC hook), so gmux owns OSC parsing. We run a *separate* [`vte::Parser`] over the same
//! PTY bytes with our own [`vte::ansi`]-free [`Perform`] impl to pull out notification / progress /
//! cwd / title / prompt-mark events. The parser is stateful across [`crate::Terminal::advance`]
//! calls, so OSC sequences split across chunks are still recognized (vte keeps its state).
//!
//! vte pre-splits OSC payloads on `;` (0x3B) into `params: &[&[u8]]` and tells us whether the
//! terminator was BEL via `bell_terminated`. It does *not* split on `:` (0x3A), so OSC 99's
//! `:`-separated metadata arrives intact inside a single param.

use vte::Perform;

use crate::{Notification, NotifyKind, ProgressState, PromptMark, TermEvent, Urgency};

/// Reassembly buffer for a single in-flight OSC 99 notification id (multi-chunk support).
struct Osc99Partial {
    title: String,
    body: String,
    urgency: Urgency,
}

impl Default for Osc99Partial {
    fn default() -> Self {
        Self { title: String::new(), body: String::new(), urgency: Urgency::Normal }
    }
}

/// The side parser's `Perform` sink. Collects [`TermEvent`]s for the current `advance()` chunk into
/// `events`, and holds cross-call state (OSC 99 chunk buffers keyed by id).
struct OscPerform<'a> {
    events: &'a mut Vec<TermEvent>,
    partials: &'a mut Vec<(String, Osc99Partial)>,
}

/// State that must persist across `advance()` calls, owned by [`crate::Terminal`].
#[derive(Default)]
pub(crate) struct OscState {
    /// In-flight OSC 99 reassembly buffers, keyed by notification id (`i=`). A `Vec` keeps this
    /// dependency-free and the count of concurrent partials is tiny in practice.
    partials: Vec<(String, Osc99Partial)>,
}

impl OscState {
    /// Feed a chunk of PTY bytes to the side parser, appending any events seen to `out`.
    pub(crate) fn advance(&mut self, parser: &mut vte::Parser, bytes: &[u8], out: &mut Vec<TermEvent>) {
        let mut perform = OscPerform { events: out, partials: &mut self.partials };
        parser.advance(&mut perform, bytes);
    }
}

impl Perform for OscPerform<'_> {
    /// C0/C1 control bytes that are not consumed by an OSC/CSI/ESC sequence. A lone BEL (0x07)
    /// here is an attention bell (Claude Code `terminal_bell`, Codex/Gemini BEL fallback). BELs
    /// that terminate an OSC never reach `execute` — vte routes them into `osc_dispatch` instead.
    fn execute(&mut self, byte: u8) {
        if byte == 0x07 {
            self.events.push(TermEvent::Bell);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let _ = bell_terminated; // We accept both terminators identically.
        if params.is_empty() {
            return;
        }
        let num = params[0];
        match num {
            b"0" | b"2" => self.osc_title(params),
            b"7" => self.osc_cwd7(params),
            b"9" => self.osc_9(params),
            b"777" => self.osc_777(params),
            b"99" => self.osc_99(params),
            b"133" => self.osc_133(params),
            _ => {}
        }
    }
}

impl OscPerform<'_> {
    /// OSC 0 / OSC 2 -> window title. The title text may itself contain `;` (vte split it), so
    /// rejoin params[1..].
    fn osc_title(&mut self, params: &[&[u8]]) {
        let text = join_from(params, 1);
        self.events.push(TermEvent::Title(sanitize(&text)));
    }

    /// OSC 7 -> cwd. Payload is `file://<host>/<percent-encoded-path>`.
    fn osc_cwd7(&mut self, params: &[&[u8]]) {
        let payload = join_from(params, 1);
        if let Some(path) = parse_file_uri(&payload) {
            self.events.push(TermEvent::Cwd(path));
        }
    }

    /// OSC 9 — ConEmu-overloaded. Disambiguate the numeric subcommand namespace *first*.
    fn osc_9(&mut self, params: &[&[u8]]) {
        // Full payload after "9;", with any `;` restored.
        let payload = join_from(params, 1);

        // Numeric-prefix test: does payload look like `<digits>;` or `<digits>` (end)?
        // vte already split on `;`, so params[1] is the leading token. Only treat as a ConEmu
        // subcommand if that token is purely digits.
        let leading = params.get(1).copied().unwrap_or(b"");
        let is_numeric_sub = !leading.is_empty() && leading.iter().all(|b| b.is_ascii_digit());

        if is_numeric_sub {
            match leading {
                b"4" => {
                    // 9;4;<state>[;<pct>] -> progress. params: ["9","4","<state>", "<pct>?"]
                    if let Some(ev) = parse_progress(params) {
                        self.events.push(ev);
                    }
                    // Unparseable 9;4;... -> swallow (unknown ConEmu-namespace form).
                }
                b"9" => {
                    // 9;9;<path> -> cwd (strip surrounding quotes). Path may contain `;`.
                    let path = join_from(params, 2);
                    let path = strip_quotes(&path);
                    self.events.push(TermEvent::Cwd(path.to_string()));
                }
                b"12" => {
                    self.events.push(TermEvent::PromptMark(PromptMark::PromptStart));
                }
                _ => {
                    // Unknown numeric ConEmu subcommand (not 4/9/12) -> swallow (emit nothing).
                }
            }
        } else {
            // Plain iTerm2-style notification: whole payload is the message.
            self.events.push(TermEvent::Notification(Notification {
                kind: NotifyKind::Osc9,
                title: sanitize(&payload),
                body: String::new(),
                urgency: Urgency::Normal,
                id: None,
            }));
        }
    }

    /// OSC 777 — `777;notify;<title>;<body>`. Body keeps any further `;`.
    fn osc_777(&mut self, params: &[&[u8]]) {
        // params: ["777", "notify", <title>, <body-part0>, <body-part1>, ...]
        if params.get(1).map(|p| *p != b"notify").unwrap_or(true) {
            return; // only the `notify` sub-verb is defined anywhere relevant
        }
        let title = params.get(2).map(|p| bytes_to_string(p)).unwrap_or_default();
        // Everything from params[3..] rejoined with `;` is the body (may be empty).
        let body = join_from(params, 3);
        self.events.push(TermEvent::Notification(Notification {
            kind: NotifyKind::Osc777,
            title: sanitize(&title),
            body: sanitize(&body),
            urgency: Urgency::Normal,
            id: None,
        }));
    }

    /// OSC 99 (kitty) — `99;<metadata>;<payload>`. Metadata is `:`-separated `key=value`.
    fn osc_99(&mut self, params: &[&[u8]]) {
        let meta_raw = params.get(1).copied().unwrap_or(b"");
        // Payload may itself contain `;` (which vte split), so rejoin params[2..].
        let payload_raw = join_from(params, 2);

        // Parse metadata k=v pairs separated by ':'.
        let mut id: Option<String> = None;
        let mut done = true; // `d` defaults to 1 (done)
        let mut is_body = false; // `p` defaults to title
        let mut base64 = false; // `e` defaults to 0
        let mut urgency = Urgency::Normal; // `u` defaults to 1 (normal)

        for kv in split_bytes(meta_raw, b':') {
            let (k, v) = match kv.iter().position(|&b| b == b'=') {
                Some(p) => (&kv[..p], &kv[p + 1..]),
                None => (kv, &b""[..]),
            };
            match k {
                b"i" => id = Some(bytes_to_string(v)),
                b"d" => done = v != b"0",
                b"p" => is_body = v == b"body",
                b"e" => base64 = v == b"1",
                b"u" => {
                    urgency = match v {
                        b"0" => Urgency::Low,
                        b"2" => Urgency::Critical,
                        _ => Urgency::Normal,
                    }
                }
                _ => {} // ignore unknown keys (spec-mandated)
            }
        }

        // Decode this chunk's payload text.
        let text = if base64 {
            match base64_decode(payload_raw.as_bytes()) {
                Some(bytes) => sanitize(&String::from_utf8_lossy(&bytes)),
                None => sanitize(&payload_raw),
            }
        } else {
            sanitize(&payload_raw)
        };

        match id {
            // Chunked / id-bearing form: accumulate under the id until `d=1`.
            Some(id) => {
                let partial = self.partial_mut(&id);
                if is_body {
                    partial.body.push_str(&text);
                } else {
                    partial.title.push_str(&text);
                }
                // Urgency from any chunk sticks (last non-default wins is fine; take latest).
                partial.urgency = urgency;
                if done {
                    let p = self.take_partial(&id);
                    self.events.push(TermEvent::Notification(Notification {
                        kind: NotifyKind::Osc99,
                        title: p.title,
                        body: p.body,
                        urgency: p.urgency,
                        id: Some(id),
                    }));
                }
            }
            // Single-shot form (no id). Only meaningful when done; kitty requires `i` for
            // multi-part, so treat a no-id frame as a complete single notification.
            None => {
                let (title, body) = if is_body {
                    (String::new(), text)
                } else {
                    (text, String::new())
                };
                self.events.push(TermEvent::Notification(Notification {
                    kind: NotifyKind::Osc99,
                    title,
                    body,
                    urgency,
                    id: None,
                }));
            }
        }
    }

    /// OSC 133 — semantic prompt marks (FinalTerm/FTCS).
    fn osc_133(&mut self, params: &[&[u8]]) {
        let mark = match params.get(1).copied().unwrap_or(b"") {
            b"A" => Some(PromptMark::PromptStart),
            b"B" => Some(PromptMark::CommandStart),
            b"C" => Some(PromptMark::CommandExecuted),
            b"D" => {
                // 133;D[;exit] — exit may be present as params[2].
                let exit = params.get(2).and_then(|p| bytes_to_string(p).trim().parse::<i32>().ok());
                Some(PromptMark::CommandFinished(exit))
            }
            _ => None,
        };
        if let Some(mark) = mark {
            self.events.push(TermEvent::PromptMark(mark));
        }
    }

    fn partial_mut(&mut self, id: &str) -> &mut Osc99Partial {
        if let Some(pos) = self.partials.iter().position(|(k, _)| k == id) {
            &mut self.partials[pos].1
        } else {
            self.partials.push((id.to_string(), Osc99Partial::default()));
            &mut self.partials.last_mut().unwrap().1
        }
    }

    fn take_partial(&mut self, id: &str) -> Osc99Partial {
        if let Some(pos) = self.partials.iter().position(|(k, _)| k == id) {
            self.partials.remove(pos).1
        } else {
            Osc99Partial::default()
        }
    }
}

/// Parse a 9;4 progress payload. params: `["9","4","<state>"[, "<pct>"]]`.
fn parse_progress(params: &[&[u8]]) -> Option<TermEvent> {
    let state_tok = params.get(2)?;
    let state = match *state_tok {
        b"0" => ProgressState::Remove,
        b"1" => ProgressState::Set,
        b"2" => ProgressState::Error,
        b"3" => ProgressState::Indeterminate,
        b"4" => ProgressState::Paused,
        _ => return None,
    };
    // pct is optional; clamp to 0..=100 like shipped terminals do.
    let pct = params.get(3).and_then(|p| bytes_to_string(p).trim().parse::<u32>().ok()).map(|v| v.min(100) as u8);
    Some(TermEvent::Progress { state, pct })
}

/// Join params[from..] back together with the `;` that vte split them on.
fn join_from(params: &[&[u8]], from: usize) -> String {
    if from >= params.len() {
        return String::new();
    }
    let mut joined: Vec<u8> = Vec::new();
    for (i, p) in params[from..].iter().enumerate() {
        if i > 0 {
            joined.push(b';');
        }
        joined.extend_from_slice(p);
    }
    bytes_to_string(&joined)
}

/// Lossy UTF-8 decode.
fn bytes_to_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Split a byte slice on a single-byte separator into sub-slices.
fn split_bytes(b: &[u8], sep: u8) -> impl Iterator<Item = &[u8]> {
    b.split(move |&x| x == sep)
}

/// Strip a single pair of surrounding double-quotes if present.
fn strip_quotes(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Parse an OSC 7 `file://host/percent-encoded-path` into a plain path string.
/// Maps `file://HOST/C:/repo` -> `C:\repo` on the Windows-flavored forms, and leaves POSIX-style
/// `/home/user` intact. Best-effort: returns None only if the scheme is entirely wrong.
fn parse_file_uri(payload: &str) -> Option<String> {
    let rest = payload.strip_prefix("file://")?;
    // rest = "<host>/<path>" ; the path starts at the first '/'.
    let path_part = match rest.find('/') {
        Some(idx) => &rest[idx + 1..],
        None => "", // "file://host" with no path
    };
    let decoded = percent_decode(path_part);
    // Windows drive form: "C:/repo" or "/C:/repo". Normalize to backslashes when it looks like a
    // drive-letter path; otherwise keep forward slashes (POSIX).
    let decoded = decoded.strip_prefix('/').map(|s| s.to_string()).filter(|s| is_drive_path(s)).unwrap_or(decoded);
    if is_drive_path(&decoded) {
        Some(decoded.replace('/', "\\"))
    } else {
        Some(decoded)
    }
}

/// Heuristic: does the string start with `X:` where X is an ASCII letter (a Windows drive path)?
fn is_drive_path(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Minimal RFC 3986 percent-decoder (`%XX` -> byte). Invalid escapes are passed through literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip C0/C1 control characters from untrusted notification/title text (security §8: agent
/// output can embed attacker-controlled control bytes). Keeps normal printable text.
fn sanitize(s: &str) -> String {
    s.chars().filter(|&c| !c.is_control() || c == ' ').collect()
}

/// Minimal RFC 4648 base64 decoder tolerant of missing final padding (kitty allows it). Returns
/// None on genuinely malformed input (bad alphabet char).
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}
