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
        ResultBody::Layout(_) | ResultBody::Grid(_) => {} // render data — not for the CLI
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

/// Entry: dispatch `gmux <subcommand> ...` API calls. Returns an exit code.
pub fn dispatch(cmd: &str, args: &[String]) -> Option<i32> {
    match cmd {
        "list-panes" => Some(run(Call::ListPanes)),
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
                eprintln!("usage: gmux capture-pane -t <pane>");
                return Some(2);
            };
            Some(run(Call::CapturePane { pane }))
        }
        "split-pane" => {
            let dir = if args.iter().any(|a| a == "-v") { "v" } else { "h" }.to_string();
            let command = args.iter().position(|a| a == "--").map(|i| args[i + 1..].join(" ")).filter(|s| !s.is_empty());
            Some(run(Call::SplitPane { dir, command }))
        }
        "new-window" => {
            let command = args.iter().position(|a| a == "--").map(|i| args[i + 1..].join(" ")).filter(|s| !s.is_empty());
            Some(run(Call::NewWindow { command }))
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
}
