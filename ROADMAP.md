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

### M6 — Detach/reattach: the daemon split ✅ COMPLETE (2026-07-05)

- **`gmux-server`** — headless `Server` owns the mux + ConPTYs; `gmux --daemon` runs it (drains pane
  events each 100 ms via `tick`, removes exited panes, queues notifications; stops when all panes exit).
- **Protocol** (`gmux-proto`): grid/layout streaming (`GetLayout`/`GetGrid`/`ResizeView`), pane control
  (`FocusPane`/`ClosePane`/`ToggleZoom`/`SwitchWindow`), and `PollNotifications`; wire cell/grid/layout types.
- **GUI is now a thin client** (`gmux-gui/app.rs` rewritten; old in-GUI pipe server deleted): on start it
  attaches to (or spawns, via `CREATE_NO_WINDOW` so its ConPTYs bind) the daemon; each frame it fetches
  `GetLayout` + `GetGrid` and renders remote grids, forwards input/control over the pipe, and toasts from
  `PollNotifications`.
- **✅ Detach/reattach verified live:** launch GUI → spawns daemon; `send-keys` a marker; **kill the GUI →
  the daemon keeps serving and `capture-pane` still shows the marker** (pane + process survived); relaunch
  GUI → reattaches. Job-object tree-kill (M1) reaps children; daemon outlives the GUI.
- Deferred to M8: reconnect-on-daemon-restart, grid diffing/binary side-channel (currently full-grid JSON
  poll at 30 fps), custom shell hand-off to the daemon. *Next:* M7 (session restore across reboot).

### M7 — Session restore across reboot ✅ COMPLETE (2026-07-05)

- **Stage A ✅ (layout + cwd)** — `gmux-mux/persist.rs`: `SessionSnapshot` serializes the window/split-tree
  layout + per-pane cwd (serde); `capture`/`restore` (respawns a **shell** per pane in its saved cwd — never
  auto-reruns agents). `gmux-pty`/`gmux-mux` gained `spawn_full`/`spawn_in` (cwd). Daemon
  (`gmux-server`) debounce-saves to `%LOCALAPPDATA%\gmux\state\session.json` every ~2 s + clears it on clean
  exit; `restore_or_new` rebuilds on start. **Verified:** persist roundtrip unit tests + restore integration
  test + **live reboot simulation** (kill daemon abruptly → new daemon restored both panes).
- **Stage B ✅ (screen restore via scrollback)** — the snapshot captures each pane's recent output
  (`PaneRecord.screen`, last 200 lines of scrollback+screen), and restore pre-seeds the fresh terminal
  with it under a dim `─── gmux: restored ───` divider (`Pane::spawn_in(.., replay)`). PowerShell's
  startup `ESC[2J` pushes the replayed content into scrollback rather than destroying it, so it is
  reachable through the M8 scrollback viewport. **Verified live (2026-07-05):** daemon force-killed and
  restarted → `capture-pane -S -` returned the pre-kill marker, its output, and the restore divider.
- Deferred: env secret-scrubbing in snapshots, per-agent resume behind approval.

### M8 — MVP hardening and release

- **Scrollback viewport ✅ (2026-07-05):** gmux-vt exposes history (`history_len` / `cells_at_offset` /
  `scrollback_text` over alacritty's 10k-line grid history); `capture-pane -S` (protocol
  `CapturePane.scrollback`, CLI `-S <n>|-S -`); `GetGrid.offset` + `GridWire.history/offset`; GUI mouse
  wheel + Shift+PageUp/PageDown scroll with Escape/typing snap-back, cursor suppressed while scrolled.
- **Daemon-reconnect ✅ (2026-07-05):** GUI `DaemonClient::call` transparently reconnects (respawning
  `gmux --daemon` if needed) and retries **idempotent calls only** — state-changing calls error instead
  of risking double-apply; ~1 s `ResizeView` heartbeat re-teaches a restarted daemon the geometry.
- **CI ✅ (2026-07-05):** GitHub Actions — x64 build+test, ARM64 cross-build (`.github/workflows/ci.yml`);
  portable-zip release job on `v*` tags (`release.yml`, signing deferred until a cert exists). ARM64
  verified locally to compile crate-by-crate; final link needs the MSVC ARM64 tools (present on runners).
