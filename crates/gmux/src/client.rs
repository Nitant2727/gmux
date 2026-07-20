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
            let Some(url) = args.iter().find(|a| !a.starts_with('-')).cloned() else {
                eprintln!("usage: gmux browse <url>");
                return Some(2);
            };
            Some(run(Call::Browse { url }))
        }
        _ => None, // not an API subcommand
    }
}

#[cfg(test)]
mod tests {
    use super::parse_pane;

    #[test]
    fn pane_targets_parse() {
        assert_eq!(parse_pane("%5"), Some(5));
        assert_eq!(parse_pane("5"), Some(5));
        assert_eq!(parse_pane("nope"), None);
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
