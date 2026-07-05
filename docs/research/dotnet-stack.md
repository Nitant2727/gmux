# Candidate Stack B: C#/.NET + WinUI 3 + ConPTY via P/Invoke

Research date: 2026-07-04. Researcher: fan-out agent (stack B evaluation for gmux).
Verification method: live web checks against Microsoft Learn, GitHub (microsoft/terminal, microsoft/WindowsAppSDK, microsoft/Win2D), and the NuGet v3 API. Confidence labels per finding; anything not re-verified this session is marked model-knowledge.

---

## TL;DR

- **Windows App SDK / WinUI 3 is emphatically NOT abandoned in mid-2026** — stable 2.2.0 shipped 2026-06-09, with a monthly-ish stable + experimental cadence (2.0.1 → 2.1.3 → 2.2.0 between April and June 2026) and a Build 2026 marketing push. But startup/memory overhead vs Win32 remains a real, Microsoft-acknowledged weakness.
- **The rendering layer is the entire risk of this stack.** Microsoft has still not productized Windows Terminal's TermControl for third parties (issue open since 2020, officially "in the Icebox"); community wrappers exist for WPF (usable) and WinUI 3 (alpha). No maintained production-grade pure-.NET VT emulator or GPU terminal renderer exists. Choosing Stack B means either depending on an unofficial one-maintainer package or building a first-of-its-kind glyph-atlas renderer + VT parser in C#.
- **Everything else is low-risk in .NET**: ConPTY has official C# samples and — new in 2025/2026 — an official MIT-licensed `Microsoft.Windows.Console.ConPTY` NuGet package from the Windows Terminal team; toasts work unpackaged via `AppNotificationManager`; named-pipe servers are trivial (`NamedPipeServerStream` + StreamJsonRpc); unpackaged + self-contained + ARM64 distribution is fully supported (at a ~200 MB self-contained size cost).

---

## (a) Windows App SDK / WinUI 3 state, mid-2026

### Release cadence and versioning — ACTIVE (web-verified)

From the official downloads page (<https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/downloads>, page updated 2026-06-11):

| Channel | Version | Date |
|---|---|---|
| Stable | **2.2.0** | 2026-06-09 |
| Stable | 2.1.3 | 2026-05-21 |
| Stable | 2.0.1 | 2026-04-29 |
| Preview | 2.0.0-preview2 | 2026-03-31 |
| Experimental | 2.2.2-experimental9 | 2026-06-09 |
| Experimental | 2.0-experimental1 | 2025-10-02 |

Observations:

