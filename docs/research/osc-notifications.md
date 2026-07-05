# OSC Notification Wire Protocols — Research for gmux

> Research date: 2026-07-04. Sources verified live where noted. This document specifies every
> escape sequence gmux's VT parser must recognize to implement its killer feature: agent
> notification hooks (toast + pane attention indicators), plus related sequences worth parsing
> in the same pass (title, semantic prompts, cwd, hyperlinks, clipboard, progress).

Notation used throughout:

- `ESC` = `0x1B`
- `OSC` = `ESC ]` (`0x1B 0x5D`). The 8-bit C1 form `0x9D` also exists; modern UTF-8 terminals
  generally do **not** honor 8-bit C1 OSC introducers (they conflict with UTF-8 continuation
  bytes), but a strict parser should decide explicitly. Recommended: accept only 7-bit `ESC ]`.
- `ST` = String Terminator: `ESC \` (`0x1B 0x5C`) or the 8-bit C1 `0x9C` (same caveat).
- `BEL` = `0x07`. Virtually every OSC in this document may be terminated by **either** BEL or ST;
  gmux must accept both everywhere.

---

## 1. OSC 9 — iTerm2-style "post a notification" + the ConEmu sub-protocol collision

### 1.1 Plain notification form (iTerm2 heritage, ~2010, originally "Growl")

```
ESC ] 9 ; <message> BEL
ESC ] 9 ; <message> ESC \
```

- Single free-text parameter, no title/body split. The whole remainder after `9;` — including any
  further semicolons that don't match a ConEmu subcommand (see 1.3) — is the message.
- iTerm2 documents it as `OSC 9 ; [Message content goes here] ST`
  (https://iterm2.com/documentation-escape-codes.html).
- WezTerm: `printf "\e]9;%s\e\\" "hello there"` shows a toast
  (https://wezterm.org/escape-sequences.html).
- Ghostty documents it as `ESC ] 9 ; t ESC \` — "Show a desktop notification with title t"
  (https://ghostty.org/docs/vt/osc/9).

### 1.2 The ConEmu OSC 9;N namespace (historical collision)

ConEmu independently claimed OSC 9 as a private namespace of numeric subcommands
(https://conemu.github.io/en/AnsiEscapeCodes.html). Full list (all accept BEL or ST):

| Sequence | Meaning |
|---|---|
| `OSC 9 ; 1 ; ms ST` | Sleep `ms` milliseconds |
| `OSC 9 ; 2 ; "txt" ST` | Show GUI MessageBox with text |
| `OSC 9 ; 3 ; "txt" ST` | Set ConEmu tab title (empty restores) |
| `OSC 9 ; 4 ; st ; pr ST` | **Taskbar/tab progress** (see 1.4) |
| `OSC 9 ; 5 ST` | Wait for keypress (Enter/Space/Esc) |
| `OSC 9 ; 6 ; "macro" ST` | Execute GuiMacro |
| `OSC 9 ; 7 ; "cmd" ST` | Run external process |
| `OSC 9 ; 8 ; "env" ST` | Print environment variable value |
| `OSC 9 ; 9 ; "cwd" ST` | **Report current working directory** (also adopted by Windows Terminal, see §7.4) |
| `OSC 9 ; 10 [; n] ST` | Toggle xterm emulation modes |
| `OSC 9 ; 11 ; "txt" ST` | Comment (ignored) |
| `OSC 9 ; 12 ST` | Mark prompt start (mintty-compatible; predecessor of OSC 133) |

### 1.3 Disambiguation: how shipped terminals tell "notification" from "subcommand"

There is no formal rule; the de-facto heuristic is a **numeric-prefix test**:

- **Ghostty** (documented): "To avoid conflicting with the ConEmu extensions, which also use
  OSC 9 due to historical happenstance, the title should not begin with a number and then a
  semicolon (`;`). Ghostty's parser will silently convert any invalid ConEmu sequence to a
  OSC 9 sequence, though this behavior should not be relied upon."
  (https://ghostty.org/docs/vt/osc/9)
- **kitty** (changelog): kitty "discards OSC 9 notifications that start with `4;`" because
  systemd ≥ v257 emits `OSC 9;4` progress on the wire (kitty issue #8011,
  https://github.com/kovidgoyal/kitty/issues/8011; changelog
  https://sw.kovidgoyal.net/kitty/changelog/). A later kitty release then implemented `9;4` as a
  real progress bar drawn at the top of the window, gated by a `progress_bar` kitty.conf option.
- **rockorager (Ghostty maintainer) write-up**: "The sequence `OSC 9;4` followed by anything
  other than a semicolon and valid state is interpreted as a desktop notification by
  ConEmu-compatible terminals, not a progress bar" — i.e. strict parse of the subcommand, and
  fall back to notification on parse failure (https://rockorager.dev/misc/osc-9-4-progress-bars/).

**Recommended gmux algorithm** (matches Ghostty/kitty observable behavior):

```
payload = bytes after "OSC 9 ;"
if payload matches ^(\d+)(;|$):
    n = leading integer
    if n is an implemented subcommand (4 → progress, 9 → cwd, 12 → prompt mark, ...):
        strict-parse it; on parse failure treat entire payload as notification text
    else:
        # unknown numeric subcommand: safest is to swallow (ConEmu namespace),
        # ghostty instead converts to notification — pick one and document it.
        swallow (recommended), or surface as notification behind a config flag
