//! Remote tmux attachments (M9 stage 2c): a [`RemoteAttachment`] owns one control-mode
//! transport ([`gmux_remote::RemoteTmux`]) and mirrors the remote session into the daemon's
//! [`Session`] — one local [`Window`] per remote window, one remote-backed [`Pane`] per remote
//! pane. The daemon's tick loop calls [`RemoteAttachment::pump`], which:
//!
//! - forwards queued keystrokes to the remote (`send-keys`),
//! - creates the initial windows from the attach-time enumeration reply,
//! - mirrors `%output` into panes (through the same OSC parser as local PTY bytes, so remote
//!   agents' OSC 777 notifications raise attention exactly like local ones),
//! - rebuilds a window's split tree on `%layout-change` (the remote layout is authoritative),
//! - and tears everything down on `%exit`/EOF.
//!
//! Everything here is unit-testable without a console: remote panes have no ConPTY, and the
//! transport accepts any stub command line that speaks control mode on stdio.
//!
//! A pane *moved between remote windows* (`break-pane`/`join-pane`) is re-homed locally: the
//! destination window's `%layout-change` extracts the mirror from its old window and re-inserts
//! it, so no tree ever dangles.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};

use gmux_mux::layout::Node;
use gmux_mux::{Pane, PaneId, Session, SplitDir, Window, WindowId};
use gmux_remote::{layout_to_node, RemoteTmux, TransportEvent};
use gmux_tmux::{Cell, Event, Layout, Notification};

/// The attach-time enumeration: one `@window %pane cols rows` line per remote pane. Only the
/// initial window/pane inventory comes from this; authoritative geometry arrives in the
/// `%layout-change` notifications tmux sends right after attach.
const ENUMERATE: &str = "list-panes -a -F '#{window_id} #{pane_id} #{pane_width} #{pane_height}'";

/// Query the remote tmux server version at attach. Flow control (`refresh-client -A` on
/// `%pause`) needs tmux >= 3.2; older servers error on it, so a low/unknown version puts the
/// attachment in [`RemoteAttachment::degraded`] mode.
const VERSION: &str = "display-message -p '#{version}'";

/// tmux versions below this lack `refresh-client -A` flow control.
const MIN_VERSION: (u32, u32) = (3, 2);

/// One live remote tmux session mirrored into the local [`Session`].
pub struct RemoteAttachment {
    transport: RemoteTmux,
    /// Cloned into every remote pane's input closure. The closure cannot borrow the transport
    /// (sending needs `&mut`), so keystrokes are queued here and [`RemoteAttachment::pump`]
    /// forwards them as `send-keys`.
    input_tx: Sender<(u64, Vec<u8>)>,
    input_rx: Receiver<(u64, Vec<u8>)>,
    /// Remote `%pane` → the local pane mirroring it.
    panes: HashMap<u64, PaneId>,
    /// Remote `@window` → the local window mirroring it. [`WindowId`]s are never reused, so a
    /// stale entry (user closed the tab locally) can only miss on lookup, never alias.
    windows: HashMap<u64, WindowId>,
    /// Local sequence number of the enumeration command; `None` once answered. Correlation is
    /// positional (the transport excludes the attach greeting), so the reply whose ordinal
    /// matches this is the enumeration.
    enumeration: Option<u64>,
    /// Local sequence number of the version query (see [`VERSION`]); `None` once answered.
    version_cmd: Option<u64>,
    /// Reported remote tmux version (major, minor); `None` until answered or if unparseable.
    version: Option<(u32, u32)>,
    /// Below [`MIN_VERSION`] or unknown: skip `refresh-client -A` on `%pause` (pre-3.2 tmux
    /// errors on it). Assumed until the version reply proves otherwise.
    degraded: bool,
    /// `Ctrl(Reply)` events drained so far — the other half of positional correlation.
    replies_seen: u64,
    /// Set once `%exit`/EOF tore the attachment down; `pump` then always reports finished.
    finished: bool,
}

impl RemoteAttachment {
    /// Spawn `command_line` (production: `ssh -tt <target> -- tmux -CC new -As gmux`; tests: any
    /// stub replaying a control-mode stream) and queue the initial enumeration.
    pub fn attach(command_line: &str) -> io::Result<RemoteAttachment> {
        let mut transport = RemoteTmux::spawn(command_line)?;
        // Positional correlation tracks each seq independently, so either order works.
        let version_cmd = Some(transport.send_command(VERSION));
        let enumeration = Some(transport.send_command(ENUMERATE));
        let (input_tx, input_rx) = channel();
        Ok(RemoteAttachment {
            transport,
            input_tx,
            input_rx,
            panes: HashMap::new(),
            windows: HashMap::new(),
            enumeration,
            version_cmd,
            version: None,
            degraded: true,
            replies_seen: 0,
            finished: false,
        })
    }

    /// Tell the remote tmux the client's size so it lays windows out at gmux's real geometry.
    pub fn resize_client(&mut self, cols: u16, rows: u16) {
        self.transport.resize_client(cols, rows);
    }

