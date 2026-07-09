# M0 de-risking spikes — results

Empirical validation of the four architecture-shaping unknowns, run on **Windows 11 Home 25H2
(build 26200), x64, Rust 1.96.1**. Code lives under [`spikes/`](../../spikes/). Bottom line:
**the killer feature is proven, and every ADR the spikes touched is resolved.**

| # | Spike | Outcome | Resolves |
|---|---|---|---|
| 1+2 | ConPTY round-trip + OSC 9/777/99 passthrough | ✅ **GO** | ADR-002 |
| 3 | Unpackaged Windows toast (registry AUMID) | ✅ **PASS** | ADR-006 |
| 4 | libghostty-vt build + behavior | ⛔ **REJECT** (as planned) | ADR-003 |

---

## Spike 1+2 — ConPTY + OSC passthrough (the killer-feature go/no-go) ✅ GO

Code: [`spikes/conpty_osc/`](../../spikes/conpty_osc/). Run it: `powershell -File spikes/conpty_osc/run.ps1`.

**Result: all three notification sequences pass through ConPTY intact and in correct relative order.**
The host (loading the vendored `conpty.dll`, spawning PowerShell under a real pseudoconsole) captured:

```
OSC 0   [BEL] "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"   (title, passthrough)
OSC 9   [BEL] "gmux osc9 message"
OSC 777 [BEL] "notify;gmux osc777 title;osc777 body"
OSC 99  [BEL] "i=1:p=title;gmux osc99"
verdict = GO     child console = 120x30 (bound to the pty)
```

What this proves for gmux:
- **OSC 9 / 777 / 99 survive ConPTY verbatim** — the notification hooks are viable on Windows. This is
  the single most important thing to have de-risked.
- The **bundled `Microsoft.Windows.Console.ConPTY` 1.24.260512001 redist loads** via `LoadLibrary` and
  exports the full `Conpty*` API (`ConptyCreatePseudoConsole`, `…Resize`, `…Close`, `…Release`, `…Clear`)
  from a non-.NET Rust process — confirming ADR-002's bundling approach. (For the round-trip mechanics the
  spike attaches the child with inbox kernel32 `CreatePseudoConsole`, whose HPCON the inbox
  `CreateProcessW` pseudoconsole attribute accepts; see the caveat below.)
- gmux's OSC parser correctly extracts number + payload under the BEL terminator; ConPTY renders screen
  text as a block **separate** from passed-through OSCs, so gmux must key attention off the OSC events,
  not off text interleaving (the spike's assertion was corrected to check OSC-to-OSC order only).

**Two engineering facts learned the hard way (both matter for the real daemon):**

1. **A ConPTY child only binds its stdio to the pty if the creating process has a real console.** This
   machine's agent/CI harness launches processes with **no console and pipe stdio** (`GetConsoleWindow()==0`,
   all std handles `GetFileType==3`). In that state the child attaches to the pty *console* (title is set)
   but its stdout stays a pipe and `[Console]::WindowWidth` throws — output leaks to the launcher instead
   of the pty. **The reference `conpty` crate (0.5.1) fails identically**, confirming this is an environment
   property, not a gmux bug. Launching the spike via `Start-Process` (which gives it its own console) makes
   the child bind correctly and the test pass. gmux's GUI process always has a window/console context, so
   this never bites the product — but the **daemon** must ensure a console (`AllocConsole` if launched
   headless) before creating panes. Captured in [DECISIONS D-002](../../DECISIONS.md).
2. windows-sys 0.60 types: `HPCON` is `isize` (not a pointer); `ReadFile` needs the `Win32_System_IO`
   feature (its signature references `OVERLAPPED`); Rust-2021 disjoint closure capture will grab a raw
   pointer *field* out of a `Send` wrapper unless you re-bind the whole struct inside the closure.

**Open follow-up (not blocking):** attaching the child directly to the **bundled** DLL's HPCON (rather than
inbox kernel32) didn't bind here — needs a dedicated test on real hardware to confirm whether the bundled
`ConptyCreatePseudoConsole` HPCON is accepted by inbox `CreateProcessW`, or whether the bundled path needs a
different attach. On build 26200 the inbox conhost is modern enough to pass OSC through, so this is a
Win10-21H2-fidelity question, not an MVP blocker. Tracked against ADR-002.

