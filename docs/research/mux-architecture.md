# Multiplexer Architecture Research — tmux Semantics Natively on Windows

Research date: 2026-07-04. Sources verified against live web this session unless marked otherwise.
Scope: how gmux should implement sessions/windows/panes, detach/reattach, reboot-surviving
restore, IPC, and scrollback storage on Windows (ConPTY, Win10 21H2+ / Win11, x64 + ARM64).

---

## TL;DR / Architecture recommendation

Adopt the **WezTerm split** (it is the only shipped, proven implementation of this exact
problem on Windows): a **per-user background mux daemon that owns all ConPTYs and the
canonical terminal state** (grid + scrollback), plus a **GUI client that renders from daemon
state over a local pipe**. Detach = GUI disconnects, daemon keeps running. Reattach = GUI
reconnects and re-fetches state. Reboot restore = daemon checkpoints layout + per-pane cwd +
VT-serialized scrollback to disk (exactly the Windows Terminal `buffer_<guid>.txt` approach)
and re-spawns shells on next start with restored scrollback replayed as inert text above a
`[Restored <timestamp>]` divider. Clone tmux's addressing (`$session` / `@window` / `%pane`,
`session:window.pane` targets) and its `send-keys` / `capture-pane` CLI semantics verbatim.
For the v1 remote story, implement a **tmux control-mode (`tmux -CC`) client** — the exact
mechanism iTerm2 uses — instead of shipping a gmux server for Linux.

---

## (a) WezTerm's mux design — the closest existing thing

Primary sources:
- https://wezterm.org/multiplexing.html (official docs)
- https://deepwiki.com/wezterm/wezterm/2.2-multiplexer-architecture (code-indexed walkthrough of the `mux` crate)
- https://wezterm.org/config/lua/config/default_mux_server_domain.html

### Object model

WezTerm's hierarchy: **Domain → Window → Tab → Pane**.

- `Mux` — a process-wide singleton (`Mux::get()`) in the `mux` crate holding thread-safe
  (`RwLock`) maps of windows by id, tabs, panes, domains, connected clients, and notification
  subscribers.
- `Domain` (trait) — *where panes execute*. Implementations:
  - `LocalDomain` — processes spawned directly (ConPTY on Windows), also WSL and serial.
  - `RemoteSshDomain` — panes over an ssh session (libssh2), requires wezterm on the remote.
  - `ClientDomain` — proxy to a remote `wezterm-mux-server` (over unix socket, ssh, or TLS).
