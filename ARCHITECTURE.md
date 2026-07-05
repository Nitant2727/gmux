# gmux — Architecture

> **Status: PROPOSED — awaiting review.** No feature code exists yet; per the working method, implementation
> starts only after this document is approved.
>
> gmux is a Windows-native, GPU-accelerated GUI terminal multiplexer purpose-built for running multiple AI
> coding agents (Claude Code, Codex CLI, Aider, Gemini CLI, …) in parallel — an independent Windows
> equivalent of cmux (macOS). Priorities, in order: (1) a correct VT/ConPTY terminal core, (2) notification
> hooks that actually work (OSC 9/777/99 → Windows toasts + pane attention indicators), (3) native
> tmux-style multiplexing, (4) programmability via CLI + named-pipe API.
>
> Everything in this document is backed by the research corpus in [docs/research/](docs/research/)
> (eight web-verified deep-dives, 2026-07-04/05). Every load-bearing claim was adversarially fact-checked
> against primary sources; the verdict log is [docs/research/verification.md](docs/research/verification.md)
> (32 verdicts: no refutations, all nuance corrections folded in).

---

## Table of contents

1. [The gap gmux fills](#1-the-gap-gmux-fills)
2. [ADR-001: Stack — Rust](#2-adr-001-stack--rust-candidate-a)
3. [System overview: the three-role process model](#3-system-overview-the-three-role-process-model)
4. [Module boundaries](#4-module-boundaries)
5. [PTY layer: ConPTY integration plan](#5-pty-layer-conpty-integration-plan)
6. [VT layer: parser and terminal state](#6-vt-layer-parser-and-terminal-state)
7. [Notification hooks: OSC → toast design (the killer feature)](#7-notification-hooks-osc--toast-design)
8. [Renderer](#8-renderer)
9. [Multiplexer / session model](#9-multiplexer--session-model)
10. [IPC: the named-pipe API and CLI](#10-ipc-the-named-pipe-api-and-cli)
11. [Session persistence and restore](#11-session-persistence-and-restore)
12. [Configuration](#12-configuration)
13. [Security](#13-security)
14. [Testing strategy](#14-testing-strategy)
15. [Risk register](#15-risk-register)

---

## 1. The gap gmux fills

As of July 2026 there is **no open-source, Windows-native, non-Electron, GPU-accelerated terminal that
(1) multiplexes ConPTY sessions in-app with detach/reattach and reboot-surviving restore, (2) converts
OSC 9/777/99 from any CLI agent into Windows toasts plus per-pane attention indicators, and (3) exposes a
local CLI + named-pipe automation API** ([research](docs/research/prior-art-gaps.md)). Every neighbor
misses structurally:

- **Windows Terminal** — no mux/detach (open since 2019), and its OSC 777→toast work only just merged to
  Canary in June 2026 as an **opt-in, OSC 777-only, default-off** compatibility flag. OSC 9 (what Codex and
  Gemini actually emit) and OSC 99 remain unsupported. No automation API beyond `wt.exe` launch args.
- **WezTerm** — real mux, but Windows toasts are buggy, no per-pane agent attention UX, and the project has
  slowed to nightlies (single maintainer).
- **Warp** — shipped multi-agent UX on Windows (April 2026) but: no detach, no external automation API, its
  notifications use Warp's own agent framework rather than standard OSC, and the client is AGPL with a
  proprietary cloud attached.
- **wmux** (Electron) and **psmux** (TUI-inside-another-terminal) validate demand but each fails a hard
  requirement (performance bar; no GUI/toast ownership respectively).
- **cmux itself** is macOS-only (Windows port is a waitlist item — a timing pressure on gmux).

gmux's wedge: **open (MIT), standards-based (any tool that emits OSC works — no SDK), scriptable
(named pipe + tmux-verb CLI), and sessions-as-infrastructure (daemon-owned, detachable, reboot-restorable).**

Non-goals (v1): no iOS/Android companion, no cloud sync, no custom shell — gmux hosts existing shells.

---

## 2. ADR-001: Stack — Rust (Candidate A)

**Status:** proposed · **Date:** 2026-07-05 · **Research:** [rust-stack](docs/research/rust-stack.md),
[dotnet-stack](docs/research/dotnet-stack.md), [prior-art-gaps](docs/research/prior-art-gaps.md)

### Context

Two candidate stacks were mandated for evaluation: (A) Rust + GPU terminal grid + native shell, and
(B) C#/.NET + WinUI 3 + ConPTY via P/Invoke. A third option (C: embed libghostty, the way cmux does)
emerged during research and is addressed below.

### Decision

**Rust, end to end** — one language for daemon, GUI, and CLI:

| Layer | Choice | Proven by |
|---|---|---|
| PTY | `windows-rs` direct ConPTY + **bundled `Microsoft.Windows.Console.ConPTY` pair** (ADR-002) | WezTerm, Alacritty, Contour, JetBrains |
| VT core | `alacritty_terminal` + side OSC watcher, with a gated `libghostty-vt` spike (ADR-003) | Zed ships alacritty_terminal on Windows |
| Renderer | `wgpu` (DX12 default) + glyphon for MVP → custom damage-tracked glyph atlas for v1 | WezTerm, Rio/Sugarloaf; AtlasEngine as design reference |
| Chrome | `winit` + `egui` (immediate-mode sidebar/tabs/overlays) | Alacritty (winit); egui ecosystem |
| Toasts | Own ~300-line layer over `windows` crate WinRT (ADR-006) | tauri-winrt-notification precedent |
| IPC | `tokio` named pipes, JSON-RPC 2.0 (ADR-005) | Docker/VS Code named-pipe precedent |

### Why not B (C#/WinUI 3)

The platform is healthy (WinAppSDK 2.2, June 2026; ARM64 fine; toasts/pipes trivial) — **but the terminal
grid itself has no viable path**:

1. **No official reusable terminal control exists.** microsoft/terminal#6999 ("Productize the WPF/UWP
   terminal controls") has been open since 2020 and is explicitly ice-boxed. No `Microsoft.Terminal.Wpf`
   package exists on NuGet. The community wrappers are one-maintainer packagings of an unproductized
   internal component; the WinUI 3 variant is self-described "very alpha" (WinUI 3 has no `HwndHost`, and
   the child-HWND swapchain means XAML can never composite attention overlays on top of the grid).
2. **No GPU glyph-atlas terminal renderer has ever shipped in .NET.** Choosing B2 ("own the renderer")
   makes gmux a first-of-its-kind .NET engineering project in exactly the layer where correctness is
   priority #1. Rust has three shipped reference implementations to stand on.
3. The .NET researcher's own verdict: "the rendering layer is the entire risk of this stack."

What B would have bought (fast XAML chrome, trivial toasts, CsWin32 interop) is real but concentrated in
the *easy* 20% of the project.

### Why not C (embed libghostty, like cmux)

libghostty (full: renderer + input) has no tagged release, a C API still in flux, and no official Windows
renderer backend (Direct3D is the stated prerequisite for any Windows work; community ports exist but are
single-maintainer). **Not viable as a dependency today.** However, its extracted VT core **libghostty-vt is
Windows-compatible now and has a high-quality Rust crate** — so gmux captures the useful subset of C inside
stack A (see ADR-003). **Re-evaluation trigger:** the moment libghostty tags a versioned release with a
Windows-capable embedding surface, revisit embedding it wholesale (Ghostty's next minor is ~Sept 2026).

### Consequences

- Single language across daemon/GUI/CLI; static binaries; no runtime installer; per-arch builds for
  x64 + ARM64 (`aarch64-pc-windows-msvc` is Rust Tier 1 since 1.91).
- We own the renderer (months of glyph-atlas work in v1) — mitigated by glyphon for MVP and three
  open-source reference codebases (WezTerm renderer, WT AtlasEngine, Rio Sugarloaf).
- egui chrome trades native-Windows look for velocity and full control of attention effects (ring/badge
  composited in the same render pass — impossible in the B1 child-HWND approach). Accessibility via
  AccessKit is weaker than XAML; accepted for a developer tool, revisit post-v1.
- License: **gmux is MIT.** Entire dependency chain is MIT/Apache-2.0. cmux is GPL-3.0 —
  **behavior-level parity only, never code** (see DECISIONS.md D-010).

---

## 3. System overview: the three-role process model

The single most consequential design fact ([research](docs/research/mux-architecture.md)): **a ConPTY dies
with the process that created it.** `ClosePseudoConsole` (or creator crash) sends `CTRL_CLOSE_EVENT` to
every attached client; there is no supported API to re-parent or serialize a live ConPTY. Windows Terminal
has no detach precisely because its GUI owns the PTYs. Therefore:

> **The ConPTYs must be owned by a long-lived, headless, per-user daemon. The GUI is a thin client.
> Detach = the GUI disconnects. Reattach = any GUI reconnects and re-mirrors state.**

This is the shipped pattern on Windows: `wezterm-mux-server.exe` (AF_UNIX transport via `uds_windows`),
VS Code's ptyHost, psmux. One honest caveat from verification: wezterm's Windows mux has open
crash/OOM issues at 26–30+ panes — it proves the *architecture*, not robustness at multi-agent scale;
VS Code's ptyHost is the at-scale proof. gmux's protocol therefore bakes in bounded frame sizes,
backpressure, and non-recursive dispatch from day one (§10).

One binary, three roles:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  gmux.exe --daemon        (headless; auto-spawned on demand; survives GUI)  │
│                                                                             │
│   mux-core: Session ▸ Window ▸ Pane tree          per pane:                 │
│   VT parser + grid + scrollback (canonical)        ┌──────────────────────┐ │
│   attention/notification state machine             │ ConPTY (bundled pair)│ │
│   persistence (layout + VT snapshots)              │ Job object (kill-on- │ │
│   pipe server  \\.\pipe\gmux.<sid>                 │  close) ▸ shell tree │ │
└──────────────────┬──────────────────────────┬──────┴──────────────────────┘ │
                   │ JSON-RPC control plane   │ binary frames (pane output,    │
                   │                          │ damage, resize)                │
        ┌──────────┴───────────┐   ┌──────────┴───────────┐                    │
        │ gmux.exe (GUI)       │   │ gmux.exe <subcommand>│  + agent hook      │
        │ winit+wgpu renderer  │   │ CLI: send-keys,      │    scripts,        │
        │ egui chrome, toasts, │   │ capture-pane, notify,│    orchestrators   │
        │ taskbar integration  │   │ split, hooks setup…  │                    │
        └──────────────────────┘   └──────────────────────┘
```

- **Daemon owns:** ConPTY handles, child job objects, the VT parser, grid + scrollback (canonical state),
  the session tree, attention state, persistence, and the pipe server. If the GUI crashes or updates,
  agents keep running.
- **GUI owns:** rendering (fonts, atlases), input translation, toast delivery + taskbar
  flash/badge/progress (they need a window), config UI. It holds only a render mirror of visible
  lines + on-demand scrollback ranges (WezTerm's `ClientPane` model — the server pushes damage, the
  client pulls history).
- **CLI:** a thin pipe client; every CLI verb is exactly one RPC method (tmux discipline: the CLI *is* the
  protocol).
- Toast *clicks* must focus a pane even if the GUI exited: the toast's COM activation relaunches
  `gmux.exe`, which connects to the daemon and attaches focused on the target pane.
- The daemon is **not a Windows service** (session 0 would break tokens/environment); it's a per-user
  process auto-started on demand and optionally at login (Run key), exiting only on `gmux kill-server`.
- **Build note:** mux-core is a *crate*, not initially a process. MVP milestones M1–M5 run it in-process in
  the GUI (same interfaces, channel transport instead of pipe); the process split lands at M6 (detach).
  This keeps early milestones simple without ever letting the GUI reach around the boundary.

---

## 4. Module boundaries

Cargo workspace; each crate owns one boundary and is testable headless:

| Crate | Responsibility | Key deps |
|---|---|---|
| `gmux-pty` | ConPTY lifecycle (bundled DLL loading, spawn, resize, job objects, reader/writer threads) | `windows` |
| `gmux-vt` | VT parser + grid + scrollback + **OSC event extraction** (ADR-003); emits `TermEvent`s | `alacritty_terminal`/`vte` or `libghostty-vt` |
| `gmux-mux` | Session/Window/Pane tree, addressing, layout, attention state machine, persistence | `gmux-pty`, `gmux-vt` |
| `gmux-proto` | Wire types for the pipe protocol (serde), version negotiation, framing | `serde` |
| `gmux-server` | Pipe server, RPC dispatch, event subscriptions, the `--daemon` entry | `tokio`, `gmux-mux` |
| `gmux-client` | Pipe client library (used by GUI and CLI) | `tokio`, `gmux-proto` |
| `gmux-gui` | winit window, wgpu renderer, egui chrome, input, IME, DPI | `wgpu`, `winit`, `egui` |
| `gmux-notify` | Toast layer (AUMID registration, XML build, activation), FlashWindowEx, ITaskbarList3 | `windows` |
| `gmux-cli` | Argument parsing → RPC calls; `hooks setup` installers | `gmux-client` |
| `gmux` (bin) | Role dispatch: no args → GUI; `--daemon` → server; else CLI | all |

Dependency rule: `gmux-vt` and `gmux-mux` know nothing about rendering, pipes, or toasts — they emit typed
events. Everything user-visible subscribes.

---

## 5. PTY layer: ConPTY integration plan

Full detail: [docs/research/conpty.md](docs/research/conpty.md). The plan:

### 5.1 Bundle Microsoft's ConPTY (ADR-002 — load-bearing for the killer feature)

The passthrough behavior of ConPTY decides whether notification hooks are even possible:

- **Modern ConPTY (WT 1.22+ rewrite, PR #17510, mid-2024): arbitrary OSC/DCS pass through verbatim and in
  order.** The old VtEngine re-render pipeline was deleted; "any VT output that an application generates
  will now be given to the terminal unmodified."
- **Inbox conhost on Windows 10 21H2 is frozen at a ~early-2020 baseline** — unknown-OSC delivery there is
  unreliable-to-broken (pre-#4896 behavior). Even recent Windows 11 inbox conhosts lag the fixed pipeline
  by months-to-years.
- **Mitigation, and the ecosystem norm:** ship the MIT-licensed matched pair `conpty.dll` + `OpenConsole.exe`
  from the **`Microsoft.Windows.Console.ConPTY` NuGet (1.24.260512001, 2026-05-22, min OS 10.0.17763)**
  beside gmux.exe. WezTerm, Alacritty (opt-in), Contour, and JetBrains do exactly this; Microsoft
  recommends it for third-party terminals (terminal discussion #17608).

Implementation: extract the nupkg in CI (pinned by hash, sourced from nuget.org directly);
`LoadLibrary("conpty.dll")` + `GetProcAddress` the `Conpty*` exports (never kernel32's
`CreatePseudoConsole` — that always uses inbox conhost). Create with
`PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES`. Feature-detect: if the bundled pair is missing/blocked, fall back to
kernel32 **with a visible warning** that notification hooks are degraded. Arch rule: `conpty.dll` matches
the gmux process arch; `OpenConsole.exe` native per arch (the DLL probes beside itself, then
`x64/arm64/x86` subfolders); ship per-arch bundles, update only as a matched pair. Verified nuance: WezTerm
currently still bundles the 1.22.250204002 pair — its 1.24 bump (wezterm#7774/#7775) is an open PR held on
supply-chain review — so gmux starts at ≥1.24 rather than inheriting anyone's lag.

### 5.2 Lifecycle per pane

```
spawn:  CreatePipe ×2 (sync, anonymous) → ConptyCreatePseudoConsole(size, flags)
        → STARTUPINFOEX + PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE → CreateProcessW (suspended)
        → CreateJobObject + KILL_ON_JOB_CLOSE → AssignProcessToJobObject → ResumeThread
        → close our copies of the child-side pipe ends
run:    dedicated reader thread (drains output pipe → gmux-vt), dedicated writer (input)
        — never single-threaded (documented deadlock)
kill:   close input write end → ConptyClosePseudoConsole → keep draining until pipe EOF
        (pipe EOF, not process exit, is session end) → close job handle (kills stragglers)
```

- **Never call Close on the reader thread**; pre-24H2 `ClosePseudoConsole` blocks until drained.
- Job object per pane guarantees agent process *trees* (node/python children) die with the pane and can't
  orphan on daemon crash.

### 5.3 Host protocol obligations (new-ConPTY contract)

The bundled 1.24 pair makes gmux a *peer* in a richer protocol; these are requirements, not options:

1. **Answer DSR-CPR (`CSI 6 n`) always** — modern ConPTY requests the cursor position after resizes
   (#18725; shipped via PR #19535 in the 1.25 line after a reverted first attempt, and headed into
   serviced inbox conhosts). An unanswered query costs a ~500 ms bounded stall per console-API call plus
   stale cursor coordinates for legacy clients — so reply generically, from day one.
2. **Answer DA1 (`CSI c`)** — new ConPTY forwards client device-attribute queries to the host.
3. **Implement win32-input-mode** (`CSI ? 9001 h`, spec #4999): encode full KEY_EVENT_RECORDs. Required
   for PSReadLine fidelity, key-up events, Ctrl+Space, shifted F-keys. Do not encode mouse as win32-input.
4. **SGR mouse encoding** when the client sets DECSET 1000/1002/1003/1006.
5. **Focus reporting (DECSET 1004)** — send `CSI I`/`CSI O` at frame boundaries. Codex CLI's
   `notification_condition=unfocused` depends on it.
6. **DECSET 2026 synchronized output** — batch frames when the client requests it.
7. **Resize discipline:** create at correct initial size; never resize between CreateProcess and first
   output; debounce ~50 ms; re-assert final geometry after drag ends (resizes near attach can be dropped,
   #10400).
8. **UTF-8 split handling:** both pipes are always UTF-8; buffer partial code points across ReadFile chunks.

### 5.4 Per-shell notes

PowerShell 5/7, cmd, Git Bash (MSYS2 bash is a Cygwin-runtime console client — no winpty needed under a
ConPTY host), and WSL (wsl.exe relays a real Linux pty; agent OSC flows Linux pty → wsl.exe → ConPTY → gmux)
are all well-trodden under bundled ConPTY ≥1.24. A PowerShell-exit crash (0x80131623) is reported against
the 1.22 pair with the 1.24 line as the proposed fix (wezterm#7774) — one more reason gmux ships ≥1.24.

---

## 6. VT layer: parser and terminal state

### ADR-003: `alacritty_terminal` + side OSC watcher now; gated `libghostty-vt` spike

**The problem:** `alacritty_terminal` (0.26, Apache-2.0, proven on Windows by Zed) has the best-tested
grid/state model in Rust — but its ANSI layer **silently discards OSC 9/99/777** (verified against vte's
`ansi.rs`: unrecognized OSC hits `unhandled()` → debug-log → drop; the `Handler` trait has no unknown-OSC
hook). gmux's killer feature lives exactly in those sequences.

**Decision (two-phase):**

1. **Default path (MVP):** `alacritty_terminal` as grid/state + a **side `vte::Parser` with a minimal
   `Perform` impl watching only `osc_dispatch`/BEL** running over the same byte stream in the daemon's
   reader path. `vte::Perform::osc_dispatch(params: &[&[u8]], bell_terminated)` surfaces raw OSC bytes —
   everything the notification pipeline needs. Double-parsing costs one linear state machine over agent
   output; negligible against a 16.6 ms frame budget.
2. **Week-one spike (time-boxed, M0):** validate `libghostty-vt` 0.2 (Rust crate by Ghostty maintainers,
   MIT OR Apache-2.0; needs Zig 0.15.x; **the same VT core cmux embeds**) on Windows x64 **and** ARM64.
   Verified: the crate's own Windows CI already builds both targets (Zig 0.15.2, passing since May 2026) —
   what remains unproven is **runtime behavior** (CI doesn't execute tests on Windows; upstream tracking
   issue open). The spike exercises OSC dispatch, resize reflow, and grapheme widths against a corpus. If
   clean, **adopt it as `gmux-vt`'s engine instead** — first-class OSC (no side parser), real reflow
   (alacritty's weak spot), cmux-parity semantics. If not, stay on path 1; re-test at each release.

`wezterm-term` was rejected: ideal OSC surface (`Alert::ToastNotification`) but not published on crates.io —
a git dependency on a single-maintainer monorepo whose release cadence has stalled.

### Events out of `gmux-vt`

The VT layer emits typed events; nothing above it re-parses bytes:

```rust
enum TermEvent {
    Damage(RegionSet), TitleChanged(String), Bell,
    Notification { kind: NotifKind /* Osc9 | Osc777 | Osc99{..} */, title: String, body: String,
                   urgency: Urgency, honor_when: HonorWhen, id: Option<String> },
    Progress { state: ProgressState, pct: Option<u8> },            // OSC 9;4
    CwdChanged(PathBuf),                                           // OSC 7 / OSC 9;9 / OSC 1337 CurrentDir
    PromptMark(PromptMark),                                        // OSC 133 A/B/C/D(exit)
    Hyperlink(..), ClipboardWrite(..),                             // OSC 8 / OSC 52 (write-only)
    PtyWriteBack(Vec<u8>),                                         // OSC 99 p=? capability replies
}
```

---

## 7. Notification hooks: OSC → toast design

The killer feature. Full wire-protocol detail: [docs/research/osc-notifications.md](docs/research/osc-notifications.md);
toast mechanics: [docs/research/windows-toasts.md](docs/research/windows-toasts.md).

### 7.1 What gmux parses (cmux-parity ingestion; exceeds every Windows terminal)

| Sequence | Form | Notes |
|---|---|---|
| **OSC 9** | `ESC ] 9 ; msg BEL/ST` | Codex CLI's `osc9` channel (its `auto` mode allowlists known terminals and falls back to BEL elsewhere — hence §7.4 onboarding). ConEmu `9;N` sub-protocol disambiguated first (ADR-008): payload matching `^\d+(;|$)` → strict-parse subcommand (`9;4` progress → taskbar, `9;9` cwd, `9;12` prompt mark); parse failure → treat as notification; unknown numeric subcommand → swallow. systemd ≥257 emits `9;4` — must never toast it. |
| **OSC 777** | `ESC ] 777 ; notify ; title ; body BEL/ST` | **What auto-mode Gemini CLI (≥0.49) emits in terminals it doesn't recognize — gmux receives these zero-config.** Split on first two `;` after `notify`; body keeps remaining semicolons; missing body → text is title. |
| **OSC 99** | kitty protocol | v1 subset: metadata parse (`:`-separated k=v, ignore unknown keys), `i=`/`d=` chunk reassembly (per-id buffers, ~1 MiB cap, stale expiry), `p=title`/`p=body`, `e=1` base64, `u=` urgency, `o=` honor-when, `a=focus`, and **the `p=?` capability query — gmux must reply on the pty** (advertising exactly the supported subset). Buttons/icons/sounds/close-events deferred; omitted from the `p=?` reply so clients degrade gracefully. |
| **BEL** | `0x07` | Attention event for unfocused panes (debounced — TUIs ring repeatedly). Claude Code `terminal_bell`, Codex/Gemini fallback. |
| **OSC 133** | `A/B/C/D;exit` | Semantic prompt marks: `D→A` = **shell idle at prompt** → "pane idle" state (catches unconfigured agents finishing); `D;≠0` → error-tinted mark. |
| **OSC 0/1/2** | title | Pane header text; title change in unfocused pane = weak attention signal. |
| **OSC 7 / 9;9** | cwd | Feeds sidebar, new-split-inherits-cwd, restore. |
| **OSC 8 / 52** | hyperlink / clipboard | 52: write-only, size-capped; queries denied (exfiltration). |

Both BEL and ST terminators accepted everywhere; CAN/SUB/ESC abort; OSC accumulation capped (64 KiB);
UTF-8 sequences split across reads buffered.

### 7.2 The attention pipeline

```
pane output ──▶ gmux-vt ──TermEvent::Notification/Bell/PromptMark──▶ attention state machine (daemon)
                                                                        │  per-pane state:
                                                                        │  Quiet → Pending(unread) → Cleared
                                                                        ▼
                                    pane-attention event on pipe (subscribable by anything)
                                                                        │
                       ┌────────────────────────────────────────────────┤
                       ▼ GUI                                            ▼ CLI/scripts
   ring + sidebar badge + notification panel                    `gmux wait-for`, orchestrators
   + policy-gated: Windows toast, FlashWindowEx,
     taskbar overlay badge, taskbar progress
```

**Semantics (cmux-parity, foot-informed):**

- Every notification is **attributed to its pane** (it arrived on that pane's PTY, or carried `GMUX_PANE`
  via the CLI). Attribution powers everything else.
- **Record always** (in-app notification panel + pane ring + sidebar badge + unread count), **toast only
  when the user isn't already looking**: suppress the OS toast iff gmux is focused AND the originating pane
  is visible in the active workspace (and honor OSC 99 `o=unfocused/invisible`). WezTerm's default-always
  is the documented anti-pattern.
- **Clear on focus:** when the originating pane gains focus, mark read, stop the ring, decrement badges,
  and `ToastNotificationHistory.Remove(tag, group)` so Action Center never shows stale "needs input".
- **Rate limiting:** per-pane token bucket (1/sec, burst 3); repeats collapse into a counter on the
  existing toast (`Tag` = pane id, `Group` = session id → same-tag re-show replaces, never stacks).
- **Escalation ladder by app state:**

| gmux state | Channels |
|---|---|
| focused, pane visible | in-app ring only |
| focused, pane elsewhere | ring + sidebar badge + panel |
| unfocused / minimized | + toast + FlashWindowEx + taskbar overlay badge (count) |
| DND / Focus Assist detected | skip toast; badge + flash carry it (detection: `NotificationMode` API on Win11; undocumented WNF query on Win10 — if undetectable, always run badge+flash alongside toasts) |
| toasts disabled per-app (`ToastNotifier.Setting`) | badge + flash + one-time in-app hint |
| elevated process | badge + flash only (toasts unavailable elevated) |
| agent running, no input needed | OSC 9;4 → `ITaskbarList3::SetProgressState/Value` only (aggregate: any-error → red; else any-indeterminate; else mean) |

### 7.3 Toast delivery (ADR-006: classic WinRT, not Windows App SDK)

- **API:** inbox `Windows.UI.Notifications.ToastNotificationManager.CreateToastNotifier(aumid)` via the
  `windows` crate. **Registration is registry-only** — `HKCU\Software\Classes\AppUserModelId\gmux`
  (`DisplayName`, `IconUri`, optional `CustomActivator` CLSID; the COM activator additionally needs an
  `HKCU\Software\Classes\CLSID\{guid}\LocalServer32` entry — still HKCU, still unelevated); no Start-Menu
  shortcut, no package identity, no elevation. Verified as the same mechanism WCT's
  `ToastNotificationManagerCompat` and WinAppSDK's unpackaged path use under the hood. Toast images must
  be local `file:///` paths (no http for unpackaged apps). Also
  `SetCurrentProcessExplicitAppUserModelID("gmux")` so taskbar grouping, the Settings toggle, and toast
  attribution agree.
- **Why not WinAppSDK `AppNotificationManager`:** it drags the machine-wide Windows App Runtime
  *Singleton* MSIX into an otherwise dependency-free Rust binary (self-contained deployments must gate on
  `IsSupported()`; Register() fails without the runtime — issue #6071). Everything gmux needs (text,
  buttons, progress, tag/group replace, urgent scenario) is in the inbox XML schema.
- **Activation:** while gmux runs (the normal case), the in-process `ToastNotification.Activated` event
  delivers `arguments` (`pane=%5;action=focus`) — no COM. A COM `INotificationActivationCallback`
  LocalServer (CLSID in `CustomActivator`) is added for click-after-GUI-exit (relaunch → connect to daemon
  → focus pane). Buttons: `Focus pane` (default click), `Dismiss`.
- **Foreground-rights reality:** the click activates while ShellExperienceHost holds foreground;
  `SetForegroundWindow` can silently degrade to a taskbar flash. Ladder: `ShowWindow(SW_RESTORE)` +
  `SetForegroundWindow` on the activation thread → verify `GetForegroundWindow()` → fall back to
  `FlashWindowEx` (never inject synthetic input). **Regardless of focus outcome, select the target
  workspace+pane internally** so the next taskbar click lands right.
- Hygiene: silent audio default, `ExpiresOnReboot=true`, `scenario="urgent"` only as an opt-in escalation
  for "agent blocked > N minutes".

### 7.4 Agent onboarding — the part everyone else misses (ADR-011)

**Claude Code's default (`preferredNotifChannel: auto`) emits *nothing* in unrecognized terminals** — it
only auto-notifies in iTerm2/Ghostty/Kitty. An unconfigured gmux would look broken. So:

1. **`gmux hooks setup [agent]`** (cmux-parity) writes per-agent config:
   - **Claude Code:** a `Notification`-event hook returning `{"terminalSequence": "]777;notify;…"}`
     (officially allowlisted: OSC 0/1/2, 9 incl. 9;4, 99, 777, BEL — race-free, works everywhere), or set
     `preferredNotifChannel` explicitly.
   - **Codex CLI:** `tui.notification_method = "osc9"` — required: its `auto` mode is a terminal-name
     allowlist (Ghostty/iTerm2/Kitty/Warp/WezTerm) that degrades to bare BEL everywhere else
     (+ document `notify = ["gmux","notify",…]`).
   - **Gemini CLI:** `general.enableNotifications = true`; since v0.49 its `auto` already emits OSC 777
     in unrecognized terminals, so gmux works immediately — `general.notificationMethod = "osc777"` pins it.
   - **Aider:** emits no escape sequences at all → `--notifications-command "gmux notify --title aider …"`.
2. **`gmux notify --title … --body … [--pane %N]`** CLI: resolves the pane from `GMUX_PANE` (injected into
   every spawned pane's environment) and injects into the same attention pipeline as wire OSC.
3. **Zero-config fallbacks still light panes up:** BEL and OSC 133 idle-at-prompt are attention signals, so
   even a totally unconfigured agent gets a ring the moment it stops at a prompt.
4. Set `TERM_PROGRAM=gmux` / `TERM_PROGRAM_VERSION` — never spoof kitty/ghostty identity (capability lies
   break other tools); pursue first-class gmux detection upstream in agent CLIs.

### 7.5 Notification security

Notification text is attacker-influenced (agents echo repo content; the Codex ANSI-injection RCE is the
cautionary tale): strip C0/C1 from title/body, XML-escape before toast payloads, sanitize OSC 99 ids
(`[a-zA-Z0-9_\-+.]`) before echo-back, cap sizes, and rate-limit (§7.2).

---

## 8. Renderer

- **MVP:** `wgpu` with **DX12 backend default** (Vulkan/GL fallback) — Zed rejected Vulkan-first on Windows
  after real driver pain, and ARM64 Adreno Vulkan drivers are new/inconsistently present (the Compatibility
  Pack can silently substitute a limited DX12 wrapper); DX12 is the low-risk path on both arches. ARM64
  build note (verified): avoid wgpu's `static-dxc` feature — it doesn't build on `aarch64-pc-windows-msvc`;
  default FXC or dynamic DXC works. Text via **glyphon** (cosmic-text shaping — HarfRust + swash; ligatures
  supported) drawing the visible grid with line-level damage tracking; pin a compatible glyphon↔wgpu pair
  (glyphon typically trails wgpu majors by a few weeks).
- **v1:** replace glyphon's generic path with a **custom cell-grid atlas renderer** — DWrite-or-swash
  rasterized glyphs cached in R8 (grayscale) + RGBA8 (emoji) atlases, instanced quads, per-row damage,
  scroll-by-offset. Design references: WT AtlasEngine (`src/renderer/atlas` — study, don't lift), WezTerm's
  GlyphCache, Rio's Sugarloaf.
- Chrome (sidebar, tabs, panel, attention ring/glow) is egui in the same render pass — the ring composites
  *over* the grid, which the child-HWND approaches structurally cannot do.
- The renderer consumes the GUI-side mirror (damage + visible lines + cursor + palette) — it never touches
  ConPTY or the parser. Frame pacing: render on damage or input, idle otherwise (agents stream for hours;
  no busy redraw).
- IME/CJK via winit `Ime` events with `set_ime_cursor_area` at the cursor cell (Alacritty precedent);
  per-monitor-v2 DPI via winit `ScaleFactorChanged`.

---

## 9. Multiplexer / session model

Full detail: [docs/research/mux-architecture.md](docs/research/mux-architecture.md).

### 9.1 Object model and addressing (tmux-compatible on purpose)

```
Session ($0, never reused)  ──  named; the detach/attach unit
 └─ Window (@1)             ──  a tab; binary split tree
     └─ Pane (%2)           ──  one ConPTY + grid + scrollback + attention state
```

- Addressing clones tmux verbatim: `-t session:window.pane`, `$id/@id/%id` forms, name-prefix and glob
  resolution, `{last}`/`{up-of}`-style tokens where cheap. Environment injected into every pane:
  `GMUX_SESSION`, `GMUX_WINDOW`, `GMUX_PANE`, plus `TERM=xterm-256color`, `TERM_PROGRAM=gmux`,
  `COLORTERM=truecolor`.
- A **format mini-language** (`#{pane_id} #{pane_current_path} #{pane_pid}`) ships early — it's what makes
  tmux scriptable and agents already know it.
- **Workspace** (the cmux concept — sidebar entry with git branch/cwd/ports/notification state) maps to a
  gmux *window group per repo*: v1 keeps it simple — one workspace = one window; the sidebar lists windows
  with live metadata (branch via lightweight `.git/HEAD` watch, cwd via OSC/PEB, listening ports via
  `GetExtendedTcpTable` filtered to the pane's job-object PIDs, refreshed lazily).

### 9.2 Splits, layout, input

Binary split tree per window (orientation, ratio, zoom flag). Keyboard-first: directional focus
(`Alt+Arrow` default), resize chords, zoom toggle. **No prefix key by default** (explicit product decision
vs tmux; all bindings rebindable — see [Configuration §12](#12-configuration)).

### 9.3 Detach/reattach

- Detach = GUI disconnect (close window / `gmux detach`). Daemon keeps ConPTYs, parsers, scrollback,
  attention state.
- Reattach = `gmux attach [-t session]` or launching the GUI: `hello` → capability negotiation → full
  topology snapshot → per-pane visible-grid snapshot → subscribe to damage. Scrollback stays server-side;
  the GUI pulls line ranges on demand (bounded render cache).
- Multiple simultaneous clients per session are protocol-legal (same PDU flow); v1 ships single-GUI,
  size = last-writer-wins, multi-client hardening in v2.

### 9.4 Remote tmux (v1 scope, design now)

Clone iTerm2's approach: spawn `ssh … tmux -CC attach` in a hidden pane and speak **tmux control mode**
(`%begin/%end` guards, `%output %pane octal-escaped`, `%layout-change`, `%pause/%continue` flow control
with `refresh-client -f pause-after`) — mapping session→gmux session, window→window, pane→pane. Requires
stock tmux ≥3.2 on the remote (version-gate features; degraded mode below 3.2). No gmux binary needed
remotely. The daemon's own event vocabulary deliberately mirrors control-mode semantics so the local and
remote state-mirroring engines share code.

---

## 10. IPC: the named-pipe API and CLI

### ADR-005: named pipe + JSON-RPC 2.0, LSP framing

- **Transport:** `\\.\pipe\gmux.<user-sid-hash>` (docs alias: `\\.\pipe\gmux`; the CLI resolves).
  `FILE_FLAG_FIRST_PIPE_INSTANCE` (defeats squatting) · `PIPE_REJECT_REMOTE_CLIENTS` · explicit DACL
  `D:P(A;;GA;;;SY)(A;;GA;;;<user-SID>)` — never the default DACL (it grants Everyone read). Unlimited
  instances; every CLI call gets its own; overlapped I/O via tokio.
- **Framing:** `Content-Length: N\r\n\r\n{json}` (LSP-style) — trivially speakable from PowerShell, Python,
  Node — the actual audience. Max frame size enforced (WezTerm's unbounded-PDU OOM is the lesson).
  JSON-RPC 2.0 requests/notifications; `hello` first (`clientVersion`, `protocolVersion`, `capabilities[]`).
- **Data plane:** pane output subscriptions default to base64-chunk JSON notifications (fine for scripts);
  the GUI upgrades to a **binary side-channel** (length-prefixed: paneId, seq, bytes) to avoid
  base64+JSON overhead at 10 panes × MB/s.
- **Client auth:** the pipe DACL already restricts to the same user; additionally the server verifies
  clients via `GetNamedPipeClientProcessId`; clients verify the *server* binary path via
  `GetNamedPipeServerProcessId` before sending input. (cmux's stricter parentage-only model rejected:
  external orchestrator access is a gmux feature, not a bug — same-user is the boundary.)

### Method surface = CLI surface (tmux verbs)

```
gmux list-sessions | new-session [-s name] | attach [-t s] | detach | kill-server
gmux new-window [-t s] [-c cwd] | split-pane [-t pane] [-h|-v] [-c cwd] [-p pct] [--] [cmd…]
gmux send-keys -t %5 [-l] "text" Enter        # -l literal, key names otherwise (tmux semantics)
gmux capture-pane -t %5 -p [-S -2000] [-e]    # -S negative = scrollback; -e = with SGR
gmux screenshot -t %5 --out pane.png          # daemon renders grid off-screen → PNG
gmux notify --title T [--body B] [--pane %N]  # GMUX_PANE default
gmux set-status/-progress/log …               # sidebar metadata (cmux parity)
gmux wait-for -t %5 [--quiet-ms 2000 | --bell | --idle]   # orchestration primitive
gmux list-panes -F '#{pane_id} #{pane_current_path}' [-f filter]
gmux hooks setup [claude-code|codex|gemini|aider|all]
gmux subscribe [--events pane-attention,layout-changed,…]  # long-lived event stream (JSON lines)
```

Events published: `pane-output`, `pane-attention`, `pane-exited`, `layout-changed`, `cwd-changed`,
`progress-changed`, `session-created/closed`. `capture-pane`, `send-keys`, `wait-for`, and `subscribe`
are the four calls agent orchestrators live on.

---

## 11. Session persistence and restore

**ADR-009: restore = respawn + replay.** Processes cannot survive reboot; pretending otherwise is the
failure mode. What survives is the *workspace*:

- **Checkpoint** (debounced 2 s after topology/cwd change; periodic scrollback snapshot; flush on
  `CTRL_SHUTDOWN_EVENT`/`WM_ENDSESSION`): layout tree (sessions→windows→split tree→panes, stable ids),
  per-pane cwd, spawn info (command line, env-at-spawn, profile), attention state, and **scrollback as
  VT-encoded UTF-8 text, zstd-compressed** (`scrollback/%5.vt.zst`, last ~5k lines) — Windows Terminal's
  `buffer_<guid>.txt` approach: one parser, no binary schema, forward-compatible; emit OSC 8/133 back out
  so links and prompt marks survive. Atomic write-to-temp + rename. Location: `%LOCALAPPDATA%\gmux\state\`.
- **cwd tracking:** OSC 7 / OSC 9;9 / OSC 1337 `CurrentDir` when shell integration is on (first-run
  snippets for PowerShell/cmd/bash, WT-tutorial style, auto-injectable for gmux-spawned PowerShell);
  fallback: PEB read (`NtQueryInformationProcess` → `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory`) at
  checkpoint time only.
- **Restore:** rebuild tree → replay decompressed VT snapshot into a fresh grid → print divider
  `─── gmux: restored 2026-07-05 09:12 · process not running ───` → spawn shell in saved cwd *under* the
  history. **Never auto-rerun agent commands** (an agent relaunched into a repo can start editing);
  instead, per-agent resume via `gmux hooks setup` (e.g. Claude Code `--resume`) behind explicit approval,
  cmux-style. Secrets (`*TOKEN*`, `*KEY*`, `*SECRET*`, `*PASSWORD*` env) scrubbed before persisting.
- **Scrollback in memory:** per-pane ring of lines (UTF-8 text + RLE attribute spans, not cell arrays);
  default 10k lines ≈ 1–2 MB/pane for typical agent output; cold segments zstd-compressed in memory past a
  per-pane budget; process high-water eviction. Alternate-screen output (TUIs) accumulates no scrollback by
  definition.

---

## 12. Configuration

- `%APPDATA%\gmux\gmux.json` (JSON with comments tolerated), hot-reloaded on change. Namespaces:
  `terminal` (font, ligatures, scrollback_lines, shell profiles), `keybindings` (every action rebindable;
  **no-prefix defaults**), `notifications` (per-channel toggles, suppression policy, rate limits, custom
  notification command with `GMUX_NOTIFICATION_*` env — cmux parity), `sidebar`, `restore`
  (`autoResumeAgentSessions: false` default), `daemon`.
- Profiles per shell (PowerShell 7 default if present → PowerShell 5 → cmd; Git Bash and WSL distros
  auto-discovered like Windows Terminal does).
- CLI: `gmux config get/set`, and the GUI settings surface writes the same file.

---

## 13. Security

| Surface | Policy |
|---|---|
| Named pipe | Same-user DACL, no remote clients, first-instance flag, server/client mutual PID+path verification, max frame size, no network exposure ever |
| Notification text | Untrusted: C0/C1 strip, XML-escape, id sanitization, size caps, rate limits (§7.5) |
| OSC 52 | Write-only, size-capped, user-notified; clipboard *read* queries denied |
| OSC 8 links | Scheme allowlist (http/https/file/mailto), never auto-open, URI length cap |
| Session snapshots | Env secret scrubbing; state dir is per-user ACL'd |
| Agent resume | Explicit approval UI before any stored resume command runs |
| Child processes | Job objects; no elevation; refuses to run its pipe server elevated (degraded mode) |
| Supply chain | Bundled conpty.dll/OpenConsole.exe pinned by hash from the MIT NuGet; signed releases |

---

## 14. Testing strategy

Per the working method every milestone ships runnable + tested; the standing test architecture:

1. **`gmux-vt` unit tests** — the largest corpus: OSC 9/777/99 (incl. chunk-split across reads, BEL vs ST,
   ConEmu 9;N disambiguation table, malformed-payload fallbacks, OSC 99 chunk reassembly + `p=?` replies),
   CSI/SGR conformance, grid ops, resize/reflow. Golden-file snapshot tests of grid state after replaying
   captured agent transcripts (vttest + esctest imports where applicable).
2. **`gmux-pty` integration** — real ConPTY round-trips on Windows CI (x64 + ARM64 runners): spawn
   cmd/PowerShell/Git Bash, echo OSC sequences, assert `TermEvent`s arrive **in order**; resize storms;
   teardown-drain (no hangs); job-object tree-kill.
3. **Killer-feature integration** — `printf '\e]9;hi\a'` in a real pane → assert `pane-attention` event +
   toast XML produced + suppression matrix honored (focused/unfocused simulated) + clear-on-focus. This
   test is the M0 exit criterion and stays green forever.
4. **`gmux-mux` unit** — addressing resolution, layout tree ops, attention state machine transitions,
   persistence round-trip (checkpoint → restore → identical tree + scrollback).
5. **Protocol tests** — golden JSON-RPC transcripts; version negotiation; frame-size rejection; a
   PowerShell-only smoke client (proves the "scriptable from anything" promise).
6. **Manual matrix per release** — CJK IME, RTL, emoji width, high-DPI mixed monitors, ARM64 device pass,
   Focus Assist on/off, elevated child shells.

---

## 15. Risk register

| # | Risk | Sev | Mitigation |
|---|---|---|---|
| R1 | libghostty-vt runtime behavior unproven on Windows (builds pass in its CI; tests don't run there; pre-1.0 API) | M | Time-boxed M0 behavior spike; alacritty_terminal+side-parser is the default path and fully sufficient |
| R2 | Inbox-conhost fallback silently degrades hooks | H | Bundle ConPTY pair; feature-detect + visible warning; integration test pins passthrough |
| R3 | Toast click can't foreground reliably | M | Fallback ladder + internal pane pre-selection (§7.3); honest flash |
| R4 | Win10 DND state undetectable via documented APIs | L | Badge+flash always accompany toasts; WNF query optional behind a flag |
| R5 | glyphon perf ceiling at 10+ panes streaming | M | Damage tracking from day 1; v1 custom atlas plan; WezTerm/AtlasEngine references |
| R6 | Daemon protocol churn / instability at high pane counts (wezterm's Windows mux OOMs at 26+ panes) | M | mux-core-as-crate first (M1–M5), version-negotiated additive-only protocol, bounded frames + backpressure from day 1, load test at 30+ streaming panes |
| R7 | cmux ships Windows port first | M | Their waitlist is unbuilt; gmux's daemon/detach is a differentiator cmux lacks; ship MVP fast |
| R8 | WT ships default-on OSC toasts | L | Merged version is opt-in 777-only; gmux's full matrix (9/99/attribution/panel/API) stays ahead |
| R9 | winit IME edge cases (IMM32) for CJK | L-M | Early manual test matrix; Alacritty precedent |
| R10 | Windows Defender flags conpty spawns/unsigned builds | M | Code-sign releases incl. bundled pair layout; keep OpenConsole.exe name |

---

*Companion documents: [ROADMAP.md](ROADMAP.md) (milestones), [DECISIONS.md](DECISIONS.md) (ADR log),
[docs/research/](docs/research/) (evidence corpus).*