else:
    desktop notification, message = payload
```

Practical note: the numeric-prefix ambiguity is real but rare in practice — agents emit prose
messages ("Codex: turn complete") that never start with `<digit>;`.

### 1.4 OSC 9;4 progress (must-implement for gmux: taskbar progress on Windows)

```
OSC 9 ; 4 ; <state> [; <progress>] ST|BEL
```

| st | Meaning (ConEmu) | Windows Terminal rendering |
|---|---|---|
| 0 | remove progress | hidden |
| 1 | set value, `pr` = 0–100 | normal (green) taskbar progress |
| 2 | error state (`pr` optional) | red |
| 3 | indeterminate | pulsing/indeterminate |
| 4 | paused (`pr` optional) | yellow ("warning") |

- ConEmu spec: https://conemu.github.io/en/AnsiEscapeCodes.html
- Windows Terminal ≥ 1.6 renders it on the taskbar via `ITaskbarList3::SetProgressState/Value`
  semantics (https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences).
- Adopted by: ConEmu, Windows Terminal 1.6+, Ghostty 1.2+, Konsole, mintty 3.4.2+, WezTerm
  (Feb 2025), kitty (opt-in `progress_bar`), xterm.js addon
  (https://rockorager.dev/misc/osc-9-4-progress-bars/).
- **systemd v257+ emits this** during long operations, so any Windows box SSH'd into modern Linux
  will see `9;4` traffic — gmux must not toast it.
- Values outside 0–100 are clamped by implementations.

gmux mapping: per-pane progress state → Windows taskbar progress (aggregate across panes:
show max-severity/frontmost) + a per-pane progress chip in the pane header. Claude Code can emit
this via its `terminalSequence` hook field (allowlisted, see §5.1).

---

## 2. OSC 777 — rxvt-unicode / VTE `notify`

```
OSC 777 ; notify ; <title> ; <body> BEL
OSC 777 ; notify ; <title> ; <body> ESC \
```

- Origin: a small urxvt Perl extension; popularized by Fedora's downstream VTE patch for
  GNOME Terminal. **No formal spec exists** — the Fedora patch is the de-facto definition
  (https://blog.vucica.net/2017/07/what-are-osc-terminal-control-sequences-escape-codes.html).
- `notify` is the only sub-verb that ever shipped anywhere relevant. WezTerm: "Only the notify
  extension works": `printf "\e]777;notify;%s;%s\e\\" "title" "body"`
  (https://wezterm.org/escape-sequences.html).
- Semicolon ambiguity: title cannot contain `;`. Implementations split on the first two `;` after
  `notify` and treat **everything remaining as body** (body may contain semicolons). gmux should
  do the same: `777;notify;<title>;<rest-including-semicolons>`.
- If only `777;notify;<text>` arrives (no second separator), treat `<text>` as title with empty
  body (this is what foot/WezTerm effectively do).
- Supported by: urxvt (ext), WezTerm, Ghostty, foot, Warp, patched VTE. **Windows Terminal
  explicitly rejected it** (see §6.6).

---

## 3. OSC 99 — kitty desktop notifications protocol (the rich one)

Spec: https://sw.kovidgoyal.net/kitty/desktop-notifications/ (verified live this session).
This is the only *designed* protocol: IDs, updates, buttons, icons, close events, capability
query, and two-way reporting.

### 3.1 Frame grammar

```
OSC 99 ; <metadata> ; <payload> ST|BEL
```

- The **two semicolons are mandatory** even with empty metadata: `ESC ] 99 ; ; Hello world ESC \`.
- `<metadata>` = zero or more `key=value` pairs **separated by `:` (colon)**.
  Keys are single chars `[a-zA-Z]`; values are words from
  `` a-zA-Z0-9-_/+.,(){}[]*&^%$#@!`~ ``.
- `<payload>` interpretation depends on `p=` in metadata.
- Payload size limits: **max 2048 bytes unencoded / 4096 bytes encoded per escape code**;
  larger content must be chunked (§3.3).
- Payload must be "escape-code-safe UTF-8" (no C0/C1 controls, no CR/LF/TAB) unless `e=1`
  (base64, RFC 4648; parser must tolerate missing final padding).