- **Semantic versioning adopted at 2.0**; breaking changes only across major versions; package family name aligns to major version (release notes, <https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-notes/windows-app-sdk-2-0>).
- Source tags for 2.x releases now live in **microsoft/microsoft-ui-xaml** (WinUI repo) — SDK and WinUI development have converged.
- `Microsoft.WindowsAppSDK` NuGet: latest listed 2.2.2-experimental9, ~24M total downloads (NuGet API, web-verified).
- Cadence has been roughly monthly through H1 2026 (experimental 4/5/6/7 in Jan–Apr, stables in Apr/May/Jun). This is the opposite of abandonment. Build 2026 featured WinUI 3 + WinAppSDK prominently (WinUI agent tooling, migration tooling) (<https://windowsforum.com/threads/build-2026-winui-3-windows-app-sdk-and-ai-agents-push-native-windows-apps.422225/>).

### OS support (web-verified)

- WinUI 3 apps "run on Windows 10, version 1809 (build 17763) and later, including Windows 11" (<https://learn.microsoft.com/en-us/windows/apps/winui/winui3/>). The 2.x line still supports Windows 10 — e.g., the 2.x ML sub-package notes explicitly call out continued 1809 support paths. **gmux's Windows 10 21H2+ floor is comfortably inside the supported range.**
- **ARM64**: every 2.x release ships x64/x86/**arm64** runtime installers and an all-arch redistributable ZIP (downloads page). ARM64 is a first-class target.

### Unpackaged app support (web-verified)

- Unpackaged deployment fully supported since WinAppSDK 1.1; self-contained deployment via `<WindowsAppSDKSelfContained>true</WindowsAppSDKSelfContained>`; **`PublishSingleFile` supported for unpackaged self-contained WinUI 3 apps since 1.5** (<https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/unpackage-winui-app>, <https://learn.microsoft.com/en-us/windows/apps/get-started/windows-developer-faq>).
- WinAppSDK 2.2 added `ApplicationData.GetForUnpackaged()` — Microsoft keeps investing in the unpackaged path specifically (2.2.0 release notes).

### Native AOT (web-verified summary; details model-knowledge)

- WinUI 3 gained **Native AOT support in WinAppSDK 1.6** (Sept 2024) via CsWinRT's AOT-compatible source-generated projections; Microsoft cites significantly reduced startup time and memory (<https://github.com/microsoft/microsoft-ui-xaml/discussions/8082>, Windows Developer Blog Nov 2024 <https://blogs.windows.com/windowsdeveloper/2024/11/07/so-whats-new-with-microsoft-native-ux-technologies/>).
- Windows Community Toolkit 8.2 (Apr 2025) added AOT compatibility across its packages (<https://visualstudiomagazine.com/articles/2025/04/03/windows-community-toolkit-8-2-adds-native-aot-support.aspx>).
- .NET 10 (GA expected Nov 2026) is expected to further broaden AOT-compatible API surface for desktop apps. Practical caveats remain (model-knowledge): reflection-heavy libraries, XAML `{Binding}` (prefer `{x:Bind}`), and some third-party NuGets are not AOT-clean; expect an AOT-compat audit of every dependency.

### Startup time / perceived quality (web-verified, mixed picture)

- Microsoft itself acknowledges that in the 2.0 era, WinUI 3 apps' "launch speeds, RAM usage, and installation size are larger/slower than seen in UWP," with active work to improve (Windows App SDK 2.0 coverage; Microsoft Q&A threads).
- A 2026 performance push is underway: File Explorer's time in WinUI 3 startup code cut ~25%, Notepad cold-start improvements of 40–55% in telemetry, XAML compiler stripping unused styles/templates at build time; improvements land with Windows 11 26H2 (<https://github.com/microsoft/microsoft-ui-xaml/discussions/11096>, windowsnews.ai coverage of the Build 2026 announcements).
- Realistic expectation for gmux (model-knowledge): a non-AOT WinUI 3 app cold-starts in roughly 1–2 s on typical hardware; AOT + trimming brings this down substantially but not to Win32/C++ levels. For a long-running multiplexer this is a minor concern; it matters mostly for "first impression."

**Verdict (a): platform is alive, actively shipped, supports gmux's OS floor and ARM64 and unpackaged distribution. Its residual weaknesses are startup/memory overhead and a long history of XAML-quality papercuts (text input, focus, DPI edge cases — model-knowledge) rather than abandonment.**

---

## (b) Terminal rendering in .NET — the crux

### Is Windows Terminal's TermControl reusable by third parties?

**Short answer: not officially, still, as of July 2026.** (web-verified)

- Tracking issue **microsoft/terminal#6999 "Productize the WPF, UWP Terminal Controls"** — opened July 2020, **still open**, milestone "Terminal v1.23", last updated 2026-02-04, only 9 comments total (GitHub API, verified this session). A Windows Terminal maintainer (zadjii-msft) stated the work is "in the Icebox" (no progress, deprioritized) — referenced from <https://github.com/microsoft/terminal/issues/13851#issuecomment-1884723720> and reaffirmed in the #6999 thread as recently as March 2025.
- **No official `Microsoft.Terminal.Wpf` package exists on NuGet** — verified: `https://api.nuget.org/v3-flatcontainer/microsoft.terminal.wpf/index.json` returns BlobNotFound.
- The WPF terminal control **does exist in the microsoft/terminal repo** (MIT): a thin .NET wrapper (`Microsoft.Terminal.Wpf`) over a C++ core (`PublicTerminalCore.dll`) that hosts the real Windows Terminal renderer in a child HWND. **Visual Studio's integrated terminal ships this control** — so the code path is production-proven — but Microsoft only distributes it internally; third parties must build it from source or use community CI builds.
- License: entire microsoft/terminal repo is MIT, so building/redistributing the control is legally clean for gmux's open-source-friendly requirement.

Community packagings (web-verified on NuGet this session):

| Package | Version | Notes |
|---|---|---|
| `CI.Microsoft.Terminal.Wpf` | 1.22.250204002 (~5.5k dl) | mitchcapper's CI build of the official WPF control from WT 1.22 sources |
| `EasyWindowsTerminalControl` | 1.0.36 | mitchcapper's higher-level WPF control on top of the WT backend; GPU-accelerated, 24-bit color, full VT; repo active (pushed 2025-05-19) (<https://github.com/mitchcapper/EasyWindowsTerminalControl>) |
| `EasyWindowsTerminalControl.WinUI` | — | **"very alpha and very un-official"** (author's words). WinUI 3 has no `HwndHost`, so a custom HWND-hosting shim was built; **airspace limitation: XAML content cannot render on top of the terminal area** |
| `WindowsTerminal.WinUI3.Control` | 1.11.3471 (repo Corillian/WindowsTerminal) | Stale — based on WT 1.11/1.14 (2021–22 era), repo last pushed 2022-08. Effectively dead |
| `Microsoft.Windows.Console.ConPTY` | 1.24.260512001 stable (2026-05-22) | Official — see section (c). PTY layer only, not a rendering control |

Key architectural notes if the WT control is used:

- The control's public API is deliberately narrow: you supply an `ITerminalConnection` (you own the ConPTY plumbing) and the control renders. **There is no OSC-sequence callback API.** For gmux's killer feature (OSC 9 / OSC 777 / OSC 99 notifications), this is workable anyway: because you own the connection object, you can **tee the raw ConPTY output stream and run your own lightweight OSC scanner before forwarding bytes to the control**. Notification detection does not require the renderer's cooperation.
- Splits/panes/tabs are NOT part of the control — Windows Terminal implements those a layer above. gmux would compose multiple controls in its own XAML layout, which is exactly the multiplexer layer gmux plans to build anyway. This is fine.
- The control hosts a child HWND with a DX swapchain → airspace constraints (overlays like "pane attention" glow must be drawn as adjacent chrome, not on top of the terminal surface) — especially acute in the WinUI 3 shim.
- Risk concentration: the practical WinUI3/WPF packages are maintained by **one person** (mitchcapper), tracking a repo whose owner has explicitly iceboxed the scenario. Version bumps of WT can break the private API boundary the wrapper relies on.

### Windows Terminal's own UI-framework status (model-knowledge, flag as uncertain)

Windows Terminal historically runs WinUI 2.8 / UWP XAML via XAML Islands in a Win32 host; a migration toward Windows App SDK/WinUI 3 has been discussed and is in progress, but I could not verify this session that any shipped WT release runs on WinUI 3. Do not architect around a hypothetical future official WinUI 3 TermControl.

### Pure-.NET VT emulator libraries — all effectively dead (web-verified activity data)

| Library | Last real activity | Assessment |
|---|---|---|
| **VtNetCore** (darrenstarr) <https://github.com/darrenstarr/VtNetCore> | core work ~2018–2019; repo pushed 2023-06; 96 stars | VT100/xterm emulation for .NET Standard 2.0. Reached decent compatibility, then stalled. Companies forked to patch (e.g., bastionzero/VtNetCorePatched). Not on NuGet under its own ID anymore (packageid query returns nothing). **Abandoned; usable only as a starting-point fork** |
| **XtermSharp** (migueldeicaza) <https://github.com/migueldeicaza/XtermSharp> | pushed 2022-11; 187 stars | Port of xterm.js internals; author's energy moved to SwiftTerm (Swift). Never published to NuGet. **Dormant** |
| **TermSharp** (Antmicro) <https://github.com/antmicro/TermSharp> | pushed 2026-06-17; 15 stars | WPF terminal widget used in Renode. **Alive but niche**; CPU-side rendering, not a general xterm-compat emulator, tiny community |
| Simple.Wpf.Terminal, WpfTerminal, etc. | various | "Console-look" log/REPL controls, **not** VT emulators. Irrelevant |
| Rebex Terminal Emulation | commercial | Real .NET VT emulator + WinForms/WPF control, CPU/GDI rendering. **Proprietary, paid — conflicts with gmux's open-source-friendly licensing requirement** |

**Conclusion: if gmux does not reuse the WT control, it must write its own VT state machine in C#** (the DEC/xterm parser grammar is well specified — Paul Williams' state machine; OSC/CSI/DCS dispatch — a few weeks for the core, plus a long tail of xterm-compat quirks that projects like alacritty/wezterm took years to burn down).

### GPU text rendering options from .NET / WinUI 3

All the raw pieces exist and are healthy (web-verified package/repo data):

- **Win2D** (`Microsoft.Graphics.Win2D` 1.4.0, ~8.1M downloads; repo pushed 2026-03-16) — Microsoft's D2D wrapper for WinUI 3. `CanvasControl` / `CanvasSwapChain` / `CanvasVirtualControl` on `SwapChainPanel`; DirectWrite text via `CanvasTextFormat`/`CanvasTextLayout`; sprite batches for atlas blitting. Caveat: the WinAppSDK port still has gaps (`CanvasAnimatedControl` partial/absent) (<https://github.com/microsoft/Win2D>, <https://learn.microsoft.com/en-us/windows/apps/develop/win2d/>).
- **Vortice.Windows** (e.g., `Vortice.Direct3D11` 3.8.3, 1.57M downloads; repo pushed 2026-03-04, 1.2k stars) — maintained low-level D3D11/D3D12/D2D/DWrite/DXGI bindings, the spiritual successor to SharpDX (which is long dead). <https://github.com/amerkoleci/Vortice.Windows>
- **TerraFX.Interop.Windows** (10.0.26100.6, 4.37M downloads) — blittable raw Windows API/D3D interop, zero-overhead style.
- **Silk.NET.Direct3D11** (2.23.0) — alternative maintained bindings.
- **CsWin32** (`Microsoft.Windows.CsWin32` 0.3.298, 3.1M downloads) — Microsoft's source generator for arbitrary Win32 P/Invokes with SafeHandles; ideal for the ConPTY + window-plumbing layer.
- WinUI 3 interop path: `SwapChainPanel` + `ISwapChainPanelNative.SetSwapChain(...)` (obtained via WinRT interop cast) gives you a DXGI swapchain surface inside XAML; DirectComposition/`DispatcherQueue` handle presentation. This is exactly how Windows Terminal composes its DX renderer into XAML (<https://devblogs.microsoft.com/commandline/building-windows-terminal-with-winui/>).

**Can it hit terminal-grade rendering?** Technically yes (model-knowledge assessment):

- The proven design is Windows Terminal's AtlasEngine: DWrite-shaped glyph runs rasterized once into a texture atlas, screen drawn as instanced quads (one per cell/run), dirty-row damage tracking, custom scrolling via viewport offset. Every required API (IDWriteTextAnalyzer shaping, IDWriteFontFallback, D3D11 instancing, DXGI flip-model present with partial dirty rects) is callable from C# via Vortice/TerraFX with near-zero marshaling overhead if done with blittable structs.
- Existence proofs that C# can sustain this class of workload: osu!lazer (custom GPU framework at 1000+ fps), Ryujinx (GPU emulation), Paint.NET, Avalonia's compositor. **But none of these is a terminal, and no GPU glyph-atlas terminal renderer has ever shipped in .NET** — gmux would be first. Font fallback, complex-script shaping, emoji (COLR/SVG), ligatures, box-drawing pixel-perfection, and cursor/selection compositing are where the months go.
- The "cheap" middle path — Win2D `CanvasTextLayout` per dirty line with damage tracking (essentially WT's pre-Atlas DxRenderer design) — is realistic for a v1 at 60 fps and much less code, with a later swap to an atlas engine if profiling demands it.

---

## (c) ConPTY from C# — well-trodden, newly de-risked

### APIs and P/Invoke pattern (model-knowledge; APIs stable since Win10 1809)

- `CreatePseudoConsole(COORD size, HANDLE hInput, HANDLE hOutput, DWORD dwFlags, HPCON* phPC)`, `ResizePseudoConsole`, `ClosePseudoConsole` (kernel32; available Windows 10 1809+, i.e., everywhere gmux targets).
- Child process launch: `System.Diagnostics.Process` **cannot** attach a pseudoconsole. You must call `CreateProcessW` with `STARTUPINFOEXW`, `InitializeProcThreadAttributeList`, and `UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE /* 0x00020016 */, hPC)`.
- Canonical C# P/Invoke shape (from Microsoft's own sample):
  `internal static extern int CreatePseudoConsole(COORD size, SafeFileHandle hInput, SafeFileHandle hOutput, uint dwFlags, out IntPtr phPC);`
- Generate all of this with **CsWin32** rather than hand-writing declarations.

### Known-good samples (web-verified existence)

- **microsoft/terminal `samples/ConPTY/MiniTerm`** — official C# ConPTY sample (<https://github.com/microsoft/terminal/blob/main/samples/ConPTY/MiniTerm/MiniTerm/Native/PseudoConsoleApi.cs>).
- **microsoft/terminal `samples/ConPTY/GUIConsole`** — WPF GUI + `GUIConsole.ConPTY` .NET Standard 2.0 library (<https://github.com/microsoft/terminal/blob/main/samples/ConPTY/GUIConsole/GUIConsole.ConPTY/PseudoConsole.cs>).
- **microsoft/vs-pty.net** — Pty.Net abstraction used by Visual Studio; repo last pushed 2024-12-09; MIT; not shipped as a supported public NuGet. Useful reference for lifecycle/edge-case handling.
- Conceptual doc: <https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session> and the original announcement <https://devblogs.microsoft.com/commandline/windows-command-line-introducing-the-windows-pseudo-console-conpty/>.

### NEW and important: official redistributable ConPTY (web-verified)

- **`Microsoft.Windows.Console.ConPTY` on NuGet — published by the Microsoft.Terminal account, MIT license, stable 1.24.260512001 (2026-05-22), prerelease 1.25.x; targets native + uap10.0, supports Windows 10.0.17763+** (<https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY>).
- Why this matters: historically, third-party terminals got whatever ConPTY behavior the in-box conhost of the user's OS provided — on Windows 10 21H2 that is a 2021-era conpty missing years of fixes (chunked repaints, alt-buffer quirks, missing passthrough of newer sequences). Shipping this package gives gmux the **current Windows Terminal team ConPTY implementation on every target OS**, which directly improves fidelity of OSC passthrough to gmux's parser. This substantially de-risks the PTY layer for ANY stack, and it is trivially consumable from .NET.

### Pipe plumbing and async IO (model-knowledge)

- The classic sample uses anonymous pipes (`CreatePipe`). **Anonymous pipes do not support overlapped IO**; the standard .NET pattern is a dedicated background reader thread wrapping the read handle in `FileStream(new SafeFileHandle(...), FileAccess.Read)` and pumping into a `System.Threading.Channels.Channel<byte[]>`; writes serialized through a single writer. This is simple and is what most C# ConPTY hosts do.
- For true async, create **named pipes with `FILE_FLAG_OVERLAPPED`** and hand those handles to `CreatePseudoConsole` — ConPTY only needs HANDLEs. Then `NamedPipeClientStream`/`FileStream` async APIs use IO completion ports naturally.
- Known pitfalls: (1) drain the output pipe before/while calling `ClosePseudoConsole` or the call can deadlock — close order is: terminate/await child, close hPC, then reader sees pipe EOF; (2) `PSEUDOCONSOLE_INHERIT_CURSOR` flag if attaching to an existing cursor position; (3) ConPTY is a *translation* layer — it re-renders the child's output into a normalized VT stream, so gmux's parser sees conpty's dialect, not the raw app output (mostly a non-issue with the modern NuGet conpty, which passes through much more faithfully).
- OSC 9 / OSC 777 / OSC 99 flow through ConPTY to the hosting app's output pipe; gmux scans for them in its connection layer regardless of renderer choice.

---

## (d) Toast notifications from WinUI 3 / unpackaged .NET

- **`AppNotificationManager` (Microsoft.Windows.AppNotifications, in WinAppSDK) explicitly supports both packaged and unpackaged apps** — unpackaged apps skip manifest registration and just call `AppNotificationManager.Default.Register()` after wiring `NotificationInvoked` (order matters: handler before Register or activations can be lost). Docs current as of 2026: <https://learn.microsoft.com/en-gb/windows/apps/develop/notifications/app-notifications/app-notifications-quickstart>, overview <https://learn.microsoft.com/en-us/windows/apps/develop/notifications/> (web-verified).
- `AppNotificationBuilder` gives fluent XML-free toast construction (title/body/buttons/audio/progress). Interactive scenarios (click to focus the right pane) work via `NotificationInvoked` args in-process — no COM activator boilerplate needed in the WinAppSDK path.
- **Known limitation (from Microsoft's docs/spec): notifications from an elevated (admin) process are not supported** — relevant if users run agent sessions elevated; document it.
- **CommunityToolkit.WinUI.Notifications is a dead end**: frozen at 7.1.2 (and `Microsoft.Toolkit.Uwp.Notifications` at 7.1.3, both 2021–22 era); the Notifications packages were dropped from Windows Community Toolkit 8.x (NuGet verified). Do not build on them; use the WinAppSDK API. For a fully non-WinAppSDK fallback, the old `ToastNotificationManagerCompat` pattern still functions but is unmaintained.
- Net: **the killer-feature notification pipeline (OSC parse → Windows toast + in-app attention badge) is genuinely easy in this stack** — hours, not weeks.

## (e) Named pipe API server in .NET

Trivial — model-knowledge, very high confidence (core BCL, stable for a decade):

- `NamedPipeServerStream("gmux", PipeDirection.InOut, NamedPipeServerStream.MaxAllowedServerInstances, PipeTransmissionMode.Byte, PipeOptions.Asynchronous)` → surfaces at `\\.\pipe\gmux`. Full async/await, multiple concurrent instances, per-user ACLs via `NamedPipeServerStreamAcl.Create(..., PipeSecurity)` (lock to current user SID to prevent cross-user command injection).
- Recommended protocol layer: **StreamJsonRpc** (Microsoft, actively maintained, powers VS/VS Code interop) over the pipe — gives request/response + notifications for `create-workspace` / `split` / `send-keys` / `capture-pane` / `screenshot` with header-delimited or length-prefixed framing for free. The `gmux` CLI is then a thin `NamedPipeClientStream` + StreamJsonRpc client; a Native-AOT-compiled CLI exe stays ~a few MB with instant startup.

## (f) Distribution

- **Options** (all supported; web-verified docs): (1) MSIX packaged; (2) unpackaged framework-dependent — ship `WindowsAppRuntimeInstall.exe` (x64/arm64) or bootstrap via the standalone installer, runtime shared machine-wide; (3) unpackaged **self-contained** (`WindowsAppSDKSelfContained=true`) — everything in-folder, xcopy-deployable, single-file publish supported since 1.5 (native DLLs extracted at first run). <https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/deploy-overview>, <https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/self-contained-deploy/deploy-self-contained-apps>
- **Size reality**: community measurements put WinAppSDK self-contained overhead at roughly **~200 MB uncompressed** for an empty project (<https://nicksnettravels.builttoroam.com/packaged-unpackaged-self-contained/>); .NET self-contained runtime adds ~70 MB more unless trimmed/AOT'd. Trimming + AOT cuts this substantially but requires the AOT-compat audit from (a). Framework-dependent unpackaged keeps the download small (~few MB app + one-time runtime install) and is the pragmatic default for a dev tool; MSIX adds identity (nice-to-have for toasts branding), clean uninstall, and AppInstaller auto-update, at the cost of signing-cert friction for an OSS project.
- ARM64: per-arch publish (`win-arm64`) is routine in .NET; WinAppSDK ships arm64 runtimes (verified above). No blockers.

## (g) Honest comparison — what has actually shipped as a .NET-rendered terminal

Be precise:

- **Windows Terminal is NOT a .NET app.** Core, VT machinery, and the AtlasEngine GPU renderer are C++ (C++/WinRT); its UI has historically been WinUI 2.x/XAML-Islands in a Win32 host. Zero of its render path is managed code.
- **Visual Studio's integrated terminal**: .NET WPF *wrapper* around WT's C++ `PublicTerminalCore.dll` — the rendering is still C++. This is the strongest "shipped in .NET-adjacent form" data point, and it's exactly the component Microsoft declined to productize (#6999).
- **FluentTerminal**: UWP shell around **xterm.js in a WebView** — the renderer is JavaScript/DOM-canvas, not .NET; project largely inactive.
- **Rebex Terminal Emulation**: genuine .NET emulator+control that ships in commercial products (e.g., Royal TS lineage) — CPU/GDI rendering, closed-source/paid; excluded by gmux's licensing requirement.
- **TermSharp (Antmicro)**: MIT WPF terminal widget shipping inside Renode — real but niche, CPU-rendered, not xterm-grade.
- **Conclusion: no high-quality, GPU-accelerated, fully .NET-rendered terminal has ever shipped.** Stack B's rendering layer is either (i) the proven-but-unofficial C++ WT control behind a thin .NET wrapper, or (ii) green-field C# engineering with no prior art of a shipped peer. Counter-evidence that C# is *capable* (osu!lazer, Ryujinx, Paint.NET) is real but none of those is a terminal with DWrite shaping/fallback demands.

---

## Two viable shapes for Stack B (decision fork)

**B1 — "Borrow the renderer": WPF (not WinUI 3) shell + WT WPF control from source or `CI.Microsoft.Terminal.Wpf`/`EasyWindowsTerminalControl`.**
Fastest path to a credible terminal grid: WT-quality rendering, own the `ITerminalConnection` (tee for OSC 9/777/99), compose splits in WPF, toasts via WinAppSDK APIs (usable from plain WPF), pipes trivial. Costs: unofficial one-maintainer packaging (or owning a WT-repo build pipeline), narrow control API (theming/scrollback/selection control is limited), HWND airspace constraints on attention overlays, WPF-not-WinUI aesthetics, and Microsoft could change internal APIs without notice (icebox status cuts both ways: also means low churn).

**B2 — "Own the renderer": WinUI 3 shell + custom C# VT core + glyph-atlas renderer on SwapChainPanel (Vortice/TerraFX or Win2D first pass).**
Full control (custom OSC hooks, attention effects composited in-renderer, session-restore-friendly buffer model), 100% MIT-able. Costs: the VT emulator + renderer is realistically **the majority of the project's total engineering** (multi-month before parity with even basic xterm compat), and it would be a first-of-kind in .NET.

Cross-cutting positives regardless of sub-path: official ConPTY NuGet (MIT) for the PTY layer; AppNotificationManager for toasts incl. unpackaged; NamedPipeServerStream + StreamJsonRpc for the `\\.\pipe\gmux` API; solid ARM64; CsWin32 for interop hygiene.

## Risks

1. **Renderer risk dominates**: either dependence on unofficial packaging of an iceboxed Microsoft component (B1) or first-of-kind renderer engineering (B2). This is the deciding factor vs stacks with existing embeddable terminal cores.
2. WinUI 3 residual quality/perf issues (startup, memory, XAML papercuts) — improving through 2026 but real; WinUI 3 also lacks `HwndHost`, complicating B1-style HWND hosting inside a WinUI shell.
3. Native AOT is supported but conditional — every dependency must be AOT/trim-clean; otherwise ship JIT + ReadyToRun and accept slower cold start.
4. Self-contained distribution is heavy (~200 MB class); framework-dependent mode trades that for a runtime-install step.
5. Elevated-process toast limitation; document for users running agents as admin.
6. Windows 10 21H2 in-box conpty is old — mitigated by shipping `Microsoft.Windows.Console.ConPTY`, but that package is young (stable line appeared ~2025/2026); watch its stability.

## Source index

- Windows App SDK downloads (versions/dates/arm64): https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/downloads
- WinAppSDK 2.0 release notes (semver, unpackaged, fixes): https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-notes/windows-app-sdk-2-0
- WinUI 3 overview (OS floor 1809+): https://learn.microsoft.com/en-us/windows/apps/winui/winui3/
- Unpackaged distribution: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/unpackage-winui-app ; self-contained: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/self-contained-deploy/deploy-self-contained-apps
- TermControl productization: https://github.com/microsoft/terminal/issues/6999 (open, milestone v1.23, updated 2026-02-04); icebox statement https://github.com/microsoft/terminal/issues/13851#issuecomment-1884723720
- Community controls: https://github.com/mitchcapper/EasyWindowsTerminalControl ; https://github.com/Corillian/WindowsTerminal ; NuGet `CI.Microsoft.Terminal.Wpf`, `WindowsTerminal.WinUI3.Control`
- Official ConPTY NuGet: https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY
- ConPTY samples: https://github.com/microsoft/terminal/tree/main/samples/ConPTY (MiniTerm, GUIConsole); https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session ; https://github.com/microsoft/vs-pty.net
- VT libs: https://github.com/darrenstarr/VtNetCore ; https://github.com/migueldeicaza/XtermSharp ; https://github.com/antmicro/TermSharp ; https://github.com/bastionzero/VtNetCorePatched
- GPU: https://github.com/microsoft/Win2D (Microsoft.Graphics.Win2D 1.4.0); https://github.com/amerkoleci/Vortice.Windows ; TerraFX.Interop.Windows; Microsoft.Windows.CsWin32; https://devblogs.microsoft.com/commandline/building-windows-terminal-with-winui/
- Notifications: https://learn.microsoft.com/en-us/windows/apps/develop/notifications/ ; https://learn.microsoft.com/en-gb/windows/apps/develop/notifications/app-notifications/app-notifications-quickstart
- Perf push: https://github.com/microsoft/microsoft-ui-xaml/discussions/11096 ; https://blogs.windows.com/windowsdeveloper/2024/11/07/so-whats-new-with-microsoft-native-ux-technologies/ ; AOT in WCT 8.2: https://visualstudiomagazine.com/articles/2025/04/03/windows-community-toolkit-8-2-adds-native-aot-support.aspx
- Size datapoint: https://nicksnettravels.builttoroam.com/packaged-unpackaged-self-contained/