    /// Gracefully end the session: close the transport's stdin (ssh's EOF goodbye). The remote
    /// side then ends control mode and a following [`RemoteAttachment::pump`] returns `false`.
    pub fn detach(&mut self) {
        self.transport.close_stdin();
    }

    /// Everything the transport child wrote to stderr (ssh auth/host-key diagnostics).
    pub fn stderr_output(&self) -> Vec<u8> {
        self.transport.stderr_output()
    }

    /// The reported remote tmux version `(major, minor)`, or `None` until the version reply
    /// arrives or if it was unparseable.
    pub fn version(&self) -> Option<(u32, u32)> {
        self.version
    }

    /// Whether flow control is disabled because the remote tmux is below [`MIN_VERSION`] or its
    /// version is unknown. `true` until the version reply proves it >= 3.2.
    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// The remote `%pane` id mirrored by local pane `id`, if this attachment owns it. The server
    /// uses this to route pane operations (split/close) to the remote instead of mutating the
    /// local mirror.
    pub fn remote_id_of(&self, id: PaneId) -> Option<u64> {
        self.panes.iter().find_map(|(remote, local)| (*local == id).then_some(*remote))
    }

    /// Ask the remote to split pane `%remote` (`-h` beside / `-v` below). The new pane arrives
    /// via the resulting `%layout-change`; nothing changes locally until then.
    pub fn split_remote(&mut self, remote: u64, horizontal: bool) {
        self.transport.split_pane(remote, horizontal);
    }

    /// Ask the remote to kill pane `%remote`. The mirror is pruned by the resulting
    /// `%layout-change` / `%window-close`.
    pub fn kill_remote(&mut self, remote: u64) {
        self.transport.kill_pane(remote);
    }

    /// The remote `@window` mirrored by local window `id`, if this attachment owns it.
    pub fn remote_window_for(&self, id: WindowId) -> Option<u64> {
        self.windows.iter().find_map(|(remote, local)| (*local == id).then_some(*remote))
    }

    /// Ask the remote to kill window `@remote`. The local mirror (window + attachment maps) is
    /// pruned by the resulting `%window-close` — closing locally first would leave a stale map
    /// entry that resurrects the tab on the next `%layout-change`.
    pub fn kill_remote_window(&mut self, remote: u64) {
        self.transport.send_command(&format!("kill-window -t @{remote}"));
    }

    /// Forward queued input and apply all pending transport events to `session`. Returns `false`
    /// once the attachment is finished (`%exit` or EOF) — its panes are then marked exited (the
    /// daemon's normal exited-pane sweep removes them) and the caller should drop it.
    pub fn pump(&mut self, session: &mut Session) -> bool {
        if self.finished {
            return false;
        }
        while let Ok((pane, bytes)) = self.input_rx.try_recv() {
            self.transport.send_keys(pane, &bytes);
        }
        for event in self.transport.drain_events() {
            match event {
                // The attach greeting (the reply to the command line's own `new -As`) carries
                // nothing the mirror needs.
                TransportEvent::Greeting(_) => {}
                TransportEvent::Eof => self.finished = true,
                TransportEvent::Ctrl(Event::Reply { body, error, .. }) => {
                    let seq = self.replies_seen;
                    self.replies_seen += 1;
                    // Positional: the Nth reply answers the Nth send_command. Only the version
                    // and enumeration replies need handling; send-keys / refresh-client replies
                    // are noise.
                    if self.version_cmd == Some(seq) {
                        self.version_cmd = None;
                        self.apply_version(&body, error);
                    } else if self.enumeration == Some(seq) {
                        self.enumeration = None;
                        if !error {
                            self.enumerate(&body, session);
                        }
                    }
                }
                TransportEvent::Ctrl(Event::Notification(n)) => match n {
                    Notification::Output { pane, data } => {
                        if let Some(p) = self.panes.get(&pane).and_then(|id| session.pane(*id)) {
                            p.push_output(&data);
                        }
                    }
                    Notification::LayoutChange { window, layout } => {
                        self.apply_layout(window, &layout, session);
                    }
                    Notification::WindowClose { window } => self.close_window(window, session),
                    // A new window's %layout-change follows immediately and creates it.
                    Notification::WindowAdd { .. } => {}
                    // gmux windows have no free-form name (the sidebar derives one from the
                    // active pane's cwd), so a remote rename has nothing to set.
                    Notification::WindowRenamed { .. } => {}
                    // Flow control (tmux >= 3.2): a paused pane may continue immediately —
                    // `refresh-client -A %pane:continue` per tmux(1). Pausing is the protocol's
                    // backpressure valve, and the daemon drains every tick, so instant continue
                    // is safe; deferred-resume policy can layer on later without protocol change.
                    Notification::Pause { pane } => {
                        // Pre-3.2 tmux errors on `refresh-client -A`; degraded mode leaves the
                        // pane paused rather than sending a command the server rejects.
                        if !self.degraded {
                            self.transport
                                .send_command(&format!("refresh-client -A %{pane}:continue"));
                        }
                    }
                    Notification::Exit { .. } => self.finished = true,
                    _ => {}
                },
            }
        }
        if self.finished {
            for id in self.panes.values() {
                if let Some(p) = session.pane(*id) {
                    p.mark_exited();
                }
            }
            self.panes.clear();
            self.windows.clear();
            return false;
        }
        true
    }

