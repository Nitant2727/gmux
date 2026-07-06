# Running AI agents in gmux

gmux is built for running multiple coding agents (Claude Code, Codex, Aider, Gemini CLI) in
parallel panes, with toasts the moment any of them needs input. This page is the practical
recipes.

## One-time setup

```powershell
# Wire agent notification hooks (writes each agent's config; idempotent):
gmux hooks setup all          # or: claude-code | codex | gemini | aider

# Optional: prompt marks + cwd tracking in PowerShell (idempotent profile block):
gmux shell-integration --install
```

With hooks set up, an agent that needs input raises a Windows toast and a pane attention ring —
click the toast to focus gmux.

## Panes for agents

Inside gmux, split and run an agent per pane (defaults: `Ctrl+Shift+D` beside, `Ctrl+Shift+E`
below — rebindable in `%APPDATA%\gmux\gmux.json`). From any terminal or script, the same over
the automation pipe:

```powershell
gmux split-pane -- claude                 # new pane beside the active one, running Claude Code
gmux split-pane -v -- codex               # stacked below
gmux new-window -- claude                 # a whole tab per agent / per repo
gmux list-panes                           # %id, window, size, flags, cwd, title
```

## Scripting agents

Every pane is addressable (`%N` from `list-panes`; agents also see their own id as `GMUX_PANE`):

```powershell
gmux send-keys -t %2 "fix the failing tests" --enter
gmux capture-pane -t %2                   # visible screen
gmux capture-pane -t %2 -S -              # full scrollback (or -S 200 for the last 200 lines)
```

`send-keys` + `capture-pane` is enough to drive an agent from another agent — an orchestrator
pane can spawn teammates and poll their output.

## Subagent panes (teammates as real panes)

Have your agent's spawn hook shell out to gmux so subagents appear as first-class panes instead
of hidden processes:

```powershell
# e.g. in a Claude Code hook or wrapper script:
gmux split-pane -- claude -p "work on the auth module"
```

Each teammate then gets its own attention ring, toasts, and scrollback.

## Fleet overview

The sidebar aggregates each tab's state: an attention dot when any pane wants input, and the
agents' OSC 9;4 progress — ` 42%` shows the least-done active agent in that tab, ` !` means one
reported an error. `gmux notify --title "..." --body "..."` raises a toast from inside any pane
(attributed to that pane).

## Remote agents

Mirror a remote tmux session (agents running on a server) as native gmux tabs:

```powershell
gmux ssh-tmux user@host                   # runs: ssh -tt user@host -- tmux -CC new -As gmux
```

Remote panes are first-class: keystrokes round-trip, splits/closes go to the remote tmux, and a
remote agent's OSC 777/9;4 raises the same toasts and progress as a local one.