### 3.2 Metadata keys (complete)

| Key | Values | Default | Meaning |
|---|---|---|---|
| `i` | identifier `[a-zA-Z0-9_\-+.]+` | unset | Notification ID — chains chunks, enables updates/close/reports. **Sanitize before echoing back** (injection risk) |
| `d` | `0`/`1` | `1` | "done": `0` = more chunks follow, `1` = display now |
| `p` | `title`, `body`, `close`, `icon`, `buttons`, `alive`, `?` | `title` | Payload type of this escape code |
| `e` | `0`/`1` | `0` | `1` = payload is base64 |
| `a` | `report`, `focus`, comma-sep, `-` prefix negates | `focus` | Click actions: focus originating window and/or report click to the app |
| `c` | `0`/`1` | `0` | `1` = send close event when the notification closes |
| `o` | `always`, `unfocused`, `invisible` | `always` | When to honor: only-when-unfocused / only-when-window-invisible |
| `u` | `0` low, `1` normal, `2` critical | `1` | Urgency |
| `w` | int ≥ −1 (ms) | `-1` | Auto-close timeout (−1 = daemon default) |
| `f` | base64 UTF-8 | unset | Application name (or .desktop filename / macOS bundle id) |
| `t` | base64 UTF-8, repeatable | unset | Notification "type" (category) |
| `n` | base64 icon name, repeatable | unset | Named icon (`error`, `warn(ing)`, `info`, `question`, `help`, `file-manager`, `system-monitor`, `text-editor`, app names) |
| `g` | identifier | unset | Icon-cache key for `p=icon` data reuse |
| `s` | base64 sound name | `system` | `system`, `silent`, `error`, `warn(ing)`, `info`, `question` |

### 3.3 Multi-part payloads

```
ESC ] 99 ; i=1:d=0 ; Hello world ESC \
ESC ] 99 ; i=1:p=body ; This is cool ESC \
```

