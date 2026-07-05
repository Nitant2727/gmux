# Prior Art & Competitive Gaps: Terminal Multiplexing for AI Agents on Windows

**Research date:** 2026-07-04
**Scope:** Definitive check on whether anything already does what gmux plans to do; which existing components are liftable; exact shape of the competitive gap.
**Method:** Web-verified against primary sources (GitHub repos/issues/PRs, official docs, NuGet, release notes) this session unless marked otherwise. Confidence labels are noted inline where a claim rests on model knowledge.

---

## TL;DR — the verdict

**Nothing ships today that combines all of gmux's pillars.** The closest overlaps are:

| Contender | Native (no Electron) | GPU render | In-app mux + detach/reattach | OSC 9/777/99 → Windows toasts | Pane attention UX for agents | CLI/pipe automation API | Open source |
|---|---|---|---|---|---|---|---|
| **Windows Terminal** | ✅ | ✅ (AtlasEngine) | ❌ (no detach; buffer-text restore only) | ❌ (PR closed unmerged) | ❌ | partial (`wt.exe` args only) | ✅ MIT |
| **WezTerm** | ✅ (Rust) | ✅ | ✅ (domains/mux-server) | ⚠️ buggy on Windows | ❌ | ✅ (`wezterm cli`) | ✅ MIT; **maintenance slowed** |
| **Warp** | ✅ (Rust) | ✅ | ❌ detach; ✅ multi-agent UI | ✅ (own agent framework) | ✅ | ❌ | ❌ closed source |
| **wmux** | ❌ **Electron** | ⚠️ xterm.js WebGL | ✅ (daemon PTYs survive reboot) | ✅ toasts | ✅ | ✅ (MCP, CLI) | ✅ MIT |
| **psmux** | ✅ (Rust TUI) | n/a (runs *inside* a terminal) | ✅ tmux-style | ❌ (host terminal's job) | ❌ | ✅ tmux command language | ✅ MIT |
| **winghostty** | ✅ (Zig/Win32) | ✅ (OpenGL 4.3) | ❌ (splits/tabs, no detach) | ⚠️ unclear | ❌ | ❌ | ✅ MIT, 1 maintainer |
| **cmux** | ✅ (macOS only) | ✅ (libghostty) | ✅ | ✅ (macOS notifications) | ✅ | ✅ (`cmux notify`, API) | ✅ |

The exact intersection gmux targets — **native non-Electron GPU GUI + ConPTY mux with detach/reattach + reboot-surviving sessions + OSC-driven Windows toast notifications + per-pane agent attention indicators + named-pipe/CLI automation, open source, x64+ARM64** — is empty. The two projects that validate demand (wmux, psmux) each fail a hard requirement (Electron; TUI-not-GUI respectively).

---

## (a) Windows Terminal

### Architecture and ConPTY relationship

Windows Terminal (WT) and the console host live in one repo, [microsoft/terminal](https://github.com/microsoft/terminal), MIT licensed. ConPTY (`CreatePseudoConsole`, Windows 10 1809+) is the in-box OS API; OpenConsole.exe is the out-of-band-updated conhost that WT spawns to service each connection ([ConPTY intro post](https://devblogs.microsoft.com/commandline/windows-command-line-introducing-the-windows-pseudo-console-conpty/), [console host architecture](https://deepwiki.com/microsoft/terminal/2.5-console-host-architecture)).

### AtlasEngine (GPU renderer) — worth studying for gmux's renderer

Verified from [PR #11623](https://github.com/microsoft/terminal/pull/11623) (lhecker's original prototype), [src/renderer/atlas](https://github.com/microsoft/terminal/tree/main/src/renderer/atlas), and [DeepWiki's Atlas Engine page](https://deepwiki.com/microsoft/terminal/3.2-atlas-engine):

- **Design:** DirectWrite + Direct2D are used *only* to rasterize glyphs into a texture atlas; glyph placement and blending onto the target are done in Direct3D 11 with a simple HLSL shader. This is the same "CPU rasterize once, GPU composite forever" pattern as Ghostty/WezTerm.
- **Backend split:** graphics isolated behind an `IBackend` interface — `BackendD3D` (primary; requires D3D 11.0+ with compute shader support) and `BackendD2D` (compatibility fallback). AtlasEngine picks per hardware capability/user setting.
- **Threading:** two parallel state structs (`_api` fields touched under the console lock by API methods; `_p` fields owned by the render thread's `Present()`), avoiding locks on the hot path.
- **Liftability:** MIT-licensed C++, but it is **not packaged as a standalone library** — it implements WT's internal `IRenderEngine`/render-thread contracts and leans on the repo's buffer types. Realistic uses for gmux: (1) study the atlas/shader design (the `.hlsl` files and `BackendD3D.cpp` are self-contained reading), (2) port the approach, not the code. Lifting it wholesale means dragging in WT's renderer plumbing. If gmux is Rust, the equivalent prior art is WezTerm's renderer or wgpu-based Sugarloaf (Rio).

### The ConPTY redist NuGet — the most liftable component

- Package: **`Microsoft.Windows.Console.ConPTY`** (CI mirror listed as [CI.Microsoft.Windows.Console.ConPTY, 1.22.250314001, MIT](https://www.nuget.org/packages/CI.Microsoft.Windows.Console.ConPTY)); ships the **matched pair `conpty.dll` + `OpenConsole.exe`**, published from the terminal repo's release assets, works on Windows 10.0.17763+ ([discussion #17608](https://github.com/microsoft/terminal/discussions/17608), [issue #8576](https://github.com/microsoft/terminal/issues/8576)).
- Newer pairs exist than the NuGet CI listing: WezTerm tracks updating its bundled pair to **1.24.260402001** ([wezterm #7774](https://github.com/wezterm/wezterm/issues/7774)) — and that issue confirms the operational rule: **update `conpty.dll` and `OpenConsole.exe` together, never separately.**
- **Implication for gmux:** bundle the redist pair rather than relying on the in-box Windows 10 21H2 conhost — this gets years of ConPTY fixes (passthrough behavior, resize handling, UTF-8 fixes) on old OS builds, MIT-licensed, redistribution-safe. This is exactly what WezTerm does.

### Why WT has no detach/mux

- WT's ConPTY connection objects live inside the window process; there is no daemon layer. Tab tear-out ([#1256](https://github.com/microsoft/terminal/issues/1256)), detaching panes/tabs ([#8244](https://github.com/microsoft/terminal/issues/8244), [#9299](https://github.com/microsoft/terminal/issues/9299), [#6280](https://github.com/microsoft/terminal/issues/6280)) have been open for 5+ years; the "content process" separation prototyped for tear-out was abandoned (model knowledge — the issues above are the verified open feature requests). Re-attaching to an orphaned process is explicitly not possible ([discussion #17348](https://github.com/microsoft/terminal/discussions/17348)).
- **Session restore is cosmetic, not live:** WT 1.21/1.22 added "restore previous session buffer" — it snapshots scrollback text to `LocalState\buffer_{guid}.txt` and *re-prints it* into a **new** shell at startup ([#961](https://github.com/microsoft/terminal/issues/961) shipped; [4sysops write-up](https://4sysops.com/archives/new-in-windows-terminal-restore-buffers-code-snippets-scratchpad-and-regex/); quirk tracked in [#17274](https://github.com/microsoft/terminal/issues/17274)). Processes do not survive the window. This is far short of tmux-style detach and validates gmux's session-daemon design as genuinely unserved.

### Notifications — the killer-feature gap, confirmed at source

- [Issue #7718](https://github.com/microsoft/terminal/issues/7718) requested OSC-based desktop notifications. [PR #14425](https://github.com/microsoft/terminal/pull/14425) implemented `OSC 777;notify;title;body ST` → Windows toast (only when tab/window inactive) — **but the PR was closed unmerged in April 2025**, with blockers cited: doesn't work for unpackaged builds, non-functional on Windows 10, broken under elevation, can't foreground the window, notification-spam abuse concerns ("we can come back to this if we ever want it").
- What WT *does* support: `OSC 9;4` ConEmu-style taskbar progress, BEL with visual/taskbar flash (model knowledge, high confidence). **It does not fire toasts from OSC 9/99/777 as of mid-2026.** The mainstream Windows terminal cannot tell you an agent finished. This is gmux's single strongest wedge.

### Extensibility

No plugin system; [#4000](https://github.com/microsoft/terminal/issues/4000) is the still-open 3rd-party-extensions megathread. Shipping extensibility is limited to [JSON fragment extensions](https://learn.microsoft.com/en-us/windows/terminal/json-fragment-extensions) (profiles/color schemes only). WT 1.24 did extension-adjacent work ([Techzine](https://www.techzine.eu/news/applications/139337/windows-terminal-1-24-focuses-on-extensions-and-search/)) but nothing that would let a third party add mux or notification behavior (uncertain on 1.24 details). Building agent features *on top of* WT is not viable.

---

## (b) WezTerm on Windows

- **Mux quality:** best-in-class design — a `Mux` layer between terminal emulation and GUI, `Domain` abstraction (local, `wezterm-mux-server`, SSH, TLS, WSL) enabling true detach/reattach ([multiplexing docs](https://wezterm.org/multiplexing.html), [mux architecture](https://deepwiki.com/wezterm/wezterm/2.2-multiplexer-architecture)). On Windows: `WslDomain` auto-discovers WSL distros; WSL2 needs proxy workarounds; long-standing mux bugs remain open ([#3633](https://github.com/wezterm/wezterm/issues/3633), [#2614](https://github.com/wezterm/wezterm/issues/2614)).
- **ConPTY:** bundles the OpenConsole/conpty.dll redist pair; the bundled pair has lagged badly — [#7774](https://github.com/wezterm/wezterm/issues/7774) (2026) requests updating to 1.24.260402001 and notes updating the pair fixed various issues.
- **Notifications on Windows:** OSC 9 and OSC 777 are parsed, and `window:toast_notification()` exists ([docs](https://wezterm.org/config/lua/window/toast_notification.html)) — it *targets* real Windows toasts — but Windows behavior is unreliable: [#5166](https://github.com/wezterm/wezterm/issues/5166) (`toast_notification()` not working properly), [#5476](https://github.com/wezterm/wezterm/issues/5476) (OSC 9 toast doesn't display, lands silently in Action Center). No per-pane attention indicators, no agent awareness.
- **Maintenance status 2026:** releases essentially stopped after 2024 (nightlies continue); [#7451, "Is this project no longer being updated?"](https://github.com/wezterm/wezterm/issues/7451) opened Dec 23 2025; wez still dogfoods bleeding-edge but pace is a fraction of 2021–2023. **Risk assessment: WezTerm is the best code to *study* (Rust, mux architecture, ConPTY handling), a poor foundation to *depend on* for upstream fixes.**

---

## (c) Alacritty, Rio, Ghostty (and the libghostty question)

### Alacritty
Actively maintained into 2026 ([alacritty.org](https://alacritty.org/); Windows via ConPTY), but philosophically minimal: **no tabs/splits by design** ([#3129](https://github.com/alacritty/alacritty/issues/3129), [#6340](https://github.com/alacritty/alacritty/issues/6340)), **no OSC notification support** ([#7105](https://github.com/alacritty/alacritty/issues/7105) open; even OSC 9;4 progress declined-ish, [#5201](https://github.com/alacritty/alacritty/issues/5201)). Not a competitor; mildly useful as OpenGL-renderer prior art.

### Rio
[raphamorim/rio](https://github.com/raphamorim/rio) — Rust, WebGPU (wgpu) via its **Sugarloaf** renderer, actively shipping Windows fixes in 2026 (font fallback discovery on Windows, window-chrome fixes per [changelog](https://rioterm.com/changelog)). Has tabs/splits (model knowledge). No mux daemon, no Windows toast/agent features found. **Sugarloaf is arguably the best Rust/wgpu glyph-rendering prior art if gmux goes wgpu** (works on D3D12 via wgpu, relevant for ARM64).

### Ghostty / libghostty — the "candidate stack C" question

Verified from [1.3.0 release notes (March 9, 2026)](https://ghostty.org/docs/install/release-notes/1-3-0), [discussion #2563](https://github.com/ghostty-org/ghostty/discussions/2563), and [discussion #12290](https://github.com/ghostty-org/ghostty/discussions/12290):

- **Official Ghostty Windows app: "still not planned"** as of 1.3.0 (March 2026). On the long-term roadmap only. Maintainers' April 2026 requirements for any eventual Windows work: Direct3D renderer required, no GTK/Qt port, Windows 10/11 only, incremental PRs — i.e., the hard rendering work has not been done upstream.
- **libghostty:** extracted as a standalone module in the 1.3.0 cycle; C API in progress; Mitchell: "I believe that ultimately libghostty will be more widely used and influential than the Ghostty desktop application itself." **But no versioned/tagged release exists yet.**
- **libghostty-vt** (the VT parser/screen-state subset) is usable today for Zig and C and **is Windows-compatible** — but it is a *terminal-state* library, not a renderer or embedding surface. cmux on macOS embeds the *full* libghostty (renderer + Metal + AppKit surface); no equivalent exists for Windows.
- **winghostty** ([amanthanvi/winghostty](https://github.com/amanthanvi/winghostty), [winghostty.com](https://www.winghostty.com/)): third-party MIT fork proving the Ghostty core (VT parser, screen/scrollback, font pipeline, renderer) compiles for native Win32 — OpenGL 4.3 via WGL, Win32 UI, tabs/splits, session restore. First release April 16, 2026; v1.3.115 on June 26, 2026; ~112 stars; **single maintainer**; upstream tracked as a git remote but prioritizes Windows behavior over upstream design.

**Verdict on stack C (embed libghostty):** not viable as a *stable dependency* in mid-2026 — no tagged release, no official Windows embedding API, C API still in flux. Viable variants: (1) use **libghostty-vt alone** as the VT/screen-model layer under a gmux-owned renderer (it is explicitly Windows-compatible and this is the exact layer that's hardest to get right); (2) fork the winghostty approach. Watch item: the moment libghostty tags a release with a Windows-capable embedding surface, the calculus flips toward embedding — same play as cmux.

### cmux (the thing gmux mirrors)
[manaflow-ai/cmux](https://github.com/manaflow-ai/cmux) / [cmux.com](https://cmux.com/) — native macOS, embeds libghostty for rendering, vertical-tab sidebar showing git branch / PR status / working dir / listening ports / latest notification per workspace; parses **OSC 9/99/777**; ships a `cmux notify` CLI to wire into agent hooks; built-in WebKit browser panel; works with any CLI agent (Claude Code, Codex, OpenCode, Gemini CLI, Aider, …). Open source (reported GPL-3.0-or-later — **verify before copying any code**; GPL would be incompatible with a more permissive gmux license). macOS-only with no stated Windows plans; a community `cmux-linux` port exists. cmux's feature list is the best available spec for what agent-parallel users actually want.

---

## (d) Tabby, Hyper, Wave, Warp on Windows

- **Tabby** ([Eugeny/tabby](https://github.com/Eugeny/tabby)): Electron + TypeScript/Angular; alive (v1.0.234, May 2026; issues active July 2026). SSH/serial/connection-manager focus. No agent features, no OSC-notification-to-toast story found. Known heavy (Electron RAM/input-latency); fails the performance bar and doesn't attempt the agent use case.
- **Hyper**: development stalled ([Slant comparison](https://www.slant.co/versus/18898/26039/~hyper_vs_tabby-terminal)); effectively dormant. Irrelevant except as an Electron cautionary tale.
- **Wave Terminal**: open-source, Electron-based block/widget terminal with inline AI chat and workspace model. Interesting UX ideas (blocks, previews) but Electron + not multiplexer-grade; no ConPTY-daemon detach; agent features are chat-centric rather than parallel-agent-ops-centric (mix of web-verified and model knowledge).
- **Warp** ([warp.dev](https://www.warp.dev/)): **the most serious commercial competitor on Windows.** Native Rust GPU renderer (not Electron), Windows build at parity since 2024–25. On **April 14, 2026** Warp shipped universal agent support wiring **Claude Code, Codex, Gemini CLI, OpenCode** in as first-class citizens, with a management UI showing all running agents, **in-app + system notifications when an agent completes or needs approval**, and attention-needed indicators on tabs in a vertical sidebar ([warp.dev/agents](https://www.warp.dev/agents), [multi-agent docs](https://docs.warp.dev/guides/agent-workflows/how-to-run-multiple-ai-coding-agents/), [Warp 2.0 ADE post](https://www.warp.dev/blog/reimagining-coding-agentic-development-environment)).
  **Why Warp still leaves the gap open:** closed source (fails gmux's licensing requirement and enterprises' auditability), account/sign-in + telemetry posture, agent features funnel toward Warp's paid AI plans, **no tmux-style detach/reattach or reboot-surviving sessions**, no local scripting/named-pipe API for external orchestration, and its notification path is Warp's own agent framework rather than standard OSC sequences from *any* tool. gmux's positioning against Warp: open, standards-based (OSC), scriptable, sessions-as-infrastructure.

Why Electron fails the bar (for the doc's record): per-window Chromium overhead (hundreds of MB baseline), xterm.js DOM/WebGL renderer latency and throughput ceilings versus native atlas renderers, GC pauses under heavy PTY output — precisely the workload of N agents streaming concurrently (model knowledge, widely corroborated; WT/Ghostty/Alacritty benchmarks exist).

---

## (e) tmux-on-Windows reality check

- **Official tmux has no native Windows port.** [tmux/tmux #1954](https://github.com/tmux/tmux/issues/1954) (feature request for native Windows) went nowhere; the supported routes remain **WSL2, Cygwin, or MSYS2** ([tmux.app Windows guide](https://tmux.app/install/windows/)). Confirmed still true mid-2026.
- **Why that doesn't serve ConPTY-based Windows shells:** tmux under WSL multiplexes *Linux* processes; driving PowerShell/cmd through WSL interop loses ConPTY semantics (resize, VT dialect, job objects, cwd tracking) and Windows credentials/paths. Cygwin/MSYS2 tmux uses Cygwin PTYs, which historically cannot host native Win32 console programs correctly without winpty/ConPTY bridging; socket-based session sharing also behaves differently (search-verified summary + model knowledge).
- **Third-party native attempts (new since 2025) — proof of demand:**
  - **psmux** ([psmux/psmux](https://github.com/psmux/psmux)): Rust, drives ConPTY directly, speaks the tmux command language, reads `.tmux.conf`, 83 commands / 140+ format variables / vim copy-mode; sessions/windows/panes, detach/reattach, persistence; runs **inside** an existing terminal (TUI, not a GUI app); MIT; v3.3.6 (June 13, 2026), ~2.8k stars, distributed via winget/cargo/scoop/choco. Documents first-class **Claude Code agent-team** support (spawning teammates into panes); there's even an open claude-code issue asking Anthropic to support it for agent teams ([anthropics/claude-code #34150](https://github.com/anthropics/claude-code/issues/34150)).
  - **bitcode/tmux-windows** ([repo](https://github.com/bitcode/tmux-windows)): claims a native Win32 port of the actual tmux C codebase (v3.6a-win32-1.0.7, April 2026) — but ~0 stars, unproven, single-author; treat as curiosity, not competition.
  - **zmx**-style attach/detach utilities exist for Unix only.
- **Strategic read:** psmux is *complementary* prior art (it validates ConPTY-mux feasibility in Rust and the tmux command-language as a compatibility surface for gmux's CLI), not a substitute — it renders through whatever host terminal it runs in, so it inherits Windows Terminal's missing toasts and can never own GPU rendering, notification routing, or a GUI attention model.

---

## (f) New 2025–2026 purpose-built agent-terminal competition

- **wmux** ([openwong2kim/wmux](https://github.com/openwong2kim/wmux)) — **the closest thing to gmux that exists.** Native-Windows *purpose* (no WSL), Electron + React 19 + xterm.js/WebGL *implementation*; node-pty→ConPTY; **daemon-owned PTYs that survive app quit, crash, and reboot**; agent auto-detection (Claude Code, Codex, Gemini CLI, Aider); **Windows toasts, taskbar flash, audio cues**; per-pane execute-approval gates; agent-to-agent messaging ("channels"); zero-config MCP server registration; CDP/Playwright browser panel; fleet/cockpit view; tmux-style prefix keybindings. MIT, v3.12.0 (July 2, 2026), 225 stars / 74 releases — young but shipping fast. **It fails exactly one gmux requirement: it's Electron.** gmux's thesis must therefore be "wmux's feature set at native performance with a real GPU grid" — and gmux should study wmux's daemon and notification UX closely.
- **Claude Squad** ([smtg-ai/claude-squad](https://github.com/smtg-ai/claude-squad)): Go TUI managing a tmux session + git worktree per agent. **No native Windows support (hard tmux dependency)** — Windows users are told to use WSL2 ([Ardalis guide](https://ardalis.com/setting-up-claude-code-agent-teams-with-wsl2-and-tmux-on-windows/)). Confirms the tmux dependency is the Windows blocker across this whole tool class.
- **Conductor**: macOS-only agent dashboard. **Crystal** (Stravu): deprecated Feb 2026, users redirected to Nimbalyst. **Vibe Kanban** (Bloop): open source, web-UI kanban for agents; company shut down early 2026, community-maintained. None is a Windows terminal grid ([Augment Code roundup](https://www.augmentcode.com/tools/open-source-agent-orchestrators), [Nimbalyst comparison](https://nimbalyst.com/blog/best-multi-agent-coding-tools-2026/)).
- **MOLTamp**: Electron "skinnable shell wrapper" with glanceable agent status, macOS+Windows (self-published blog; low trust, low threat).
- **Warp** (see §d): shipped the multi-agent management UI + notifications on Windows in April 2026 — the incumbent to beat on UX, beatable on openness/scriptability/detach.

---

## The gap, precisely stated

As of July 2026 there is **no open-source, Windows-native, non-Electron, GPU-accelerated GUI terminal that (1) multiplexes ConPTY sessions in-app with tmux-style detach/reattach and reboot-surviving session restore, (2) converts OSC 9/777/99 from any CLI agent into real Windows toast notifications plus per-pane attention indicators, and (3) exposes a local CLI + named-pipe automation API (`\\.\pipe\gmux`) for orchestrators.**

Every neighbor misses structurally:
- **Windows Terminal**: no mux/detach, toast PR abandoned, no extension path to add either. Contributes: MIT ConPTY redist to bundle, AtlasEngine design to copy.
- **WezTerm**: has the mux and Lua, but Windows toasts are buggy, no agent UX, and the project has slowed to nightlies. Contributes: Rust architecture blueprints.
- **Warp**: has the agent UX on Windows but is closed, unscriptable-by-outsiders, and has no detach or standard-OSC story.
- **wmux**: has the features, on Electron — the performance/footprint ceiling is its permanent weakness.
- **psmux**: has the mux semantics, but is a TUI inside someone else's renderer and notification stack.
- **libghostty/winghostty**: rendering core exists for Windows but no mux, no agent features, no stable embedding release.

**Nearest-term threats to this gap:** (1) libghostty tagging a Windows-capable release → someone builds "cmux for Windows" quickly; (2) Windows Terminal reviving the OSC 777 toast PR; (3) wmux rewriting its renderer off Electron; (4) Warp adding detach + an automation API. None had happened as of 2026-07-04.

---

## Component lift list for gmux

| Component | Source | License | Verdict |
|---|---|---|---|
| ConPTY redist pair (`conpty.dll` + `OpenConsole.exe`) | [NuGet](https://www.nuget.org/packages/CI.Microsoft.Windows.Console.ConPTY) / microsoft/terminal release assets | MIT | **Bundle it** (ship matched pair; newer than in-box; what WezTerm does) |
| AtlasEngine | microsoft/terminal `src/renderer/atlas` | MIT | Study/port the design (atlas + D3D11 + IBackend split); don't lift code |
| libghostty-vt | ghostty-org/ghostty | MIT | Candidate VT parser/screen-model layer — Windows-compatible today, but pin a commit (no tagged releases) |
| WezTerm mux/domain architecture | wezterm/wezterm | MIT | Reference architecture for session daemon + domains; fork-mining OK |
| tmux command language surface | psmux precedent | MIT (psmux) | Adopt tmux-compatible CLI verbs for instant familiarity |
| cmux feature spec (sidebar, `notify` CLI, OSC handling) | manaflow-ai/cmux | reported GPL-3.0 — verify | **Spec only, no code** until license confirmed |
