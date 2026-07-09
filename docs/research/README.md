# gmux research corpus

Eight web-verified deep-dives (2026-07-04) that back every claim in [../../ARCHITECTURE.md](../../ARCHITECTURE.md),
plus the adversarial verification log. Each file cites primary sources inline (Microsoft Learn, GitHub
source/issues/PRs, nuget.org/crates.io, official docs) and labels confidence
(web-verified / model-knowledge / uncertain).

| File | Question it answers |
|---|---|
| [cmux-product.md](cmux-product.md) | What is the macOS product gmux mirrors? Feature set, notification pipeline, socket API, tmux story, session restore, licensing (GPL-3.0). |
| [conpty.md](conpty.md) | The PTY layer: does ConPTY pass OSC 9/777/99 through? (Yes with the bundled modern pair; unreliable on inbox Win10 conhost.) Lifecycle, quirks, per-shell behavior, ARM64. |
| [osc-notifications.md](osc-notifications.md) | Exact wire formats for OSC 9 / 777 / 99 / 133 / 7 / 8 / 52 and what each agent CLI actually emits. The killer-feature spec. |
| [windows-toasts.md](windows-toasts.md) | Toast delivery from an unpackaged Rust app: registry AUMID vs WinAppSDK, activation, foreground-rights, DND detection, the full attention-channel stack. |
| [rust-stack.md](rust-stack.md) | Candidate stack A: VT crates, PTY, wgpu rendering, winit/egui chrome, ARM64, toasts+pipes from Rust, licenses, maturity. |
| [dotnet-stack.md](dotnet-stack.md) | Candidate stack B: WinUI 3 state, the terminal-control gap, GPU text in .NET, ConPTY P/Invoke, toasts, distribution. |
| [mux-architecture.md](mux-architecture.md) | tmux semantics natively on Windows: WezTerm's mux design, detach/reattach process model, reboot restore, IPC, scrollback storage. |
| [prior-art-gaps.md](prior-art-gaps.md) | What already exists (Windows Terminal, WezTerm, Warp, wmux, psmux, libghostty) and the exact empty intersection gmux fills. |
| [verification.md](verification.md) | **The adversarial fact-check log** — 32 verdicts across two passes; no refutations; every nuance correction folded into the architecture docs. |
| [m0-spikes.md](m0-spikes.md) | **M0 empirical results** — the four de-risking spikes run on real Windows 11: killer-feature GO (OSC 9/777/99 through ConPTY), toast PASS, libghostty-vt rejected. |

Research was run as parallel fan-out workflows; the durable record is these Markdown files. They are
reference material, not living design docs — the design lives in ARCHITECTURE.md / DECISIONS.md.