Chunks with the same `i` accumulate (title chunks concatenate; body chunks concatenate);
the frame with `d=1` (or with `d` omitted) commits and displays. **If chunking is used without
`i=`, behavior is undefined — kitty requires `i` for multi-part.** A lone
`ESC ] 99 ; ; text ESC \` is the minimal single-shot form (text = title).

### 3.4 Buttons, click reports, close events (two-way traffic gmux would have to WRITE to the pty)

- Buttons: `p=buttons`, payload = button labels separated by **U+2028 LINE SEPARATOR**
  (`0xE2 0x80 0xA8`).
- With `a=report`, click on notification body → terminal writes to the pty:
  `ESC ] 99 ; i=<id> ; ESC \` — button `n` click → `ESC ] 99 ; i=<id> ; <n> ESC \` (1-based).
- With `c=1`, on close → `ESC ] 99 ; i=<id>:p=close ; ESC \`; if the platform can't track closes
  (macOS), reply payload is `untracked`.
- `p=alive` poll → terminal replies with the comma-separated list of still-alive IDs.

### 3.5 Capability query (`p=?`) — implement this or clients misbehave

Client sends:

```
ESC ] 99 ; i=<queryid>:p=? ; ESC \
```

Terminal MUST reply (on the pty) with the keys/values it supports, e.g.:

```
ESC ] 99 ; i=<queryid>:p=? ; a=report,focus:c=1:o=always,unfocused:p=title,body:u=0,1,2:w=1 ESC \
```

Omit unsupported keys entirely. Clients (e.g. kitty's `kitten notify`) use this to detect
support; not replying = "unsupported", replying with a subset = graceful degradation.
(kitty issue #7658 tracks client-side query use.)

### 3.6 Minimum viable OSC 99 subset for gmux v1

**Must support** (needed by Claude Code's kitty channel and cmux-parity workflows):

1. Frame parse: metadata (`:`-separated k=v) + payload split; ignore unknown keys (spec-mandated).
2. `i=`, `d=` chunk reassembly with per-id buffers (cap total at ~1 MiB, expire stale partials).
3. `p=title` and `p=body`; `e=1` base64 decode; UTF-8 validation.
4. `u=` urgency → toast priority; `o=` honor-when (respect `unfocused` — agents use it).
5. `a=focus` (click toast → focus pane/window — gmux's headline UX) and the `p=?` query reply.

**May ignore in v1** (reply to `p=?` without these keys): `p=icon`/`g`/`n` (icons), `s` (sounds),
`p=buttons` + `a=report` (button reporting), `c=1` close events, `p=alive`, `w=` expiry, `f`/`t`.

---

## 4. Terminator variants — parser summary

| Form | Bytes | Accept? |
|---|---|---|
| BEL | `0x07` | Yes — used by most agent emitters (`\x1b]9;...\x07`) |
| ST (7-bit) | `0x1B 0x5C` | Yes |
| ST (C1) | `0x9C` | Only if you process C1 in non-UTF-8 mode; recommended: no in UTF-8 |
| CAN/SUB abort | `0x18`/`0x1A` | Abort OSC accumulation, discard |
| ESC + other | | xterm aborts the OSC; safest to discard buffer and reprocess ESC |

Also cap OSC accumulation length (xterm-style; e.g. 64 KiB) to bound memory against hostile
output, but note OSC 99 legitimately sends 4 KiB frames repeatedly.

---

## 5. What the agent CLIs actually emit (as of 2026-07)

### 5.1 Claude Code (Anthropic)

Verified against https://code.claude.com/docs/en/terminal-config,
https://code.claude.com/docs/en/settings, https://code.claude.com/docs/en/hooks,
https://code.claude.com/docs/en/hooks-guide (all fetched this session).

- **Event model**: fires a "notification event" when it finishes a task or pauses for permission.
  Notification types (usable as hook matchers): `permission_prompt`, `idle_prompt`,
  `auth_success`, `elicitation_dialog`, `elicitation_complete`, `elicitation_response`,
  `agent_needs_input`, `agent_completed` (last two: v2.1.198+).
- **`preferredNotifChannel`** (settings, shown as "Notifications" in `/config`):
  `auto` (default) | `iterm2` | `iterm2_with_bell` | `terminal_bell` | `kitty` | `ghostty` |
  `notifications_disabled`.
  - `auto`: "sends a desktop notification in iTerm2, Ghostty, and Kitty and **does nothing in
    other terminals**" — i.e. terminal detection via environment (TERM_PROGRAM etc.).
    **In an unrecognized terminal like gmux, the default emits NOTHING.** (Architecturally
    load-bearing — see Recommendation.)
  - `terminal_bell`: writes BEL.
- **Channel → wire sequence** (docs don't print the bytes; mapping assembled from docs naming +
  community/ghostty reports — mark: high-confidence but not source-verified since cli.js is
  obfuscated):
  - `iterm2` → `ESC ] 9 ; <message> BEL`
  - `ghostty` → `ESC ] 777 ; notify ; <title> ; <body> BEL` (ghostty discussion #10215 confirms
    Claude Code notifications arrive via OSC 777 in Ghostty)
  - `kitty` → OSC 99 frame(s)
- **`terminalSequence` hook output field (v2.1.141+)** — the strongest official statement of the
  byte streams involved. Any hook can return
  `{"terminalSequence": "]777;notify;Title;Body"}` and Claude Code writes it through
  its own terminal write path (race-free, works in tmux, works on Windows). The **allowlist**:
  - OSC `0`/`1`/`2` (titles)
  - OSC `9` — "iTerm2, ConEmu, Windows Terminal, and WezTerm notifications, **including `9;4`
    taskbar progress**"
  - OSC `99` (kitty), OSC `777` (urxvt/Ghostty/Warp)
  - bare BEL; BEL or ST terminators
  - Rejected: CSI, palette OSCs, OSC 8, OSC 52, OSC 1337.
  So a gmux user's Claude Code hook can drive **all** of gmux's notification + progress surface
  with zero extra software.
- **Notification hook stdin JSON**:

```json
{
  "session_id": "abc123",
  "transcript_path": "~/.claude/projects/.../transcript.jsonl",
  "cwd": "/home/user/my-project",
  "hook_event_name": "Notification",
  "message": "Permission required",
  "notification_type": "permission_prompt"
}
```

- tmux swallows these unless `allow-passthrough on` — the docs call out that both notifications
  and "the progress bar" need passthrough, confirming Claude Code emits OSC 9;4 progress in
  supported terminals (settings page excerpt didn't name the setting; minor uncertainty).
- Windows hook example in official docs uses a PowerShell MessageBox (no native toast helper).

### 5.2 OpenAI Codex CLI

Verified against https://developers.openai.com/codex/config-advanced (fetched this session).

- **`tui.notification_method`** = `auto` (default) | `osc9` | `bel`.
  - `auto`: "Codex prefers OSC 9 notifications (a terminal escape sequence some terminals
    interpret as a desktop notification) and falls back to BEL (`\x07`) otherwise."
  - Wire format: `ESC ] 9 ; <message> BEL` (`\x1b]9;...\x07`).
- **`tui.notification_condition`** = `unfocused` | `always` — Codex itself decides whether the
  terminal is focused (it tracks focus via terminal focus-reporting; gmux should support
  DECSET 1004 focus events so this works correctly inside gmux).
- **`tui.notifications`** — enable/disable or filter to event types, e.g.
  `["agent-turn-complete", "approval-requested"]`.
- **`notify = ["program", ...]`** — spawns external program with one JSON argument:
  `{"type":"agent-turn-complete","thread-id":...,"turn-id":...,"cwd":...,"input-messages":...,"last-assistant-message":...}`.
- Security note: Codex had an ANSI-injection RCE writeup (dganev.com, 2026-02) — gmux should
  treat notification text as untrusted (see §8).

### 5.3 Gemini CLI (Google)

Verified against https://geminicli.com/docs/cli/notifications/ (fetched this session).

- Experimental feature; `settings.json`: `{"general": {"enableNotifications": true}}` (or
  `/settings` → General).
- "The CLI uses the **OSC 9** terminal escape sequence to trigger system notifications."
  Supported-terminal list in docs: iTerm2, WezTerm, Ghostty, Kitty. Otherwise "falls back to a
  terminal bell (BEL)".
- Events: **Action Required** (waiting for input/tool approval) and **Session Complete**.
- History: bell-on-idle PRs #10029, #21618 (cross-platform BEL fallback) —
  https://github.com/google-gemini/gemini-cli/pull/21618.

### 5.4 Aider

Verified against https://aider.chat/docs/usage/notifications.html (fetched this session).

- **Emits no OSC sequences and no BEL.** `--notifications` triggers OS-level notifiers directly:
  macOS `terminal-notifier`/AppleScript, Linux `notify-send`/`zenity`, Windows **PowerShell
  MessageBox**. `--notifications-command "<cmd>"` overrides; config `notifications: true` /
  `AIDER_NOTIFICATIONS=true`.
- Consequence for gmux: aider will never light up the pane via the VT stream. Parity plan:
  ship a documented `--notifications-command "gmux notify --pane %GMUX_PANE% ..."` recipe using
  gmux's CLI/named-pipe API (mirrors cmux's `cmux notify`).

### 5.5 Ecosystem direction

Other agent CLIs are converging on OSC 9 + 777 (e.g. kimi-cli issue #1342 requesting exactly
"OSC 9/777 terminal notifications for task completion",
https://github.com/MoonshotAI/kimi-cli/issues/1342; opencode proposal issue #4454). OSC 9 with
BEL fallback is the lingua franca; OSC 99 is the power protocol; OSC 777 persists because
Ghostty/foot/WezTerm support it.

---

## 6. How shipped terminals implement notification UX (design reference for gmux)

### 6.1 kitty

- OSC 99 reference implementation (full spec §3). Legacy OSC 9 accepted as body-only
  notification; **discards OSC 9 payloads starting `4;`** (systemd progress), later versions
  render `9;4` as an in-window progress bar behind the `progress_bar` option
  (https://sw.kovidgoyal.net/kitty/changelog/).
- Suppression is **app-controlled** via `o=` (`always` default / `unfocused` / `invisible`);
  user-side filtering via `filter_notification` kitty.conf rules (match by app/type, actions
  ignore/focus etc.).
- Click behavior: `a=focus` default focuses the originating OS window *and* kitty tab/window;
  `a=report` writes the click back to the app. Sounds/icons supported.

### 6.2 Ghostty

- Supports **OSC 9 and OSC 777 only** (no OSC 99 as of mid-2026 — open discussions #4405/#10998,
  https://github.com/ghostty-org/ghostty/discussions/10998).
- Config: `desktop-notifications = true` (default on).
- Documented ConEmu disambiguation heuristic (§1.3). OSC 9;4 progress supported since 1.2.0.
- Click-to-focus of the originating window is still an open request (issue #9145) — i.e. even
  good terminals lag here; gmux focusing the exact *pane* on toast click is a differentiator.

### 6.3 WezTerm

- OSC 9 + OSC 777 `notify` + OSC 1337 `SetUserVar` (https://wezterm.org/escape-sequences.html).
- **`notification_handling`** config (since 20240127-113634-bbcac864): `AlwaysShow` (default!),
  `NeverShow`, `SuppressFromFocusedPane`, `SuppressFromFocusedTab`, `SuppressFromFocusedWindow`
  (https://wezterm.org/config/lua/config/notification_handling.html). Default-always is widely
  considered annoying — gmux should default to suppress-when-pane-visible-and-window-focused.
- OSC 9;4 progress added Feb 2025.

### 6.4 iTerm2 (the OSC 9 origin)

- OSC 9 posts a Notification Center alert, but **only after the user enables** Settings →
  Profiles → Terminal → "Notification Center Alerts" + Filter Alerts → "Send escape
  sequence-generated alerts" (per Claude Code terminal-config docs). Alerts are generally
  suppressed while the session is focused.
- Extras: `OSC 1337 ; RequestAttention=yes|once|no|fireworks ST` (dock bounce),
  `OSC 1337 ; SetUserVar=name=<base64> ST` (https://iterm2.com/documentation-escape-codes.html).

### 6.5 foot (Wayland; best-in-class semantics to copy)

Verified via foot.ini(5) (https://man.archlinux.org/man/foot.ini.5.en):

- Triggers: **OSC 777 and OSC 99** (no plain OSC 9).
- `[desktop-notifications]` section: `command` template (`${title}`, `${body}`, `${app-id}`,
  `${window-title}`, `${urgency}` low/normal/critical, `${category}`, `${icon}`,
  `${expire-time}`, `${replace-id}`, `${muted}`, `${sound-name}`, `${action-argument}`),
  `close` command with `${id}`, `command-action-argument` for OSC 99 buttons.
- **`inhibit-when-focused = yes` by default** — notifications suppressed while the window has
  keyboard focus. This is the right default; gmux should adopt the pane-level analogue.
- Click activation via XDG activation tokens → focuses window; action names reported back to the
  client per OSC 99.

### 6.6 Windows Terminal — the gap gmux fills

- **No OSC notification support at all.** PR #14425 (OSC 777 → toast, only when tab/window
  inactive, click summons window) was **closed unmerged on 2025-04-25** by DHowett: doesn't work
  for unpackaged builds, doesn't work elevated, foreground-activation limits, Windows 10 issues,
  and toast-spam concerns; "we can come back to this if we ever want it"
  (https://github.com/microsoft/terminal/pull/14425, issue #7718).
- What WT does support: OSC 9;4 taskbar progress (1.6+), OSC 9;9 cwd, OSC 133 marks (stable in
  1.21), BEL with `bellStyle` (audible / taskbar flash / both).
- Lesson list for gmux's toast layer (from DHowett's objections): toasts need app identity
  (AUMID; packaged or registered via shortcut/registry), elevated-process and foreground-
  activation rules must be handled, and per-pane rate-limiting is required to prevent spam.

### 6.7 cmux (the product gmux mirrors) — parity checklist

From https://cmux.com/docs/notifications (fetched this session):

- Parses **OSC 777 and OSC 99** ("Use OSC 777 for simple notifications. Use OSC 99 when you need
  subtitles or notification IDs"). Plain OSC 9 is not mentioned — **gmux should exceed cmux by
  also parsing OSC 9**, since Codex/Gemini emit it.
- Suppresses desktop alerts when: cmux window focused, the sending workspace is active, or the
  notification panel is open. Notifications persist in a panel; unread badges on workspace tabs;
  `⌘⇧I` opens panel, `⌘⇧U` jumps to most-recent-unread workspace; `paneFlash` effect.
- `cmux notify --title ... --body ...` CLI; hooks receive every notification policy as JSON on
  stdin and can suppress/route; env vars `CMUX_NOTIFICATION_TITLE/SUBTITLE/BODY`.
- Ships Claude Code hook integration (Stop / PostToolUse) and Copilot CLI config integration.

---

## 7. Related sequences gmux should parse in the same OSC dispatcher

### 7.1 BEL as attention (`0x07`)

The universal fallback: Claude Code `terminal_bell`, Codex `bel`/auto-fallback, Gemini fallback
all emit bare BEL. gmux should treat BEL in an unfocused/invisible pane as an attention event
(pane badge + optional toast, debounced — TUIs can ring repeatedly). Windows Terminal precedent:
`bellStyle` taskbar flash.

### 7.2 OSC 0 / 1 / 2 — icon/title

```
OSC 0 ; <text> ST|BEL   (icon name + window title)
OSC 1 ; <text> ST|BEL   (icon name)
OSC 2 ; <text> ST|BEL   (window title)
```

Agents actively set titles (Claude Code updates the tab title with status; allowlisted in
`terminalSequence`). gmux: per-pane title → pane header/tab text; title changes in unfocused
panes are a cheap secondary attention signal.

### 7.3 OSC 133 — semantic prompt marks (FinalTerm/FTCS): detect "waiting at prompt"

Verified via Windows Terminal docs
(https://learn.microsoft.com/en-us/windows/terminal/tutorials/shell-integration):

```
OSC 133 ; A ST                 FTCS_PROMPT           start of prompt
OSC 133 ; B ST                 FTCS_COMMAND_START    end of prompt / start of commandline
OSC 133 ; C ST                 FTCS_COMMAND_EXECUTED start of command output
OSC 133 ; D [; <ExitCode>] ST  FTCS_COMMAND_FINISHED end of command; 0 = success
```

The fuller Per-Bothner/FinalTerm proposal adds params (`aid=`, `cl=`, `k=` on `A`/`D`) —
accept-and-ignore params after the letter (split on `;`). State machine per pane:
`D→A` = **shell idle at prompt** (agent process exited / REPL waiting) → "pane idle" indicator;
`C` without `D` = command running; `D;non-zero` = failed command → error-tinted mark.
Emitted by: WT shell-integration profiles, kitty/ghostty/wezterm shell integration, and gmux can
inject it into its own default PowerShell profile (WT's documented PowerShell/CMD snippets reuse
directly; CMD via `PROMPT $e]133;D$e\$e]133;A$e\$e]9;9;$P$e\$P$G$e]133;B$e\`).

### 7.4 OSC 7 (and Windows-flavored OSC 9;9) — cwd reporting

```
OSC 7 ; file://<hostname>/<percent-encoded-path> ST|BEL     (xterm/iTerm2/WezTerm lineage)
OSC 9 ; 9 ; "<windows-path>" ST|BEL                          (ConEmu / Windows Terminal)
```

- WezTerm: `printf "\033]7;file://HOSTNAME/CURRENT/DIR\033\\"`; PowerShell example uses
  `file://${env:COMPUTERNAME}/<path>` (https://wezterm.org/shell-integration.html).
