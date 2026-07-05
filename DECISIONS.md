# gmux — Decision log

Short ADR-style entries; newest last. Long-form rationale lives in [ARCHITECTURE.md](ARCHITECTURE.md);
evidence in [docs/research/](docs/research/). Inline `ADR-0NN` labels in ARCHITECTURE.md map 1:1 to the
`D-0NN` entries below.

---

## D-001 · Stack: Rust end-to-end (over C#/WinUI 3; over embedding libghostty)

2026-07-05 · **Accepted (pending review)**

C#/WinUI 3 fails on the terminal grid: Microsoft's terminal control is unproductized and ice-boxed
(microsoft/terminal#6999, no NuGet), community wrappers are one-maintainer alphas with HWND-airspace
limits, and no GPU glyph-atlas terminal renderer has ever shipped in .NET. Rust has three shipped Windows
references (Zed, WezTerm, Alacritty) covering every layer gmux needs. Full libghostty embedding (cmux's
approach) has no tagged release and no official Windows renderer — **re-evaluate when libghostty tags a
Windows-capable release** (~Ghostty 1.4, Sept 2026 cycle).

## D-002 · Bundle Microsoft's ConPTY redist; never rely on inbox conhost

2026-07-05 · **Accepted** · **M0-verified** ([m0-spikes](docs/research/m0-spikes.md)): the redist
`conpty.dll` (1.24.260512001) loads via `LoadLibrary` from Rust and exports the full `Conpty*` API; OSC
9/777/99 pass through a real ConPTY intact and in relative order. Two facts added from the spike: (1) a
ConPTY child only binds stdio to the pty if the creating process has a real console — the **daemon must
`AllocConsole` when launched headless** (the GUI always has one); (2) attaching the child to the *bundled*
DLL's HPCON vs inbox kernel32 needs a real-hardware follow-up (not an MVP blocker on modern builds).

Notification hooks depend on OSC passthrough. Only the modern ConPTY (WT 1.22+ rewrite) passes arbitrary
OSC through verbatim and in order; Windows 10 21H2's inbox conhost is frozen at a ~2020 baseline that
predates the passthrough fixes. Ship `conpty.dll` + `OpenConsole.exe` from the MIT
`Microsoft.Windows.Console.ConPTY` NuGet (≥1.24 — a PowerShell-exit crash 0x80131623 is reported against
the 1.22 pair), load via LoadLibrary, per-arch pairs, matched versions only, pinned by hash from nuget.org.
Fallback to kernel32 ConPTY only with a visible degraded-hooks warning. (WezTerm/Alacritty/Contour/JetBrains
precedent — noting WezTerm itself still ships the 1.22 pair with its 1.24 bump an open PR; Microsoft
explicitly recommends the NuGet for third-party terminals.)

## D-003 · VT core: alacritty_terminal + side vte OSC watcher

2026-07-05 · **Accepted** · **M0-resolved** ([m0-spikes](docs/research/m0-spikes.md))

alacritty_terminal (proven on Windows by Zed) silently drops OSC 9/99/777 with no unknown-OSC hook, so a
second minimal `vte::Perform` parser watches `osc_dispatch` on the same byte stream (cheap: one linear
state machine). The libghostty-vt alternative was spiked and **rejected**: it builds on Windows x64 (Zig
0.15.2) but surfaces OSC notifications uselessly — no `Terminal` notification callback, and its standalone
`osc::Parser` collapses OSC 9/777 into a payload-less unit variant and **panics on OSC 99** — plus a
Debug-profile `vt_write` segfault. Its one real edge (reflow) doesn't outweigh that. Revisit trigger: a
libghostty-vt release with an `on_desktop_notification` callback that exposes title/body and stops
panicking on OSC 99. wezterm-term stays rejected (unpublished, stalled monorepo).

## D-004 · Process model: daemon owns ConPTYs; GUI is a thin client; one binary, three roles

2026-07-05 · **Accepted (pending review)**

ConPTYs die with their creating process and cannot be re-parented — so detach/reattach requires a
long-lived per-user daemon owning PTYs, parser, scrollback, and attention state (wezterm-mux-server /
VS Code ptyHost / psmux precedent). Not a Windows service (session-0 breaks tokens/env). Job object per
pane with KILL_ON_JOB_CLOSE for tree cleanup. mux-core is built as a crate and runs in-process until M6
to keep early milestones simple; the interface boundary exists from day 1.

## D-005 · IPC: named pipe + JSON-RPC 2.0 (LSP framing); binary side-channel for pane output

2026-07-05 · **Accepted (pending review)**

`\\.\pipe\gmux.<sid-hash>` with explicit same-user DACL, FIRST_PIPE_INSTANCE, REJECT_REMOTE_CLIENTS, max
frame size (WezTerm's unbounded-PDU OOM is the cautionary tale). JSON-RPC chosen over a binary protocol
because scriptability-from-anything (PowerShell/Python/Node) is the product; the GUI's hot path upgrades
to length-prefixed binary frames. CLI verbs mirror tmux (`send-keys -l`, `capture-pane -p -S -2000`,
`#{}` formats) — agents already know them. Unlike cmux's *default* parentage-only mode (it ships five
access modes incl. password and allowAll), gmux allows same-user external clients by design
(orchestrators are a feature); mutual PID/path verification both directions.

**Amended 2026-07-05 (M5):** framing is **newline-delimited JSON** (one request/response object per
line, 1 MiB line cap) instead of LSP `Content-Length` headers — strictly simpler for the scripting
clients the API exists for (cmux precedent), with the same bounded-frame guarantee. Pipe name is
`gmux.<username>` for now (SID-hash suffix when the daemon lands in M6); DACL = SYSTEM + current-user
SID only.

## D-006 · Toasts: classic inbox WinRT + registry AUMID; not Windows App SDK

2026-07-05 · **Accepted** · **M0-verified** ([m0-spikes](docs/research/m0-spikes.md)): registry-only AUMID
+ `CreateToastNotifierWithId` + `Show()` works unpackaged, unelevated, on Win11 (`windows` crate 0.62.2).
Added rule: **do not gate on `notifier.Setting() == Enabled` before the first `Show()`** — a fresh AUMID
returns `0x80070490` "Element not found" on run 1 (Enabled from run 2); treat a `Setting()` error as
first-run, not disabled.

Registry-only registration (`HKCU\Software\Classes\AppUserModelId\gmux`) needs no shortcut, no package
identity, no elevation, and works on Win10 21H2+. WinAppSDK's AppNotificationManager requires the
machine-wide Windows App Runtime Singleton MSIX (self-contained deployments excluded; Register() failures
documented) — wrong trade for a dependency-free Rust binary. In-process `Activated` handles the normal
case; COM activator added for click-after-exit. Foreground-rights fallback ladder; never inject input.

## D-007 · Renderer: wgpu DX12-default; glyphon for MVP, custom atlas for v1

2026-07-05 · **Accepted (pending review)**

DX12 over Vulkan on Windows: Zed rejected Vulkan after driver pain, and ARM64 Adreno Vulkan has
historically been a DX12 wrapper. glyphon/cosmic-text is maintained and good enough for MVP grids;
the v1 damage-tracked glyph-atlas renderer follows WT AtlasEngine (study, don't lift), WezTerm GlyphCache,
and Rio Sugarloaf designs. Chrome is egui in the same render pass so attention rings composite over the
grid (impossible with child-HWND terminal controls).

## D-008 · OSC 9 disambiguation: numeric-prefix test (ConEmu 9;N vs notification)

2026-07-05 · **Accepted (pending review)**

Payload matching `^\d+(;|$)`: implemented subcommands (`9;4` progress → taskbar, `9;9` cwd, `9;12` prompt
mark) strict-parse with fall-back-to-notification on parse failure; unknown numeric subcommands are
swallowed (ConEmu namespace), not toasted. Matches Ghostty/kitty observable behavior; critical because
systemd ≥257 emits `9;4` progress on the wire and Codex/Gemini emit plain OSC 9 prose.

## D-009 · Session restore = respawn + replay; never auto-rerun agents

2026-07-05 · **Accepted (pending review)**

Processes don't survive reboot; the workspace does: layout tree + cwd + spawn info + VT-encoded
zstd-compressed scrollback snapshots (Windows Terminal `buffer_<guid>.txt` precedent — one parser, no
binary schema), replayed as inert history under a divider. Agent resume only via explicit per-agent
commands behind an approval UI (an auto-relaunched agent can start editing a repo). Env secrets scrubbed
before persisting.

## D-010 · License: gmux is MIT; cmux is spec-only reference

2026-07-05 · **Accepted (pending review)**

Entire chosen dependency chain is MIT/Apache-2.0 (incl. the bundled ConPTY pair). cmux is GPL-3.0-or-later:
gmux copies behavior and CLI ergonomics, **never code**. WezTerm/Windows Terminal (both MIT) are legal to
study and mine.

## D-011 · Agent onboarding is a product feature, not documentation

2026-07-05 · **Accepted (pending review)**

Verified per-agent reality: Claude Code's default channel does nothing in unrecognized terminals; Codex's
`auto` is a terminal allowlist that degrades to bare BEL elsewhere; Aider emits no escape sequences at all;
only Gemini CLI (≥0.49) auto-emits OSC 777 in unknown terminals. Without `gmux hooks setup [agent]`
(Claude Code Notification-hook `terminalSequence`, Codex `tui.notification_method=osc9`, Gemini
`enableNotifications`, Aider `--notifications-command "gmux notify …"`), the killer feature would look
broken for the most important tools. BEL + OSC 133 idle-at-prompt serve as zero-config fallback attention.
`TERM_PROGRAM=gmux` — never spoof another terminal's identity.
