//! Per-window "workspace" metadata for the sidebar: git branch, working directory, and attention.
//! Kept lightweight — the git branch is read straight from `.git/HEAD` (no git process, no deps).

use std::path::{Path, PathBuf};

/// A pull request's state, as the sidebar badges it. Mirrors cmux's `PullRequestStatus` plus a
/// `Draft` variant (cmux distinguishes drafts too). The daemon never queries GitHub — this is
/// pushed in via `gmux pr` (which optionally shells `gh` in the short-lived CLI process), keeping
/// the no-timers/0%-idle-CPU invariant intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

impl PrStatus {
    /// Parse a wire / CLI token (`open`/`draft`/`merged`/`closed`, any case). `None` otherwise.
    pub fn parse(s: &str) -> Option<PrStatus> {
        match s.trim().to_ascii_lowercase().as_str() {
            "open" => Some(PrStatus::Open),
            "draft" => Some(PrStatus::Draft),
            "merged" => Some(PrStatus::Merged),
            "closed" => Some(PrStatus::Closed),
            _ => None,
        }
    }

    /// The canonical token (round-trips with [`parse`]).
    pub fn as_str(self) -> &'static str {
        match self {
            PrStatus::Open => "open",
            PrStatus::Draft => "draft",
            PrStatus::Merged => "merged",
            PrStatus::Closed => "closed",
        }
    }

    /// Map GitHub's `gh pr view` output to a status: its `state` is `OPEN`/`MERGED`/`CLOSED`, and an
    /// open PR with `isDraft: true` is a draft. `None` for an unrecognized state.
    pub fn from_github(state: &str, is_draft: bool) -> Option<PrStatus> {
        match state.trim().to_ascii_uppercase().as_str() {
            "OPEN" => Some(if is_draft { PrStatus::Draft } else { PrStatus::Open }),
            "MERGED" => Some(PrStatus::Merged),
            "CLOSED" => Some(PrStatus::Closed),
            _ => None,
        }
    }
}

/// A pull request badge for a workspace: its number and state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrBadge {
    pub number: u32,
    pub status: PrStatus,
}

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
    /// Notifications across the window's panes since each was last focused — the sidebar badges
    /// this count, so "one agent finished" and "nine did" don't look identical.
    pub unread: u32,
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
    fn pr_status_round_trips_and_maps_github_states() {
        for s in [PrStatus::Open, PrStatus::Draft, PrStatus::Merged, PrStatus::Closed] {
            assert_eq!(PrStatus::parse(s.as_str()), Some(s), "{s:?} must round-trip");
        }
        assert_eq!(PrStatus::parse("OPEN"), Some(PrStatus::Open), "case-insensitive");
        assert_eq!(PrStatus::parse(" merged "), Some(PrStatus::Merged), "whitespace tolerated");
        assert_eq!(PrStatus::parse("bogus"), None);
        assert_eq!(PrStatus::parse(""), None);

        // GitHub reports OPEN + isDraft for drafts; everything else maps straight across.
        assert_eq!(PrStatus::from_github("OPEN", false), Some(PrStatus::Open));
        assert_eq!(PrStatus::from_github("OPEN", true), Some(PrStatus::Draft));
        assert_eq!(PrStatus::from_github("MERGED", false), Some(PrStatus::Merged));
        // A merged PR is never a draft, so the flag must not override the state.
        assert_eq!(PrStatus::from_github("MERGED", true), Some(PrStatus::Merged));
        assert_eq!(PrStatus::from_github("CLOSED", false), Some(PrStatus::Closed));
        assert_eq!(PrStatus::from_github("SOMETHING_NEW", false), None);
    }

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
