//! CLI client for the gmux automation API: connects to `\\.\pipe\gmux.<user>`, sends one
//! JSON-line request, prints the result.

use std::io::BufReader;

use gmux_proto::{read_msg, write_msg, Call, PaneInfo, Request, Response, ResultBody};

/// Send one call to the running gmux and return its response.
fn call(call: Call) -> Result<Response, String> {
    let name = gmux_pipe::pipe_name_for_user("gmux");
    let stream = gmux_pipe::client_connect(&name)
        .map_err(|e| format!("cannot reach gmux at \\\\.\\pipe\\{name} — is gmux running? ({e})"))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    write_msg(&mut writer, &Request { id: 1, call }).map_err(|e| e.to_string())?;
    match read_msg::<Response>(&mut reader) {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Err("gmux closed the connection".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn run(c: Call) -> i32 {
    match call(c) {
        Ok(Response { error: Some(e), .. }) => {
            eprintln!("gmux: {e}");
            1
        }
        Ok(Response { result: Some(body), .. }) => {
            print_result(&body);
            0
        }
        Ok(_) => {
            eprintln!("gmux: empty response");
            1
        }
        Err(e) => {
            eprintln!("gmux: {e}");
            1
        }
    }
}

fn print_result(body: &ResultBody) {
    match body {
        ResultBody::Hello { server_version, protocol } => {
            println!("gmux {server_version} (protocol v{protocol})");
        }
        ResultBody::Panes(panes) => print_panes(panes),
        ResultBody::Text(t) => println!("{t}"),
        ResultBody::PaneId(id) => println!("%{id}"),
        ResultBody::Busy(b) => println!("{b}"),
        ResultBody::Layout(_) | ResultBody::Grid(_) | ResultBody::Notifications(_) | ResultBody::Browses(_) | ResultBody::Matches(_) => {} // not for the CLI
        ResultBody::Done => {}
    }
}

fn print_panes(panes: &[PaneInfo]) {
    for p in panes {
        let flags = format!(
            "{}{}",
            if p.active { "*" } else { "" },
            if p.attention { "!" } else { "" }
        );
        let cwd = p.cwd.as_deref().unwrap_or("-");
        println!("%{}\t@{}\t{}x{}\t{}\t{}\t{}", p.id, p.window, p.cols, p.rows, flags, cwd, p.title);
    }
}

/// Parse `%5` / `5` into a pane id.
fn parse_pane(s: &str) -> Option<u64> {
    s.trim_start_matches('%').parse().ok()
}

/// `@3` / `3` -> a stable window id (the `@N` column `list-panes` prints).
fn parse_window(s: &str) -> Option<u64> {
    s.trim_start_matches('@').parse().ok()
}

/// Resolve the current branch's PR via `gh pr view --json number,state,isDraft`, returning
/// `(number, status_token)`. Runs in THIS short-lived CLI process (the user's cwd), never in the
/// daemon — so the daemon stays free of network calls and timers. `None` if `gh` is missing/
/// unauthenticated, there's no PR, or the JSON can't be parsed. The parse is dependency-free
/// (serde isn't a dep of the gmux binary): it scavenges the three fields from the flat object.
fn resolve_pr_via_gh() -> Option<(u32, String)> {
    let out = std::process::Command::new("gh")
        .args(["pr", "view", "--json", "number,state,isDraft"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let json = String::from_utf8_lossy(&out.stdout);
    let number: u32 = json_number(&json, "number")?;
    let state = json_string(&json, "state")?;
    let is_draft = json.contains("\"isDraft\":true") || json.contains("\"isDraft\": true");
    let status = gmux_status_from_github(&state, is_draft)?;
    Some((number, status.to_string()))
}

/// Extract `"key":<digits>` from a flat JSON object.
fn json_number(json: &str, key: &str) -> Option<u32> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start_matches([':', ' ']);
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Extract `"key":"<value>"` from a flat JSON object.
fn json_string(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start_matches([':', ' ']);
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Map GitHub's state + draft flag to a gmux status token (mirrors `PrStatus::from_github`, kept
/// here so the gmux binary needn't depend on gmux-mux just for the CLI). `None` on an unknown state.
fn gmux_status_from_github(state: &str, is_draft: bool) -> Option<&'static str> {
    match state.trim().to_ascii_uppercase().as_str() {
        "OPEN" => Some(if is_draft { "draft" } else { "open" }),
        "MERGED" => Some("merged"),
        "CLOSED" => Some("closed"),
        _ => None,
    }
}

/// `#rrggbb` / `rrggbb` -> a canonical `#rrggbb`, or `None` if it isn't six hex digits. Mirrors
/// cmux's `WorkspaceTabColorSettings.normalizedHex`, so a typo is rejected at the CLI instead of
/// being stored and silently ignored by the renderer. Pure/tested.
fn normalize_hex(s: &str) -> Option<String> {
    let body = s.trim().trim_start_matches('#');
    if body.len() == 6 && body.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("#{}", body.to_ascii_lowercase()))
    } else {
        None
    }
}

/// Rebuild a command line from argv pieces after `--`, re-quoting arguments the shell had
/// unwrapped: a plain `join(" ")` would turn `claude -p "work on auth"` into
/// `claude -p work on auth`, splintering the prompt into stray positionals when the child
/// re-parses its command line.
fn join_command(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c == ' ' || c == '\t') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse the `-S` argument into a line count. `-` (or missing / `0`) means "all retained history";
/// a positive number `n` means "the most-recent n lines". `0` is the sentinel for "all".
fn parse_scrollback(arg: Option<&str>) -> usize {
    match arg {
        None | Some("-") => 0,
        Some(n) => n.parse().unwrap_or(0),
    }
}

/// Parse `ssh-tmux` arguments: the first non-flag argument is the ssh target; `--command <raw>`
/// overrides the entire transport command line. `None` when no target (and no override) is given.
fn parse_ssh_tmux(args: &[String]) -> Option<(String, Option<String>)> {
    let (mut target, mut command) = (None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--command" => {
                i += 1;
                command = args.get(i).cloned();
            }
            other if target.is_none() => target = Some(other.to_string()),
            _ => {}
        }
        i += 1;
    }
    // A raw --command needs no target (the override replaces the ssh line entirely).
    match (target, command) {
        (Some(t), c) => Some((t, c)),
        (None, Some(c)) => Some((String::new(), Some(c))),
        (None, None) => None,
    }
}

/// `gmux subscribe [--output]` — register as a push subscriber and print one JSON line per event
/// batch the daemon streams, until the connection closes or Ctrl+C. Reuses the raw `Response` JSON
/// as the output line (each is `{"id":0,"result":{"notifications":[...]}}`), so scripts can parse
/// it with the same reader they use for any other reply. `--output` also streams per-pane
/// `pane-output` damage wires (noisy — for rendering clients, not toast scripts).
fn subscribe(output: bool) -> i32 {
    let name = gmux_pipe::pipe_name_for_user("gmux");
    let stream = match gmux_pipe::client_connect(&name) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gmux: cannot reach gmux at \\\\.\\pipe\\{name} — is gmux running? ({e})");
            return 1;
        }
    };
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("gmux: {e}");
            return 1;
        }
    };
    let mut reader = BufReader::new(stream);
    if let Err(e) = write_msg(&mut writer, &Request { id: 1, call: Call::Subscribe { output } }) {
        eprintln!("gmux: {e}");
        return 1;
    }
    // First reply is the ok(Done) ack; every subsequent line is a pushed batch. Print each raw
    // Response line as it arrives (the ack included — it's a valid, harmless line for scripts).
    loop {
        match read_msg::<Response>(&mut reader) {
            Ok(Some(resp)) => match serde_json::to_string(&resp) {
                Ok(line) => println!("{line}"),
                Err(e) => {
                    eprintln!("gmux: {e}");
                    return 1;
                }
            },
            Ok(None) => return 0, // daemon closed the connection
            Err(e) => {
                eprintln!("gmux: {e}");
                return 1;
            }
        }
    }
}