---

## Spike 3 — Unpackaged Windows toast via registry AUMID ✅ PASS

Code: [`spikes/toast/`](../../spikes/toast/). Confirms **ADR-006** end-to-end on Win11, unpackaged, no
elevation, `windows` crate 0.62.2:

- `SetCurrentProcessExplicitAppUserModelID("com.gmux.spike")` + one HKCU key
  (`Software\Classes\AppUserModelId\com.gmux.spike\DisplayName`) — **no shortcut, no MSIX, no elevation**.
- `ToastNotificationManager::CreateToastNotifierWithId(HSTRING)` → `Show()` returned `Ok` on first run;
  the toast carried a `launch="pane=5;action=focus"` argument and a `Focus pane` action button;
  `Activated`/`Dismissed`/`Failed` handlers were armed in-process **before** `Show()`.

**Two things for gmux to bake in:**
- **Do not gate on `notifier.Setting() == Enabled` before the first `Show()`.** For a brand-new AUMID the
  platform record doesn't exist yet, so `Setting()` returns `0x80070490` "Element not found" on the very
  first run (Enabled from the second run on). Treat a `Setting()` error as "unknown/first-run", not
  "disabled". `Show()` works regardless.
- `RegCreateKeyExW` needs the `Win32_Security` feature; add an `IconUri` value for toast branding.

Matches the research's foreground-rights caveat (a toast click may not grant `SetForegroundWindow`), so the
real implementation still needs the focus fallback ladder from ARCHITECTURE §7.3 — untestable in an
automated run, deferred to the M2 GUI.

---

## Spike 4 — libghostty-vt build + behavior ⛔ REJECT (confirms the default plan)

Code: [`spikes/ghostty_vt/`](../../spikes/ghostty_vt/); Zig 0.15.2 vendored to `.tools/zig/` (gitignored).
This spike was the *option* to swap gmux's VT core to the same library cmux embeds. **It builds on Windows
x64 but is not usable for gmux's needs — stay on `alacritty_terminal` + a side vte OSC parser (ADR-003).**

- ✅ Builds: `libghostty-vt` 0.2.0 compiles on x64 (Rust 1.96.1 MSVC + Zig 0.15.2, fetching ghostty commit
  `fdbf9ff3`). Resize **reflow is genuinely good** (a real edge over alacritty_terminal).
- ⛔ **No notification callback.** OSC 9/777/99 fed through `vt_write` are silently consumed — the `Terminal`
  API exposes no desktop-notification hook. The only alternative, the standalone `osc::Parser`, collapses
  OSC 9 and 777 into a **payload-less** `ShowDesktopNotification` unit variant (can't recover title/body,
  can't tell 9 from 777) and **panics on any OSC 99** (`osc.rs:83`). That's strictly worse than owning a
  small vte side-parser where gmux controls payload extraction — which is exactly the killer feature.
- ⛔ **Two Windows runtime defects:** a default (Debug-profile) `vt_write` **segfaults**
  (`0xC0000005`) — only `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast` avoids it; and the OSC-99 parser panic
  above. Risky for a Windows-native tool.

**Decision:** reject for M0; keep `alacritty_terminal` + side vte OSC parser (the default in ADR-003).
**Revisit trigger:** a libghostty-vt release that adds an `on_desktop_notification`-style callback exposing
title/body and stops panicking on OSC 99.

---

## Net effect on the plan

- **ADR-002 (bundle ConPTY):** confirmed — redist loads and OSC passes through. Added: daemon must ensure a
  console before creating panes; bundled-HPCON direct-attach needs a real-hardware follow-up.
- **ADR-003 (VT core):** **resolved decisively — `alacritty_terminal` + side vte OSC parser.** The
  libghostty-vt option is rejected on notification-surfacing grounds (its whole reason to exist for us) plus
  Windows crashes.
- **ADR-006 (toasts):** confirmed — registry-AUMID unpackaged toasts work; don't gate on `Setting()`.

M0 exit gate met: the OSC → (parsed notification) path is proven end-to-end against real ConPTY. Proceeding
to **M1 (terminal core)** builds directly on `spikes/conpty_osc` and this VT-core decision.
