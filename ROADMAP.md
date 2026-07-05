# gmux — Roadmap

> Milestones build vertically: each ends with a runnable build, a demo script, and tests.
> Numbers are sequencing, not calendar promises. The notification hooks are front-loaded by design —
> M0 proves them end-to-end before any product code, and M2 productizes them right after the terminal
> core exists.

## MVP

### M0 — De-risking spikes ✅ COMPLETE (2026-07-05)

**Results in [docs/research/m0-spikes.md](docs/research/m0-spikes.md).** Killer feature proven:
OSC 9/777/99 pass through a real ConPTY intact and in order (`spikes/conpty_osc`, verdict GO). Unpackaged
registry-AUMID toasts confirmed (`spikes/toast`). VT-core decision resolved: libghostty-vt rejected →
`alacritty_terminal` + side vte OSC parser (`spikes/ghostty_vt`). Exit gate met → proceeding to M1.

The four architecture-shaping unknowns, each a standalone spike under `spikes/`:

1. **ConPTY round-trip with the bundled pair** — load `conpty.dll`/`OpenConsole.exe` from the
   `Microsoft.Windows.Console.ConPTY` NuGet, run PowerShell, prove read/write/resize/teardown-drain.
2. **OSC passthrough proof** — inside that pane, `printf '\e]9;hi\a'`, `\e]777;…`, `\e]99;…`; assert all
   three arrive intact and in order at the host parser. **This is the killer-feature go/no-go.**
3. **Toast from an unpackaged exe** — registry-only AUMID registration, `Show()`, click → in-process
   `Activated` callback with arguments, `History.Remove`. Measure the foreground-rights behavior.
4. **libghostty-vt behavior spike** — x64/ARM64 `cargo build` is already proven by the crate's own
   Windows CI (Zig 0.15.2); what CI doesn't do is *run tests* on Windows. Exercise OSC dispatch, resize
   reflow, and grapheme widths against a corpus → resolves ADR-003.

**Exit gate:** hooks proven end-to-end in a harness (OSC in a real ConPTY pane → Windows toast on screen);
ADR-003 decided. *Tests: the passthrough assertion becomes a permanent CI integration test.*

### M1 — Terminal core: one pane, four shells, correct

- `gmux-pty` + `gmux-vt` + minimal `gmux-gui` (winit + wgpu DX12 + glyphon): a single pane that runs
  PowerShell 7/5, cmd, Git Bash, and WSL bash **correctly** — colors, cursor, scrollback, wide
  chars/emoji, UTF-8 split reads, resize with debounce, DSR-CPR/DA1 replies, win32-input-mode, mouse.
- mux-core state model (`gmux-mux`) exists as a crate from day 1 (in-process; no daemon yet).
- *Demo:* run `claude` in the pane, watch it behave identically to Windows Terminal.
  *Tests:* VT corpus units; ConPTY integration matrix (per shell); vttest subset.

### M2 — Notification hooks, productized (the killer feature)

- Attention state machine in `gmux-mux`; `gmux-notify` toast layer (AUMID, XML, activation, History);
  FlashWindowEx + ITaskbarList3 overlay badge + OSC 9;4 → taskbar progress; pane ring + unread badge in
  the GUI; suppression matrix + clear-on-focus + rate limiting; BEL + OSC 133 idle detection.
- Minimal pipe stub so `gmux notify` works (full API comes in M5); `GMUX_PANE` env injection.
- `gmux hooks setup claude-code|codex|gemini|aider`.
- *Demo:* three agents in three panes; each firing OSC 9/777/99 rings its pane and toasts when gmux is
  unfocused; click the toast → the right pane focuses. *Tests:* the standing killer-feature integration
  suite (§14.3 of ARCHITECTURE.md).

### M3 — Splits and multiplexing UI

- Binary split tree: horizontal/vertical splits, keyboard-driven focus movement + resize, zoom,
  close-with-tree-kill (job objects). Multiple windows (tabs).
- *Demo:* 2×2 grid of agents, keyboard-only navigation. *Tests:* layout-tree units, input routing.