/// `gmux wait-for -t <pane> (--text <substr> | --exit | --idle <secs>) [--timeout <secs>]` —
/// block until the condition holds. The orchestrator primitive: gate a script on an agent's
/// output appearing, its pane closing, or its screen going quiet. Polls the existing API every
/// 400ms (search-pane / capture-pane) — no daemon support needed. Exit codes: 0 condition met,
/// 1 timeout or daemon unreachable, 2 usage.
fn wait_for(args: &[String]) -> i32 {
    use std::time::{Duration, Instant};
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1));
    let pane = get("-t").and_then(|s| parse_pane(s));
    let text = get("--text").cloned();
    let idle = get("--idle").and_then(|s| s.parse::<f64>().ok());
    let wants_exit = args.iter().any(|a| a == "--exit");
    let timeout = get("--timeout").and_then(|s| s.parse::<f64>().ok());
    let modes = [text.is_some(), wants_exit, idle.is_some()].iter().filter(|b| **b).count();
    let (Some(pane), 1) = (pane, modes) else {
        eprintln!("usage: gmux wait-for -t <pane> (--text <substr> | --exit | --idle <secs>) [--timeout <secs>]");
        return 2;
    };
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs_f64(s));
    // Idle detection: the visible screen text unchanged for the requested window. ponytail:
    // screen-content compare, not an output-event stream — survives daemon reconnects and needs
    // no protocol; a repainting-but-static TUI reads as idle, which is the useful answer anyway.
    let mut last_screen: Option<String> = None;
    let mut quiet_since = Instant::now();
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return 1;
        }
        if let Some(q) = &text {
            match call(Call::SearchPane { pane, query: q.clone() }) {
                Ok(r) if matches!(r.result, Some(ResultBody::Matches(ref m)) if !m.is_empty()) => {
                    return 0;
                }
                Ok(r) if r.error.is_some() => return 1, // pane gone
                Ok(_) => {}
                Err(_) => return 1,
            }
        } else if wants_exit {
            match call(Call::CapturePane { pane, scrollback: None }) {
                Ok(r) if r.error.is_some() => return 0, // no such pane: it exited
                Ok(_) => {}
                Err(_) => return 0, // daemon gone entirely counts as exited
            }
        } else {
            match call(Call::CapturePane { pane, scrollback: None }) {
                Ok(r) => match r.result {
                    Some(ResultBody::Text(t)) => {
                        if last_screen.as_deref() != Some(t.as_str()) {
                            last_screen = Some(t);
                            quiet_since = Instant::now();
                        } else if quiet_since.elapsed() >= Duration::from_secs_f64(idle.unwrap()) {
                            return 0;
                        }
                    }
                    _ => return 1, // pane gone while waiting for idle
                },
                Err(_) => return 1,
            }
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// `gmux screenshot -t <pane> [-o <file.bmp>]` — fetch the pane's live grid and render it to a
/// BMP via the GUI crate's headless offscreen renderer (same GPU path the windowed app uses).
/// ponytail: BMP, not PNG — zero dependencies, opens everywhere; swap in a PNG encoder the day a
/// dep is worth it. Exit codes: 0 written, 1 daemon/render failure, 2 usage.
fn screenshot(args: &[String]) -> i32 {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1));
    let Some(pane) = get("-t").and_then(|s| parse_pane(s)) else {
        eprintln!("usage: gmux screenshot -t <pane> [-o <file.bmp>]");
        return 2;
    };
    let out = get("-o").cloned().unwrap_or_else(|| format!("gmux-pane{pane}.bmp"));
    let grid = match call(Call::GetGrid { pane, offset: 0 }) {
        Ok(r) => match r.result {
            Some(ResultBody::Grid(g)) => g,
            _ => {
                eprintln!("gmux: {}", r.error.unwrap_or_else(|| "no grid".into()));
                return 1;
            }
        },
        Err(e) => {
            eprintln!("gmux: {e}");
            return 1;
        }
    };
    let snap = gmux_gui::app::grid_to_snapshot(&grid);
    // ponytail: 12x24 is a safe upper bound on the 18px-font cell — the render fills cells from
    // the top-left and any excess is background margin, which beats clipping.
    let (px_w, px_h) = (snap.cols as u32 * 12, snap.rows as u32 * 24);
    let Some((w, h, rgba)) = gmux_gui::render_offscreen(
        &snap,
        gmux_gui::Attention::Quiet,
        px_w.max(1),
        px_h.max(1),
    ) else {
        eprintln!("gmux: no GPU adapter available for offscreen rendering");
        return 1;
    };
    match write_bmp(&out, w, h, &rgba) {
        Ok(()) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("gmux: write failed: {e}");
            1
        }
    }
}

