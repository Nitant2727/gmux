# gmux

**A Windows-native, GPU-accelerated terminal multiplexer built for running many AI coding agents in
parallel.** gmux is the independent Windows equivalent of [cmux](https://cmux.com) (macOS): vertical-tab
workspaces, tmux-style splits and detach/reattach, reboot-surviving sessions, a scriptable named-pipe
API — and, above all, **notification hooks that actually work**: any agent that emits an OSC 9/777/99
escape (Claude Code, Codex CLI, Aider, Gemini CLI, …) lights up its pane and fires a real Windows toast
the moment it needs you.

> **Status: working application.** All roadmap milestones (M0–M12) are implemented, plus 16 GUI
> iteration rounds: scrollback search with match highlighting, clickable URLs and OSC 8 hyperlinks
> (with hover tooltips showing the real target), IME input, mouse reporting, per-pane scrollback
> with a draggable scrollbar, font zoom, a command palette, prompt-jump navigation, OSC 52
> clipboard, busy-pane close protection, session-surviving tab renames, and more. `cargo build
> --release -p gmux` and run `gmux.exe`.

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

## Install / build

Portable, no installer: `scripts/package.ps1` builds `dist\gmux-<version>-x64.zip` (gmux.exe +
docs) — unzip anywhere and run `gmux.exe`. Or build from source: `cargo build --release -p gmux`.
The binary is currently unsigned (SmartScreen may prompt on first run of a downloaded copy).

## Default keybindings

Rebindable in `%APPDATA%\gmux\gmux.json` (`Ctrl+,` opens it; see the generated template for names).

| Chord | Action |
|---|---|
| `Ctrl+Shift+T` / middle-click tab | New tab / close tab (busy tabs ask first) |
| `Ctrl+Shift+D` / `Ctrl+Shift+E` | Split side-by-side / stacked |
| `Ctrl+Shift+W` | Close pane (busy panes ask first) |
| `Ctrl+Shift+Z` | Zoom pane (tmux-style maximize; title strip shows the state) |
| `Alt+arrows` / `Alt+Shift+arrows` | Focus pane / resize split |
| `Alt+1..9`, `Ctrl+PageUp/PageDown` | Jump to tab N / cycle tabs |
| `Ctrl+Shift+P` | Command palette (fuzzy actions + tab switcher) |
| `Ctrl+Shift+F` | Scrollback search (Enter/Shift+Enter cycle matches) |
| `Ctrl+Up` / `Ctrl+Down` | Jump to previous / next command prompt (needs shell integration) |
| `Ctrl+=` / `Ctrl+-` / `Ctrl+0`, `Ctrl+wheel` | Font zoom |
| `Ctrl+Shift+S` | Export the pane's scrollback to Downloads |
| `Ctrl+Shift+C` / `Ctrl+Shift+V`, right-click | Copy selection / paste |
| `Ctrl+Shift+M` | Copy mode (arrows/hjkl move, `v` marks, `y` copies, Esc exits) |
| Double-click a divider | Equalize the split to 50/50 |
| Double-click / triple-click | Select word / line |
| `Ctrl+click` | Open the underlined URL/hyperlink (hover shows the real target) |
| Double-click a tab | Rename it (persists across restarts) |

`"focus_follows_mouse": true` in the config enables hover focus. Drag files onto a pane to paste
their quoted paths. Wheel scrolls the pane under the cursor; the viewport is content-pinned while
a background pane keeps producing output.

## Workspaces

A workspace is a sidebar row anchored to a **directory**. Click `+ open workspace` in the sidebar
(or press `Ctrl+Shift+O`) to pick a folder, and every terminal in that workspace starts there — the
first shell, every split (horizontal or vertical), and every pane restored after a daemon restart.
No more splitting into whatever directory the daemon happened to start in.

From the CLI: `gmux new-window --cwd <dir>` opens one, and `gmux workspace -t @2 <dir>` re-anchors
an existing workspace (`--clear` unpins it). `Ctrl+Shift+T` still opens a plain unanchored tab.

**At a glance.** Every workspace row carries a leading dot: **filled** while something is running in
it (a build, an agent), a **hollow ring** when it's idle — so "which agents are still working" is
one look down the sidebar rather than a hunt for spinners.

**Filtering.** `Ctrl+Shift+K` turns the sidebar header into a filter box; typing narrows the list by
workspace name *or* git branch (fuzzy, like the command palette), Enter jumps to the first match,
Escape restores the full list. Groups whose members all filter out disappear with them.

**Reordering.** Drag a sidebar row to move it: a line shows where it will land and the dragged row
fades while it's in flight. Dropping onto a row inside a group (or onto the group's header) files
the workspace into that group; dropping it among the ungrouped rows takes it back out.