### M4 — Workspaces and the sidebar

- Vertical sidebar: per-workspace git branch, cwd (OSC 7/9;9 + PEB fallback), listening ports
  (job-object PID → `GetExtendedTcpTable`), latest-notification text + unread badge; jump-to-unread.
- `gmux set-status/set-progress/log` sidebar metadata (cmux parity).
- *Demo:* three repos, three workspaces, live branch/port/notification state at a glance.

### M5 — Programmability: the pipe API and CLI

- Full `\\.\pipe\gmux` JSON-RPC server + `gmux` CLI: list/new/attach sessions, new-window, split-pane,
  **send-keys, capture-pane (with scrollback ranges + SGR), screenshot**, wait-for, list-panes with
  `#{}` formats, subscribe (event stream), notify/set-status.
- *Demo:* a PowerShell script that creates a workspace, splits it, launches an agent, waits for
  `pane-attention`, captures the screen, and screenshots the pane — no gmux UI touched.
  *Tests:* protocol golden files; PowerShell smoke client in CI.

### M6 — Detach/reattach: the daemon split

- mux-core moves behind `gmux.exe --daemon`; GUI becomes a pipe client (binary damage side-channel);
  close the GUI → agents keep running; `gmux attach` (or relaunching the GUI) restores the picture;
  toast-click-after-GUI-exit relaunches and attaches to the right pane.
- *Demo:* start a long agent task, close the GUI, reopen — scrollback and the running process are intact.
  *Tests:* reattach state-sync; daemon crash → job objects reap children; GUI crash → daemon unaffected.

### M7 — Session restore across reboot

- Debounced checkpoints (layout + cwd + spawn info + zstd VT scrollback snapshots + attention state);
  restore-on-launch with inert-history replay + divider; per-agent resume commands behind approval UI;
  secret scrubbing.
- *Demo:* reboot the machine; relaunch gmux; every workspace/pane/cwd/scrollback is back, agents offer
  to resume. *Tests:* checkpoint→restore round-trip equality; snapshot-corruption tolerance.

### M8 — MVP hardening and release

- x64+ARM64 CI matrix, code signing, installer (plus portable zip), first-run experience
  (shell-integration snippets, `hooks setup` prompt), docs site, crash reporting (opt-in, local dumps).
- **MVP definition of done:** a developer on Windows 11 runs three parallel Claude Code sessions in three
  workspaces with splits, gets a toast + pane ring the moment any agent needs input, scripts
  send-keys/capture-pane over the pipe from an external tool, detaches and reattaches, and has everything
  restored after a reboot.

## v1

### M9 — Remote tmux (control-mode client)

- `gmux ssh-tmux user@host`: spawn `ssh … tmux -CC attach`, parse control mode (%begin/%end, %output
  octal-unescape, %layout-change, pause-based flow control), map session→session/window→window/pane→pane,
  bidirectional (split/send-keys/paste); tmux ≥3.2 gate with degraded mode below.

### M10 — Keybindings & configuration polish

- Full rebindable action map in `gmux.json` (no-prefix defaults), config hot-reload, profile editor UI,
  theme support (import Windows Terminal / iTerm color schemes).

### M11 — Agent orchestration surfacing

- When an agent spawns teammates/subagents (Claude Code teams-style), surface them as real panes/splits:
  detection via `gmux`-aware hooks (`gmux split-pane -- claude …` recipes + `subscribe` integration),
  fleet overview in the sidebar (aggregate attention/progress).

### M12 — Browser pane (flag-gated)

- WebView2 split pane, scriptable over the same pipe (navigate/snapshot/click/type/eval/console/network)
  — cmux's second differentiator, explicitly lower priority than terminal correctness.

## Standing (every milestone)

- Runnable build + short demo script (`demos/mX.ps1`) + tests as listed.
- DECISIONS.md entry for anything non-obvious decided during the milestone.
- The M0 passthrough + killer-feature integration tests stay green on the CI matrix (x64, ARM64;
  Win10 21H2 VM, Win11 latest).
