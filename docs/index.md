# gmux

A Windows-native, GPU-accelerated terminal multiplexer built for running multiple AI coding
agents (Claude Code, Codex, Aider, Gemini CLI) in parallel — an independent Windows equivalent
of cmux.

**The killer feature:** the moment any agent needs input, gmux raises a Windows toast and a pane
attention ring — OSC 9 / 777 / 99 notifications flow from local ConPTY panes *and* remote tmux
mirrors through the same pipeline.

- **Terminal core:** ConPTY + alacritty-based VT parsing, wgpu (DX12) glyph rendering.
- **Multiplexing:** native splits/tabs/zoom, daemon-owned panes (detach/reattach), session
  restore across reboots with scrollback replay.
- **Programmability:** `\\.\pipe\gmux.<user>` JSON API — `send-keys`, `capture-pane -S`,
  `split-pane -- <agent>`, `ssh-tmux`, `browse` — plus a full CLI.
- **Remote:** mirror a remote tmux session (≥ 3.2) over ssh as native tabs.

Start with [Running AI agents in gmux](agents.md). Architecture, roadmap, and decision log live
in the repository root ([ARCHITECTURE.md](https://github.com/Nitant2727/gmux/blob/main/ARCHITECTURE.md),
[ROADMAP.md](https://github.com/Nitant2727/gmux/blob/main/ROADMAP.md),
[DECISIONS.md](https://github.com/Nitant2727/gmux/blob/main/DECISIONS.md)); the research
deep-dives that shaped them are in this book's Research section.
