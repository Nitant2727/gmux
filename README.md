# gmux

**A Windows-native, GPU-accelerated terminal multiplexer built for running many AI coding agents in
parallel.** gmux is the independent Windows equivalent of [cmux](https://cmux.com) (macOS): vertical-tab
workspaces, tmux-style splits and detach/reattach, reboot-surviving sessions, a scriptable named-pipe
API — and, above all, **notification hooks that actually work**: any agent that emits an OSC 9/777/99
escape (Claude Code, Codex CLI, Aider, Gemini CLI, …) lights up its pane and fires a real Windows toast
the moment it needs you.

> **Status: architecture proposed, awaiting review. No feature code yet.**
> The project is in its docs-first phase per the agreed working method.

## What makes gmux different

No open-source, Windows-native, non-Electron terminal today does all of this at once:

- **Notifications that work** — OSC 9 / OSC 777 / OSC 99 → Windows toast + persistent per-pane attention
  ring + taskbar badge, suppressed when you're already looking, cleared when you focus the pane. Windows
  Terminal abandoned its toast PR; WezTerm's Windows toasts are buggy; Warp is closed and uses its own
  framework. This is gmux's core wedge, and it works with *any* tool via standard escapes — no SDK.
- **Real multiplexing** — sessions/windows/panes, splits, and true **detach/reattach** with a per-user
  daemon owning the ConPTYs, so agents survive closing the GUI. tmux doesn't run natively on Windows;
  gmux builds the model in-app.
- **Programmable** — a local named-pipe API (`\\.\pipe\gmux`) and tmux-style CLI: `send-keys`,
  `capture-pane`, `screenshot`, `wait-for`, `subscribe`. Orchestrators drive gmux from outside.
- **Native & open** — Rust + `wgpu`, ConPTY, no Electron for the grid; MIT-licensed; x64 + ARM64.

## Documents

| Doc | What's in it |
|---|---|
| [ARCHITECTURE.md](ARCHITECTURE.md) | The design: stack ADR (Rust), ConPTY integration, the OSC→toast notification pipeline, mux/daemon model, IPC, persistence, renderer, security, testing, risks |
| [ROADMAP.md](ROADMAP.md) | MVP → v1 milestones (M0 spikes → terminal core → hooks → splits → sidebar → CLI/API → detach → restore → remote tmux → browser) |
| [DECISIONS.md](DECISIONS.md) | The ADR log — every non-obvious decision, one entry each |
| [docs/research/](docs/research/) | The evidence corpus: eight web-verified deep-dives + the adversarial [verification log](docs/research/verification.md) |

## Priorities (in order)

1. **A correct VT/ConPTY terminal core** — everything else is worthless if the terminal is wrong.
2. **Notification hooks** — the killer feature, front-loaded and non-negotiable.
3. **Native tmux-style multiplexing** — sessions, splits, detach/reattach, restore.
4. **Programmability** — CLI + named-pipe API for agents and scripts.

## Platform

Windows 10 21H2+ and Windows 11, x64 and ARM64. ConPTY (`CreatePseudoConsole`) for the PTY layer,
GPU glyph rendering, no Electron in the terminal grid.

## License

MIT (planned). gmux studies cmux's behavior only — cmux is GPL-3.0 and no code is copied.
