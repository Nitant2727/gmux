//! gmux — the entry point. For M1 it opens a single window running a shell. Role dispatch
//! (GUI / `--daemon` / CLI subcommands) arrives with the later milestones (ARCHITECTURE §3).
//!
//! Usage:
//!   gmux                       open a window running the default shell (PowerShell)
//!   gmux <command line...>     open a window running that command (e.g. `gmux cmd.exe`)

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let shell = if args.is_empty() { default_shell() } else { args.join(" ") };

    if let Err(e) = gmux_gui::run(shell) {
        eprintln!("gmux: {e}");
        std::process::exit(1);
    }
}

/// Prefer PowerShell 7 (`pwsh`) if on PATH, else Windows PowerShell, else cmd.
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

/// True if `exe` is found on PATH.
fn which(exe: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(exe).exists()))
        .unwrap_or(false)
}