/// Write RGBA8 pixels as a bottom-up 24-bit BMP (BGR). Dependency-free.
fn write_bmp(path: &str, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let row_bytes = (w * 3 + 3) & !3; // rows padded to 4 bytes
    let pixel_bytes = row_bytes * h;
    let file_size = 54 + pixel_bytes;
    let mut out = Vec::with_capacity(file_size as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]); // no compression, default ppm/palette fields
    for y in (0..h).rev() {
        let mut written = 0;
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            out.extend_from_slice(&[rgba[o + 2], rgba[o + 1], rgba[o]]);
            written += 3;
        }
        while written % 4 != 0 {
            out.push(0);
            written += 1;
        }
    }
    std::fs::write(path, out)
}

/// Resolve `gmux browse` input to a URL: explicit scheme passes through; a single dotted,
/// space-free token is a domain (https:// prefixed); everything else becomes a DuckDuckGo search (no captcha wall on fresh WebView2 profiles, unlike Google).
/// Pure/tested.
fn browse_target(input: &str) -> String {
    let t = input.trim();
    if t.starts_with("http://") || t.starts_with("https://") {
        return t.to_string();
    }
    if !t.contains(' ') && t.contains('.') {
        return format!("https://{t}");
    }
    let mut q = String::new();
    for b in t.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => q.push(b as char),
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    format!("https://duckduckgo.com/?q={q}")
}

