# Candidate Stack A: Rust + GPU terminal grid + native Windows shell

Research date: 2026-07-04. Findings verified against live web sources this session unless marked otherwise.
Scope: VT/terminal-state crates, PTY layer, GPU text rendering, window/chrome frameworks, ARM64, WinRT toast + named pipes, licenses, maturity/risk for gmux (Windows-native GUI terminal multiplexer for AI coding agents).

---

## TL;DR

Stack A is **viable and proven**. Two shipped products already validate almost every layer on Windows:

- **Zed on Windows** (GA 2025): GPUI (custom Rust GPU UI, DirectX 11 + DirectWrite on Windows) + `alacritty_terminal` for its integrated terminal, incl. ConPTY ([zed.dev/windows](https://zed.dev/windows), [windowsforum.com coverage](https://windowsforum.com/threads/zed-editor-arrives-on-windows-with-native-rust-gpu-ui-and-directx-11.384963/), [DeepWiki: Zed terminal integration](https://deepwiki.com/zed-industries/zed)).
- **WezTerm** (Rust, Windows-native, multiplexing built in): termwiz + wezterm-term + custom glyph atlas + OpenGL/wgpu + bundled ConPTY ([wezterm.org](https://wezterm.org/index.html)).

The two decisions that actually matter:

1. **VT core**: `alacritty_terminal` (battle-tested, but its ANSI layer **drops OSC 9/777/99 with no passthrough hook** — needs a cheap side-parser on raw `vte`), vs **`libghostty-vt` via the new official-adjacent Rust crate** (v0.2.0, June 2026 — the same core cmux builds on; Windows-compatible; requires Zig toolchain; pre-1.0), vs `wezterm-term` (surfaces OSC 9/777 as `Alert::ToastNotification` natively but is **not published on crates.io**).
2. **ConPTY strategy**: do NOT rely solely on the in-box OS conhost — **bundle Microsoft's MIT-licensed ConPTY pair** (`conpty.dll` + `OpenConsole.exe`, NuGet `Microsoft.Windows.Console.ConPTY`) like WezTerm/Alacritty/Contour/JetBrains do, to get modern OSC/DCS passthrough fixes independent of the Windows release cycle.

---

## (a) VT parsing / terminal-state crates

### alacritty_terminal

- **Latest: 0.26.0 (2026-04-06), license Apache-2.0** — verified on [docs.rs/alacritty_terminal](https://docs.rs/alacritty_terminal/latest/alacritty_terminal/).
- API shape: `Term<T: EventListener>` (grid + terminal state), `grid` (2D grid optimized for terminals), `event_loop` (PTY I/O thread), `tty` (ConPTY on Windows via `windows-sys`/`miow`), `selection`. ANSI handling lives in the `vte` crate's `ansi` feature (`vte::ansi::Processor`/`Handler`), re-exported by alacritty_terminal.
- **Windows**: full ConPTY support (`x86_64-pc-windows-msvc` etc. are documented targets; the tty module uses ConPTY, requires Win10 1809+). **Proven in production on Windows by Zed**, whose terminal "leverages the alacritty_terminal crate for PTY management and VTE parsing" ([DeepWiki: Zed terminal integration](https://deepwiki.com/zed-industries/zed), [zed.dev/docs/terminal](https://zed.dev/docs/terminal)).
- **CRITICAL LIMITATION — OSC notifications**: verified against [vte's `src/ansi.rs` source](https://raw.githubusercontent.com/alacritty/vte/master/src/ansi.rs): the internal `osc_dispatch` match recognizes OSC `0, 2, 4, 8, 10, 11, 12, 22, 50, 52, 104, 110, 111, 112` only. Everything else — including **OSC 9, 99, 777** — falls into `_ => unhandled(params)` which merely `debug!`-logs and **discards** the sequence. The `Handler` trait has **no passthrough/unknown-OSC hook** (methods are typed: `set_title`, `set_color`, `clipboard_store`, `set_hyperlink`, `dynamic_color_sequence`, `reset_color`, `set_mouse_cursor_icon`, ...).
- **Workaround (cheap and proven pattern)**: run a second, tiny `vte::Parser` over the same PTY byte stream with a minimal `Perform` impl that only watches `osc_dispatch`. The low-level `vte::Perform` trait *does* surface raw OSC: `fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool)` — raw byte slices, so OSC 9 / 777 / 99 can be parsed by gmux directly (verified: [docs.rs/vte Perform trait](https://docs.rs/vte/latest/vte/trait.Perform.html)). vte parsing is a state machine over bytes; double-parsing agent-CLI output volumes is negligible CPU. Alternative: skip `alacritty_terminal::event_loop` and drive `vte::ansi::Processor::advance(&mut handler, bytes)` yourself, tee-ing bytes into the side parser at the same point.
- vte crate itself: **0.15.0 (2026-06-10), Apache-2.0 OR MIT** (verified [docs.rs/vte](https://docs.rs/vte/latest/vte/)). Maintained by the Alacritty org.

### wezterm's termwiz / wezterm-term

- **termwiz**: published on crates.io, **0.23.3, MIT** (verified via [docs.rs termwiz](https://docs.rs/termwiz/latest/termwiz/escape/osc/enum.OperatingSystemCommand.html)). Its escape parser is *ideal* for the notification feature: `OperatingSystemCommand` enum has **`SystemNotification` (OSC 9)**, **`RxvtExtension` (OSC 777)**, and an **`Unspecified(Vec<Vec<u8>>)` catch-all** that preserves raw data for unknown OSC (would capture kitty OSC 99). ~20 variants total (titles, selection/clipboard, colors, hyperlinks, iTerm2 proprietary, ...).
- **wezterm-term** (the `Terminal` state machine that consumes termwiz events and exposes `Alert::ToastNotification`-style alerts to the embedder — WezTerm itself turns OSC 9/777 into toasts, see [wezterm notification_handling docs](https://wezterm.org/config/lua/config/notification_handling.html) and [wezterm #489](https://github.com/wezterm/wezterm/issues/489)): **NOT published on crates.io**. Verified via crates.io search: only a third-party fork exists, `tattoy-wezterm-term` 0.1.0-fork.5 (2025-07-11), plus `shadow-terminal` (a headless terminal built on wezterm components, v0.2.3, 2025-07-28). Using the real thing means a **git dependency on the wezterm monorepo**.
- **Maintenance**: WezTerm is alive — official nightly Copr maintained by the author, nightly release `20260331-040028` exists (2026-03-31) ([Copr](https://copr.fedorainfracloud.org/coprs/wezfurlong/wezterm-nightly/), [releases](https://github.com/wezterm/wezterm/releases)) — but tagged stable releases are infrequent and it is effectively a single-maintainer project. Depending on unpublished workspace crates from a single-maintainer monorepo is a real supply-chain/maintenance risk.

### libghostty / libghostty-vt — status changed materially (game-changer confirmed, with caveats)

- **Official Ghostty Windows app: still not shipped.** Windows support remains "long term roadmap"; as of April–June 2026 maintainers want a Direct3D renderer, Win32/WinUI shell, Windows 10/11 only, incremental PRs — no functional official builds yet (verified: [ghostty-org/ghostty discussion #2563](https://github.com/ghostty-org/ghostty/discussions/2563)).
- **BUT `libghostty-vt` (the extracted VT core — the layer cmux builds on) explicitly supports Windows**: "libghostty-vt is already available and usable today for Zig and C and is compatible for macOS, Linux, Windows, and WebAssembly" (Ghostty project statements surfaced in #2563 and search results). A Windows build blocker (libxml2 symlinks during `zig build`, from the fontconfig dep chain) was **fixed 2026-03-20** by skipping fontconfig/libxml2 on Windows targets ([discussion #11697](https://github.com/ghostty-org/ghostty/discussions/11697), PR #11698). Cross-compiling *from* Windows *to* Linux remains unsupported.
- **Rust bindings now exist and are high quality**: crate **`libghostty-vt` 0.2.0 (~2026-06-20), MIT OR Apache-2.0**, with raw FFI crate `libghostty-vt-sys`, repo [github.com/Uzaaft/libghostty-rs](https://github.com/Uzaaft/libghostty-rs) — built by Ghostty maintainers and publicly endorsed by Mitchell Hashimoto ("Some Ghostty maintainers came together and made a really high quality Rust crate in front of libghostty-vt... The API is beautiful", [x.com/mitchellh](https://x.com/mitchellh/status/2037966943282696213)). Verified on [docs.rs/libghostty-vt](https://docs.rs/libghostty-vt).
  - API: `Terminal` (core type: `vt_write()`, `on_pty_write()`), `TerminalOptions`, `RenderState` + row/cell iterators, `KeyEncoder`, `MouseEncoder`, and a dedicated **`osc` module** for OSC sequence handling. Handles scrollback, line wrapping, **reflow on resize** (something alacritty_terminal is weaker at).
  - Threading: handles are `!Send + !Sync` by design — drive the terminal from one thread (fine for a per-pane reader-thread or single VT thread design).
  - **Build requirement: Zig 0.15.x on PATH**; build.rs fetches Ghostty source (pinned) and builds `libghostty-vt.a` (static by default, dynamic via feature). Network-free builds supported via `GHOSTTY_ZIG_SYSTEM_DIR`. MSRV 1.90.
  - **Stability: pre-1.0, "breaking changes expected"** — explicit warning on docs.rs.
  - Windows caveat: the crate's docs/README only explicitly exercise Linux/macOS; Windows should work (libghostty-vt is Windows-compatible upstream and the fontconfig blocker is fixed) but **treat "cargo build of libghostty-vt-sys on Windows x64 + ARM64" as a week-one spike**, not an assumption.
- Ecosystem proof: [gpui-ghostty](https://xuanwo.io/2026/01-gpui-ghostty/) (Xuanwo, Jan 2026) embeds libghostty-vt in Zed's GPUI and reached htop-grade fidelity; [awesome-libghostty](https://github.com/Uzaaft/awesome-libghostty) lists dozens of consumers. Community Windows ports of full Ghostty exist: [winghostty](https://www.winghostty.com/) (releases since Apr 2026, Win10/11 x64 **and ARM64**, v1.3.115 on 2026-06-26) and [InsipidPoint/ghostty-windows](https://github.com/InsipidPoint/ghostty-windows) (Win32 + OpenGL + ConPTY) — useful as existence proofs that Ghostty's core runs on Windows/ConPTY, not as dependencies.

### VT-core recommendation matrix

| | alacritty_terminal 0.26 | libghostty-vt 0.2 (Rust crate) | wezterm-term (git) |
|---|---|---|---|
| OSC 9/777/99 to embedder | No — dropped; needs side vte parser | Yes — `osc` module; cmux-parity | Yes — `SystemNotification`/`RxvtExtension`/`Unspecified` |
| Windows ConPTY proof | Zed ships it | winghostty/ghostty-windows (indirect) | WezTerm ships it |
| crates.io published | Yes | Yes (needs Zig to build) | **No** (fork only) |
| Resize reflow | Limited | Yes | Yes |
| License | Apache-2.0 | MIT OR Apache-2.0 (Ghostty core: MIT) | MIT |
| Stability | Mature, versioned | Pre-1.0, breaking changes expected | Mature but unpublished, 1-maintainer |

---

## (b) PTY layer: portable-pty vs direct windows-rs ConPTY

### portable-pty (wezterm workspace crate; on crates.io, MIT)

Works — it is what WezTerm ships — but documented quality gaps (verified via search this session):

- **Missing modern ConPTY creation flags**: upstream does not pass `PSEUDOCONSOLE_RESIZE_QUIRK (0x2)` (fixes resize artifacts), `PSEUDOCONSOLE_WIN32_INPUT_MODE (0x4)` (proper key encoding), `PSEUDOCONSOLE_PASSTHROUGH_MODE (0x8)` (relay VT directly). A patched fork exists: [`portable-pty-psmux`](https://lib.rs/crates/portable-pty-psmux).
- **Teardown race**: `SlavePty`/`MasterPty` both hold strong refs to the inner handle; drop order is non-deterministic and can tear down the PTY while live, causing crashes ([rust-scratch #117 investigation](https://github.com/nazmulidris/rust-scratch/issues/117), portable-pty-psmux rationale).
- API docs: [docs.rs portable-pty ConPtyMasterPty](https://docs.rs/portable-pty/latest/i686-pc-windows-msvc/portable_pty/win/conpty/struct.ConPtyMasterPty.html).

### Direct windows-rs `CreatePseudoConsole` (recommended)

gmux is Windows-only, so portability abstraction buys nothing. Direct use of `CreatePseudoConsole`/`ResizePseudoConsole`/`ClosePseudoConsole` via the `windows` crate (`Win32_System_Console` feature) gives full control over creation flags, handle lifetime, and — crucially — **which ConPTY implementation you load** (model-knowledge, high confidence; this is the pattern WezTerm's conpty loader uses).

### Bundle Microsoft's ConPTY pair — the single most important PTY decision

- In-box conhost re-serializes VT **lossily**: OSC sequences were re-ordered/front-loaded relative to text, and DCS wasn't forwarded to third-party terminal hosts at all ([microsoft/terminal #17313](https://github.com/microsoft/terminal/issues/17313), [#17314](https://github.com/microsoft/terminal/issues/17314), [#11220](https://github.com/microsoft/terminal/issues/11220), [#1173 passthrough](https://github.com/microsoft/terminal/issues/1173)). Fixes for OSC ordering/DCS passthrough landed targeting **Terminal v1.23 (Nov 2024)** — i.e., in the *open-source* ConPTY, on its own release cycle, not necessarily in the conhost of every supported Windows 10 21H2 install.
- **The ecosystem answer**: drop `conpty.dll` + `OpenConsole.exe` (from NuGet package `Microsoft.Windows.Console.ConPTY`, MIT-licensed, e.g. `1.24.260402001`) next to your exe and load that instead of the OS one. WezTerm bundles the pair ([wezterm #7774: update bundled pair to 1.24.260402001](https://github.com/wezterm/wezterm/issues/7774)); Alacritty supports OpenConsole ([alacritty PR #4501](https://github.com/alacritty/alacritty/pull/4501), [#4794](https://github.com/alacritty/alacritty/issues/4794)); Contour reuses WezTerm's binaries; JetBrains tracks the same ([IJPL-102628](https://youtrack.jetbrains.com/issue/IJPL-102628/Bundle-recent-version-of-ConPTY)).
- ARM64 note from wezterm's investigation: `conpty.dll` must match the **app's** architecture; `OpenConsole.exe` must match the **system** architecture — ship per-arch pairs.
- Notification passthrough proof: Windows Terminal itself implements **OSC 777 notifications on top of ConPTY** ([microsoft/terminal PR #14425](https://github.com/microsoft/terminal/pull/14425), tracking [#7718](https://github.com/microsoft/terminal/issues/7718)) — so a modern ConPTY delivers these sequences to the hosting terminal. With the bundled pair + gmux's own parser (termwiz or vte-side-parser or libghostty-vt), OSC 9/777/99 → toast is achievable end-to-end.
- Known ConPTY quirks to engineer around: `ClosePseudoConsole` can hang (esp. with `PSEUDOCONSOLE_INHERIT_CURSOR`, [microsoft/terminal discussion #17716](https://github.com/microsoft/terminal/discussions/17716)); close on a dedicated thread with timeout.

---

## (c) GPU text rendering

### What shipped Windows terminals actually use (maturity anchor)

- **WezTerm**: HarfBuzz shaping + own `GlyphCache` texture atlas + quad batching; backends OpenGL (default) and wgpu (`front_end = "WebGpu"`, wgpu ≥0.18) ([wezterm front_end docs](https://wezterm.org/config/lua/config/front_end.html), [changelog](https://wezterm.org/changelog.html), [DeepWiki rendering pipeline](https://deepwiki.com/wezterm/wezterm)). Multi-level caches: shape cache, glyph cache, line-state cache, line-quad cache.
- **Zed/GPUI on Windows**: **DirectX 11** device + **DirectWrite** shaping/ClearType rasterization; they *rejected Vulkan* after real-world driver compatibility problems across Windows hardware/VMs ([zed.dev/windows](https://zed.dev/windows), [Windows Alpha DX issue #36798](https://github.com/zed-industries/zed/issues/36798)). This is a strong signal: on Windows, D3D beats Vulkan for compatibility.
- **Alacritty**: custom OpenGL glyph-atlas renderer (no ligatures by design).
- Conclusion: every serious terminal uses a **custom glyph atlas + cached shaping**, not a generic scene/text library. Generic text renderers are fine for chrome, not proven for the 60fps full-grid redraw path at 10k+ cells.

### The crates

- **wgpu**: pure-Rust, backends Vulkan/Metal/**DX12**/GL; MIT OR Apache-2.0 ([gfx-rs/wgpu](https://github.com/gfx-rs/wgpu)). On Windows default to **DX12** (auto on Windows; discussion in [wgpu #2719](https://github.com/gfx-rs/wgpu/issues/2719)). Actively developed (frequent releases; by mid-2026 in the 2x series — exact current version not re-verified, model-knowledge).
- **glyphon 0.11.0 (2026-04-13), MIT OR Apache-2.0 OR Zlib**, ~1.4k LoC, built on cosmic-text + etagere atlas allocation; successor to wgpu_glyph; actively maintained by @grovesNL (verified via crates.io API). Good for an MVP grid and for UI text; ~1M total downloads.
- **cosmic-text** (Pop!_OS/System76): pure-Rust shaping/layout — **shaping now via HarfRust** (the HarfBuzz Rust port; formerly rustybuzz), rasterization via **swash**, "full ligature support", BiDi, color emoji; "Linux, macOS, and Windows are supported with the full feature set" ([pop-os/cosmic-text README](https://github.com/pop-os/cosmic-text/blob/main/README.md)). License MIT OR Apache-2.0 (model-knowledge for license string).
- **Ligatures in terminals**: possible with cosmic-text/HarfRust or harfbuzz-rs, but require shaping across cell runs + atlas entries wider than one cell — WezTerm's shape-cache design is the reference implementation. Alacritty proves you can ship without them; agent-CLI users care more about throughput and correctness.
- Recommended architecture: **wgpu (DX12 default, GL fallback) + custom cell-grid renderer** (damage-tracked quad batches, R8 atlas for grayscale + RGBA8 atlas for emoji, subpixel optional via DirectWrite-style dual-source blending), cosmic-text/swash (or DirectWrite via windows-rs for maximal ClearType fidelity) for rasterization, glyphon acceptable for the first milestone. Font fallback on Windows: use DirectWrite font enumeration (`windows` crate `Win32_Graphics_DirectWrite`) or `font-kit`.

---

## (d) Window & chrome options (sidebar + tabs + panes app)

| Option | Verdict for gmux |
|---|---|
| **winit + custom-drawn UI (egui for chrome)** | **Recommended.** One wgpu surface; grid drawn by custom renderer, chrome (sidebar/tabs/status/attention badges) via egui in the same render pass. Proven combo (`egui_term` embeds alacritty_terminal in egui: [Harzu/egui_term](https://github.com/Harzu/egui_term) — note: tested macOS/Linux, *not* Windows-tested). egui = MIT OR Apache-2.0, immediate-mode, trivial to build docks/tabs (egui_dock). Weaknesses: egui's own text shaping is basic (fine for chrome), accessibility limited (AccessKit integration exists). |
| **winit alone + fully custom UI** | What WezTerm effectively does (it has its own window layer). Maximum control, highest effort — tabs/splits UI, scrollbars, tooltips all hand-rolled. |
| **GPUI (Zed's framework)** | Now open source, Windows backend shipped (DX11+DirectWrite); gpui-ghostty proves GPUI+libghostty works. Risk: GPUI is versioned for Zed's needs, docs thin; but it is the *most proven* Rust GPU UI on Windows for exactly this app shape (Apache-2.0 — model-knowledge). Serious dark-horse candidate. |
| **Slint** | Tri-license: **GPLv3 OR royalty-free (requires visible Slint attribution) OR commercial** ([slint.dev/pricing](https://slint.dev/pricing), [LICENSE.md](https://github.com/slint-ui/slint/blob/master/LICENSE.md)). Attribution requirement is awkward for an OSS terminal; embedding a raw wgpu grid inside Slint is possible but less trodden. Pass. |
| **Tauri (WebView2 chrome around native grid)** | Violates the spirit of "no Electron for the grid" only technically: chrome in WebView2, grid in a native child HWND. Input routing, focus, IME, DPI, and composition across the webview/native boundary are chronic pain; WebView2 runtime dependency; two UI stacks to maintain. Only attractive if the team is web-first. Not recommended for a terminal-first app. |

Platform integration specifics (winit path):

- **DPI**: winit handles per-monitor-v2 DPI on Windows well (`ScaleFactorChanged`); mature (model-knowledge, high confidence).
- **IME/CJK**: winit exposes `Ime::Preedit`/`Ime::Commit` events + `set_ime_allowed`/`set_ime_cursor_area`; Windows backend is IMM32-based (not TSF). Works for CJK input incl. composition windows positioned at the cursor; the long-running meta-issues are [winit #1497 (IME tracking)](https://github.com/rust-windowing/winit/issues/1497) and [#1806 (keyboard meta)](https://github.com/rust-windowing/winit/issues/1806) — Windows is among the more complete backends. Alacritty (winit-based) supports CJK IME on Windows. Must-test, but not a blocker.
- **Drag & drop**: winit delivers `HoveredFile`/`DroppedFile` on Windows (model-knowledge, high confidence). Drag-*out* and rich DnD needs OLE via windows-rs.
- **Jumplist / taskbar**: not winit's job — use `windows` crate COM: `ICustomDestinationList` (jump lists: "New workspace", recent sessions), `ITaskbarList3::SetOverlayIcon` (perfect for the **agent-needs-attention badge** on the taskbar) and `SetProgressValue` (agent progress). All available in windows-rs Win32 bindings (model-knowledge, high confidence). Get the HWND from winit via `raw-window-handle`.

---

## (e) ARM64 Windows

- **`aarch64-pc-windows-msvc` is a Tier 1 Rust target since Rust 1.91** (promoted via [rust-lang/rust PR #145682](https://github.com/rust-lang/rust/pull/145682), [RFC 3817](https://github.com/rust-lang/rfcs/pull/3817); [InfoWorld coverage](https://www.infoworld.com/article/4082150/rust-1-91-promotes-windows-on-arm64-to-tier-1-target.html)) — full test suite on every merge, prebuilt toolchains, host tools. Minor open issue example: coverage instrumentation profile bug [#150123](https://github.com/rust-lang/rust/issues/150123) (non-blocking).
- **GPU on ARM64 (Snapdragon X / Adreno)**: D3D11/D3D12 drivers are first-class from Qualcomm; **Vulkan has historically been a DX12 wrapper** installed via the "OpenCL/OpenGL/Vulkan Compatibility Pack", with missing features vs native ([xemu #1878](https://github.com/xemu-project/xemu/issues/1878)); native Vulkan drivers are only now rolling out. **Default wgpu to the DX12 backend on Windows** and ARM64 is low-risk. Zed ships Windows ARM64 builds on DX11 ([zed.dev/docs/windows](https://zed.dev/docs/windows)); winghostty ships ARM64 ([winghostty.com](https://www.winghostty.com/)).
- ConPTY, WinRT toasts, named pipes: architecture-neutral OS APIs — fine on ARM64. Remember the bundled-ConPTY arch rule (dll = app arch, OpenConsole.exe = system arch).
- CI: GitHub-hosted Windows ARM64 runners exist (`windows-11-arm`) for OSS (model-knowledge — confirm quota/label when setting up CI).
- libghostty-vt on ARM64 Windows: Zig cross-compiles well by design, and winghostty ships ARM64 — but **verify `libghostty-vt-sys` builds for aarch64-pc-windows-msvc in the week-one spike** (uncertain; not explicitly documented).

---

## (f) WinRT toast notifications + named pipes from Rust

### Toasts

- **windows-rs coverage is complete**: `windows::UI::Notifications::{ToastNotificationManager, ToastNotifier, ToastNotification}` + `windows::Data::Xml::Dom::XmlDocument` ([microsoft.github.io windows-docs-rs: ToastNotificationManager](https://microsoft.github.io/windows-docs-rs/doc/windows/UI/Notifications/struct.ToastNotificationManager.html)). The `windows`/`windows-sys` crates are Microsoft-maintained, MIT OR Apache-2.0.
- Higher-level: **`winrt-toast` 0.1.1 (2026-05-25), MIT** — text/images/actions, `ToastManager`, and a `register()` helper that writes the registry entries for an **AppUserModelID** ([docs.rs/winrt-toast](https://docs.rs/winrt-toast)). Also `tauri-winrt-notification`/`winrt-notification` (Toast + sounds, [docs.rs](https://docs.rs/winrt-notification/latest/winrt_notification/struct.Toast.html)).
- **Unpackaged-app gotchas (plan for these)**: an unpackaged Win32 exe must register an AUMID (registry under `HKCU\...\AppUserModelId` or a Start-Menu shortcut carrying the AUMID) before `CreateToastNotifier(&aumid)` works reliably. **Click-to-activate/focus is the hard part**: Windows Terminal's own OSC-777 implementation notes that clicking the toast "launches a new windowsterminal.exe... doesn't work for unpackaged builds (requires package identity)" ([microsoft/terminal PR #14425](https://github.com/microsoft/terminal/pull/14425)). For reliable click-to-focus-pane, gmux should either (1) register a **COM toast activator** (`INotificationActivationCallback`) with the shortcut AUMID, or (2) ship with **sparse MSIX package identity**. Budget real engineering time here; showing the toast is trivial, activation is not. (Mechanism details: model-knowledge, high confidence; the WT quote is web-verified.)
- In-app attention indicators need none of this — they're just gmux UI + `ITaskbarList3::SetOverlayIcon`/`FlashWindowEx`.

### Named pipes (`\\.\pipe\gmux`)

- **tokio** has first-class Windows named-pipe support: `tokio::net::windows::named_pipe::{ServerOptions, ClientOptions, NamedPipeServer, NamedPipeClient}` — async, multiple-instance servers, works with any framing/RPC layer (model-knowledge, high confidence; stable in tokio 1.x for years). This is the obvious transport for the CLI ↔ GUI API (`create-workspace`/`split`/`send-keys`/`capture-pane`/`screenshot`).
- Raw alternative: `windows` crate `Win32_System_Pipes` (`CreateNamedPipeW`, etc.). Cross-platform-flavored alternative: `interprocess` crate. No gaps; this layer is zero-risk.

---

## (g) License summary

| Component | License | Redistribution-safe |
|---|---|---|
| alacritty_terminal 0.26 | Apache-2.0 | Yes |
| vte 0.15 | Apache-2.0 OR MIT | Yes |
| termwiz 0.23.3 | MIT | Yes |
| wezterm-term (git) / portable-pty | MIT | Yes |
| libghostty-vt (Rust crate) 0.2.0 | MIT OR Apache-2.0 | Yes |
| Ghostty / libghostty core | MIT | Yes |
| wgpu | MIT OR Apache-2.0 | Yes |
| glyphon 0.11 | MIT OR Apache-2.0 OR Zlib | Yes |
| cosmic-text / swash / HarfRust | MIT OR Apache-2.0 (family) | Yes |
| winit / egui | Apache-2.0 / MIT OR Apache-2.0 | Yes |
| GPUI (Zed) | Apache-2.0 (model-knowledge) | Yes |
| Slint | GPLv3 OR royalty-free-with-attribution OR commercial | Conditional — avoid |
| windows / windows-sys (Microsoft) | MIT OR Apache-2.0 | Yes |
| winrt-toast | MIT | Yes |
| tokio | MIT | Yes |
| **Bundled conpty.dll + OpenConsole.exe** (`Microsoft.Windows.Console.ConPTY` NuGet) | **MIT** (Windows Terminal repo) | **Yes — explicitly designed to be bundled** |

No proprietary blockers anywhere in Stack A.

---

## (h) Maturity / risk assessment

**Proven in shipped Windows terminals:**
- alacritty_terminal + ConPTY: **Zed on Windows** (GA, Nov 2025 logs confirm) — highest-confidence VT choice.
- termwiz/wezterm-term + bundled ConPTY + custom glyph atlas + OpenGL/wgpu: **WezTerm** — proves the whole Stack-A shape including built-in multiplexing on Windows.
- Custom Rust GPU UI on Windows: **GPUI/DX11 (Zed)**; wgpu itself ships in many production apps.
- libghostty-vt on Windows: proven indirectly (winghostty, ghostty-windows) but the *Rust crate path* on Windows is **not yet proven publicly** — spike required.

**Risk register:**

| Risk | Severity | Mitigation |
|---|---|---|
| alacritty_terminal drops OSC 9/777/99 (no hook) | High if unhandled | Side `vte::Parser` watching `osc_dispatch` on the raw PTY stream; or choose termwiz/libghostty-vt |
| In-box conhost strips/reorders OSC on older Win10 21H2 | High | Bundle MIT ConPTY pair (≥1.23/1.24); per-arch pairs |
| libghostty-vt pre-1.0 + Zig 0.15 build dep + unverified Windows cargo build | Medium | Week-one build spike on x64+ARM64; pin versions; alacritty_terminal as fallback core |
| wezterm-term not on crates.io, single maintainer | Medium | Prefer alacritty_terminal or libghostty-vt; use termwiz only for its OSC parser types if desired |
| portable-pty missing modern ConPTY flags + teardown race | Medium | Direct windows-rs ConPTY, or the psmux-style patches |
| Toast click-to-focus for unpackaged apps | Medium | COM activator or sparse-MSIX identity; schedule explicitly |
| Vulkan on ARM64 Adreno (wrapper, feature gaps) | Low | wgpu DX12 backend default |
| glyphon/cosmic-text perf for full-grid 60fps | Low-Medium | Fine for MVP; plan custom damage-tracked atlas renderer like WezTerm/Zed for v1 |
| egui chrome polish/accessibility | Low | AccessKit; or evaluate GPUI |
| winit IMM32 (not TSF) IME edge cases for CJK | Low-Medium | Early manual CJK test matrix; Alacritty precedent says workable |

**Bottom-line recommended composition (Stack A concrete):**
`windows-rs` direct ConPTY (+ bundled `Microsoft.Windows.Console.ConPTY` pair) → per-pane reader thread → VT core = **alacritty_terminal 0.26 + side vte OSC-watcher** (default) with a **time-boxed spike on libghostty-vt 0.2** (upgrade to it if the Windows build is clean — buys cmux parity, reflow, first-class OSC) → **wgpu DX12** + glyphon-for-MVP → custom atlas renderer for v1 → **winit + egui** chrome (GPUI as evaluated alternative) → `tokio` named pipes for `\\.\pipe\gmux` → `windows::UI::Notifications` (+ COM activator / sparse MSIX) for toasts + `ITaskbarList3` overlay badges for attention.