- Windows Terminal consumes **OSC 9;9** (quoted Windows path; quotes optional) for
  duplicate-tab-in-same-cwd; known quirks with empty/invalid paths (WT issues #12378, #8930 —
  validate before use).
- gmux uses cwd for: new-split-inherits-cwd, pane header path display, `create-workspace` API
  defaults. Accept both forms; percent-decode OSC 7 and map `file://host/C:/...` → `C:\...`.

### 7.5 OSC 8 — hyperlinks

```
OSC 8 ; [params] ; <URI> ST|BEL   ...link text...   OSC 8 ; ; ST|BEL
```

`params` = `:`-separated `key=value`; only `id=` is defined (groups multi-cell/multi-line links
for unified hover). De-facto spec: egmontkob gist ("Hyperlinks in terminal emulators").
Recommended limits: URI ≤ 2083 bytes, scheme allowlist (http/https/file/mailto), never auto-open.
Agents print OSC 8 links in output (Claude Code status line docs mention OSC 8 rendering).

### 7.6 OSC 52 — clipboard

```
OSC 52 ; <clipboard> ; <base64-data> ST|BEL    set clipboard
OSC 52 ; <clipboard> ; ?           ST|BEL      query (reply in same format)
```

`<clipboard>` = any of `c` (clipboard), `p` (primary), `s`, `q`, cut-buffers `0`–`7`; empty
defaults to `s0`/`c`. Invalid base64 clears. Policy: allow write (size-capped ~1 MiB, notify
user), **deny read/query by default** (silent-exfiltration risk); WezTerm likewise ignores
queries (https://wezterm.org/escape-sequences.html).

### 7.7 iTerm2 OSC 1337 (optional, low priority)

`RequestAttention=yes|once|no|fireworks` → could map to taskbar-flash; `SetUserVar` → ignore.
Claude Code's terminalSequence allowlist rejects 1337, and no surveyed agent emits it for
notifications.

---

## 8. Cross-cutting parser & security requirements

1. **Single OSC dispatcher**: accumulate OSC bytes → split selector at first `;` → dispatch on
   {0,1,2,7,8,9,52,99,133,777,1337}. OSC 9 gets the ConEmu subcommand test (§1.3).
2. **Both terminators everywhere** (BEL and `ESC \`). Handle CAN/SUB/ESC-abort. Cap buffer size.
3. **Untrusted text**: notification titles/bodies come from the wire (agent output can embed
   attacker-controlled repo text — the Codex ANSI-injection RCE is the cautionary tale). Strip
   all C0/C1 controls from strings before rendering; XML-escape before Windows toast XML;
   sanitize OSC 99 identifiers `[a-zA-Z0-9_\-+.]` before any echo-back (spec-mandated).
4. **Rate limiting**: per-pane token bucket for toasts (e.g. ≤1/sec, burst 3, collapse repeats
   into a counter badge) — DHowett's toast-spam objection is real.
5. **Suppression policy** (synthesis of foot/cmux/WezTerm): show OS toast only if
   (gmux window unfocused) OR (pane not visible in active workspace) OR (notification panel not
   open); always record to in-app notification panel + pane badge regardless; honor OSC 99 `o=`.
6. **Click-to-focus**: toast activation → focus gmux window → activate workspace → focus pane.
   On Windows this needs an AUMID (registered via Start-menu shortcut or MSIX identity) and
   a COM activator (`INotificationActivationCallback`) or protocol activation for click-through;
   plain `Shell_NotifyIcon` balloons are the degraded fallback.
7. **Focus reporting (DECSET 1004)**: implement `CSI ? 1004 h/l` + `ESC [I`/`ESC [O` focus
   events — Codex's `notification_condition=unfocused` and other TUIs rely on it to self-suppress.
8. **tmux passthrough irrelevant on native Windows**, but if gmux panes SSH into tmux sessions,
   users must set `allow-passthrough on` remotely; document it (Claude Code docs do).

---

## 9. Recommendation for gmux

**Parse, in priority order: OSC 9 (with ConEmu 9;N dispatch incl. 9;4 progress + 9;9 cwd),
OSC 777 notify, OSC 99 (subset of §3.6 + `p=?` reply), bare BEL, OSC 0/2, OSC 133, OSC 7,
OSC 8, OSC 52 (write-only).** That covers 100% of surveyed agent emissions and exceeds both
cmux (no OSC 9) and every Windows terminal (WT has none).

Because Claude Code's `auto` channel does nothing in unknown terminals, gmux must ship an
onboarding step: (a) detect Claude Code/Codex/Gemini/aider in panes, (b) offer one-click config
injection — Claude Code: `preferredNotifChannel` (or a `Notification` hook returning
`terminalSequence` with OSC 777/9), Codex: `tui.notification_method = "osc9"`, Gemini:
`general.enableNotifications = true`, aider: `--notifications-command "gmux notify ..."` — and
(c) also treat BEL + OSC 133 idle-at-prompt as attention signals so unconfigured agents still
light up panes. Do NOT spoof `TERM_PROGRAM` of other terminals (kitty/ghostty detection often
implies other capabilities gmux may not have); instead set `TERM_PROGRAM=gmux` and pursue
first-class detection upstream in the agent CLIs.

---

## Appendix A — Quick sequence cheat sheet

```
Notification (simple):    \x1b]9;Task complete\x07
Notification (title+body):\x1b]777;notify;Claude Code;Needs your approval\x07
Notification (rich):      \x1b]99;i=1:d=0:u=2:o=unfocused;Claude Code\x1b\\
                          \x1b]99;i=1:p=body;Waiting for permission\x1b\\