- **ConPTY teardown deadlock ✅ FIXED (2026-07-05):** `Pty::drop` now terminates the child job *before*
  `ClosePseudoConsole` — on Win11 26100+ the close returns immediately without disconnecting a live
  client, the ConPTY host holds the output pipe open, the reader never EOFs, and the old `join()`
  blocked forever (the job-close kill sat *after* the join). Pre-existing since M7; all three console
  suites (`gmux-pty/spawn`, `gmux-mux/pane`, `gmux-server/daemon`) now exit 0.
- **First-run experience ✅ (2026-07-06):** `gmux shell-integration [--print|--install]` (PowerShell
  prompt wrapper emitting OSC 133;A + OSC 9;9 cwd, gated on `TERM_PROGRAM=gmux`, marker-guarded
  idempotent install into both CurrentUserAllHosts profiles); one-time welcome toast on first GUI
  launch pointing at `hooks setup all` + `shell-integration --install` (marker in
  `%LOCALAPPDATA%\gmux\state\first-run`, live-verified no re-fire); local crash reports — panic hook
  in daemon+GUI appends message/location/backtrace to `%LOCALAPPDATA%\gmux\crash\` (never leaves the
  machine).
- Remaining: installer, code signing (blocked on a cert), docs site.
- **MVP definition of done:** a developer on Windows 11 runs three parallel Claude Code sessions in three
  workspaces with splits, gets a toast + pane ring the moment any agent needs input, scripts
  send-keys/capture-pane over the pipe from an external tool, detaches and reattaches, and has everything
  restored after a reboot.

## v1

### M9 — Remote tmux (control-mode client)

- **Stage 1 ✅ (2026-07-06): `gmux-tmux` parser crate** — sans-io control-mode parser (std-only):
  `Parser::feed(bytes) -> Vec<Event>` with cross-feed line buffering; `%begin/%end/%error` reply
  assembly correlated by command number (column-0-anchored guards; `%`-prefixed body lines stay in
  the body, preserved as **raw bytes**); `%output` octal-unescape (`\ooo`, invalid escapes pass
  through, non-UTF-8 survives); all notification variants + forward-compatible `Unknown`;
  layout-string parser (recursive descent, 64-level depth cap — a remote-deliverable deep-nesting
  stack overflow was caught by adversarial review pre-commit); 1 MiB unterminated-line cap
  (bounded memory against a hostile peer). 43 tests + doc-test.
- **Stage 2a ✅ (2026-07-06): Pane local/remote backend** — `Pane` is now backed by either a local
  ConPTY (unchanged semantics, teardown fix intact) or a remote tmux mirror (`Pane::remote` +
  `push_output`; no local process). Both funnel through one `pump_bytes` mapping, so **OSC 9/777/99
  from remote agents raise attention/toasts identically to local panes**. Persist prunes remote
  leaves from the layout tree (indices stay consistent; remote panes re-attach via the transport,
  never respawn as local shells). 152 tests; console suites green.
- **Stage 2b ✅ (2026-07-06): `gmux-remote` transport crate** — `RemoteTmux::spawn(command_line)`
  (injectable: production `ssh -tt … tmux -CC new -As gmux`, tests stub processes) with piped stdio +
  kill-on-close **job object** (tree-kill so `kill()`/Drop can't deadlock on a grandchild holding the
  pipe); DCS-intro strip (chunk-boundary safe; ST deliberately NOT scanned — `capture-pane -e` bodies
  contain raw `ESC \`; detach = the protocol's own `%exit`); **attach-greeting reply surfaced as
  `TransportEvent::Greeting`** so positional correlation never desyncs (the classic control-mode
  trap, caught by adversarial review); `send-keys -H` hex input, resize/split/kill/new-window
  helpers; chunk-appended stderr for live ssh diagnostics; layout→`Node` converter with midpoint
  ratios (`(first+0.5)/span`) so `floor(span·ratio)` reproduces tmux sizes for every geometry
  (exhaustively tested to span 400). 13 tests. **Not yet live-tested against a real tmux** (no
  WSL/ssh peer on the dev machine) — needs a real remote before M9 is declared done.
- Stage 2c: server/CLI wiring — `Call::SshTmux`/`gmux ssh-tmux`, daemon tick pumping transport
  events into remote windows/panes (%output→push_output, %layout-change→layout_to_node,
  %window-add/close, %exit→mark_exited), `%pause` flow control; tmux ≥3.2 gate with degraded mode.

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
