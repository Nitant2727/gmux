# ConPTY deep-dive (research for gmux)

Researched: 2026-07-04. Sources verified live where noted. This is the PTY layer gmux must build on; the single most important conclusion is in "The load-bearing question" and "Mitigations" below.

---

## TL;DR / Architecture-shaping conclusions

1. **Modern ConPTY (Windows Terminal 1.22+, Aug 2024 onward) passes VT output through essentially unmodified** — the old "VtEngine" re-render pipeline was deleted in PR [#17510](https://github.com/microsoft/terminal/pull/17510). Unknown OSC/DCS (OSC 9, 777, 99, 8, 52, …) reach the hosting terminal, in order.
2. **The inbox conhost.exe is NOT that ConPTY.** Windows 10 (21H2 = 19044, frozen at a ~2020 20H1 code baseline) and even current Windows 11 builds lag the terminal repo by months-to-years. On Windows 10 inbox conhost, unknown OSC sequences are unreliable at best (pre-[#4896](https://github.com/microsoft/terminal/pull/4896) behavior on the 19041 baseline) and ordering is broken until the 1.22 rewrite everywhere.
3. **Therefore gmux MUST bundle its own ConPTY**: `conpty.dll` + `OpenConsole.exe` from the MIT-licensed [`Microsoft.Windows.Console.ConPTY`](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY) NuGet (latest stable **1.24.260512001**, released 2026-05-22). This is exactly what WezTerm does ([wezterm#7774](https://github.com/wezterm/wezterm/issues/7774)) and what Microsoft explicitly recommends third-party terminals do ([terminal discussion #17608](https://github.com/microsoft/terminal/discussions/17608)). With the bundled pair, the notification-hook feature (OSC 9/777/99) is safe on every supported OS ≥ 10.0.17763.
4. The bundled pair changes some host obligations: gmux must answer **DSR-CPR (`ESC[6n`) queries after resizes** (new in the 1.24 line, [#18725](https://github.com/microsoft/terminal/issues/18725)), should implement **win32-input-mode** (`CSI ? 9001 h`), and must obey the **ClosePseudoConsole drain rules** (blocking close on pre-24H2 kernel32; the bundled `ConptyClosePseudoConsole` has its own semantics).

---

## (a) API surface

### Core Win32 API (kernel32.dll, since Windows 10 1809 / Server 2019)

Verified against Microsoft Learn ([CreatePseudoConsole](https://learn.microsoft.com/en-us/windows/console/createpseudoconsole), [ClosePseudoConsole](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole), [Creating a Pseudoconsole session](https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session)):

```c
HRESULT WINAPI CreatePseudoConsole(COORD size, HANDLE hInput, HANDLE hOutput,
                                   DWORD dwFlags, HPCON* phPC);
HRESULT WINAPI ResizePseudoConsole(HPCON hPC, COORD size);
void    WINAPI ClosePseudoConsole(HPCON hPC);
```

- Header `ConsoleApi.h` (via `Windows.h`), lib `Kernel32.lib`, also exported from `KernelBase.dll` / `API-MS-Win-Core-Console-l1-2-1.dll`.
- `hInput` = read end of the terminal→client pipe; `hOutput` = write end of the client→terminal pipe. **Synchronous (non-OVERLAPPED) handles only.** Anonymous `CreatePipe` pipes are the canonical choice.
- Streams are **UTF-8 text interleaved with VT sequences**, both directions (per the CreatePseudoConsole doc).
- `dwFlags`: `0` or `PSEUDOCONSOLE_INHERIT_CURSOR (1)` — inherit cursor position from a parent console session. If set, ConPTY emits a **DSR cursor query (`ESC[6n`) on `hOutput` at startup and blocks until a CPR reply (`ESC[row;colR`) arrives on `hInput`**. The docs warn the host must service this asynchronously on a background thread or the API caller can hang.
- The pseudoconsole is attached to a child via `STARTUPINFOEX` + `InitializeProcThreadAttributeList` + `UpdateProcThreadAttribute(..., PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, hPC, sizeof(hPC), ...)` + `CreateProcess(..., EXTENDED_STARTUPINFO_PRESENT, ...)`.
- After `CreateProcess`, the host must **close its copies of `inputReadSide` and `outputWriteSide`** so broken-pipe detection works at teardown.
- Docs warning: service input and output pipes **on separate threads**; single-threaded synchronous use deadlocks when a buffer fills.
- Startup race: killing the ConPTY while the client is still initializing produces client-side `0xc0000142` "failed to initialize" error dialogs (documented in "Creating a Pseudoconsole session").

### ClosePseudoConsole semantics (version-dependent — landmine)

From the [ClosePseudoConsole doc](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole) (web-verified):

- Closing sends **`CTRL_CLOSE_EVENT`** to every attached client; clients may keep writing output until they disconnect.
- **Pre-24H2 Windows: `ClosePseudoConsole` blocks indefinitely** until the pseudoconsole exits — so the host must either close the output pipe first or keep draining it from another thread. *"Don't call ClosePseudoConsole on the same thread that you're reading the output pipe from."*
- **Windows 11 24H2 (build 26100)+: `ClosePseudoConsole` returns immediately.** To learn when all clients are gone, read the output pipe until EOF.
- Practical rule for gmux: dedicate a reader thread per pane that drains until pipe EOF; treat pipe EOF (not process exit) as session end; never join reader thread while calling Close on it.

### The redistributable adds more surface (winconpty / conpty.dll)

From [`src/winconpty/winconpty.cpp`](https://github.com/microsoft/terminal/blob/main/src/winconpty/winconpty.cpp) (web-verified against main, 2026):

Exports (all `extern "C" HRESULT WINAPI`):

- `ConptyCreatePseudoConsole`, `ConptyCreatePseudoConsoleAsUser`
- `ConptyResizePseudoConsole`
- `ConptyClosePseudoConsole` — terminate + close
- `ConptyReleasePseudoConsole` — release the conpty handles without terminating (lets the session outlive the HPCON; pairs with the 24H2-style non-blocking teardown)
- `ConptyClearPseudoConsole` — clears ConPTY's internal buffer (Windows Terminal uses this for its "clear buffer" action so ConPTY's model matches the terminal after a scrollback clear)
- `ConptyShowHidePseudoConsole`, `ConptyReparentPseudoConsole` (window-ownership/handoff plumbing), `ConptyPackPseudoConsole`

Extra creation flags beyond `PSEUDOCONSOLE_INHERIT_CURSOR` (bundled DLL only, not inbox kernel32):

- `PSEUDOCONSOLE_AMBIGUOUS_IS_WIDE`
- `PSEUDOCONSOLE_GLYPH_WIDTH__MASK` with `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` / `PSEUDOCONSOLE_GLYPH_WIDTH_WCSWIDTH` / `PSEUDOCONSOLE_GLYPH_WIDTH_CONSOLE` — selects text-measurement mode (grapheme clusters vs wcswidth vs legacy console), matching the 1.22 grapheme-cluster work.

Host discovery: the redistributable `conpty.dll` launches **`OpenConsole.exe` found next to the DLL** (with `x64`/`arm64`/`x86` arch-subfolder fallback), and only falls back to system `conhost.exe` if not found. Command line it spawns (web-verified from source):

```
OpenConsole.exe --headless [--inheritcursor] [--ambiguousIsWide]
                [--textMeasurement graphemes|wcswidth|console]
                --width %hd --height %hd --signal 0x%tx --server 0x%tx
```

So consuming from Rust/C++ (non-.NET) = ship `conpty.dll` + `OpenConsole.exe` side by side, `LoadLibrary("conpty.dll")`, `GetProcAddress` the `Conpty*` exports. WezTerm does exactly this (`assets/windows/conhost/{conpty.dll, OpenConsole.exe}`, [wezterm#7774](https://github.com/wezterm/wezterm/issues/7774)); the files must be updated **as a matched pair**.

### Pipe-pair model recap

Terminal (gmux) writes keystrokes/VT-input to input pipe → ConPTY (conhost/OpenConsole `--headless`) cooks them into `INPUT_RECORD`s (or passes VT through if the client enabled `ENABLE_VIRTUAL_TERMINAL_INPUT`) → client reads via console APIs or stdin. Client writes via WriteFile/WriteConsole/console APIs → ConPTY maintains an internal screen buffer (so legacy `GetConsoleScreenBufferInfoEx`/`WriteConsoleOutput` apps still work) and emits UTF-8+VT to the output pipe → gmux parses and renders.

---

## (b) THE LOAD-BEARING QUESTION: does ConPTY pass through OSC 9 / 777 / 99 / 8 / 52?

### Short answer

- **Bundled modern ConPTY (≥ 1.22, current 1.24 line): YES — passed through, unmodified, in order.** This is the configuration gmux must ship.
- **Inbox conhost, Windows 11 (22H2–24H2 era): mostly yes but out-of-order/front-loaded relative to text** (pre-rewrite VtEngine flushing), with DCS dropped; exact behavior depends on which OS build's conhost snapshot you got.
- **Inbox conhost, Windows 10 21H2: unreliable — assume swallowed.** The Win10 conhost is frozen at a ~20H1 (19041, early-2020) baseline ([discussion #17608](https://github.com/microsoft/terminal/discussions/17608)), which predates most of the passthrough work.

### The history, with receipts

- **Old architecture ("ConPTY v1" / VtEngine, 2018–2024):** conhost parsed all client VT into its internal buffer, then an asynchronous renderer (`VtEngine`) *re-synthesized* normalized VT for the terminal. Sequences conhost recognized were re-emitted only if a re-emit path existed; unknown sequences went through an "unhandled → flush to terminal" path.
  - [PR #4896](https://github.com/microsoft/terminal/pull/4896) (merged **2020-03-12**): "When Conpty encounters an unknown string, flush immediately" — fixed reordering (#2011) by flushing the frame when an unknown sequence appears. Maintainer caveat: works "okay, but not amazing" because the unknown string lands *between rendered frames*. **This merge date is after the Windows 10 19041 code fork — do not count on it in Windows 10 inbox conhost.**
  - [OSC 8 hyperlinks: PR #7251](https://github.com/microsoft/terminal/pull/7251) (Sept 2020, WT 1.4 era) added OSC 8 to conhost + conpty re-emission. Not in Win10 inbox conhost.
  - [OSC 52: PR #5823](https://github.com/microsoft/terminal/pull/5823) (2020) added copy-to-clipboard in Windows Terminal itself; conhost-proper OSC 52 came only in 2025 ([#18943](https://github.com/microsoft/terminal/issues/18943) / [PR #18949](https://github.com/microsoft/terminal/pull/18949)). Under old ConPTY with a third-party host, OSC 52 travelled the "unknown → flush" path (post-#4896) — worked, but out of order.
  - [Issue #17313](https://github.com/microsoft/terminal/issues/17313) (Warp, June 2024, v1.19): **DCS sequences were dispatched only if recognized — arbitrary DCS dropped**; unhandled *OSC* were "flushed to the terminal" but **ordering was not preserved** when interleaved with text ([#17314](https://github.com/microsoft/terminal/issues/17314), [#11220](https://github.com/microsoft/terminal/issues/11220) "OSC sequences in prompt get front loaded").
- **New architecture (PR [#17510](https://github.com/microsoft/terminal/pull/17510), merged mid-2024; shipped Windows Terminal Preview 1.22, blog dated 2024-08-27):** VtEngine deleted; console API calls are translated to VT *synchronously*; **"any VT output that an application generates will now be given to the terminal unmodified."** Official blog claims: "higher fidelity for VT applications, 2x the I/O speed for VT heavy workloads (SGR), up to 16x the I/O speed for plaintext workloads", better resize/reflow ([WT Preview 1.22 release blog](https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/)). Follow-up [PR #17741](https://github.com/microsoft/terminal/pull/17741) "ConPTY: Flush unhandled sequences to the buffer" tightened unhandled-sequence handling (e.g. DA1 requests now pass to the hosting terminal and conhost stops answering them itself). #17313 and the passthrough umbrella [#1173](https://github.com/microsoft/terminal/issues/1173) (open since 2019) were closed by this work.
- **Consequence for gmux's specific sequences** (with bundled 1.24 pair):
  - `OSC 9 ; <text> BEL/ST` (iTerm2/ConEmu-style notification), `OSC 777 ; notify ; <title> ; <body> ST` (urxvt/VTE), `OSC 99 ... ST` (kitty desktop notifications): conhost has no consuming handler for these → passed through verbatim → **gmux's parser will see them**. Caution: OSC 9 is overloaded — ConEmu progress is `OSC 9 ; 4 ; st ; pct` and Windows Terminal interprets `9;4` as taskbar progress ([MS progress-bar tutorial](https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences), [Ghostty ConEmu OSC docs](https://ghostty.org/docs/vt/osc/conemu)); gmux must disambiguate `9;4;…` (progress) from `9;<free text>` (notification).
  - `OSC 8` hyperlinks: passed through (and understood by conhost since 2020, re-emitted; in 1.22+ passthrough is verbatim).
  - `OSC 52` clipboard: passed through; note WT deliberately supports only *write* (clipboard set), never read/query, for security ([#2946](https://github.com/microsoft/terminal/issues/2946), [#9479](https://github.com/microsoft/terminal/issues/9479)) — a sane policy for gmux too. DA1 advertising of OSC 52 support is being discussed in [#19017](https://github.com/microsoft/terminal/issues/19017).
  - Both `BEL` (0x07) and `ST` (`ESC \`) terminators are accepted by the conhost parser; C1 ST (0x9C) is off by default ([PR #11690](https://github.com/microsoft/terminal/pull/11690)).

### Confidence statement

That 1.22+ ConPTY passes arbitrary OSC through verbatim: **web-verified** (#17510 PR text, #17313 closure, 1.22 blog). That Windows 10 21H2 inbox conhost drops/mangles these specific sequences: **high-confidence inference** from the 20H1-baseline statement in discussion #17608 plus merge dates of #4896/#7251 — not directly tested this session. The mitigation (bundling) makes the inference moot architecturally.

---

## (c) Mitigations: bundle your own ConPTY

### The redistributable NuGet (the supported path)

- Package: **`Microsoft.Windows.Console.ConPTY`** on nuget.org. Latest stable **1.24.260512001, released 2026-05-22. License MIT. Minimum OS 10.0.17763** (web-verified on [nuget.org](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY)). CI builds exist as `CI.Microsoft.Windows.Console.ConPTY` (e.g. 1.22.250314001).
- Also attached to Windows Terminal GitHub releases as `Microsoft.Windows.Console.ConPTY.*.nupkg` ([wezterm#7774](https://github.com/wezterm/wezterm/issues/7774)). Note the WT MSIX contains `OpenConsole.exe` but the matched `conpty.dll` comes only from the NuGet.
- Contents: `conpty.dll` + `OpenConsole.exe` (a from-source conhost). Version numbers track Windows Terminal releases (1.22.x → 1.24.x; wezterm moved from 1.22.250204002 to 1.24.260402001 to fix a PowerShell-exit crash `0x80131623`).
- **Non-.NET consumption:** a .nupkg is a zip. Extract at build time (CI step), ship `conpty.dll` + `OpenConsole.exe` next to gmux.exe, `LoadLibrary` + `GetProcAddress` the `Conpty*` exports (do NOT link kernel32's `CreatePseudoConsole` if you want the bundled behavior — kernel32 always uses inbox conhost). In Rust: `libloading`, or the `conpty`/`portable-pty` crates patched to prefer the side-by-side DLL (wezterm's `portable-pty` does this dance already).
- **Version-pin and update deliberately**: the pair is matched; mixing versions or using a bundled `conpty.dll` with inbox conhost is unsupported.

### How Windows Terminal itself does it

WT spawns its own packaged `OpenConsole.exe --headless ... --signal ... --server ...` per session (plus "defterm" COM handoff for takeover of inbox-conhost sessions — not something gmux needs). gmux replicating the conpty.dll route gets identical behavior without the COM handoff machinery.

### Inbox vs bundled: what you gain by bundling

| Concern | Win10 21H2 inbox | Win11 24H2 inbox | Bundled 1.24 pair |
|---|---|---|---|
| OSC 9/777/99 reach host | unreliable/likely dropped | yes, may reorder (pre-1.22 flush path) — build-dependent | yes, verbatim, in order |
| Arbitrary DCS | dropped | dropped (pre-1.22) | passed through |
| OSC 8 | dropped (pre-#7251 baseline) | yes | yes |
| OSC 52 | dropped | flushed (ordering caveat) | yes |
| ClosePseudoConsole | blocks until drained | returns immediately | `ConptyClosePseudoConsole`/`ConptyReleasePseudoConsole` semantics, host-controlled |
| Resize reflow | old reflow bugs | improved | 1.22 rewrite: "we simply don't need to do anything during a reflow anymore" |
| Grapheme/emoji width | legacy | partial | `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` |
| Fix cadence | frozen (servicing only, ~2020 baseline) | yearly-ish snapshots, "bug fixes contained for a while" | you choose; ship updates with gmux |

(Per [discussion #17608](https://github.com/microsoft/terminal/discussions/17608): delta between Win10 conhost and main was "1000 files changed, 115079 insertions, 68605 deletions" — backporting is infeasible; Microsoft's answer is the NuGet.)

Residual gap bundling cannot fix: when a *user* launches a console app outside gmux, the OS attaches inbox conhost. Irrelevant for gmux's own panes — every pane is created by gmux through the bundled DLL.

---

## (d) Known quirks and landmines

1. **ClosePseudoConsole hang (pre-24H2)** — see section (a). Drain or close the output pipe first; never Close on the reader thread. Web-verified from MS Learn.
2. **`PSEUDOCONSOLE_INHERIT_CURSOR` handshake** — if set, respond to `ESC[6n` on the output stream with `ESC[<r>;<c>R` on input, from a background thread, or creation/teardown deadlocks (MS Learn, web-verified). gmux (a GUI host, not itself a console client) generally should NOT set this flag.
3. **NEW in the 1.24 line: ConPTY sends DSR-CPR (`ESC[6n`) after every resize** ([#18725](https://github.com/microsoft/terminal/issues/18725), closed via PRs #19535/#19089, milestone Terminal v1.24) because ConPTY's reflow may disagree with the host terminal's reflow. **gmux MUST answer CPR queries generically** (track them and reply with the current cursor cell) — this is now core protocol, not an edge case. The request blocks the console server until answered (with debouncing for rapid resizes).
4. **Resize behavior**:
   - Old ConPTY: reflow bugs, duplicated prompts, content loss when growing rows ([discussion #16879](https://github.com/microsoft/terminal/discussions/16879)), wrong cursor after resize in VS Code, and cmux itself hit "resizing duplicates terminal contents many times" ([cmux#3052](https://github.com/manaflow-ai/cmux/issues/3052)) — a warning specifically relevant to gmux's product category.
   - **Resize near client attach can be ignored** ([#10400](https://github.com/microsoft/terminal/issues/10400)): a `ResizePseudoConsole` issued just before/after the client connects may be lost. Mitigation: create the ConPTY at the correct initial size; after spawning, debounce and re-assert the final size; avoid resizing during the first output burst.
   - Rapid successive resizes may coalesce with the *last* one not necessarily applied (#10400) — re-send final geometry after drag-resize ends.
   - `ResizePseudoConsole` with garbage accepted historically ([#3447](https://github.com/microsoft/terminal/issues/3447)) — validate before calling.
   - 1.22+ rewrite claims reflow largely no-ops inside ConPTY (host owns reflow) — much better, but keep the debounce.
5. **ConPTY's internal buffer & scrollback**: ConPTY keeps a viewport-sized internal screen buffer (needed to answer console-API queries from legacy apps). It has **no scrollback**; the host terminal owns scrollback. Under old ConPTY, output was re-rendered from that buffer (normalized SGR, repositioned cursor, wrapping quirks [#405](https://github.com/microsoft/terminal/issues/405)); under 1.22+ VT flows through and the buffer is only a shadow for API clients. Clear-scrollback: `ESC[3J` (ED3) passes through; the bundled DLL additionally offers `ConptyClearPseudoConsole` to reset ConPTY's own buffer when gmux implements a "clear buffer" action (keeps client cursor math consistent).
6. **Input encoding / win32-input-mode**:
   - Plain path: write UTF-8 (incl. `0x03` for Ctrl+C — conhost cooks it into `CTRL_C_EVENT` for `ENABLE_PROCESSED_INPUT` clients) and xterm-style CSI for arrows/F-keys.
   - ConPTY emits **`CSI ? 9001 h`** to ask the host for **win32-input-mode** ([spec: doc/specs/#4999 Improved keyboard handling in Conpty](https://github.com/microsoft/terminal/blob/main/doc/specs/%234999%20-%20Improved%20keyboard%20handling%20in%20Conpty.md), implemented in [PR #6309](https://github.com/microsoft/terminal/pull/6309)). Format: `CSI Vk;Sc;Uc;Kd;Cs;Rc _` encoding full KEY_EVENT_RECORDs. gmux should implement it — it is the only way to deliver key-up events, exotic chords (Ctrl+Space, Ctrl+/, shifted F-keys) and correct PSReadLine behavior. Quirks: hard reset (RIS) turns it off ([#15461](https://github.com/microsoft/terminal/issues/15461)); DECRQM reporting was buggy ([#17737](https://github.com/microsoft/terminal/issues/17737)); mouse events must NOT be encoded as win32-input ([#15083](https://github.com/microsoft/terminal/issues/15083)); community writeups: [Discussion #13239](https://github.com/microsoft/terminal/discussions/13239), [dev.to on taming win32-input-mode from Go](https://dev.to/andylbrummer/taming-windows-terminals-win32-input-mode-in-go-conpty-applications-7gg).
   - **Mouse**: old ConPTY swallowed client mouse-mode requests ([#376](https://github.com/microsoft/terminal/issues/376)); [PR #9970](https://github.com/microsoft/terminal/pull/9970) made ConPTY pass DECSET 1000/1002/1003/1006 through to the host. Host sends SGR-encoded mouse (`CSI < b;x;y M/m`) into the input pipe; conhost synthesizes MOUSE_EVENT INPUT_RECORDs for API clients. Bundled 1.24 pair: fully working; ancient inbox conhost: no mouse for VT apps.
   - `ENABLE_VIRTUAL_TERMINAL_INPUT` is a *client-side* mode; ConPTY historically failed to notify the host when clients toggle related DECSET state ([#6859](https://github.com/microsoft/terminal/issues/6859)) — mostly resolved by passthrough in 1.22+.
7. **UTF-8 details**: both pipes are always UTF-8 regardless of client codepage; ConPTY transcodes for the client's codepage on the inside. gmux's parser must tolerate UTF-8 code points **split across ReadFile chunks** (buffer partial sequences). Historic bugs with responses >4KB being corrupted were fixed in the 1.22 line (1.22 blog). Emoji/wide-glyph measurement: bundled DLL supports `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES` — pick it and implement matching grapheme-cluster width logic in gmux's renderer, or widths will disagree with ConPTY's internal buffer (cursor drift for API apps).
8. **Ctrl+C / signals**: no POSIX signals. `0x03` byte → CTRL_C_EVENT (if client processed-input); `CTRL_CLOSE_EVENT` on ClosePseudoConsole. `GenerateConsoleCtrlEvent` from gmux's process does not reach ConPTY clients (different console); to force-kill, use the process handle / job objects. Put each pane's child in a **Job Object with KILL_ON_JOB_CLOSE** for reliable tree cleanup on pane close (ClosePseudoConsole terminates attached clients, but detached grandchildren that allocated their own console can escape).
9. **Focus events**: WT/ConPTY use focus-in/out (`CSI I` / `CSI O`); PR #17510 notes host focus events could split ongoing VT sequences mid-stream in edge cases — send focus events only at frame boundaries.
10. **Anti-virus**: AVs sometimes block conpty/OpenConsole spawns (seen with VS Code's terminal, [VS Code troubleshooting doc](https://code.visualstudio.com/docs/supporting/troubleshoot-terminal-launch)); sign gmux + bundled binaries, keep OpenConsole.exe name intact.

---

## (e) Per-shell behavior under ConPTY

- **cmd.exe**: pure legacy console-API client; fully dependent on ConPTY's buffer emulation. Unremarkable. `cls` clears viewport; ED3 for scrollback.
- **Windows PowerShell 5.1**: console-API heavy (PSReadLine mixes API + VT). Works; needs win32-input-mode for full PSReadLine fidelity. Codepage mojibake possible if scripts assume OEM CP; not ConPTY's fault. Watch: SGR 37/40 emission artifact noted in #17510.
- **PowerShell 7**: mostly VT; best case. Known crash `0x80131623` on exiting some TUIs with the 1.22.250204002 bundled pair — **fixed in the 1.24 pair** ([wezterm#7774](https://github.com/wezterm/wezterm/issues/7774)); ship ≥1.24.
- **WSL (wsl.exe)**: wsl.exe is a Windows console client that relays a Linux pty; inside is a real pty, so Linux-side apps behave natively and their OSC sequences flow Linux pty → wsl.exe → ConPTY → gmux (subject to the same passthrough rules — another reason to bundle 1.22+). Historic warts: tty size reporting ([WSL#4327](https://github.com/microsoft/WSL/issues/4327)); resize propagation to the Linux pty is driven by ConPTY resize (SIGWINCH arrives when ResizePseudoConsole lands). WSL is the workload the original passthrough-mode issue (#1173) was about; the 1.22 rewrite is effectively that.
- **Git Bash / MSYS2**: two distinct situations.
  - gmux spawns `bash.exe` (msys) directly under its ConPTY: bash is a Cygwin-runtime console client; the MSYS runtime (Cygwin ≥3.1) detects the console and does its own translation. This is the same path as Windows Terminal — well-trodden. `winpty` wrappers are NOT needed under a ConPTY host (they exist for mintty's pipe-based ptys) ([ConEmu docs](https://conemu.github.io/en/CygwinMsys.html), [mintty](https://mintty.github.io/)).
  - Cygwin's *internal* pty + its own ConPTY usage (`MSYS=enable_pcon` / `disable_pcon`) only matters when mintty is the host — irrelevant to gmux.
  - Quirks: MSYS path conversion (`MSYS_NO_PATHCONV`) surprises when gmux passes Windows-style args; Ctrl+C in msys processes is emulated (Cygwin translates console ctrl events to SIGINT) and can be laggy for native children.
- **Claude Code / Codex CLI / Aider / Gemini CLI** (node/python TUI apps): all pure-VT clients; with bundled 1.22+ their OSC notification output arrives verbatim. Claude Code on Windows commonly runs under Git Bash or PowerShell — both fine per above.

---

## (f) ARM64

- ConPTY API is architecture-neutral; Windows on ARM64 (incl. Windows 11 ARM64) has it since the same 1809 baseline.
- Windows Terminal (and thus OpenConsole/conpty.dll) builds and ships **x86, x64, ARM64** ([building docs](https://deepwiki.com/microsoft/terminal/7.1-building-and-testing)); `winconpty.cpp` explicitly probes `x64`/`arm64`/`x86` subfolders when locating OpenConsole.exe — the redistributable model is arch-aware by design (web-verified from source).
- Rule: **conpty.dll must match the gmux process architecture; OpenConsole.exe should be native ARM64 on ARM64** (an x64 OpenConsole would run emulated — works but wastes CPU). Ship per-arch bundles (gmux-x64 with x64 pair, gmux-arm64 with arm64 pair) rather than the subfolder layout, or use the subfolder convention conpty.dll already understands.
- No ARM64-specific ConPTY bugs surfaced in this research (searched terminal repo; nothing notable). Confidence: moderately verified (absence of evidence).

---

## Recommended gmux PTY-layer design (synthesis)

1. Vendor `Microsoft.Windows.Console.ConPTY` **1.24.260512001** (extract nupkg in CI; it is MIT — redistribution-clean). Load `conpty.dll` beside gmux.exe; call `ConptyCreatePseudoConsole` with `PSEUDOCONSOLE_GLYPH_WIDTH_GRAPHEMES`; never fall back silently to kernel32 CreatePseudoConsole (feature-detect and warn — notification hooks are degraded on inbox conhost).
2. Per pane: input-writer thread + output-reader thread; reader drains to EOF; teardown = close input write end → `ConptyClosePseudoConsole` → keep draining until EOF → reap process via Job Object.
3. VT parser must handle: OSC with BEL and ST terminators (route 9/777/99/8/52 to the notification/hyperlink/clipboard subsystems; disambiguate OSC 9;4 progress), DSR-CPR queries (`CSI 6 n`) answered always, DA1 (`CSI c`) answered (new ConPTY forwards client queries to the host), win32-input-mode emission when `?9001h` received, SGR mouse encoding when client requests mouse modes, DECSET 2026 synchronized output (WT 1.23 added it — good frame batching signal).
4. Resize: debounce (~50ms), re-assert final size after drag end, never resize between CreateProcess and first output.
5. Session restore across reboot: ConPTY sessions do NOT survive the host process — "detach/reattach" must be gmux-level (keep OpenConsole+client alive under a hidden host service/process owning the HPCON, reconnect the GUI to it via gmux's own pipe server; `ConptyReleasePseudoConsole`/`ConptyReparentPseudoConsole` exist for related scenarios). True reboot survival = re-spawn + replay saved layout/cwd, like tmux-resurrect, not process persistence.

## Source index

- https://learn.microsoft.com/en-us/windows/console/createpseudoconsole
- https://learn.microsoft.com/en-us/windows/console/closepseudoconsole
- https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
- https://learn.microsoft.com/en-us/windows/console/resizepseudoconsole
- https://github.com/microsoft/terminal/pull/17510 (VtEngine removal; passthrough)
- https://github.com/microsoft/terminal/pull/17741 (flush unhandled sequences)
- https://github.com/microsoft/terminal/issues/17313, /issues/17314, /issues/11220 (pre-1.22 OSC/DCS gaps)
- https://github.com/microsoft/terminal/issues/1173 (passthrough mode, closed by rewrite)
- https://github.com/microsoft/terminal/pull/4896 (2020 unknown-sequence flush)
- https://github.com/microsoft/terminal/pull/7251 (OSC 8), /pull/5823 + /issues/18943 + /pull/18949 (OSC 52)
- https://github.com/microsoft/terminal/issues/376 + /pull/9970 (mouse), /pull/6309 + doc/specs/#4999 (win32-input-mode)
- https://github.com/microsoft/terminal/issues/18725 (CPR after resize, v1.24), /issues/10400, /discussions/16879, /issues/3447 (resize)
- https://github.com/microsoft/terminal/discussions/17608 (inbox conhost servicing policy)
- https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY (1.24.260512001, MIT)
- https://github.com/wezterm/wezterm/issues/7774 (bundled pair consumption, PS crash fix)
- https://github.com/microsoft/terminal/blob/main/src/winconpty/winconpty.cpp (exports, flags, OpenConsole discovery)
- https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/ (1.22 ConPTY claims)
- https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences, https://ghostty.org/docs/vt/osc/conemu (OSC 9 overloading)
- https://github.com/manaflow-ai/cmux/issues/3052 (competitor resize-duplication bug)
- https://mintty.github.io/, https://conemu.github.io/en/CygwinMsys.html, https://www.msys2.org/news/ (MSYS2/Cygwin)
- https://github.com/microsoft/WSL/issues/4327 (WSL tty size)