OSC99 capability query:   \x1b]99;i=q1:p=?;\x1b\\        (terminal must reply)
Progress 45%:             \x1b]9;4;1;45\x07
Progress done:            \x1b]9;4;0\x07
Progress error:           \x1b]9;4;2\x07
Cwd (xterm style):        \x1b]7;file://HOST/C:/repo\x1b\\
Cwd (Windows style):      \x1b]9;9;"C:\repo"\x07
Title:                    \x1b]2;gmux — claude\x07
Prompt marks:             \x1b]133;A\x07 ... \x1b]133;B\x07 ... \x1b]133;C\x07 ... \x1b]133;D;0\x07
Hyperlink:                \x1b]8;id=x;https://example.com\x1b\\text\x1b]8;;\x1b\\
Clipboard set:            \x1b]52;c;aGVsbG8=\x07
Bell:                     \x07
```

## Appendix B — Source URLs

- kitty OSC 99 spec: https://sw.kovidgoyal.net/kitty/desktop-notifications/
- kitty changelog (OSC 9 `4;` discard, progress_bar): https://sw.kovidgoyal.net/kitty/changelog/
- kitty 9;4-conflict issue: https://github.com/kovidgoyal/kitty/issues/8011
- ConEmu OSC 9;N spec: https://conemu.github.io/en/AnsiEscapeCodes.html
- OSC 9;4 survey: https://rockorager.dev/misc/osc-9-4-progress-bars/
- Windows Terminal progress: https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences
- Windows Terminal shell integration (OSC 133/9;9): https://learn.microsoft.com/en-us/windows/terminal/tutorials/shell-integration
- Windows Terminal OSC 777 PR (closed): https://github.com/microsoft/terminal/pull/14425 (issue #7718)
- Ghostty OSC 9: https://ghostty.org/docs/vt/osc/9 ; OSC 99 discussions: https://github.com/ghostty-org/ghostty/discussions/10998
- WezTerm escape sequences: https://wezterm.org/escape-sequences.html ; notification_handling: https://wezterm.org/config/lua/config/notification_handling.html ; shell integration: https://wezterm.org/shell-integration.html
- iTerm2 escape codes: https://iterm2.com/documentation-escape-codes.html
- foot.ini(5): https://man.archlinux.org/man/foot.ini.5.en
- OSC 777 background: https://blog.vucica.net/2017/07/what-are-osc-terminal-control-sequences-escape-codes.html
- Claude Code: https://code.claude.com/docs/en/terminal-config , /en/settings , /en/hooks , /en/hooks-guide
- Codex CLI config: https://developers.openai.com/codex/config-advanced
- Gemini CLI notifications: https://geminicli.com/docs/cli/notifications/
- Aider notifications: https://aider.chat/docs/usage/notifications.html
- cmux notifications: https://cmux.com/docs/notifications