/// Entry: dispatch `gmux <subcommand> ...` API calls. Returns an exit code.
pub fn dispatch(cmd: &str, args: &[String]) -> Option<i32> {
    match cmd {
        "list-panes" => Some(run(Call::ListPanes)),
        "wait-for" => Some(wait_for(args)),
        "screenshot" => Some(screenshot(args)),
        "subscribe" => Some(subscribe(args.iter().any(|a| a == "--output"))),
        "hello" => Some(run(Call::Hello { client_version: env!("CARGO_PKG_VERSION").into() })),
        "send-keys" => {
            let (mut pane, mut enter, mut text_parts) = (None, false, Vec::new());
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "-t" => {
                        i += 1;
                        pane = args.get(i).and_then(|s| parse_pane(s));
                    }
                    "--enter" | "Enter" => enter = true,
                    other => text_parts.push(other.to_string()),
                }
                i += 1;
            }
            let Some(pane) = pane else {
                eprintln!("usage: gmux send-keys -t <pane> [--enter|Enter] <text...>");
                return Some(2);
            };
            Some(run(Call::SendKeys { pane, text: text_parts.join(" "), enter }))
        }
        "capture-pane" => {
            let pane = args.iter().position(|a| a == "-t").and_then(|i| args.get(i + 1)).and_then(|s| parse_pane(s));
            let Some(pane) = pane else {
                eprintln!("usage: gmux capture-pane -t <pane> [-S <n>|-S -]");
                return Some(2);
            };
            // -S includes scrollback: `-S -` (or `-S 0`) = all history; `-S <n>` = last n lines.
            let scrollback = args
                .iter()
                .position(|a| a == "-S")
                .map(|i| parse_scrollback(args.get(i + 1).map(String::as_str)));
            Some(run(Call::CapturePane { pane, scrollback }))
        }
        "group" => {
            // `gmux group -t @<window-id> <name...>` files a window under a sidebar group;
            // `--clear` (or no name) takes it back out.
            let id = args.iter().position(|a| a == "-t").and_then(|i| args.get(i + 1)).and_then(|s| parse_window(s));
            let Some(id) = id else {
                eprintln!("usage: gmux group -t @<window-id> <name...> | --clear");
                return Some(2);
            };
            let clear = args.iter().any(|a| a == "--clear");
            let name = if clear {
                String::new()
            } else {
                // Everything that isn't the flag pair is the group name.
                let mut words = Vec::new();
                let mut i = 0;
                while i < args.len() {
                    match args[i].as_str() {
                        "-t" => i += 1, // skip the id too
                        w if !w.starts_with('-') => words.push(w.to_string()),
                        _ => {}
                    }
                    i += 1;
                }
                words.join(" ")
            };
            Some(run(Call::GroupWindow { id, group: name }))
        }
        "color" => {
            // `gmux color -t @<window-id> #rrggbb` tags a workspace row; `--clear` untags it.
            let id = args.iter().position(|a| a == "-t").and_then(|i| args.get(i + 1)).and_then(|s| parse_window(s));
            let Some(id) = id else {
                eprintln!("usage: gmux color -t @<window-id> #rrggbb | --clear");
                return Some(2);
            };
            let clear = args.iter().any(|a| a == "--clear");
            let hex = if clear {
                String::new()
            } else {
                // The color is the first non-flag word that isn't the id. '#rrggbb' starts with
                // '#', not '-', so it never looks like a flag.
                let mut found = String::new();
                let mut i = 0;
                while i < args.len() {
                    match args[i].as_str() {
                        "-t" => i += 1,
                        w if !w.starts_with('-') && found.is_empty() => found = w.to_string(),
                        _ => {}
                    }
                    i += 1;
                }
                if normalize_hex(&found).is_none() {
                    eprintln!("gmux color: expected a #rrggbb color, got '{found}'");
                    return Some(2);
                }
                found
            };
            Some(run(Call::ColorWindow { id, color: hex }))
        }
        "pr" => {
            // `gmux pr -t @<win> <number> <open|draft|merged|closed>` sets a PR badge;
            // `gmux pr -t @<win> --resolve` shells `gh` here (NOT in the daemon) to read the
            // current branch's PR; `--clear` removes it.
            let id = args.iter().position(|a| a == "-t").and_then(|i| args.get(i + 1)).and_then(|s| parse_window(s));
            let Some(id) = id else {
                eprintln!("usage: gmux pr -t @<win> <number> <open|draft|merged|closed> | --resolve | --clear");
                return Some(2);
            };
            if args.iter().any(|a| a == "--clear") {
                return Some(run(Call::SetPr { id, number: 0, status: String::new() }));
            }
            if args.iter().any(|a| a == "--resolve") {
                return Some(match resolve_pr_via_gh() {
                    Some((number, status)) => run(Call::SetPr { id, number, status }),
                    None => {
                        eprintln!("gmux pr: no PR found for the current branch (is `gh` installed and authenticated?)");
                        1
                    }
                });
            }
            // Explicit: the non-flag words are the number then the status.
            let words: Vec<&str> = args
                .iter()
                .enumerate()
                .filter(|(i, a)| !a.starts_with('-') && args.get(i.wrapping_sub(1)).map(String::as_str) != Some("-t"))
                .map(|(_, a)| a.as_str())
                .collect();
            let (Some(num), Some(status)) = (words.first().and_then(|n| n.parse::<u32>().ok()), words.get(1)) else {
                eprintln!("usage: gmux pr -t @<win> <number> <open|draft|merged|closed> | --resolve | --clear");
                return Some(2);
            };
            if !matches!(status.to_ascii_lowercase().as_str(), "open" | "draft" | "merged" | "closed") {
                eprintln!("gmux pr: status must be open, draft, merged, or closed");
                return Some(2);
            }
            Some(run(Call::SetPr { id, number: num, status: status.to_string() }))
        }
        "split-pane" => {
            let dir = if args.iter().any(|a| a == "-v") { "v" } else { "h" }.to_string();
            let command = args.iter().position(|a| a == "--").map(|i| join_command(&args[i + 1..])).filter(|s| !s.is_empty());
            Some(run(Call::SplitPane { dir, command }))
        }
        "new-window" => {
            let command = args.iter().position(|a| a == "--").map(|i| join_command(&args[i + 1..])).filter(|s| !s.is_empty());
            Some(run(Call::NewWindow { command }))
        }
        "ssh-tmux" => {
            let Some((target, command)) = parse_ssh_tmux(args) else {
                eprintln!("usage: gmux ssh-tmux <target> [--command <raw transport command>]");
                return Some(2);
            };
            Some(run(Call::SshTmux { target, command }))
        }
        "browse" => {
            // All non-flag args form the input: a URL passes through, a bare domain gets https://,
            // and anything else becomes a web search — `gmux browse rust wgpu present` just works.
            // Default target is the SYSTEM browser (explorer.exe hands the url to the protocol
            // handler with no shell parsing — the same hardened path Ctrl+click uses). `--pane`
            // routes to the in-app WebView2 pane instead (currently unreliable; see PARKED.md).
            let words: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(|s| s.as_str()).collect();
            if words.is_empty() {
                eprintln!("usage: gmux browse [--pane] <url | search terms...>");
                return Some(2);
            }
            let url = browse_target(&words.join(" "));
            if args.iter().any(|a| a == "--pane") {
                return Some(run(Call::Browse { url }));
            }
            match std::process::Command::new("explorer").arg(&url).spawn() {
                Ok(_) => {
                    println!("{url}");
                    Some(0)
                }
                Err(e) => {
                    eprintln!("gmux: could not open browser: {e}");
                    Some(1)
                }
            }
        }
        _ => None, // not an API subcommand
    }
}

