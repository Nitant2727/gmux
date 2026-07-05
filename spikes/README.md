# M0 — de-risking spikes

Throwaway code that empirically resolves the four architecture-shaping unknowns **before** committing to
the real workspace. Each spike is a standalone cargo project (own `target/`, no shared workspace) so they
build and run independently. Findings are consolidated in
[../docs/research/m0-spikes.md](../docs/research/m0-spikes.md).

| Spike | Crate | Resolves | Exit criterion |
|---|---|---|---|
| 1+2 | [`conpty_osc/`](conpty_osc/) | ADR-002 — bundled ConPTY + OSC passthrough | `printf '\e]9;…'`, `\e]777;…`, `\e]99;…` inside a real pane arrive **intact and in order** at the host parser. **Killer-feature go/no-go.** |
| 3 | [`toast/`](toast/) | ADR-006 — unpackaged toast | Registry-only AUMID → `Show()` displays a real toast; click fires the in-proc `Activated` callback with arguments. Foreground-rights behavior observed. |
| 4 | [`ghostty_vt/`](ghostty_vt/) | ADR-003 — VT core choice | `libghostty-vt` builds on Windows x64 **and** its OSC dispatch + resize reflow behave as documented (or: fall back to alacritty_terminal + side parser). |

## Setup

The ConPTY spike needs the redistributable pair (`conpty.dll` + `OpenConsole.exe`). Fetch it (MIT-licensed,
from nuget.org) — not committed to the repo:

```powershell
powershell -ExecutionPolicy Bypass -File spikes/fetch-conpty.ps1
```
