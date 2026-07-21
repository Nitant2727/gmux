# Parked

Cut scope doesn't die; it parks. One line per item: date · item · why parked · wake condition.

## Open

- 2026-07-19 · **Owed live checks** (desktop-interactive, each <30s; desktop was in use during every automated verify window): OSC 8 Ctrl+click open · Alt+1..9 tab switch · prompt jump Ctrl+Up/Down · busy-close band (`Start-Sleep 60` then Ctrl+Shift+W) · palette flow (Ctrl+Shift+P, type, Enter) · scrollback export Ctrl+Shift+S · split resize Alt+Shift+arrows · link hover tooltip · focus-follows-mouse config · copy mode Ctrl+Shift+M · divider double-click equalize · sidebar overflow wheel (12+ tabs) · zoom title badge · toast click lands on the notifying pane · wire: one idle minute at the machine, or organic use.
- 2026-07-19 · Search band covers the bottom pty row while searching (pane not resized daemon-side) · accepted round-6 tradeoff to avoid pty churn · wake: user complaint, or a SEARCH_BAR-aware pane_chrome_y redesign.
- 2026-07-19 · Drop-file path quoting is PowerShell-style only (bash/wsl panes get the same double quotes) · the GUI doesn't know the pane's shell · wake: per-pane shell detection (shell integration could report it).
- 2026-07-19 · Workspace-unified test binaries intermittently WDAC-blocked (os error 4551) — the block roams with content hashes (gmux-gui r7, vt r10, pipe r11, mux-pane r14) · machine policy, not code; every crate green solo · wake: policy change or CI-only battery (CI runs the full workspace green).
- 2026-07-20 · ARM64 zip **compiles** in the release workflow but has never executed on ARM64 hardware · no device · wake: an ARM64 Windows machine.
- (user-blocked) · Code signing for released binaries · needs a certificate · wake: cert available; README documents the SmartScreen consequence meanwhile.
- 2026-07-21 · Browser pane occasionally opens collapsed (~276x45) instead of 1024x768 (2nd launch of a session; 1st was correct) · low-priority WebView2 window-sizing quirk · wake: reproduce + fix in a browser-polish round.

## Cleared

- Rename-survives-restart — proven live 2026-07-19 (round 14: pipe rename + hard daemon kill + relaunch).
- `wait-for` all four modes + zombie-pane reaping — proven live 2026-07-20 (round 23).
- `screenshot` CLI — proven live 2026-07-20 (round 24).
- Cell wire elision (80% cut) — measured live 2026-07-21 (round 26).
- Release pipeline — proven end-to-end 2026-07-20 (round 27: tag → green run → published v0.1.0 → downloaded asset runs).
- Sidebar tab overflow — fixed round 21 (windowed rows + wheel scroll; live check folded into the owed list above).