    /// Handle the `#{version}` reply: parse it, and clear [`degraded`](Self::degraded) only if
    /// the version is known and >= [`MIN_VERSION`]. An error reply or unparseable body leaves
    /// the attachment degraded and warns once.
    fn apply_version(&mut self, body: &[Vec<u8>], error: bool) {
        let raw = (!error)
            .then(|| body.first())
            .flatten()
            .map(|line| String::from_utf8_lossy(line).into_owned());
        self.version = raw.as_deref().and_then(parse_version);
        self.degraded = self.version.is_none_or(|v| v < MIN_VERSION);
        // NOTE: a %pause arriving BEFORE this reply is dropped (degraded starts true — fail-safe
        // for old servers). Dormant today: the attach command never enables pause-after, so tmux
        // emits no %pause. If flow control is ever enabled at attach, track pauses seen while
        // degraded and re-answer them here when the version clears.
        if self.degraded {
            eprintln!(
                "gmux: remote tmux version {} lacks flow control (need {}.{}); \
                 running degraded (no %pause auto-continue)",
                raw.as_deref().unwrap_or("<unknown>"),
                MIN_VERSION.0,
                MIN_VERSION.1,
            );
        }
    }

    /// Build one window per remote window from the enumeration reply, with a minimal even-split
    /// placeholder layout (see [`ENUMERATE`]). Unparseable lines are skipped — the reply is
    /// remote input, and a partial mirror beats none.
    fn enumerate(&mut self, body: &[Vec<u8>], session: &mut Session) {
        let mut order: Vec<u64> = Vec::new();
        let mut panes_of: HashMap<u64, Vec<(u64, u16, u16)>> = HashMap::new();
        for line in body {
            let Some((window, pane, cols, rows)) = parse_enum_line(line) else { continue };
            if !panes_of.contains_key(&window) {
                order.push(window);
            }
            panes_of.entry(window).or_default().push((pane, cols, rows));
        }
        for window in order {
            if self.windows.contains_key(&window) {
                continue; // a %layout-change beat the reply and already created it
            }
            let mut panes = HashMap::new();
            let mut ids = Vec::new();
            for (remote, cols, rows) in panes_of.remove(&window).unwrap_or_default() {
                let pane = make_pane(&self.input_tx, remote, cols, rows);
                self.panes.insert(remote, pane.id);
                ids.push(pane.id);
                panes.insert(pane.id, pane);
            }
            let Some(root) = even_split(&ids) else { continue };
            let active = root.first_leaf();
            let win = Window::from_parts(panes, root, active);
            self.windows.insert(window, win.id);
            session.push_window(win);
        }
    }

    /// Mirror a `%layout-change`: convert the tmux layout to a split tree (creating panes for
    /// leaves not seen before, sized from the layout cells; **re-homing** panes the remote moved
    /// in from another window via `join-pane`/`break-pane`), then replace the window's tree —
    /// pruned panes are marked exited and forgotten. Unknown windows (a fresh `%window-add`, or
    /// one the user closed locally) are (re)created outright.
    fn apply_layout(&mut self, window: u64, layout: &Layout, session: &mut Session) {
        let mut sizes = HashMap::new();
        leaf_sizes(&layout.root, &mut sizes);
        // Re-home: a remote pane referenced by this layout but currently mirrored in a DIFFERENT
        // window was moved here remotely. Extract it from its old window (collapsing that tree,
        // keeping its active pane valid) and re-insert it below — leaving it referenced by two
        // trees at once would dangle the old window's active pane and panic the layout path.
        let target = self.windows.get(&window).copied();
        let mut rehomed: Vec<Pane> = Vec::new();
        for remote in sizes.keys() {
            if let Some(&id) = self.panes.get(remote) {
                let in_target = target
                    .and_then(|wid| session.window_mut(wid))
                    .is_some_and(|w| w.pane(id).is_some());
                if !in_target {
                    if let Some(pane) = session.extract_pane(id) {
                        rehomed.push(pane);
                    }
                }
            }
        }
        let mut created: Vec<Pane> = Vec::new();
        let (root, _leaf_order) = {
            let panes = &mut self.panes;
            let input_tx = &self.input_tx;
            let rehomed = &rehomed;
            layout_to_node(layout, &mut |remote| {
                if let Some(&id) = panes.get(&remote) {
                    if session.pane(id).is_some() || rehomed.iter().any(|p| p.id == id) {
                        return id;
                    }
                    panes.remove(&remote); // stale: its window was closed locally
                }
                let (cols, rows) = sizes.get(&remote).copied().unwrap_or((80, 24));
                let pane = make_pane(input_tx, remote, cols, rows);
                let id = pane.id;
                panes.insert(remote, id);
                created.push(pane);
                id
            })
        };
        created.extend(rehomed);
        let known = self.windows.get(&window).copied();
        match known.and_then(|wid| session.window_mut(wid)) {
            Some(win) => {
                for pane in win.replace_tree(root, created) {
                    pane.mark_exited();
                    if let Some(remote) = pane.remote_id() {
                        if self.panes.get(&remote) == Some(&pane.id) {
                            self.panes.remove(&remote);
                        }
                    }
                }
            }
            None => {
                let active = root.first_leaf();
                let mut panes = HashMap::new();
                for pane in created {
                    panes.insert(pane.id, pane);
                }
                let win = Window::from_parts(panes, root, active);
                self.windows.insert(window, win.id);
                session.push_window(win);
            }
        }
    }

