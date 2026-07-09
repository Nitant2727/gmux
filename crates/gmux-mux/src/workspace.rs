//! Per-window "workspace" metadata for the sidebar: git branch, working directory, and attention.
//! Kept lightweight — the git branch is read straight from `.git/HEAD` (no git process, no deps).

use std::path::{Path, PathBuf};

/// A snapshot of a window's sidebar metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// Short display name (the cwd's last component, or the window title).
    pub name: String,
    /// Working directory of the active pane, if known.
    pub cwd: Option<String>,
    /// Current git branch, if the cwd is in a git repo.
    pub branch: Option<String>,
    /// Any pane in the window is requesting attention.
    pub attention: bool,
}

/// Read the current git branch for `cwd` by walking up to a `.git` dir and parsing `HEAD`.
/// Returns the branch name, a short detached-HEAD hash, or `None` if not a repo.
pub fn git_branch(cwd: &Path) -> Option<String> {
    let git_dir = find_git_dir(cwd)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(rest) = head.strip_prefix("ref: ") {
        // e.g. "ref: refs/heads/main" -> "main"
        Some(rest.rsplit('/').next().unwrap_or(rest).to_string())
    } else if head.len() >= 7 {
        // Detached HEAD: a raw commit hash.
        Some(format!("({})", &head[..7]))
    } else {
        None
    }
}

/// Walk up from `start` looking for a `.git` directory (or file, for worktrees).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            // A `.git` file (worktree/submodule): "gitdir: <path>".
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Some(p) = content.trim().strip_prefix("gitdir: ") {
                    return Some(PathBuf::from(p));
                }
            }
        }
        dir = d.parent();
    }
    None
}

/// The short display name for a working directory (its last path component).
pub fn cwd_name(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['\\', '/']);
    trimmed.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(trimmed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_from_head_ref() {
        let dir = std::env::temp_dir().join(format!("gmux-ws-{}-{}", std::process::id(), line!()));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/foo\n").unwrap();
        assert_eq!(git_branch(&dir), Some("foo".to_string()));
    }

    #[test]
    fn detached_head_shows_short_hash() {
        let dir = std::env::temp_dir().join(format!("gmux-ws-{}-{}", std::process::id(), line!()));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(git_branch(&dir), Some("(0123456)".to_string()));
    }

    #[test]
    fn no_repo_returns_none() {
        let dir = std::env::temp_dir().join(format!("gmux-ws-none-{}-{}", std::process::id(), line!()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(git_branch(&dir), None);
    }

    #[test]
    fn cwd_name_takes_last_component() {
        assert_eq!(cwd_name(r"C:\Workspace\gmux"), "gmux");
        assert_eq!(cwd_name(r"C:\Workspace\gmux\"), "gmux");
        assert_eq!(cwd_name("/home/user/proj"), "proj");
    }
}
