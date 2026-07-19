# Parked

Cut scope doesn't die; it parks. One line per item: date · item · why parked · wake condition.

- 2026-07-19 · Live verification of round-11 features (OSC 8 Ctrl+click, Alt+1..9 tab switch) · desktop in active use during every verify window so far · wake: a free desktop, or user runs the manual checks. **Rename-survives-restart: CLEARED 2026-07-19 (round 14, proven live via pipe rename + hard daemon kill + relaunch).** Prompt jump (r12), busy-close band (r13), and palette flow (r14) join the owed-live list.
- 2026-07-19 · Workspace-unified test binaries intermittently WDAC-blocked (os error 4551): gmux-gui (r7), gmux-vt (r10), gmux-pipe (r11) — the block roams with content hashes · machine policy, not code; every crate is green built solo · wake: WDAC policy change, or CI on an unrestricted machine.
- 2026-07-19 · Search band covers the bottom pty row while searching (pane not resized daemon-side) · accepted round-6 tradeoff to avoid pty churn · wake: user complaint, or a SEARCH_BAR-aware pane_chrome_y redesign.
- 2026-07-19 · Drop-file path quoting is PowerShell-style only (double quotes + doubling); bash/wsl panes receive the same quoting · round-10 ponytail — active pane's shell is unknown to the GUI · wake: per-pane shell detection (OSC 7/shell integration already tracks cwd; shell kind could ride along).