    /// Mirror a `%window-close`: drop the local window and mark its panes exited.
    fn close_window(&mut self, window: u64, session: &mut Session) {
        let Some(wid) = self.windows.remove(&window) else { return };
        let Some(win) = session.remove_window(wid) else { return };
        for pane in win.panes() {
            pane.mark_exited();
            if let Some(remote) = pane.remote_id() {
                if self.panes.get(&remote) == Some(&pane.id) {
                    self.panes.remove(&remote);
                }
            }
        }
    }
}

/// A local mirror of remote pane `%remote`, its input closure queuing keystrokes onto the
/// attachment's channel (drained by `pump` into `send-keys`).
fn make_pane(input_tx: &Sender<(u64, Vec<u8>)>, remote: u64, cols: u16, rows: u16) -> Pane {
    let tx = input_tx.clone();
    Pane::remote(
        remote,
        cols.max(1),
        rows.max(1),
        Box::new(move |bytes| {
            let _ = tx.send((remote, bytes.to_vec()));
        }),
    )
}

/// Parse one `@<window> %<pane> <cols> <rows>` enumeration line; `None` for anything else.
fn parse_enum_line(line: &[u8]) -> Option<(u64, u64, u16, u16)> {
    let text = std::str::from_utf8(line).ok()?;
    let mut fields = text.split_whitespace();
    let window = fields.next()?.strip_prefix('@')?.parse().ok()?;
    let pane = fields.next()?.strip_prefix('%')?.parse().ok()?;
    let cols = fields.next()?.parse().ok()?;
    let rows = fields.next()?.parse().ok()?;
    Some((window, pane, cols, rows))
}

/// Parse a tmux `#{version}` string into `(major, minor)`. tmux reports things like `3.4`,
/// `3.2a` (patch letter), `next-3.5`, or `openbsd-7.4` — strip any non-numeric prefix, then read
/// the leading integer of each of the first two dot-fields (trailing patch letters ignored).
/// `None` if there's no numeric major.minor.
fn parse_version(raw: &str) -> Option<(u32, u32)> {
    // Skip to the first digit (drops `next-`, `openbsd-`, etc.).
    let start = raw.find(|c: char| c.is_ascii_digit())?;
    let mut fields = raw[start..].split('.');
    let leading_num = |s: &str| -> Option<u32> {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    let major = leading_num(fields.next()?)?;
    let minor = fields.next().and_then(leading_num).unwrap_or(0);
    Some((major, minor))
}

/// A right-leaning chain of horizontal splits giving `ids` equal widths (the placeholder until
/// the window's real `%layout-change` arrives). `None` when `ids` is empty.
fn even_split(ids: &[PaneId]) -> Option<Node> {
    match ids {
        [] => None,
        [only] => Some(Node::Leaf(*only)),
        [first, rest @ ..] => Some(Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 1.0 / ids.len() as f32,
            a: Box::new(Node::Leaf(*first)),
            b: Box::new(even_split(rest).expect("rest is non-empty")),
        }),
    }
}