#[cfg(test)]
mod tests {
    use super::{browse_target, normalize_hex, parse_pane, parse_window};

    /// URL passthrough, bare-domain https, and search fallback with percent-encoding.
    #[test]
    fn browse_target_resolves() {
        assert_eq!(browse_target("https://a.test/x"), "https://a.test/x");
        assert_eq!(browse_target("docs.rs"), "https://docs.rs");
        assert_eq!(
            browse_target("rust wgpu present"),
            "https://duckduckgo.com/?q=rust+wgpu+present"
        );
        assert_eq!(browse_target("c# & more"), "https://duckduckgo.com/?q=c%23+%26+more");
    }

    #[test]
    fn pane_targets_parse() {
        assert_eq!(parse_pane("%5"), Some(5));
        assert_eq!(parse_pane("5"), Some(5));
        assert_eq!(parse_pane("nope"), None);
    }

    #[test]
    fn window_targets_and_hex_parse() {
        assert_eq!(parse_window("@3"), Some(3));
        assert_eq!(parse_window("3"), Some(3));
        assert_eq!(parse_window("nope"), None);
        // The color CLI canonicalizes to lowercase #rrggbb and rejects anything else.
        assert_eq!(normalize_hex("#FF8800").as_deref(), Some("#ff8800"));
        assert_eq!(normalize_hex("ff8800").as_deref(), Some("#ff8800"));
        assert_eq!(normalize_hex("#fff"), None);
        assert_eq!(normalize_hex("#gggggg"), None);
        assert_eq!(normalize_hex("red"), None);
    }