**Renaming and closing.** Double-click a sidebar row to rename it inline (`Ctrl+Shift+R` renames
the active one); the name is persisted. Hovering a row reveals a close button at its right edge —
click it, middle-click the row, or press `Ctrl+Shift+Q`. Closing a workspace whose panes are
running something asks for confirmation first. From the CLI: `gmux rename -t @2 <name>` (an empty
name reverts to the derived one) and `gmux close-window -t @2`.

> In PowerShell, quote window ids — `@2` is the array-literal operator, so `-t '@2'` (or `-t 2`).

**Importing existing projects.** `Ctrl+Shift+I` (or `gmux import <dir>`) points at a folder holding
several projects and opens a workspace for each one inside it. Only folders containing a `.git` are
taken — pointing at your projects directory shouldn't also open `Downloads` — and `--all` takes
every subfolder instead. Dot-folders are never imported, folders already open are skipped (so
re-importing adds only what's new), and an import opens at most 24 workspaces at a time, reporting
anything left over.

Workspaces can be filed under collapsible groups: `gmux group -t @2 backend` puts window `@2`
(the id `gmux list-panes` prints) under a "backend" header, `--clear` takes it back out, and
clicking a header folds the group away — a folded header keeps showing its member count and its
workspaces' unread total. Grouping is persisted, so it survives a daemon restart.

A workspace can carry a pull-request badge: `gmux pr -t @2 --resolve` reads the current branch's PR
with `gh` and pushes it, or set it by hand with `gmux pr -t @2 128 open [url]` (`open`/`draft`/
`merged`/`closed`; `--clear` removes it). The chip shows `#128` in GitHub's state colors next to the
branch, and **clicking it opens the pull request** when the badge carries a URL (`--resolve` always
supplies one).

`Ctrl+Shift+B` toggles an embedded browser panel down the right-hand side (needs a
`--features browser` build); the terminal panes reflow around it, and `gmux browse --pane <url>`
opens the panel and loads the page there. Hiding the panel keeps the page and its login session
loaded, so toggling back is instant.

Auto-refresh is **opt-in**: set `"pr_refresh_secs": 300` in `%APPDATA%\gmux\gmux.json` and the
daemon re-resolves badges on that cadence — one workspace per cycle, on a worker thread, using each
pane's own directory. Left at its default of `0` it never runs, and gmux makes no network calls at
all. Values below 30 s are clamped up.

Tag a workspace with a color via `gmux color -t @2 #e0533d` (a left rail on the row, brightened so
it reads on the dark sidebar; `--clear` removes it). A workspace running something — a build, an
agent — shows a small activity spinner; it animates only while work is in flight, so an idle gmux
still sits at 0% CPU. Both are persisted.

The chrome follows cmux's look: the selected workspace is a solid accent pill, a workspace wanting
attention washes blue, and the focused pane carries an accent ring. Set `"theme": { "accent":
"system" }` to follow your Windows accent color instead, or `"#rrggbb"` to pin your own. Terminal
cell colors are separate and come from `theme.scheme` / `theme.ansi`.

## Platform

Windows 10 21H2+ and Windows 11, x64 and ARM64. ConPTY (`CreatePseudoConsole`) for the PTY layer,
GPU glyph rendering, no Electron in the terminal grid.

## License

MIT (see [LICENSE](LICENSE)). gmux studies cmux's behavior only — cmux is GPL-3.0 and no code is copied.