/// Each leaf's `(cols, rows)` in a tmux layout cell tree, keyed by remote pane id — the sizes
/// remote panes are created at when a `%layout-change` introduces them.
fn leaf_sizes(cell: &Cell, out: &mut HashMap<u64, (u16, u16)>) {
    match cell {
        Cell::Leaf { w, h, pane, .. } => {
            let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
            out.insert(*pane, (clamp(*w), clamp(*h)));
        }
        Cell::Split { children, .. } => {
            for child in children {
                leaf_sizes(child, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests. No tmux/ssh exists on dev machines; every child is a stub (`cmd /c type` replays a
// canned control-mode stream, `powershell` records stdin) — the same injectable-command contract
// production relies on. No console needed anywhere: remote panes have no ConPTY.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_mux::PaneEvent;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// A collision-free temp path (tests run in parallel in one process).
    fn temp_path(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("gmux-server-remote-{}-{n}-{tag}", std::process::id()))
    }

    /// Poll `pump` until it reports finished (the stub's stream always ends in EOF).
    fn pump_until_finished(att: &mut RemoteAttachment, session: &mut Session, cap: Duration) {
        let deadline = Instant::now() + cap;
        while att.pump(session) {
            assert!(
                Instant::now() < deadline,
                "attachment never finished; stderr {:?}",
                String::from_utf8_lossy(&att.stderr_output()),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poll until `path` exists and its content passes `done` (the recording stub writes it only
    /// on exit, after stdin closes).
    fn wait_for_file(path: &PathBuf, cap: Duration, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + cap;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if done(&text) {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "stub never wrote {path:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn empty_session() -> Session {
        Session::from_windows("remote-test", Vec::new(), 0)
    }

    fn find_pane<'s>(session: &'s Session, remote: u64) -> Option<&'s Pane> {
        session.windows().iter().flat_map(|w| w.panes()).find(|p| p.remote_id() == Some(remote))
    }

    fn snapshot_text(pane: &Pane) -> String {
        pane.snapshot()
            .cells
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The full attach-to-exit arc against one canned stream: greeting, enumeration (two windows),
    /// authoritative %layout-change, %output (with an OSC 777 inside — remote notifications must
    /// reach PaneEvent), %window-close, %exit.
    #[test]
    fn canned_stream_mirrors_windows_output_notification_close_and_exit() {
        let path = temp_path("full.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p"); // -CC DCS introducer
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // attach greeting
        canned.extend_from_slice(b"%begin 1000 6 1\n3.4\n%end 1000 6 1\n"); // version reply
        canned.extend_from_slice(b"%begin 1000 7 1\n"); // enumeration reply
        canned.extend_from_slice(b"@1 %0 80 24\n");
        canned.extend_from_slice(b"@1 %1 80 23\n");
        canned.extend_from_slice(b"@2 %5 80 24\n");
        canned.extend_from_slice(b"%end 1000 7 1\n");
        // Authoritative geometry for @1: two panes side by side.
        canned.extend_from_slice(b"%layout-change @1 bb62,159x48,0,0{79x48,0,0,0,79x48,80,0,1}\n");
        canned.extend_from_slice(b"%output %0 hello-from-remote\n");
        // OSC 777 from a remote agent, octal-escaped as tmux does (ESC = \033, BEL = \007).
        canned.extend_from_slice(b"%output %1 \\033]777;notify;agent;needs input\\007\n");
        canned.extend_from_slice(b"%window-close @2\n");
        canned.extend_from_slice(b"%exit\n");
        canned.extend_from_slice(b"\x1b\\");
        std::fs::write(&path, &canned).unwrap();

        let mut session = empty_session();
        let mut att =
            RemoteAttachment::attach(&format!("cmd.exe /c type {}", path.display())).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(20));

        // Window @2 was closed; @1 survives with both panes under the %layout-change tree.
        assert_eq!(session.window_count(), 1, "only @1 should remain");
        assert_eq!(session.pane_count(), 2);
        let win = &session.windows()[0];
        match win.root() {
            Node::Split { dir, ratio, .. } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert!((ratio - 79.5 / 159.0).abs() < 1e-6, "ratio {ratio}");
            }
            other => panic!("expected the %layout-change split, got {other:?}"),
        }
        assert!(find_pane(&session, 5).is_none(), "@2's pane must be gone");

        // %output reached the grid.
        let p0 = find_pane(&session, 0).expect("pane %0 mirrored");
        assert!(
            snapshot_text(p0).contains("hello-from-remote"),
            "grid: {:?}",
            snapshot_text(p0),
        );

        // The OSC 777 inside %output surfaced as a PaneEvent::Notification (and Exited followed).
        let p1 = find_pane(&session, 1).expect("pane %1 mirrored");
        let events = p1.drain_events();
        assert!(
            events.iter().any(|e| matches!(
                e,
                PaneEvent::Notification(n) if n.title == "agent" && n.body == "needs input"
            )),
            "events: {events:?}",
        );
        assert!(events.iter().any(|e| matches!(e, PaneEvent::Exited)), "events: {events:?}");

        // %exit marked every mirrored pane dead, and pump stays finished.
        for w in session.windows() {
            for p in w.panes() {
                assert!(!p.is_alive(), "pane {:?} must be dead after %exit", p.id);
            }
        }
        assert!(!att.pump(&mut session), "a finished attachment stays finished");
    }

    /// %layout-change drives the full window lifecycle: grow a known window (create pane), shrink
    /// it (prune pane), and create an unknown window announced only by %window-add.
    #[test]
    fn layout_change_creates_prunes_and_builds_unknown_windows() {
        let path = temp_path("layouts.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p");
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // greeting
        canned.extend_from_slice(b"%begin 1000 6 1\n3.4\n%end 1000 6 1\n"); // version reply
        canned.extend_from_slice(b"%begin 1000 7 1\n@1 %0 80 24\n%end 1000 7 1\n"); // enumeration
        // Split appears: %2 joins %0…
        canned.extend_from_slice(b"%layout-change @1 aaaa,159x48,0,0{79x48,0,0,0,79x48,80,0,2}\n");
        // …then %0 goes away entirely.
        canned.extend_from_slice(b"%layout-change @1 aaaa,159x48,0,0,2\n");
        // A brand-new window: %window-add carries no geometry, its %layout-change does.
        canned.extend_from_slice(b"%window-add @3\n");
        canned.extend_from_slice(b"%layout-change @3 aaaa,80x24,0,0,9\n");
        canned.extend_from_slice(b"%exit\n");
        canned.extend_from_slice(b"\x1b\\");
        std::fs::write(&path, &canned).unwrap();

        let mut session = empty_session();
        let mut att =
            RemoteAttachment::attach(&format!("cmd.exe /c type {}", path.display())).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(20));

        assert_eq!(session.window_count(), 2, "@1 (shrunk) and @3 (new)");
        assert_eq!(session.pane_count(), 2);
        assert!(find_pane(&session, 0).is_none(), "%0 was pruned by the second layout");
        let p2 = find_pane(&session, 2).expect("%2 created by the first layout");
        let snap = p2.snapshot();
        assert_eq!((snap.cols, snap.rows), (79, 48), "sized from the layout cell");
        assert!(find_pane(&session, 9).is_some(), "@3's pane created from its layout");
    }

    /// The command pipe end to end: attach sends the enumeration, pane keystrokes queued through
    /// the input channel leave as hex `send-keys` on the next pump. The stub records stdin.
    #[test]
    fn attach_enumerates_and_pump_forwards_input_as_send_keys() {
        let out = temp_path("input.txt");
        let cl = format!(
            "powershell -NoProfile -Command \"$input | Set-Content -Path {}\"",
            out.display(),
        );
        let mut att = RemoteAttachment::attach(&cl).unwrap();
        // A pane wired to this attachment's input channel (what enumerate/apply_layout build).
        let pane = make_pane(&att.input_tx, 5, 80, 24);
        pane.write(b"ls\r").unwrap();
        let mut session = empty_session();
        assert!(att.pump(&mut session), "stub is silent, so the attachment stays live");
        att.detach(); // EOF ends the stub's $input pipeline; it writes the file and exits

        let written = wait_for_file(&out, Duration::from_secs(60), |t| t.lines().count() >= 3);
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines, [VERSION, ENUMERATE, "send-keys -t %5 -H 6c 73 0d"]);
        let _ = std::fs::remove_file(&out);
    }

    /// %pause is answered with an immediate continue (`refresh-client -A %pane:continue`). The
    /// stub both replays a canned stream (type) and records stdin (powershell), so the sent
    /// command is observable.
    #[test]
    fn pause_is_answered_with_refresh_client_continue() {
        let canned_path = temp_path("pause.bin");
        let out = temp_path("pause-out.txt");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p");
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // greeting
        canned.extend_from_slice(b"%begin 1000 6 1\n3.4\n%end 1000 6 1\n"); // version reply
        canned.extend_from_slice(b"%begin 1000 7 1\n@1 %0 80 24\n%end 1000 7 1\n"); // enumeration
        canned.extend_from_slice(b"%pause %0\n");
        canned.extend_from_slice(b"%exit\n");
        std::fs::write(&canned_path, &canned).unwrap();

        // cmd runs `type` (dumps the canned stream), then powershell (records our stdin).
        let cl = format!(
            "cmd.exe /c type {} & powershell -NoProfile -Command \"$input | Set-Content -Path {}\"",
            canned_path.display(),
            out.display(),
        );
        let mut session = empty_session();
        let mut att = RemoteAttachment::attach(&cl).unwrap();
        // Events are ordered, so by the time pump sees %exit it has answered the %pause.
        pump_until_finished(&mut att, &mut session, Duration::from_secs(60));
        assert_eq!(att.version(), Some((3, 4)), "3.4 parsed");
        assert!(!att.degraded(), "3.4 >= 3.2, flow control on");
        att.detach();

        let written = wait_for_file(&out, Duration::from_secs(60), |t| t.lines().count() >= 3);
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines, [VERSION, ENUMERATE, "refresh-client -A %0:continue"]);
        let _ = std::fs::remove_file(&canned_path);
        let _ = std::fs::remove_file(&out);
    }

    /// Pre-3.2 tmux: version `3.1` reports degraded, and `%pause` is NOT answered — the recorded
    /// stdin never contains `refresh-client` (sending it would error on the old server).
    #[test]
    fn old_version_degrades_and_pause_is_not_answered() {
        let canned_path = temp_path("old.bin");
        let out = temp_path("old-out.txt");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p");
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // greeting
        canned.extend_from_slice(b"%begin 1000 6 1\n3.1\n%end 1000 6 1\n"); // version reply
        canned.extend_from_slice(b"%begin 1000 7 1\n@1 %0 80 24\n%end 1000 7 1\n"); // enumeration
        canned.extend_from_slice(b"%pause %0\n");
        canned.extend_from_slice(b"%exit\n");
        std::fs::write(&canned_path, &canned).unwrap();

        let cl = format!(
            "cmd.exe /c type {} & powershell -NoProfile -Command \"$input | Set-Content -Path {}\"",
            canned_path.display(),
            out.display(),
        );
        let mut session = empty_session();
        let mut att = RemoteAttachment::attach(&cl).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(60));
        assert_eq!(att.version(), Some((3, 1)), "3.1 parsed");
        assert!(att.degraded(), "3.1 < 3.2, degraded");
        att.detach();

        // Only the attach-time commands are ever written; no refresh-client for the %pause.
        let written = wait_for_file(&out, Duration::from_secs(60), |t| t.lines().count() >= 2);
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines, [VERSION, ENUMERATE], "no refresh-client in degraded mode");
        assert!(
            !written.contains("refresh-client -A"),
            "%pause must not be answered when degraded: {written:?}",
        );
        let _ = std::fs::remove_file(&canned_path);
        let _ = std::fs::remove_file(&out);
    }

    /// An unparseable `#{version}` (e.g. a stub tmux that prints garbage) is treated as unknown:
    /// degraded, no reported version, and one warning line on stderr.
    #[test]
    fn garbage_version_degrades_with_warning() {
        let path = temp_path("garbage.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p");
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // greeting
        canned.extend_from_slice(b"%begin 1000 6 1\nnot-a-version\n%end 1000 6 1\n"); // version
        canned.extend_from_slice(b"%begin 1000 7 1\n@1 %0 80 24\n%end 1000 7 1\n"); // enumeration
        canned.extend_from_slice(b"%exit\n");
        std::fs::write(&path, &canned).unwrap();

        let mut session = empty_session();
        let mut att =
            RemoteAttachment::attach(&format!("cmd.exe /c type {}", path.display())).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(20));
        assert_eq!(att.version(), None, "unparseable => unknown version");
        assert!(att.degraded(), "unknown version => degraded");
        // The mirror still built despite the bad version.
        assert!(find_pane(&session, 0).is_some(), "enumeration still applied");
        let _ = std::fs::remove_file(&path);
    }

    /// Ordering: a `%pause` that arrives BEFORE the version reply is seen must not be answered —
    /// the attachment is degraded-by-default until the version reply clears it, so an early
    /// `%pause` on an as-yet-unknown server is left paused (safe on pre-3.2). A later 3.4 reply
    /// then clears degraded for subsequent pauses.
    #[test]
    fn pause_before_version_reply_is_not_answered() {
        let canned_path = temp_path("early-pause.bin");
        let out = temp_path("early-pause-out.txt");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"\x1bP1000p");
        canned.extend_from_slice(b"%begin 1000 5 1\n%end 1000 5 1\n"); // greeting
        // %pause arrives before either attach reply — still degraded-by-default.
        canned.extend_from_slice(b"%pause %0\n");
        canned.extend_from_slice(b"%begin 1000 6 1\n3.4\n%end 1000 6 1\n"); // version reply
        canned.extend_from_slice(b"%begin 1000 7 1\n@1 %0 80 24\n%end 1000 7 1\n"); // enumeration
        canned.extend_from_slice(b"%exit\n");
        std::fs::write(&canned_path, &canned).unwrap();

        let cl = format!(
            "cmd.exe /c type {} & powershell -NoProfile -Command \"$input | Set-Content -Path {}\"",
            canned_path.display(),
            out.display(),
        );
        let mut session = empty_session();
        let mut att = RemoteAttachment::attach(&cl).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(60));
        assert_eq!(att.version(), Some((3, 4)), "version still parsed");
        assert!(!att.degraded(), "cleared once the 3.4 reply landed");
        att.detach();

        // The early %pause was dropped (degraded at the time); no refresh-client written.
        let written = wait_for_file(&out, Duration::from_secs(60), |t| t.lines().count() >= 2);
        assert!(
            !written.contains("refresh-client -A"),
            "early %pause must not be answered: {written:?}",
        );
        let _ = std::fs::remove_file(&canned_path);
        let _ = std::fs::remove_file(&out);
    }

    // -- pure helpers --

    #[test]
    fn version_strings_parse_and_reject_garbage() {
        assert_eq!(parse_version("3.4"), Some((3, 4)));
        assert_eq!(parse_version("3.2a"), Some((3, 2)), "patch letter ignored");
        assert_eq!(parse_version("next-3.5"), Some((3, 5)), "prefix stripped");
        assert_eq!(parse_version("openbsd-7.4"), Some((7, 4)), "os prefix stripped");
        assert_eq!(parse_version("3"), Some((3, 0)), "no minor => 0");
        assert_eq!(parse_version("3.2"), Some((3, 2)));
        // The gate: below 3.2 is degraded, 3.2 and up is not.
        assert!(parse_version("3.1a").unwrap() < MIN_VERSION);
        assert!(parse_version("3.2").unwrap() >= MIN_VERSION);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("unknown"), None, "no digits at all");
        assert_eq!(parse_version("next-"), None, "prefix but no number");
    }

    #[test]
    fn enum_lines_parse_and_reject_garbage() {
        assert_eq!(parse_enum_line(b"@1 %0 80 24"), Some((1, 0, 80, 24)));
        assert_eq!(parse_enum_line(b"@12 %34 159 48"), Some((12, 34, 159, 48)));
        assert_eq!(parse_enum_line(b""), None);
        assert_eq!(parse_enum_line(b"%0 @1 80 24"), None, "sigils are positional");
        assert_eq!(parse_enum_line(b"@1 %0 80"), None, "missing rows");
        assert_eq!(parse_enum_line(b"@1 %0 eighty 24"), None);
    }

    #[test]
    fn even_split_chains_equal_ratios() {
        let ids = [PaneId(1), PaneId(2), PaneId(3)];
        let root = even_split(&ids).unwrap();
        let Node::Split { ratio, a, b, .. } = &root else { panic!("expected split") };
        assert!((ratio - 1.0 / 3.0).abs() < 1e-6);
        assert!(matches!(**a, Node::Leaf(id) if id == ids[0]));
        let Node::Split { ratio, .. } = &**b else { panic!("expected nested split") };
        assert!((ratio - 0.5).abs() < 1e-6);
        assert!(even_split(&[]).is_none());
        assert!(matches!(even_split(&ids[..1]), Some(Node::Leaf(id)) if id == ids[0]));
    }

    /// F1/F2 regression (review of dc4322d): a remote `join-pane` moves %0 from @1 into @2 —
    /// the destination's %layout-change must RE-HOME the mirror (extract from @1, insert into
    /// @2), not leave one pane referenced by two trees with @1's active dangling (which
    /// panicked workspace_info -> poisoned the server mutex -> killed the daemon).
    #[test]
    fn cross_window_pane_move_rehomes_the_mirror() {
        let path = temp_path("rehome.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"P1000p");
        canned.extend_from_slice(b"%begin 1000 5 1
%end 1000 5 1
");
        canned.extend_from_slice(b"%begin 1000 6 1
3.4
%end 1000 6 1
");
        canned.extend_from_slice(b"%begin 1000 7 1
@1 %0 80 24
@1 %1 80 24
@2 %5 80 24
%end 1000 7 1
");
        // Remote join-pane: %0 leaves @1 (its layout shrinks to just %1)...
        canned.extend_from_slice(b"%layout-change @1 aaaa,80x24,0,0,1
");
        // ...and joins @2 (its layout now references %0 beside %5).
        canned.extend_from_slice(b"%layout-change @2 bbbb,159x48,0,0{79x48,0,0,5,79x48,80,0,0}
");
        canned.extend_from_slice(b"%exit
");
        std::fs::write(&path, &canned).unwrap();

        let mut session = empty_session();
        let mut att =
            RemoteAttachment::attach(&format!("cmd.exe /c type {}", path.display())).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(20));

        assert_eq!(session.window_count(), 2);
        assert_eq!(session.pane_count(), 3, "no duplicate/dangling mirrors");
        // Exactly ONE window references %0's mirror.
        let homes = session
            .windows()
            .iter()
            .filter(|w| w.panes().any(|p| p.remote_id() == Some(0)))
            .count();
        assert_eq!(homes, 1, "%0 must live in exactly one window");
        // The reviewer's exact crash path: workspace_info on every window (GetLayout does this).
        for w in session.windows() {
            let _ = w.workspace_info(); // panicked before the fix
        }
    }

    /// Ordering variant: the destination window's %layout-change arrives BEFORE the source
    /// window's shrink. Re-homing must still hold (extract collapses the source tree first).
    #[test]
    fn cross_window_move_destination_layout_first() {
        let path = temp_path("rehome2.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(b"P1000p");
        canned.extend_from_slice(b"%begin 1000 5 1
%end 1000 5 1
");
        canned.extend_from_slice(b"%begin 1000 6 1
3.4
%end 1000 6 1
");
        canned.extend_from_slice(b"%begin 1000 7 1
@1 %0 80 24
@1 %1 80 24
@2 %5 80 24
%end 1000 7 1
");
        canned.extend_from_slice(b"%layout-change @2 bbbb,159x48,0,0{79x48,0,0,5,79x48,80,0,0}
");
        canned.extend_from_slice(b"%layout-change @1 aaaa,80x24,0,0,1
");
        canned.extend_from_slice(b"%exit
");
        std::fs::write(&path, &canned).unwrap();

        let mut session = empty_session();
        let mut att =
            RemoteAttachment::attach(&format!("cmd.exe /c type {}", path.display())).unwrap();
        pump_until_finished(&mut att, &mut session, Duration::from_secs(20));

        let homes = session
            .windows()
            .iter()
            .filter(|w| w.panes().any(|p| p.remote_id() == Some(0)))
            .count();
        assert_eq!(homes, 1, "%0 must live in exactly one window");
        for w in session.windows() {
            let _ = w.workspace_info();
        }
    }

}
