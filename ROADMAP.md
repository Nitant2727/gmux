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

### M1 — Terminal core: one pane, real shells, correct ✅ COMPLETE (2026-07-05)

Cargo workspace `crates/gmux-{pty,vt,mux,gui,gmux}`, 45 tests green + 5 console-gated ConPTY
integration tests (run via `scripts/console-tests.ps1`). The `gmux` GUI launches, opens a window,
spawns a shell, and renders. Delivered:
- **`gmux-pty`** — `Pty` over ConPTY (Job-Object kill-on-close, reader→channel, resize, `ensure_console`).
- **`gmux-vt`** — `alacritty_terminal` grid + side `vte` OSC parser → `TermEvent`s (OSC 9/777/99/9;4/7/133).
- **`gmux-mux`** — `Pane` (Pty+Terminal+pump→attention), `$/@/%` ids, Session/Window/Mux tree.
- **`gmux-gui`** — wgpu 30 (DX12) renderer: monospace glyph atlas + bg/glyph pipelines + cursor + attention
  ring, verified by **offscreen pixel-readback tests**; winit window + keyboard input + resize.
- **`gmux`** bin — opens a window running the default shell (PowerShell).

Deferred from M1 to later milestones (tracked): glyphon/complex-shaping + emoji/wide chars (custom atlas is
ASCII for now), win32-input-mode + mouse (basic key mapping done), damage-tracked rendering (full redraw
now), scrollback viewport (visible grid only), Git Bash/WSL shell matrix pass. *Next:* M2 (toasts —
attention→`Pending` path already proven; productize the WinRT toast from the M0 spike).

### M2 — Notification hooks, productized (the killer feature) ✅ COMPLETE (2026-07-05)

- **`gmux-notify`** (built + verified by a workflow; a real toast fired on the live desktop): registry-AUMID
  unpackaged toasts (sanitize + XML-escape, tag/group replace-in-place, urgent scenario, History clear,
  in-proc click activation queue), `flash_window` (FlashWindowEx), `Taskbar` progress (ITaskbarList3).
- **App wiring** (`gmux-gui`): pane attention → toast + flash (suppressed when focused), OSC 9;4 → taskbar
  progress, clear-on-focus (toast removed + flash stopped), click-to-focus, per-pane 1 s rate limit; pane
  ring already renders (M1).
- **`GMUX_PANE` env injection** into every pane (+ `TERM_PROGRAM=gmux`, `COLORTERM`).
- **`gmux notify --title --body`** emits OSC 777 to stdout (pane-attributed via the PTY stream, no pipe).
- **`gmux hooks setup claude-code|codex|gemini|aider|all`** merges agent configs (idempotent, preserves
  existing); **`gmux _hook claude-code`** turns a Notification event into Claude Code's `terminalSequence`.

Deferred: OSC 133 idle→attention (BEL covered); overlay-icon count badge (flash+progress done); multi-pane
toast attribution refinements land with M3 splits. *Next:* M3 (splits).

### M3 — Splits and multiplexing UI ✅ COMPLETE (2026-07-05)

- `gmux-mux` binary split tree ([layout.rs](crates/gmux-mux/src/layout.rs)): split/collapse, spatial
  focus (`neighbor`), ratio resize, zoom, windows (tabs) — 11 unit tests. `Window` = pane HashMap +
  split-tree `Node` + active + zoom; `Session` = windows + tabs; `remove_pane` collapses on exit.
- `gmux-gui` multi-pane rendering: `Renderer::render_panes` draws each pane into its viewport in one
  pass, with an active-pane border + per-pane attention ring. App holds a `Session`; input routes to the
  active pane; panes resize to their rects.
- Keybindings: **Ctrl+Shift+D/E** split side-by-side / stacked, **Alt+Arrows** focus, **Ctrl+Shift+Arrows**
  resize, **Ctrl+Shift+Z** zoom, **Ctrl+Shift+W** close pane, **Ctrl+Shift+T** new tab, **Ctrl+Shift+N/P**
  switch tab. Pane process exit collapses the split (job-object tree-kill from M1).
- *Verified:* layout units + offscreen render + GUI launch smoke. *Next:* M4 (sidebar/workspaces).

### M4 — Workspaces and the sidebar ✅ CORE COMPLETE (2026-07-05)

- `gmux-mux` [workspace.rs](crates/gmux-mux/src/workspace.rs): `git_branch` (reads `.git/HEAD`, handles
  refs + detached + worktree `.git` files, no deps), `cwd_name`, `WorkspaceInfo`; `Window::workspace_info()`
  aggregates active-pane cwd → branch + any-pane attention. 4 tests.
- `gmux-gui` vertical sidebar (`Renderer::render_frame` + `build_sidebar` + `text_run`): a left column of
  one row per window (tab) showing name, `git:<branch>`, an attention dot, and active-row highlight; panes
  render into the remaining content area.
- Deferred: listening ports (job-object PID → `GetExtendedTcpTable`), `gmux set-status/log` sidebar
  metadata, jump-to-unread. *Next:* M5 (named-pipe API + full CLI).

### M5 — Programmability: the pipe API and CLI ✅ CORE COMPLETE (2026-07-05)

- **`gmux-proto`**: newline-delimited JSON protocol (D-005 amended), `hello/list-panes/send-keys/
  capture-pane/split-pane/new-window/notify`, 1 MiB line cap, 5 tests.
- **`gmux-pipe`** (workflow-built + adversarially verified): blocking named-pipe server/client,
  thread-per-connection, **DACL locked to SYSTEM+current-user (verified by ACL read-back test)**,
  REJECT_REMOTE_CLIENTS, FIRST_PIPE_INSTANCE, ERROR_NO_DATA race fixed, `try_clone`; 9 tests.
- **App bridge** (`gmux-gui/api.rs`): pipe threads → command channel → `EventLoopProxy` wake →
  main-thread execution against the Session; 5 s reply timeout.
- **CLI client**: `gmux hello|list-panes|send-keys -t %N --enter <text>|capture-pane -t %N|`
  `split-pane [-h|-v] [-- cmd]|new-window [-- cmd]`.
- **End-to-end verified from an external process**: split → send-keys → capture-pane round-trip read
  back live screen contents. Demo: [demos/m5.ps1](demos/m5.ps1).
- Deferred: scrollback ranges + SGR in capture-pane, screenshot, wait-for, subscribe event stream,
  `#{}` formats, session verbs (attach/detach land with M6 daemon). *Next:* M6 (detach/daemon).

### M6 — Detach/reattach: the daemon split 🔶 STAGE 1 COMPLETE (2026-07-05)

- **Stage 1 ✅** — `gmux-server` crate: headless `Server { session, shell }` owns the mux + ConPTYs and
  serves the protocol; `gmux --daemon` runs it (blocks until all panes exit). **Verified end-to-end:** the
  daemon process owns a real pane and the CLI drives it (hello/list-panes/split/send-keys/capture) with no
  GUI — console-gated integration test + live daemon+CLI smoke.
- **Stage 2 (next)** — rewire the GUI as a thin client: add `GetLayout`/`GetGrid`/`resize` protocol
  methods (grid streaming), make the GUI attach to the daemon (auto-spawn if absent) and render remote
  state instead of owning a Mux; close GUI → daemon+agents keep running; reopen → reattach.
- *Tests so far:* daemon serves protocol headlessly; job-object tree-kill from M1 covers child reaping.

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