- `Window` — ordered list of tabs + active-tab index.
- `Tab` — a **binary tree of splits** containing panes, plus size and zoom state. (tmux calls
  this a "window"; wezterm's Tab == tmux window.)
- `Pane` (trait) — two key impls:
  - `LocalPane` — wraps a live PTY + `Terminal` (the VT parser/grid state machine from the
    `term`/`wezterm-term` crate). Per-pane threads: `read_from_pane_pty()` pulls raw PTY bytes
    into a socketpair; `parse_buffered_data()` parses escape sequences into batched `Action`s.
  - `ClientPane` — proxy for a pane living in a mux server; caches remote screen state in a
    `RenderableState`, fetches line ranges **on demand** rather than streaming everything.

### GUI process vs mux server process

- **GUI mode**: `wezterm-gui` embeds a *full* Mux in-process; local panes run inside the GUI
  process (lowest latency). This means the default local domain **dies with the GUI** —
  wezterm's local domain has no detach.
- **Server mode**: `wezterm-mux-server` is a separate headless daemon hosting the Mux and all
  pane lifecycles. The GUI connects as a client (`wezterm connect <domain>`), creating
  `ClientPane` proxies. Multiple GUIs can attach to the same server; sessions persist when the
  GUI exits. This is wezterm's detach/reattach.
- **Hybrid is the norm**: a single GUI can simultaneously host a local domain and attach
  client domains. `config.default_gui_startup_args = { 'connect', 'unix' }` makes the GUI
  attach to the daemon by default, giving "everything is detachable" behavior.

### Wire protocol (the `codec` crate)

Custom PDU-based RPC, not JSON. Frame = `tagged_len (leb128) | serial (leb128) | ident
(leb128) | data (bincode)`; tagged_len carries a compression bit. `serial` matches responses
to requests; `ident` selects the PDU variant. The server-side `SessionHandler` does
**dirty-tracking via `compute_changes()`** and pushes only changed screen lines to each
client; clients pull scrollback line ranges on demand (`GetLines`-style PDUs). Known
weakness worth avoiding in gmux: unbounded PDU allocation caused OOM/stack-overflow issues
(https://github.com/wezterm/wezterm/issues/7527) — enforce max frame sizes.

**Key design point for gmux**: the *server* owns the VT parser and scrollback; clients hold
only a render cache. That is what makes reattach cheap and multi-client consistent.

### Windows specifics

- Unix domains "are supported on all systems, even Windows" (AF_UNIX, available on Win10
  1803+; model knowledge: via `afunix.sys`). wezterm ships `wezterm-mux-server.exe` in the
  Windows distribution. Socket-path discovery on Windows also uses a shared-memory NameHolder
  in the per-desktop namespace (per DeepWiki code index — treat as plausible, verify in source).
- Documented WSL use: host config `unix_domains = { { name='wsl', serve_command={'wsl',
  'wezterm-mux-server','--daemonize'} } }`; **WSL2 does not support AF_UNIX interop** (WSL1
  only) — a real limitation wezterm documents (https://wezterm.org/multiplexing.html).
- Rough edges on Windows (GitHub issues): WSL serve_command failing with error 10061
  (https://github.com/wezterm/wezterm/issues/3860, /issues/2919), spawning into a domain
  creating spurious tabs (https://github.com/wezterm/wezterm/issues/4408), reattach after all
  tabs closed requires window restart (https://github.com/wezterm/wezterm/issues/2614).
- ssh domains require a compatible wezterm version installed on the remote host — this is the
  pain gmux avoids by speaking tmux control mode to remotes instead.
- Latency mitigation for remote domains: predictive local echo, `local_echo_threshold_ms`.

### What state lives where (wezterm answer)

| State | Location |
|---|---|
| PTY handles, child processes | mux (server) process |
| VT parser + grid + scrollback | mux (server) process |
| Window/tab/pane tree, titles, ids | mux (server) process |
| Render cache of visible lines | client (GUI) |
| Fonts, GPU atlases, input mapping, config | GUI only |
| Clipboard, mouse selection | GUI, with PDUs to fetch text |

---

## (b) tmux reference semantics worth cloning

Primary sources:
- man page: https://man.openbsd.org/tmux.1 (fetched this session)
- control mode: https://github.com/tmux/tmux/wiki/Control-Mode (fetched this session)
- iTerm2 integration: https://iterm2.com/documentation-tmux-integration.html

### Naming & addressing (clone exactly)

- Every session/window/pane gets a **server-lifetime-unique, never-reused id**: sessions
  `$0`, windows `@1`, panes `%2`. The pane id is exported to the child as `TMUX_PANE` —
  gmux should export `GMUX_PANE` (and `GMUX_SESSION`, `GMUX_WINDOW`) so agent hook scripts
  can self-address.
- Target syntax for `-t`: `session:window.pane`. Session resolution order: `$id` → exact
  name → unique prefix → glob; `=name` forces exact match. Window tokens: `{start}`/`^`,
  `{end}`/`$`, `{last}`/`!`, `{next}`/`+`, `{previous}`/`-`. Pane tokens: `{last}`/`!`,
  `{top}`, `{bottom}`, `{left}`, `{right}`, `{top-left}` … `{up-of}`, `{down-of}`,
  `{active}`, `{marked}`/`~` (one marked pane server-wide; default source for join/swap/move).
- `list-panes -F '#{pane_id} #{pane_current_path} #{pane_pid}' -f '<filter>'` — format
  variables + filters are what make tmux scriptable; gmux should ship a `#{}`-style format
  mini-language early (agents and scripts depend on it).

### send-keys (clone exactly)

`send-keys [-FHKlMRX] [-N repeat-count] [-t target-pane] key ...`
- Each arg is looked up as a key name (`C-a`, `Enter`, `NPage`); unknown names are sent as
  literal UTF-8.
- `-l` = literal (no key-name lookup) — **critical for agent scripting** (paste prompts
  verbatim), `-H` = hex bytes, `-N` = repeat count, `-R` = reset terminal state, `-F` =
  expand formats.
- gmux CLI mirror: `gmux send-keys -t mysession:1.%5 -l "explain this test failure" Enter`.

### capture-pane (clone exactly)

`capture-pane [-aepPqCJN] [-b buffer-name] [-S start] [-E end] [-t target-pane]`
- `-p` → stdout (agents will use this constantly), else into a named paste buffer (`-b`).
- `-S/-E` line range: `0` = top of visible screen, **negative = into scrollback**, `-S -`
  = start of history, `-E -` = end of visible screen.
- `-e` include SGR escape sequences (colors); `-J` join wrapped lines + preserve trailing
  spaces; `-C` octal-escape unprintables; `-N` preserve trailing spaces.
- gmux mirror: `gmux capture-pane -t %5 -p -S -2000 -e`.

### Control mode (`tmux -C` / `-CC`) — gmux's v1 remote-tmux protocol

This is exactly how iTerm2 does native-UI-over-remote-tmux, and it is the recommended gmux
v1 remote story: run `ssh host tmux -CC new -A -s work` inside a hidden pane, parse the
control stream, and materialize native gmux tabs/panes.

Protocol facts (from the tmux wiki, verified):
- `-C` keeps canonical mode (for testing); `-CC` disables it and emits DCS `\033P1000p` at
  start (client detection) and `%exit` + ST (`\033\\`) at exit. An **empty line detaches**.
- Commands are plain text lines (`new -n mywindow`, `send-keys -t %1 ls Enter`); every
  command's reply is wrapped in guards:
  `%begin <unix-timestamp> <command-number> <flags>` … output … `%end <same args>` (or
  `%error`). Flags currently always 1. Replies are strictly ordered — a simple request queue
  correlates them.
- Pane output arrives as `%output %<pane-id> <data>` where all bytes < 0x20 and `\` are
  octal-escaped (`\` → `\134`, CR → `\015`). UTF-8 arrives as raw high bytes.
- Notifications to mirror UI state: `%window-add @n`, `%window-close @n`, `%window-renamed`,
  `%session-changed $n name`, `%sessions-changed`, `%pane-mode-changed %n`,
  `%layout-change @n <layout>`, `%exit`.
- Flow control (important when 10 agent panes stream at once): `refresh-client -f
  pause-after=30` enables pausing; server sends `%pause %n`, client resumes with
  `refresh-client -A '%0:continue'`; with flow control on, output arrives as
  `%extended-output %pane <age-ms> : data`.
- Format subscriptions: `refresh-client -B name:type:format` pushes `%subscription-changed`
  at most once/second — use for cwd/title/status mirroring instead of polling.

**Design bonus**: if gmux's own daemon protocol keeps a control-mode-compatible *surface*
(same notification names/semantics over the pipe, even if framed as JSON), the remote-tmux
client and the local-daemon client can share one state-mirroring engine.

---

## (c) Detach/reattach on Windows — process model

### The ConPTY lifetime constraint (load-bearing fact)

- ConPTY: `CreatePseudoConsole(COORD, hInput, hOutput, dwFlags, *phPC)` creates a pseudo
  console hosted by a conhost/OpenConsole instance; you attach children via
  `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` in `STARTUPINFOEX`.
- Per Microsoft Learn: **closing the pseudoconsole (`ClosePseudoConsole`) terminates all
  attached client applications** — a CTRL_CLOSE_EVENT is sent to each connected client, and
  for shell-type children the whole attached tree goes down.
  (https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session,
  https://learn.microsoft.com/en-us/windows/console/closepseudoconsole)
- If the owning process *crashes*, the conhost sees its pipes/handles die and the effect is
  the same (model knowledge; consistent with docs). Also note: on Win11 24H2+
  `ClosePseudoConsole` returns immediately, on older builds it can block until the hosting
  window/console exits — drain the output pipe before/while closing to avoid deadlocks.
- There is **no supported API to re-parent, re-open, or serialize a live ConPTY** from
  another process. Handle duplication of the hPC across processes is not a supported detach
  mechanism.

**Conclusion**: whichever process calls `CreatePseudoConsole` is the process that must stay
alive for the shells to survive. Detach therefore *requires* PTY ownership outside the GUI.

### The three options

1. **Single GUI process owns all ConPTYs.** Detach = hide window. Shells die on GUI exit,
   crash, or updater restart. This is Windows Terminal's model — and precisely why WT has no
   detach: microsoft/terminal#8244 ("Detaching panes and tabs") and discussion #17348
   ("Re-attach to orphaned process?") remain open/unanswered; the stock answer in the
   community is "use tmux" (https://github.com/microsoft/terminal/issues/8244,
   https://github.com/microsoft/terminal/discussions/17348). WT 1.18+ can *move tabs between
   its own windows* only because all WT windows live in one process (model knowledge).
   Rejected for gmux — detach is a hard requirement.

2. **Background mux daemon owns ConPTYs + terminal state; thin GUI over local IPC.**
   True detach; GUI crash/update never kills agents. Complexity that must be accepted:
   - the daemon runs the VT parser and owns scrollback (see (a) — do NOT make the GUI the
     parser, or reattach/multi-client breaks);
   - a damage/refresh protocol (send changed lines + cursor + palette; client pulls
     scrollback ranges on demand);
   - input path adds one IPC hop (~tens of µs on a loopback named pipe — negligible vs
     16.6 ms frame budget; model knowledge).
   Shipped precedents proving this works on Windows:
   - **wezterm-mux-server.exe** (see (a));
   - **psmux** (https://github.com/psmux/psmux, MIT, Rust) — native Windows tmux clone,
     server process owns ConPTYs, client discovery via `.port`/`.key` files under `~/.psmux`
     (localhost TCP + key auth rather than named pipe), sessions survive terminal crashes,
     detach/reattach works, ~83 tmux commands, reads `.tmux.conf`;
   - **VS Code's pty host** — a separate ptyHost process owns ConPTYs so terminal sessions
     survive window reloads and reconnect ("Detach and attach terminal sessions",
     https://github.com/microsoft/vscode/issues/127195) (model knowledge + issue).

3. **Hybrid (wezterm-style)**: GUI embeds a mux for throwaway local panes, daemon for
   persistent ones. Two code paths, two behaviors to explain, and "oops, that pane wasn't
   persistent." Not worth it when the daemon hop is this cheap locally.

**Recommendation: option 2, daemon-owned everything.** Single binary, three roles:
`gmux.exe` (GUI, auto-starts daemon if absent), `gmux.exe --daemon` (headless mux),
`gmux.exe <subcommand>` (CLI client). Auto-start the daemon per-user at login via a Run key
or Task Scheduler task — **not a Windows Service**: services live in session 0, would spawn
shells with the wrong token/environment/session, and complicate the pipe ACL story (model
knowledge, standard Windows guidance).

### Job objects for child-tree management

Verified at https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects:
- Nested job objects exist since Windows 8/Server 2012 (a process can be in multiple jobs).
- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (set via `SetInformationJobObject` +
  `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`): closing the **last handle** to the job kills all
  processes in the job and its child jobs.
- Design: **one job object per pane**, kill-on-close set, spawn the shell suspended →
  `AssignProcessToJobObject` → resume. Kill-pane = close the job handle (kills the whole
  tree, including node/python children agents spawn). Daemon death (even crash) closes all
  job handles → no orphaned agent trees.
- Caveat (https://github.com/dotnet/runtime/issues/107992): children that entered *their
  own* kill-on-close job with breakaway semantics can escape tree-kill; acceptable edge.
- Deliberate orphaning for "keep running after daemon stops" (if ever wanted): don't use
  kill-on-close; but for gmux the daemon *is* the persistence layer, so kill-on-close is
  right.
- ConPTY interaction note: the conhost/OpenConsole host process is spawned by the ConPTY
  API; rely on `ClosePseudoConsole` for it, and on the job for the client tree. Watch for
  lingering conhost processes (historical bug: microsoft/terminal#4050).

---

## (d) Session restore across reboot

Processes are gone after reboot; restore = re-create the *workspace*, not the processes.

### What Windows Terminal ships (the model to clone)

Verified via https://github.com/microsoft/terminal/issues/17274, issue #961, and
https://4sysops.com/archives/new-in-windows-terminal-restore-buffers-code-snippets-scratchpad-and-regex/:
- Layout: window/tab/pane tree persisted in `state.json` in the package LocalState dir.
- Buffers: per-pane snapshot files `buffer_<guid>.txt`, stored as **VT-encoded text**
  (human-readable, UTF-16 files) next to settings — i.e., WT serializes the buffer back into
  the escape-sequence stream that would reproduce it (SGR runs, line contents), then
  **replays it through the normal VT parser** on restore.
- Restored content is inert history with a `[Restored <DateTime>]` divider line; a fresh
  shell is spawned underneath. Users complained restored text got pushed into scrollback
  rather than staying on-screen (#17274) — a UX detail to get right.
- Reliability notes from the team: saving happens at close and "when your computer reboots
  to update" (https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/).

### What tmux-resurrect persists (the other reference)

(model knowledge; https://github.com/tmux-plugins/tmux-resurrect) — session name, window
index/name/flags, `#{window_layout}` string, per-pane cwd (`#{pane_current_path}`), the
running foreground command (restored only if on an allowlist), optional pane contents via
`capture-pane`; tmux-continuum adds periodic autosave (default 15 min).

### gmux persistence design

Checkpoint (debounced, e.g. 2 s after any topology/cwd change, plus a periodic scrollback
snapshot, plus on `WM_ENDSESSION`/`CTRL_SHUTDOWN_EVENT` in the daemon):

1. **Layout tree** — JSON: sessions → windows → split tree (orientation, ratios, zoom) →
   panes (stable ids, titles).
2. **Per-pane cwd** — see below.
3. **Per-pane spawn info** — original command line, env *at spawn time* (do not attempt to
   read live env later), profile/shell identity.
4. **Scrollback** — VT-encoded snapshot per pane (WT-style), zstd-compressed
   (`scrollback\%5.vt.zst`).
5. **Attention state** — pending notification badges (nice-to-have).

Restore UX: rebuild the tree; for each pane spawn the shell with `lpCurrentDirectory` =
saved cwd and saved env additions; replay the decompressed VT snapshot into the fresh grid
*before* connecting the ConPTY output; print divider `─── [gmux: restored 2026-07-04 09:12,
process not running] ───`; optionally offer per-pane "rerun last command" (never auto-rerun
agents — an agent re-launched into a repo can start editing).

### Tracking cwd of arbitrary shells on Windows

1. **Shell integration (primary, reliable)** — have gmux's VT parser record:
   - **OSC 9;9** (ConEmu convention, Windows-first): `ESC ] 9 ; 9 ; "<C:\path>" BEL/ST` —
     what Windows Terminal uses for duplicate-tab-same-cwd; implemented in WT via
     https://github.com/microsoft/terminal/pull/8330; shell snippets documented at
     https://learn.microsoft.com/en-us/windows/terminal/tutorials/new-tab-same-directory
     (PowerShell prompt: `"$([char]27)]9;9;`"$($ExecutionContext.SessionState.Path.CurrentLocation)`"$([char]27)\"`).
     Path must be a Windows path (WSL needs `wslpath`).
   - **OSC 7**: `ESC ] 7 ; file://<hostname>/<percent-encoded-path> ST` — the cross-platform
     convention (macOS Terminal, wezterm, kitty); accept both.
   - **OSC 1337 `CurrentDir=`** (iTerm2/wezterm convention) — cheap to accept too.
   Ship first-run profile snippets (PowerShell `$PROMPT` hook, cmd via `PROMPT $e]9;9;$P$e\`,
   bash/zsh precmd) exactly like WT's tutorial does, and inject automatically for shells
   gmux spawns (e.g. PowerShell via a startup module when the user opts in).
2. **PEB fallback (works with zero shell config)** — `NtQueryInformationProcess(
   ProcessBasicInformation)` → PEB address → `ReadProcessMemory` → `RTL_USER_PROCESS_PARAMETERS
   .CurrentDirectory.DosPath` (a `UNICODE_STRING`). Undocumented-but-stable technique
   (https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntqueryinformationprocess
   explicitly warns these internals may change). Caveats (model knowledge): needs
   PROCESS_VM_READ (fine — daemon is the parent); cross-architecture reads (x64 daemon →
   ARM64EC/x86 child) need the right PEB variant (`NtWow64QueryInformationProcess64` /
   `PROCESS_BASIC_INFORMATION` bitness care) — gmux ships native ARM64 builds, reducing this;
   fails for elevated children; reads the *shell's* cwd, not a nested program's. Use only at
   checkpoint time when no OSC has been seen; there is no reliable event for cwd change.
3. Do **not** rely on `GetFinalPathNameByHandle` of the process's cd handle or WMI — slower
   and no better (model knowledge).

---

## (e) IPC: the `\\.\pipe\gmux` protocol

### Transport

- **Named pipe**, message-oriented framing on top of byte mode:
  `CreateNamedPipe(L"\\\\.\\pipe\\gmux.<user-sid-hash>", PIPE_ACCESS_DUPLEX |
  FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_TYPE_BYTE | PIPE_READMODE_BYTE |
  PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, ...)`.
  - Suffix the pipe name with the user SID (or a hash) so multiple interactive users never
    collide, while keeping plain `\\.\pipe\gmux` as a doc-level alias resolved by the CLI.
  - `FILE_FLAG_FIRST_PIPE_INSTANCE` defeats pipe-squatting (another process pre-creating the
    name); `PIPE_REJECT_REMOTE_CLIENTS` kills SMB-borne remote access at the transport level.
- **Security descriptor**: never NULL — the default named-pipe DACL grants Everyone/ANONYMOUS
  read (https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights).
  Build an explicit DACL: current user's token SID `GENERIC_ALL`, `SYSTEM` `GENERIC_ALL`,
  nothing else — SDDL shape `D:P(A;;GA;;;SY)(A;;GA;;;<user-SID>)`. Client side: open with
  `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (block full impersonation by a rogue
  server) and verify the server via `GetNamedPipeServerProcessId` + `QueryFullProcessImageNameW`
  matching the gmux install path (model knowledge, standard hardening).
- AF_UNIX sockets are the alternative (wezterm uses them on Windows) but named pipes give
  DACLs, `ImpersonateNamedPipeClient`-free peer PID lookup, and no filesystem socket cleanup;
  stay with named pipes.

### Framing & protocol

- **JSON-RPC 2.0 with LSP-style framing** (`Content-Length: N\r\n\r\n{...}`) for the control
  plane. Rationale: trivially implementable from PowerShell/Python/Node agent scripts (the
  audience!), self-describing, order-independent via `id`. wezterm's leb128+bincode is faster
  but locks the ecosystem into one client library — wrong trade-off for a tool whose selling
  point is scriptability. Enforce a max frame size (learn from wezterm issue #7527).
- **Data plane**: pane output subscriptions are the hot path. Two tiers:
  - default: `pane/output` JSON notifications with base64 chunks (fine for CLI `pipe-pane`
    style consumers);
  - GUI: after `attach`, the connection is upgraded to a binary side-channel (length-prefixed
    frames: paneId u32, seq u64, bytes) or simply a second dedicated pipe instance per
    client — avoids base64+JSON overhead for 10 panes × MB/s agent streams.
- **Versioning**: `hello` request first: `{clientVersion, protocolVersion, capabilities[]}` →
  server replies with its own + negotiated capability set; server refuses only on major
  mismatch with a human-readable upgrade message. Additive-only JSON fields thereafter.
- **Method surface mirrors the CLI 1:1** (tmux discipline: the CLI *is* the protocol):
  `list-sessions`, `new-session`, `new-window`, `split-pane`, `send-keys`, `capture-pane`,
  `kill-pane`, `respawn-pane`, `attach`, `detach`, `subscribe` (events: `pane-output`,
  `pane-attention` (OSC 9/777/99), `layout-changed`, `pane-exited`, `cwd-changed`),
  `screenshot` (server renders grid → PNG, or client-side), `wait-for` (block until pane
  quiet/bell — very useful for agent orchestration scripts).
- Every `gmux.exe` CLI invocation opens its own pipe instance (`PIPE_UNLIMITED_INSTANCES`);
  the daemon services each with overlapped I/O — no sharing/locking issues between the GUI,
  CLI calls, and agent hook scripts firing concurrently.
- Precedent for this exact shape: Docker Engine on Windows serves its HTTP API on
  `\\.\pipe\docker_engine`; VS Code and OpenSSH's `\\.\pipe\openssh-ssh-agent` (model
  knowledge).

---

## (f) Scrollback storage

### In-memory (daemon)

- **Per-pane ring buffer of lines**, not a flat cell grid. Store each line as UTF-8 text +
  run-length-encoded attribute spans (SGR runs) + flags (wrapped-continuation, marked). A
  naive `Cell { char, fg, bg, attrs }` array costs 8–16 B/cell → 120 cols × 10k lines ≈
  10–19 MB/pane — unacceptable at 10+ panes. Run-length lines average ~100–200 B for typical
  agent output → **10k lines ≈ 1–2 MB/pane; 12 panes ≈ 12–24 MB** (model estimate; wezterm
  and alacritty both use clustered/compressed line storage for the same reason).
- Budget: default `scrollback_lines = 10000` per pane; hard memory guard per pane (e.g.
  32 MB) and process-wide high-water (e.g. 512 MB) that triggers eviction of *cold segments*:
  chunks of 1–4k lines older than the live window get zstd-compressed in memory (agent logs
  compress 5–10×), decompressed transparently on scroll — this is how you honestly support
  "agents that ran all night."
- Alternate screen (TUIs like Claude Code's UI) has **no scrollback** by definition — only
  the primary screen buffer accumulates; agents in full-screen TUIs cost only grid + history
  of the primary screen beneath.

### Serialization for reboot restore

- **VT-encoded text, WT-style** (`buffer_<guid>.txt` precedent, see (d)): walk the ring
  buffer emitting text + minimal SGR transitions + CRLF/wrap markers; write UTF-8 (WT's
  UTF-16 choice is not worth copying), then zstd. Restore = decompress → feed through the
  normal VT parser into a fresh grid before attaching the new ConPTY.
  - Pros: one parser, no versioned binary schema, human-debuggable, naturally forward-
    compatible.
  - Cons: loses non-visual metadata (hyperlink ids survive if you emit OSC 8; semantic
    prompt marks survive if you emit FinalTerm OSC 133 back out — do both).
- Snapshot cadence: on graceful shutdown + every N minutes for crash-safety (write-to-temp +
  atomic rename). Cap snapshot size (e.g. last 5k lines/pane) — restore is context, not an
  archive.

---

## Prior-art scorecard

| App | PTY owner | Detach | Reboot restore | Notes |
|---|---|---|---|---|
| Windows Terminal | GUI process | No (#8244 open) | Yes — layout + VT buffer files | restore tech worth cloning; no daemon |
| WezTerm (local domain) | GUI process | No | No | |
| WezTerm (unix domain) | wezterm-mux-server.exe | Yes | No (panes die with daemon/reboot) | architecture worth cloning |
| psmux | server process (Rust) | Yes | ? (sessions survive *crashes*) | localhost TCP + key file, MIT |
| Tabby | Electron GUI | No | Layout only; cwd restore buggy (#4259, #9468) | cautionary tale |
| VS Code terminal | ptyHost process | Window-reload reattach | Layout + partial buffer replay | precedent for daemon model |
| iTerm2 + tmux -CC | remote tmux server | Yes (tmux) | tmux-resurrect | gmux's remote v1 blueprint |
| cmux (macOS) | GUI (libghostty) | No daemon found | — | gmux differentiator: real detach |

---

## Sources

- https://wezterm.org/multiplexing.html
- https://deepwiki.com/wezterm/wezterm/2.2-multiplexer-architecture
- https://github.com/wezterm/wezterm/issues/7527 , /issues/3860 , /issues/2919 , /issues/4408 , /issues/2614
- https://man.openbsd.org/tmux.1
- https://github.com/tmux/tmux/wiki/Control-Mode
- https://iterm2.com/documentation-tmux-integration.html
- https://learn.microsoft.com/en-us/windows/console/creating-a-pseudoconsole-session
- https://learn.microsoft.com/en-us/windows/console/closepseudoconsole
- https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
- https://github.com/dotnet/runtime/issues/107992
- https://github.com/microsoft/terminal/issues/961 , /issues/17274 , /issues/8244 , /discussions/17348 , /pull/8330 , /issues/4050
- https://devblogs.microsoft.com/commandline/windows-terminal-preview-1-22-release/
- https://4sysops.com/archives/new-in-windows-terminal-restore-buffers-code-snippets-scratchpad-and-regex/
- https://learn.microsoft.com/en-us/windows/terminal/tutorials/new-tab-same-directory
- https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
- https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntqueryinformationprocess
- https://github.com/psmux/psmux
- https://github.com/Eugeny/tabby/issues/4259 , /issues/9468
- https://github.com/microsoft/vscode/issues/127195
- https://github.com/manaflow-ai/cmux , https://cmux.com/
- (model knowledge, flagged inline): AF_UNIX on Win10 1803+, VS Code ptyHost details, session-0 isolation guidance, tmux-resurrect internals, memory estimates.
