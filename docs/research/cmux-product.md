# cmux Product Research — Feature Parity Target for gmux

Researched: 2026-07-04. All findings verified against live web sources this session unless marked otherwise.
Primary sources: https://cmux.com, https://cmux.com/docs/*, https://github.com/manaflow-ai/cmux, Show HN thread https://news.ycombinator.com/item?id=47079718.

---

## 1. Product identity and status

- **cmux** (https://cmux.com) — "The terminal built for multitasking, organization, and programmability." A native macOS terminal app purpose-built for running many AI coding agents in parallel (Claude Code, Codex, OpenCode, Gemini CLI, Kiro, Aider, Goose, Amp, Cline, Cursor Agent — "any CLI tool").
- Built by **Manaflow (YC S24)**. Repo: **https://github.com/manaflow-ai/cmux** (public).
- Launched **February 2026**, hit **#2 on Hacker News** (Show HN: https://news.ycombinator.com/item?id=47079718), reported **~21,700 GitHub stars by June 2026** (secondary source: rywalker.com/research/cmux).
- Latest release at research time: **v0.64.17 (2026-06-23)**; release cadence roughly **every 1–2 weeks**, plus an automated **nightly** channel with a separate bundle ID (`com.cmuxterm.app.nightly`). Source: https://github.com/manaflow-ai/cmux/releases
- Requirements: **macOS 14.0+**, Apple Silicon and Intel. Install via DMG (Sparkle auto-update) or `brew install --cask cmux` (tap `manaflow-ai/cmux`).
- **Platform roadmap: Linux, Windows, and Android ports have a public waitlist** (cmux.com). Community ports already exist: `bradwilson331/cmux-linux` (Linux port of cmux), and independent Ghostty-on-Windows efforts `amanthanvi/winghostty` (first releases 2026-04-16) and `deblasis/wintty`. **This is a direct competitive risk to gmux's window of opportunity.**

## 2. Licensing and pricing

- **Free and open source, "always will be."** License: **GPL-3.0-or-later**, dual-licensed — commercial license available from Manaflow (founders@manaflow.com) for organizations that can't comply with GPL. Source: README at https://github.com/manaflow-ai/cmux
- Paid tier: **cmux Founders Edition** — backs development; grants early access to cmux AI (workspace-context AI), the iOS app (TestFlight), Cloud VMs, Voice mode, prioritized feature requests, and direct iMessage/WhatsApp access to founders. **Price not published** in any fetched source (see https://github.com/manaflow-ai/cmux/issues/542).
- **gmux licensing implication:** cmux source is readable for behavioral reference, but copying GPL code into a non-GPL gmux is contamination. Clean-room/behavior-level parity only, or adopt a GPL-compatible license.

## 3. Architecture

- **Native Swift + AppKit. Explicitly "no Electron."**
- **Not a Ghostty fork.** Embeds **libghostty** as a rendering/terminal-emulation library — founder Lawrence Chen: uses libghostty "the same way apps use WebKit for web views." libghostty provides GPU-accelerated glyph rendering (Metal), VT emulation, and terminal state; cmux built everything else: sidebar/workspace model, notification routing, socket API, session restore, embedded browser, SSH/tmux bridging. Sources: README; HN thread.
- Reads the user's existing **`~/.config/ghostty/config`** for themes, fonts, colors, and terminal keybindings; cmux-specific settings live in **`~/.config/cmux/cmux.json`**. Every keyboard shortcut is editable.
- Founder on portability: "Planning on adding iOS app since libghostty works there too!" (HN thread) — the iOS app shipped to TestFlight.

### libghostty vs libghostty-vt (relevant to gmux)

- Official Ghostty app: **Windows support still not planned** (https://github.com/ghostty-org/ghostty/discussions/2563), but the project position is that "a capable and powerful libghostty will enable better Windows support in the long run."
- **libghostty-vt** — the VT-parsing/terminal-state core split out as a Zig library with a **C ABI**, usable from C today, **cross-platform: macOS, Linux, Windows, WebAssembly** (Ghostty 1.2 release notes: https://ghostty.org/docs/install/release-notes/1-2-0; heise coverage). The *full* libghostty (rendering stack) on Windows is not an officially supported path; community ports (winghostty, wintty) are building on the libghostty pieces.
- **gmux implication:** gmux could adopt libghostty-vt (MIT-licensed Ghostty core) for VT parsing/terminal state on Windows x64/ARM64 instead of writing a parser, while doing its own GPU renderer + ConPTY layer. Needs a dedicated feasibility check (Zig toolchain targets Windows ARM64 well).

## 4. Object model / concepts

Source: https://cmux.com/docs/concepts

Hierarchy: **Window → Workspace → Pane → Surface → Panel**

| Concept | Meaning | Notes |
|---|---|---|
| Window | A macOS window (⌘⇧N) | Each has its own sidebar and workspaces |
| Workspace | A sidebar entry ("tab" in UI language) | ⌘N to create; ⌘1–⌘9 to switch; env `CMUX_WORKSPACE_ID` |
| Pane | A split region within a workspace | ⌘D split right, ⌘⇧D split down; directional focus ⌥⌘arrows |
| Surface | A tab *within* a pane (each pane has its own tab bar) | ⌘T; env `CMUX_SURFACE_ID`; a surface is a terminal or a browser |
| Panel | The actual content (Ghostty terminal or embedded browser) | Mostly internal concept |

- **Workspace Groups**: collapsible named sidebar sections grouping related workspaces; pinning, renaming, custom icons/colors, spawn-into-group. (https://cmux.com/docs/workspace-groups)

## 5. Sidebar (vertical tabs) — the organizational core

Each workspace row in the sidebar shows, live:
- **git branch**
- **linked PR status / number**
- **working directory**
- **listening ports**
- **latest notification text** and **unread badge**

Agents/scripts can also push arbitrary metadata into the sidebar via CLI (see §7): `set-status <key> <value> [--icon] [--color] [--priority]`, `clear-status <key>`, `set-progress <0.0-1.0> [--label]`, `log --level {info|progress|success|warning|error}`.

## 6. Notifications — end-to-end (the feature gmux must match)

Source: https://cmux.com/docs/notifications (mirrored at https://manaflow-ai-cmux.mintlify.app/features/notifications)

### 6.1 Ingestion (what triggers attention state)

Four ingestion paths; the first three are parsed from the pane's output stream by the terminal layer:

1. **OSC 9** (iTerm2/ConEmu-style, message only):
   ```sh
   printf '\033]9;%s\007' "Task complete"
   ```
2. **OSC 777** (rxvt protocol, fixed title;body format, BEL-terminated):
   ```sh
   printf '\e]777;notify;My Title;Message body here\a'
   ```
3. **OSC 99** (Kitty notification protocol — rich, multi-part, ST-terminated `\e\\`; supports ids, done-flags, and typed payload parts):
   ```sh
   printf '\e]99;i=1;e=1;d=0:Hello World\e\\'
   printf '\e]99;i=1;e=1;d=0;p=title:Build Complete\e\\'
   printf '\e]99;i=1;e=1;d=0;p=subtitle:Project X\e\\'
   printf '\e]99;i=1;e=1;d=1;p=body:All tests passed\e\\'
   ```
4. **CLI**: `cmux notify --title "Task Complete" [--subtitle "S"] --body "Your build finished"` — designed to be wired into agent hook systems (Claude Code hooks, OpenCode, etc.). Founder (HN): "The notification system picks up terminal sequences...and has a CLI (cmux notify) you can wire into agent hooks."

Because notifications arrive through the pane's own PTY stream (or a CLI call that carries `CMUX_SURFACE_ID` from the environment), **cmux knows exactly which surface/workspace fired each notification** — that attribution is what powers per-pane attention UI.

### 6.2 Attention UX

When a notification fires on an unfocused surface:
- **Blue notification ring around the originating pane** ("panes light up when agents need attention")
- **Sidebar tab illuminates + unread badge**, and the sidebar row shows the latest notification text
- **Notification panel/popover** listing all pending alerts — open with **⌘⇧I**; **⌘⇧U jumps to the most recent unread** notification's surface
- **macOS desktop notification** (system toast)

### 6.3 Suppression and clearing semantics

- Desktop alerts are **suppressed** when: cmux window is focused AND that workspace is active, or the notification panel is open (prevents noise when you're already looking).
- Lifecycle: **Received** (panel entry + desktop alert if not suppressed) → **Unread** (badge on workspace tab) → **Read** ("cleared when you view that workspace" — i.e., **auto-marked read when the surface that triggered it gains focus**) → **Cleared** (removed from panel; `cmux clear-notifications`).

### 6.4 Extensibility

- **Custom notification command** (Settings > App > Notification Command): an arbitrary shell command run per notification with env vars **`CMUX_NOTIFICATION_TITLE`**, **`CMUX_NOTIFICATION_SUBTITLE`**, **`CMUX_NOTIFICATION_BODY`** (examples in docs: `say`, `afplay`, append to log).
- **Notification hooks in `cmux.json`**: a hook program receives every notification as **JSON on stdin** and returns JSON controlling an **`effects` object** — per-notification control of desktop alert, sidebar history, sounds, and pane flashing.
- **Agent hook installers**: `cmux hooks setup [agent]` supports 14+ agents (Claude Code, Codex, Grok, OpenCode, Pi, Amp, Cursor CLI, Gemini, Rovo Dev, Copilot, CodeBuddy, Factory, Qoder, Antigravity, OMP…).

### 6.5 gmux mapping

Windows equivalents: parse OSC 9/777/99 in the VT layer → in-app pane ring + sidebar badge + **Windows toast (AppNotificationManager / WinRT `ToastNotificationManager`)**; suppress on focused-workspace; auto-clear on surface focus; `gmux notify` CLI resolving target from an env var injected into every spawned pane.

## 7. CLI + socket API

Source: https://cmux.com/docs/api (CLI Reference), https://manaflow-ai-cmux.mintlify.app/automation/socket-api

### 7.1 Transport and protocol

- **Unix domain socket**: `/tmp/cmux.sock` (release), `/tmp/cmux-debug.sock` (debug); override with **`CMUX_SOCKET_PATH`**.
- Protocol: **one newline-terminated JSON request per call** (JSON-RPC-style): `{"id":"req-1","method":"workspace.list","params":{}}`. CLI commands are thin wrappers over socket methods (v1 CLI names ↔ v2 dotted method names, e.g. `read-screen` ↔ `surface.read_text`).
- **Security model: process-parentage authentication** — only processes started *inside* cmux may connect; external processes get `"ERROR: Access denied — only processes started inside cmux can connect"`. Open feature request #1864 asks for password-based external access (`CMUX_SOCKET_PASSWORD`) for read-only commands; no maintainer response yet. (https://github.com/manaflow-ai/cmux/issues/1864)
- **gmux implication:** the `\\.\pipe\gmux` named pipe needs an equivalent auth story — e.g., a per-session token injected into pane environments, plus `GetNamedPipeClientProcessId` ancestry checks; default-deny external clients.

### 7.2 Command surface (v1 CLI names)

| Area | Commands |
|---|---|
| Workspaces | `cmux list-workspaces [--json]`, `new-workspace`, `select-workspace --workspace <id>`, `current-workspace [--json]`, `close-workspace --workspace <id>` |
| Splits/panels | `new-split {left\|right\|up\|down}`, `list-panels [--json]`, `list-pane-surfaces [--json]`, `focus-panel --panel <id>` |
| Input | `send "text"` / `send --surface <id> "text"`, `send-key {enter\|tab\|escape\|backspace\|delete\|up\|down\|left\|right}` |
| Screen read | `read-screen [--workspace <id>] [--surface <id>] [--scrollback] [--lines N]` (socket: `surface.read_text`) — was debug-only, exposed in production via issue #152 / PR #219 |
| Screenshot | `browser.screenshot` socket method (surface_id → base64 PNG); browser CLI `screenshot --out /path/file.png` |
| Notifications | `notify --title --subtitle --body`, `list-notifications [--json]`, `clear-notifications` |
| Sidebar metadata | `set-status`, `clear-status`, `set-progress`, `log` (see §5) |
| Utility | `ping`, `capabilities [--json]`, `identify [--json]` |
| Session | `restore-session`, `hooks setup [agent]`, `surface resume set/show/clear` (see §9) |
| SSH/tmux | `ssh user@remote`, `ssh-tmux <dest> [--port] [--identity]`, `remote.tmux.*` socket methods (see §10) |
| Agents | `claude-teams` |

Global flags: `--socket PATH`, `--json`, `--workspace ID`, `--surface ID`, `--id-format {refs|uuids|both}`.

Ecosystem: a third-party **cmux-mcp** MCP server exists (mcpservers.org/servers/daegweon/cmux-mcp), and official **Skills** teach agents the CLI (§11).

## 8. Embedded browser + automation API

Source: https://cmux.com/docs/browser-automation

- A real browser (WebKit) can open as a **split pane next to the terminal**; agents use it to verify their web changes without leaving cmux. Browser panes on SSH workspaces **route through the remote network**.
- **Browser import**: cookies, history, sessions from Chrome, Firefox, Arc, and 20+ browsers.
- Unified CLI: `cmux browser [surface] <subcommand>`:
  - Navigation: `open`, `open-split`, `navigate`, `back`, `forward`, `reload`, `url`, `zoom`
  - DOM: `click`, `dblclick`, `hover`, `focus`, `check`, `uncheck`, `type`, `fill`, `press`, `select`, `scroll`
  - Inspection: `snapshot` (DOM/a11y snapshot), `screenshot --out file.png`, `get`, `is`, `find`, `highlight`
  - JS: `eval`, `addinitscript`, `addscript`, `addstyle`
  - State: `cookies`, `storage`, `state` (save/load session to JSON)
  - Diagnostics: `console list/clear`, `errors list/clear`; network activity readable over the socket
  - Sync: `wait` (selector/text/load-state/JS-expression conditions), `dialog`, `frame`, `download`, `tab`
- Selectors: CSS plus accessibility locators (`role`, `label`, `testid`); `--snapshot-after` flags return structured post-action state so agents avoid screenshot round-trips.
- **gmux note:** this is cmux's second-biggest differentiator after notifications. On Windows the analog is WebView2 — but it is a large scope item; likely post-v1 for gmux.

## 9. Session restore

Source: https://cmux.com/docs/session-restore

- **Saved state**: window/workspace/pane layout, working directories, **terminal scrollback (best effort — stored as bounded text, replayed via temp files;** terminal apps may redraw/clear), browser URL + navigation history. Written as a **versioned JSON snapshot** to `~/Library/Application Support/cmux/session-<bundle-id>.json`, with a previous-session cache for manual recovery.
- **Restore paths**: automatic on relaunch (survives reboot); manual via History > Restore Previous App Launch, **⌘⇧O**, or `cmux restore-session`. Rebuild order: windows/panes first, then optional agent resume commands.
- **Explicit non-goal**: does *not* checkpoint arbitrary live process state — tmux/vim/shells reopen as fresh terminals unless a dedicated resume integration exists.
- **Agent resume integrations**: `cmux hooks setup` installs hooks for all supported agents (or per-agent: codex, grok, antigravity, omp, opencode, …; 14+ agents with per-agent binary + resume command, e.g. Claude Code session-resume). Review/approve resume commands under **Settings > Terminal > Resume Commands**. Disable via `{"terminal": {"autoResumeAgentSessions": false}}` in `~/.config/cmux/cmux.json`. **Sensitive env keys (tokens, passwords, secrets, API keys) are scrubbed before storage.**
- **Generic checkpoint API** for anything (including tmux):
  ```sh
  cmux surface resume set --kind tmux --checkpoint work --shell "tmux attach -t work"
  cmux surface resume show --json
  cmux surface resume clear --checkpoint work
  ```

## 10. tmux story

Two distinct answers:

1. **Locally, cmux replaces tmux.** Multiplexing (workspaces/splits/tabs) is implemented natively in the app — "no config files or prefix keys." There is no local tmux dependency. This matches gmux's plan exactly (tmux doesn't run natively on Windows anyway).
2. **Remote tmux mirroring (beta, off by default; Settings → Beta Features).** Source: https://cmux.com/docs/remote-tmux
   - cmux spawns **`ssh … tmux -CC attach`** and **parses the tmux control-mode stream itself** (no built-in tmux viewer). Requires **tmux ≥ 3.2** on the remote; stock tmux, no special build.
   - Mapping: tmux **session → workspace**, **window → tab**, **pane → pane**. Bidirectional: splitting/closing in cmux executes `tmux split-window`; tab drag-reorder → `swap-window`; keystrokes/mouse → `tmux send-keys`; paste/drop → `tmux paste-buffer -p`; sizing via `refresh-client -C`; pane output fed from `%output` events into dedicated surfaces.
   - Entry: `cmux ssh-tmux <destination> [--port 2222] [--identity ~/.ssh/id_ed25519]`; socket methods `remote.tmux.sessions/attach/mirror/window/detach/state`.
   - Limitations: reconnection with exponential backoff on drops; multi-line paste sent as keystrokes (no bracketed paste); scrollback reflow left to tmux; **remote attach not included in session restore**; interactive SSH auth runs inline.
3. Plain SSH workspaces (non-tmux): `cmux ssh user@remote`; image drag-drop uploads via scp; browser panes route through remote network.

## 11. Skills (agent enablement)

Source: https://cmux.com/docs/skills

- Seven bundled skills that "teach coding agents how to use cmux CLI control, current-workspace automation, settings, customization, diagnostics, browser surfaces, and markdown panels."
- Layout: `skills/<name>/SKILL.md` + optional `references/*.md`, `scripts/*`, `templates/*`, and `agents/openai.yaml` metadata.
- Install: `npx skills add manaflow-ai/cmux` (Vercel skills CLI) or `curl -fsSL https://raw.githubusercontent.com/manaflow-ai/cmux/main/skills.sh | bash` (defaults into `~/.codex/skills` / `$CODEX_HOME/skills`).
- Includes a **live-reloading markdown panel** surface agents can render reports into.

## 12. Other features (docs TOC: https://cmux.com/docs)

- **TextBox (beta)**, **Vault** (secrets), **Task Manager**, **Dock**, **Custom Commands** (project-specific commands in cmux.json), full keyboard-shortcut editor.
- **Claude Code Teams** mode (`cmux claude-teams`); **oh-my-opencode / oh-my-codex / oh-my-claudecode** multi-model orchestration integrations.
- **Agent Hibernation** (pause idle agent sessions — recent release note), GPU memory optimization, browser focus mode, markdown viewer.
- **iOS app** (TestFlight, Founders Edition): terminals synced desktop↔phone, "Telegram-style chat surface for agent sessions", push notifications.

## 13. Parity matrix for gmux v1

| cmux feature | gmux v1 target | Notes |
|---|---|---|
| Vertical sidebar: branch, PR, cwd, ports, notif text | **Yes — core** | ports via TCP table polling per pane child PIDs |
| OSC 9/777/99 → pane ring + badge + system toast | **Yes — killer feature** | Windows toasts; identical suppress/clear-on-focus semantics |
| `notify` CLI + agent hooks installer | **Yes** | `gmux hooks setup claude-code` etc. |
| Splits/panes/surfaces, native mux | **Yes** | ConPTY per surface |
| Session restore + agent resume | **Yes** | versioned JSON snapshot; scrub secrets |
| CLI + local socket (JSON-line RPC) | **Yes** | named pipe `\\.\pipe\gmux`; parentage/token auth |
| read-screen / capture-pane, screenshot | **Yes** | explicit hard requirement |
| set-status/set-progress/log sidebar metadata | **Yes — cheap, high value** | |
| Remote tmux -CC mirroring | Later | good v2; parse control mode over ssh.exe |
| Embedded scriptable browser | Later | WebView2; large scope |
| Skills, markdown panels, Vault, iOS | Later/no | |

## 14. Open questions / follow-ups

- Confirm exact `cmux.json` notification-hook JSON schema (effects object fields) from source once needed — repo is public GPL, read for behavior only.
- Feasibility check: **libghostty-vt** (C ABI, MIT, Windows/ARM64/wasm targets) as gmux's VT layer vs. Rust crates (alacritty_terminal, vte).
- Track cmux's own Windows-port waitlist progress — timing pressure on gmux.
