//! gmux — the entry point + CLI.
//!
//!   gmux                         open a window running the default shell (PowerShell)
//!   gmux <command line...>       open a window running that command (e.g. `gmux cmd.exe`)
//!   gmux notify --title T [--body B] [--urgency low|normal|critical]
//!                                emit an OSC 777 notification to stdout (run inside a gmux pane;
//!                                gmux attributes it to that pane and shows a toast)
//!   gmux ssh-tmux <target>       mirror a remote tmux session over ssh (tmux -CC)
//!   gmux browse <url>            open the url in the browser pane (needs a GUI built with
//!                                `--features browser`; queued in the daemon otherwise)
//!   gmux hooks setup <agent>     configure claude-code | codex | gemini | aider | all
//!   gmux shell-integration       print (or --install into $PROFILE) the PowerShell snippet
//!
//! Role dispatch (`--daemon` / more subcommands) grows with later milestones (ARCHITECTURE §3).

mod client;
mod crash;
mod hooks;
mod shell_integration;

use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => launch_gui(default_shell()),
        Some("--daemon") => daemon(),
        Some("notify") => notify(&args[1..]),
        Some("hooks") => cmd_hooks(&args[1..]),
        Some("shell-integration") => cmd_shell_integration(&args[1..]),
        Some("_hook") => internal_hook(&args[1..]),
        Some("--help" | "-h" | "help") => print_help(),
        Some(sub) => {
            // API subcommands talk to the running gmux over the pipe.
            if let Some(code) = client::dispatch(sub, &args[1..]) {
                std::process::exit(code);
            }
            // Anything else is treated as a command line to run in the GUI.
            launch_gui(args.join(" "))
        }
    }
}

fn launch_gui(shell: String) {
    crash::install("gui");
    if let Err(e) = gmux_gui::run(shell) {
        eprintln!("gmux: {e}");
        std::process::exit(1);
    }
}

/// `gmux --daemon` — run the headless multiplexer server (owns the panes; survives GUI detach).
fn daemon() {
    crash::install("daemon");
    if let Err(e) = gmux_server::run(default_shell(), "gmux") {
        eprintln!("gmux daemon: {e}");
        std::process::exit(1);
    }
}

/// `gmux notify` — emit an OSC 777 notification to stdout. When run inside a gmux pane this flows
/// through the pane's PTY and gmux attributes it to that pane (via the stream) and shows a toast.
fn notify(args: &[String]) {
    let (mut title, mut body) = (String::new(), String::new());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" => title = take(args, &mut i),
            "--body" => body = take(args, &mut i),
            "--subtitle" => {
                // Fold subtitle into the body (OSC 777 has no subtitle field).
                let s = take(args, &mut i);
                body = if body.is_empty() { s } else { format!("{s} — {body}") };
            }
            "--urgency" => {
                let _ = take(args, &mut i); // reserved; OSC 777 has no urgency field
            }
            _ => {}
        }
        i += 1;
    }
    // OSC 777 title must not contain ';' (the field separator); the body may.
    let title = osc_field(&title, true);
    let body = osc_field(&body, false);
    let seq = format!("\x1b]777;notify;{title};{body}\x07");
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// `gmux _hook claude-code` — read the Notification-event JSON on stdin and print a
/// `{"terminalSequence": "]777;notify;Claude Code;<message>"}` object that Claude Code writes to
/// the terminal (its allowlisted, race-free notification path).
fn internal_hook(args: &[String]) {
    let agent = args.first().map(String::as_str).unwrap_or("claude-code");
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let message = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "needs your attention".to_string());
    let title = match agent {
        "codex" => "Codex",
        "gemini" => "Gemini",
        _ => "Claude Code",
    };
    let seq = format!("]777;notify;{};{}", osc_field(title, true), osc_field(&message, false));
    let obj = serde_json::json!({ "terminalSequence": seq });
    println!("{obj}");
}