    #[test]
    fn scrollback_arg_parses() {
        use super::parse_scrollback;
        assert_eq!(parse_scrollback(Some("-")), 0); // all history
        assert_eq!(parse_scrollback(None), 0); // bare -S = all
        assert_eq!(parse_scrollback(Some("0")), 0);
        assert_eq!(parse_scrollback(Some("200")), 200);
        assert_eq!(parse_scrollback(Some("garbage")), 0);
    }

    #[test]
    fn ssh_tmux_args_parse() {
        use super::parse_ssh_tmux;
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_ssh_tmux(&a(&["dev@box"])), Some(("dev@box".into(), None)));
        assert_eq!(
            parse_ssh_tmux(&a(&["dev@box", "--command", "cmd.exe /c type x"])),
            Some(("dev@box".into(), Some("cmd.exe /c type x".into()))),
        );
        // A raw override needs no target.
        assert_eq!(
            parse_ssh_tmux(&a(&["--command", "stub.exe"])),
            Some((String::new(), Some("stub.exe".into()))),
        );
        assert_eq!(parse_ssh_tmux(&[]), None);
    }

    /// M11 review regression: multi-word quoted args after `--` must survive re-joining
    /// (`claude -p "work on the auth module"` was splintered by a plain join).
    #[test]
    fn join_command_requotes_multiword_args() {
        use super::join_command;
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(join_command(&a(&["claude"])), "claude");
        assert_eq!(
            join_command(&a(&["claude", "-p", "work on the auth module"])),
            "claude -p \"work on the auth module\"",
        );
        assert_eq!(join_command(&a(&["run", "say \"hi\" now"])), "run \"say \\\"hi\\\" now\"");
        assert_eq!(join_command(&a(&["x", ""])), "x \"\"");
    }
}
