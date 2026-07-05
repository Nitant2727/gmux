//! `gmux shell-integration` — PowerShell profile snippet so shells report state to gmux.
//!
//! The snippet wraps the user's `prompt` function to emit, on every prompt (only when
//! `$env:TERM_PROGRAM -eq 'gmux'`):
//! - **OSC 133;A** — prompt start, so gmux can mark command boundaries.
//! - **OSC 9;9;"<cwd>"** — cwd report, so panes track the working directory.
//!
//! `--install` appends the snippet to the PowerShell profile (CurrentUserAllHosts), guarded by
//! marker comments so re-running replaces the block instead of duplicating it. The functions take
//! explicit paths so they're unit-testable against a temp directory.

use std::io;
use std::path::{Path, PathBuf};

use crate::hooks::{read_or_default, write_atomic};

pub const BEGIN_MARKER: &str = "# >>> gmux shell integration >>>";
pub const END_MARKER: &str = "# <<< gmux shell integration <<<";

/// The PowerShell snippet body (between the markers). Wraps the existing prompt and preserves
/// its output; the `__gmux_prompt` guard keeps a double-sourced profile from wrapping twice.
const SNIPPET: &str = r#"if ($env:TERM_PROGRAM -eq 'gmux' -and -not $global:__gmux_prompt) {
    $global:__gmux_prompt = $function:prompt
    function global:prompt {
        Write-Host -NoNewline "$([char]27)]133;A$([char]7)"
        Write-Host -NoNewline "$([char]27)]9;9;`"$($PWD.Path)`"$([char]7)"
        & $global:__gmux_prompt
    }
}"#;

/// The full marker-guarded block, as printed and as installed into profiles.
pub fn block() -> String {
    format!("{BEGIN_MARKER}\n{SNIPPET}\n{END_MARKER}\n")
}

/// Install the snippet into the CurrentUserAllHosts profiles under `home`: both
/// `Documents/PowerShell` (PowerShell 7) and `Documents/WindowsPowerShell` (5.1) if the dirs
/// exist; if neither exists, create the PowerShell 7 one. Returns what was changed.
pub fn install(home: &Path) -> io::Result<Vec<String>> {
    let docs = home.join("Documents");
    let dirs = [docs.join("PowerShell"), docs.join("WindowsPowerShell")];
    let mut profiles: Vec<PathBuf> =
        dirs.iter().filter(|d| d.exists()).map(|d| d.join("profile.ps1")).collect();
    if profiles.is_empty() {
        profiles.push(dirs[0].join("profile.ps1"));
    }
    profiles.iter().map(|p| install_into(p)).collect()
}

/// Idempotently install the snippet into the profile at `path`: replace an existing
/// marker-guarded block in place, else append one (creating the file if missing).
pub fn install_into(path: &Path) -> io::Result<String> {
    let existing = read_or_default(path)?;
    let (out, action) = match (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        (Some(start), Some(end)) if end >= start => {
            // Replace the old block (through the end-marker line, newline included).
            let mut after = end + END_MARKER.len();
            while existing[after..].starts_with(['\r', '\n']) {
                after += 1;
            }
            (format!("{}{}{}", &existing[..start], block(), &existing[after..]), "replaced")
        }
        _ => {
            let mut out = existing;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&block());
            (out, "added")
        }
    };
    write_atomic(path, out.as_bytes())?;
    Ok(format!("shell-integration: {action} snippet in {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("gmux-shellint-test-{}", std::process::id()));
        let unique = base.join(format!("{:?}", std::time::SystemTime::now()).replace([' ', ':'], "_"));
        fs::create_dir_all(&unique).unwrap();
        unique
    }

    #[test]
    fn snippet_emits_osc_markers_only_inside_gmux() {
        let b = block();
        assert!(b.contains("$([char]27)]133;A$([char]7)"), "{b}");
        assert!(b.contains("$([char]27)]9;9;"), "{b}");
        assert!(b.contains("$env:TERM_PROGRAM -eq 'gmux'"), "{b}");
    }

    #[test]
    fn install_into_is_idempotent() {
        let profile = tmp_home().join("profile.ps1");
        let first = install_into(&profile).unwrap();
        assert!(first.contains("added"), "{first}");
        let second = install_into(&profile).unwrap();
        assert!(second.contains("replaced"), "{second}");
        let out = fs::read_to_string(&profile).unwrap();
        assert_eq!(out.matches(BEGIN_MARKER).count(), 1, "block must not be duplicated: {out}");
        assert_eq!(out.matches(END_MARKER).count(), 1, "{out}");
    }

    #[test]
    fn install_into_replaces_stale_block_and_preserves_rest() {
        let profile = tmp_home().join("profile.ps1");
        fs::write(
            &profile,
            format!("Set-Alias g git\n{BEGIN_MARKER}\n# old snippet\n{END_MARKER}\noh-my-posh init\n"),
        )
        .unwrap();
        install_into(&profile).unwrap();
        let out = fs::read_to_string(&profile).unwrap();
        assert!(!out.contains("# old snippet"), "stale block must be replaced: {out}");
        assert!(out.contains("]133;A"), "{out}");
        assert!(out.contains("Set-Alias g git"), "must preserve content before: {out}");
        assert!(out.contains("oh-my-posh init"), "must preserve content after: {out}");
    }

    #[test]
    fn install_creates_pwsh7_profile_when_no_dirs_exist() {
        let home = tmp_home();
        let actions = install(&home).unwrap();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(home.join("Documents").join("PowerShell").join("profile.ps1").exists());
    }

    #[test]
    fn install_targets_both_profiles_when_dirs_exist() {
        let home = tmp_home();
        let docs = home.join("Documents");
        fs::create_dir_all(docs.join("PowerShell")).unwrap();
        fs::create_dir_all(docs.join("WindowsPowerShell")).unwrap();
        let actions = install(&home).unwrap();
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert!(docs.join("PowerShell").join("profile.ps1").exists());
        assert!(docs.join("WindowsPowerShell").join("profile.ps1").exists());
    }
}