fn cmd_hooks(args: &[String]) {
    if args.first().map(String::as_str) != Some("setup") {
        eprintln!("usage: gmux hooks setup <claude-code|codex|gemini|aider|all>");
        std::process::exit(2);
    }
    let Some(agent) = args.get(1) else {
        eprintln!("usage: gmux hooks setup <claude-code|codex|gemini|aider|all>");
        std::process::exit(2);
    };
    let home = home_dir();
    match hooks::setup(agent, &home) {
        Ok(actions) => {
            for a in actions {
                println!("✓ {a}");
            }
        }
        Err(e) => {
            eprintln!("gmux hooks: {e}");
            std::process::exit(1);
        }
    }
}

/// `gmux shell-integration [--print|--install]` — print the PowerShell prompt snippet, or
/// install it into the CurrentUserAllHosts profile(s) (guarded by markers; safe to re-run).
fn cmd_shell_integration(args: &[String]) {
    match args.first().map(String::as_str) {
        None | Some("--print") => print!("{}", shell_integration::block()),
        Some("--install") => match shell_integration::install(&home_dir()) {
            Ok(actions) => {
                for a in actions {
                    println!("✓ {a}");
                }
            }
            Err(e) => {
                eprintln!("gmux shell-integration: {e}");
                std::process::exit(1);
            }
        },
        Some(_) => {
            eprintln!("usage: gmux shell-integration [--print|--install]");
            std::process::exit(2);
        }
    }
}

fn print_help() {
    print!(
        "\
gmux — a Windows-native terminal for AI coding agents

USAGE:
  gmux                                 open a window running the default shell
  gmux <command...>                    open a window running that command
  gmux notify --title T [--body B]     emit an OSC 777 notification (run inside a pane)
  gmux ssh-tmux <target>               mirror a remote tmux session over ssh (tmux -CC);
                                       --command <raw> overrides the transport command line
  gmux browse <url | search terms...>  open a url — or web-search free text — in the system
                                       browser (--pane targets the in-app WebView2 pane)
  gmux new-window [--cwd <dir>]        open a tab; --cwd anchors it as a workspace (every pane
                                       in it, splits included, opens in that directory)
  gmux rename -t @<win> <name...>      rename a workspace (empty name = the derived one)
  gmux close-window -t @<win>          close a workspace (no busy prompt; see `window-busy`)
  gmux workspace -t @<win> <dir>       re-anchor an existing workspace (--clear unpins it)
  gmux import <dir> [--all]            open a workspace per project folder inside <dir> (git
                                       projects only unless --all); skips ones already open
  gmux group -t @<win> <name...>       file a window under a collapsible sidebar group (--clear
                                       removes it); ids come from `gmux list-panes`
  gmux color -t @<win> #rrggbb         tag a workspace row with a color (--clear removes it)
  gmux pr -t @<win> --resolve          badge a workspace with its branch's PR (via `gh`); or set
                                       it by hand: <number> <open|draft|merged|closed> [url],
                                       --clear. Click the chip to open the PR.
  gmux wait-for -t <pane> ...          block until --text <substr> appears, the pane --exit s,
                                       or its screen is --idle <secs>; [--timeout <secs>]
  gmux screenshot -t <pane> [-o F.bmp] render the pane's live grid to an image (headless GPU)
  gmux hooks setup <agent>             configure claude-code | codex | gemini | aider | all
  gmux shell-integration [--install]   print (or install into $PROFILE) the PowerShell snippet
  gmux --help                          show this help
"
    );
}

// --- helpers ---

fn take(args: &[String], i: &mut usize) -> String {
    *i += 1;
    args.get(*i).cloned().unwrap_or_default()
}

/// Strip control chars from an OSC field; if `is_title`, also replace ';' (the separator) with ','.
fn osc_field(s: &str, is_title: bool) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .map(|c| if is_title && c == ';' { ',' } else { c })
        .collect()
}

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn default_shell() -> String {
    if which("pwsh.exe") {
        return "pwsh.exe -NoLogo".into();
    }
    let win_ps = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
    if std::path::Path::new(win_ps).exists() {
        return format!("{win_ps} -NoLogo");
    }
    "cmd.exe".into()
}

fn which(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(exe).exists()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::osc_field;

    #[test]
    fn osc_field_strips_controls_and_title_semicolons() {
        assert_eq!(osc_field("a;b\x07c\nd", true), "a,bcd");
        assert_eq!(osc_field("a;b\x07c\nd", false), "a;bcd"); // body keeps ';'
    }
}
