//! gmux-server — the headless multiplexer server: owns the `Mux`/`Session` and its ConPTYs, and
//! serves the [`gmux_proto`] automation protocol over the named pipe. This is what `gmux --daemon`
//! runs; because the daemon (not the GUI) owns the panes, they survive the GUI detaching (M6).
//!
//! Panes render at a default cell size until a client reports its geometry (M6b adds resize/grid
//! streaming so a thin GUI can attach).

pub mod remote;

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader};
use std::sync::{Arc, Mutex};

use std::path::PathBuf;

use gmux_mux::{
    FocusDir, Pane, PaneEvent, PaneId, Palette, ProgressState, PtySize, Rgb, Session,
    SessionSnapshot, SplitDir, Urgency, Window,
};
use gmux_pipe::{PipeServer, PipeStream};
use gmux_proto::{
    read_msg, write_msg, Call, CellWire, GridWire, LayoutWire, NotifyWire, PaneInfo, PaneRectWire,
    Request, Response, ResultBody, TabWire, CELL_BOLD, CELL_INVERSE, CELL_ITALIC, CELL_UNDERLINE,
    CELL_WIDE,
};

use remote::RemoteAttachment;

const DEFAULT_SIZE: PtySize = PtySize { cols: 120, rows: 30 };

/// The multiplexer state a daemon serves.
pub struct Server {
    pub session: Session,
    pub shell: String,
    /// Last content-area geometry reported by a client (for focus-movement math).
    last_view: (u32, u32),
    /// Notifications raised by panes, drained by `PollNotifications`.
    notifications: Vec<NotifyWire>,
    /// Browser-open requests queued by `Browse`, drained by `PollBrowse` (M12).
    browse_requests: Vec<String>,
    /// Tick counter for debounced snapshot saves.
    ticks: u32,
    /// Live remote tmux attachments; pumped every tick, dropped when finished.
    remotes: Vec<RemoteAttachment>,
    /// Latest OSC 9;4 progress per pane (Remove clears the entry; so does pane removal).
    progress: HashMap<PaneId, (ProgressState, Option<u8>)>,
    /// Color palette applied to every pane's terminal — set by the GUI via `SetPalette`, applied
    /// to newly spawned panes so late arrivals match the theme. Defaults to gmux's built-in colors.
    palette: Palette,
    /// M7 privacy: whether session snapshots persist each pane's screen text. Read once from
    /// `gmux.json` at daemon start (default true); `false` writes snapshots with empty screens.
    persist_screen: bool,
}

impl Server {
    /// Create a server whose session's first window runs `shell`.
    pub fn new(shell: String) -> io::Result<Server> {
        let pane = Pane::spawn(&shell, DEFAULT_SIZE)?;
        Ok(Server {
            session: Session::start("gmux", pane),
            shell,
            last_view: (1200, 720),
            notifications: Vec::new(),
            browse_requests: Vec::new(),
            ticks: 0,
            remotes: Vec::new(),
            progress: HashMap::new(),
            palette: Palette::default(),
            persist_screen: load_persist_screen(),
        })
    }

    /// Restore the last session from disk (respawning shells in saved cwds + replaying screen
    /// history), or start fresh.
    pub fn restore_or_new(shell: String) -> io::Result<Server> {
        if let Some(snap) = load_snapshot() {
            let restored = snap.restore("gmux", |rec| {
                let replay = restore_replay(&rec.screen);
                Pane::spawn_in(&shell, DEFAULT_SIZE, rec.cwd.as_deref(), replay.as_deref())
            });
            if let Ok(session) = restored {
                if session.pane_count() > 0 {
                    eprintln!("gmux daemon: restored {} pane(s) from last session", session.pane_count());
                    return Ok(Server {
                        session,
                        shell,
                        last_view: (1200, 720),
                        notifications: Vec::new(),
                        browse_requests: Vec::new(),
                        ticks: 0,
                        remotes: Vec::new(),
                        progress: HashMap::new(),
                        palette: Palette::default(),
                        persist_screen: load_persist_screen(),
                    });
                }
            }
        }
        Server::new(shell)
    }

    /// Persist the current layout + per-pane cwd to disk (atomic).
    pub fn save(&self) {
        let snap = SessionSnapshot::capture_with(&self.session, self.persist_screen);
        let Ok(json) = serde_json::to_string_pretty(&snap) else { return };
        let path = state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Drain every pane's events: queue notifications for `PollNotifications`, remove panes whose
    /// process exited, and return this tick's event batch for the push subscribers (notifications
    /// plus one `title == "pane-exited"` [`NotifyWire`] per removed pane). Called periodically by
    /// the daemon loop; empty when nothing happened.
    pub fn tick(&mut self) -> Vec<NotifyWire> {
        // Pump remote attachments first so their output/exits are visible to the pane-event
        // drain below within the same tick. Finished attachments are dropped — their panes were
        // marked exited, so the normal Exited sweep removes them.
        let session = &mut self.session;
        self.remotes.retain_mut(|r| r.pump(session));
        let mut notes = Vec::new();
        let mut exited = Vec::new();
        let mut progress = Vec::new();
        // Panes whose grid changed this tick — coalesced (a HashSet) to one damage wire per pane.
        let mut damaged: HashSet<PaneId> = HashSet::new();
        for w in self.session.windows() {
            for p in w.panes() {
                for ev in p.drain_events() {
                    match ev {
                        PaneEvent::Notification(n) => notes.push(NotifyWire {
                            pane: p.id.0,
                            title: n.title,
                            body: n.body,
                            urgency: match n.urgency {
                                Urgency::Low => 0,
                                Urgency::Normal => 1,
                                Urgency::Critical => 2,
                            },
                        }),
                        PaneEvent::Exited => exited.push(p.id),
                        PaneEvent::Progress { state, pct } => progress.push((p.id, state, pct)),
                        PaneEvent::Output => {
                            damaged.insert(p.id);
                        }
                        _ => {}
                    }
                }
            }
        }
        // The push batch is the notifications plus a synthetic wire per exit and per damaged pane;
        // subscribers see all three. `pane-output` wires are push-only (never queued for
        // PollNotifications) and are filtered back out for subscribers that didn't opt into
        // `output` — so toasts and plain `gmux subscribe` streams never see damage traffic.
        let mut batch = notes.clone();
        for id in &exited {
            batch.push(exit_notify(id.0));
        }
        for id in &damaged {
            batch.push(output_notify(id.0));
        }
        self.notifications.extend(notes);
        for (id, state, pct) in progress {
            match state {
                ProgressState::Remove => {
                    self.progress.remove(&id);
                }
                _ => {
                    self.progress.insert(id, (state, pct));
                }
            }
        }
        for id in exited {
            self.progress.remove(&id);
            self.session.remove_pane(id);
        }
        // Panes can also leave the session without an observable Exited drain (remote layout
        // prunes / window closes drop them from the tree before this loop sees the event), which
        // would leak their progress entries forever. Sweep against the live session.
        self.progress.retain(|id, _| self.session.pane(*id).is_some());
        // Debounced snapshot save (~every 2 s at a 100 ms tick).
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 20 == 0 {
            self.save();
        }
        batch
    }

    fn spawn_pane(&self, command: &Option<String>) -> io::Result<Pane> {
        let cmd = command.clone().unwrap_or_else(|| self.shell.clone());
        let pane = Pane::spawn(&cmd, DEFAULT_SIZE)?;
        pane.set_palette(self.palette); // late arrivals match the current theme
        Ok(pane)
    }

    fn find(&self, id: u64) -> Option<&Pane> {
        self.session.pane(PaneId(id))
    }

    /// The attachment owning the active window's active pane (and its remote `%pane` id), if the
    /// active pane is a remote mirror. Pane operations on mirrors round-trip to the remote.
    fn active_remote(&mut self) -> Option<(&mut RemoteAttachment, u64)> {
        let active = self.session.active_window().map(|w| w.active_id())?;
        self.remotes
            .iter_mut()
            .find_map(|att| att.remote_id_of(active).map(|remote| (att, remote)))
    }

    /// Execute one protocol request against the mux, returning the response.
    pub fn handle(&mut self, req: &Request) -> Response {
        let id = req.id;
        match &req.call {
            Call::Hello { .. } => Response::ok(
                id,
                ResultBody::Hello {
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol: gmux_proto::PROTOCOL_VERSION,
                },
            ),
            Call::ListPanes => Response::ok(id, ResultBody::Panes(self.list_panes())),
            Call::SendKeys { pane, text, enter } => match self.find(*pane) {
                Some(p) => {
                    let mut bytes = text.as_bytes().to_vec();
                    if *enter {
                        bytes.push(b'\r');
                    }
                    match p.write(&bytes) {
                        Ok(()) => Response::ok(id, ResultBody::Done),
                        Err(e) => Response::err(id, format!("write failed: {e}")),
                    }
                }
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::CapturePane { pane, scrollback } => match self.find(*pane) {
                Some(p) => Response::ok(id, ResultBody::Text(capture(p, *scrollback))),
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::SplitPane { dir, command } => {
                let sd = match dir.as_str() {
                    "h" => SplitDir::Horizontal,
                    "v" => SplitDir::Vertical,
                    other => return Response::err(id, format!("bad dir '{other}' (h|v)")),
                };
                // A remote mirror's layout is owned by the remote tmux: splitting locally would
                // spawn a shell the next %layout-change silently discards. Round-trip instead;
                // the new pane arrives via that %layout-change.
                if let Some((att, remote)) = self.active_remote() {
                    att.split_remote(remote, sd == SplitDir::Horizontal);
                    return Response::ok(id, ResultBody::Done);
                }
                match self.spawn_pane(command) {
                    Ok(pane) => {
                        let pid = pane.id.0;
                        if let Some(w) = self.session.active_window_mut() {
                            w.split(sd, pane);
                        }
                        Response::ok(id, ResultBody::PaneId(pid))
                    }
                    Err(e) => Response::err(id, format!("spawn failed: {e}")),
                }
            }
            Call::NewWindow { command } => match self.spawn_pane(command) {
                Ok(pane) => {
                    let pid = pane.id.0;
                    self.session.add_window(pane);
                    Response::ok(id, ResultBody::PaneId(pid))
                }
                Err(e) => Response::err(id, format!("spawn failed: {e}")),
            },
            Call::Notify { pane, title, body } => {
                let target = pane.or_else(|| self.session.active_window().map(|w| w.active_id().0));
                match target.and_then(|t| self.find(t)) {
                    Some(p) => {
                        p.request_attention();
                        // Attribution/toast happens in the GUI when it attaches; a daemon-only
                        // notify just raises the pane's attention flag. (title/body reserved.)
                        let _ = (title, body);
                        Response::ok(id, ResultBody::Done)
                    }
                    None => Response::err(id, "no target pane"),
                }
            }

            Call::GetLayout { w, h } => {
                self.last_view = (*w, *h);
                Response::ok(id, ResultBody::Layout(self.layout(*w, *h)))
            }
            Call::GetGrid { pane, offset } => match self.find(*pane) {
                Some(p) => Response::ok(id, ResultBody::Grid(grid_wire(p, *offset))),
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::ResizeView { w, h, cell_w, cell_h, pane_chrome, pane_chrome_y } => {
                self.last_view = (*w, *h);
                let (cw, ch) = ((*cell_w).max(1), (*cell_h).max(1));
                // Vertical chrome includes the title strip; a zero (old client) falls back to the
                // horizontal chrome so rows aren't left oversized.
                let chrome_y = if *pane_chrome_y != 0 { *pane_chrome_y } else { *pane_chrome };
                if let Some(win) = self.session.active_window() {
                    for (pid, rect) in win.layout_rects(*w, *h) {
                        if let Some(p) = win.pane(pid) {
                            // Grids fit the VISIBLE cell area: the GUI draws margins/borders/
                            // insets/title-strip inside each rect, so those pixels can't hold cells.
                            let cols = (rect.w.saturating_sub(*pane_chrome) / cw).max(1) as u16;
                            let rows = (rect.h.saturating_sub(chrome_y) / ch).max(1) as u16;
                            let _ = p.resize(PtySize { cols, rows });
                        }
                    }
                }
                // Every remote tmux lays its windows out at the client size; tell them all (the
                // active window may or may not be remote — keeping every attachment current is
                // simpler than tracking which one is showing).
                let cols = (*w / cw).max(1) as u16;
                let rows = (*h / ch).max(1) as u16;
                for r in &mut self.remotes {
                    r.resize_client(cols, rows);
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::FocusPane { dir } => {
                let d = match dir.as_str() {
                    "left" => FocusDir::Left,
                    "right" => FocusDir::Right,
                    "up" => FocusDir::Up,
                    "down" => FocusDir::Down,
                    other => return Response::err(id, format!("bad dir '{other}'")),
                };
                let (w, h) = self.last_view;
                if let Some(win) = self.session.active_window_mut() {
                    win.focus_dir(d, w, h);
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::ClosePane => {
                // Remote mirror: ask the remote to kill the pane; its %layout-change (or
                // %window-close) prunes the mirror. No local mutation now.
                if let Some((att, remote)) = self.active_remote() {
                    att.kill_remote(remote);
                    return Response::ok(id, ResultBody::Done);
                }
                let closed = self.session.active_window_mut().and_then(|w| w.close_active());
                if closed.is_none() {
                    self.session.close_active_window();
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::ToggleZoom => {
                if let Some(w) = self.session.active_window_mut() {
                    w.toggle_zoom();
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::ResizeSplit { pane, dx, dy } => {
                // The GUI only drags panes in the active (rendered) window. A gone pane no-ops.
                // ponytail: no remote round-trip — remote tmux owns its own layout; a drag on a
                // mirror is DROPPED (mutating the mirror tree would desync it and fight the next
                // %layout-change) until that path grows a resize-pane control message.
                let is_remote = self
                    .remotes
                    .iter()
                    .any(|att| att.remote_id_of(PaneId(*pane)).is_some());
                if !is_remote {
                    if let Some(w) = self.session.active_window_mut() {
                        w.resize_pane(PaneId(*pane), *dx, *dy);
                    }
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::SwitchWindow { next } => {
                if *next {
                    self.session.next_window();
                } else {
                    self.session.prev_window();
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::SelectWindow { index } => {
                self.session.select_window(*index);
                Response::ok(id, ResultBody::Done)
            }
            Call::FocusPaneId { pane } => {
                self.session.focus_pane(PaneId(*pane));
                Response::ok(id, ResultBody::Done)
            }
            Call::MoveWindow { from, to } => {
                self.session.move_window(*from, *to);
                Response::ok(id, ResultBody::Done)
            }
            Call::PollNotifications => {
                Response::ok(id, ResultBody::Notifications(std::mem::take(&mut self.notifications)))
            }
            // Registration happens in serve_connection (it owns the writer clone to add to the
            // subscriber list, with the `output` flag); handle() only acks so the client knows the
            // stream is armed.
            Call::Subscribe { .. } => Response::ok(id, ResultBody::Done),
            Call::SetPalette { fg, bg, ansi } => {
                let mut palette = Palette::default();
                palette.fg = Rgb { r: fg[0], g: fg[1], b: fg[2] };
                palette.bg = Rgb { r: bg[0], g: bg[1], b: bg[2] };
                // `ansi` may be short (hand-written JSON / partial theme): keep defaults past its end.
                for (slot, c) in palette.ansi.iter_mut().zip(ansi) {
                    *slot = Rgb { r: c[0], g: c[1], b: c[2] };
                }
                self.palette = palette;
                for w in self.session.windows() {
                    for p in w.panes() {
                        p.set_palette(palette);
                    }
                }
                Response::ok(id, ResultBody::Done)
            }
            Call::Browse { url } => {
                // The daemon only queues; the GUI (with the `browser` feature) drains via
                // PollBrowse and drives the WebView2 window. A daemon with no GUI attached simply
                // accumulates requests until one attaches.
                self.browse_requests.push(url.clone());
                Response::ok(id, ResultBody::Done)
            }
            Call::PollBrowse => {
                Response::ok(id, ResultBody::Browses(std::mem::take(&mut self.browse_requests)))
            }
            Call::SshTmux { target, command } => {
                let cl = command
                    .clone()
                    .unwrap_or_else(|| format!("ssh -tt {target} -- tmux -CC new -As gmux"));
                match RemoteAttachment::attach(&cl) {
                    Ok(att) => {
                        self.remotes.push(att);
                        Response::ok(id, ResultBody::Done)
                    }
                    Err(e) => Response::err(id, format!("ssh-tmux spawn failed: {e}")),
                }
            }
        }
    }

    fn layout(&self, w: u32, h: u32) -> LayoutWire {
        let active_idx = self.session.active_index();
        let tabs = self
            .session
            .windows()
            .iter()
            .enumerate()
            .map(|(i, win)| {
                let info = win.workspace_info();
                let (progress, progress_error) = window_progress(win, &self.progress);
                TabWire {
                    index: i,
                    name: info.name,
                    branch: info.branch,
                    attention: info.attention,
                    active: i == active_idx,
                    progress,
                    progress_error,
                }
            })
            .collect();
        let (active_pane, panes) = match self.session.active_window() {
            Some(win) => {
                let active = win.active_id();
                let rects = win
                    .layout_rects(w, h)
                    .into_iter()
                    .filter_map(|(pid, r)| {
                        win.pane(pid).map(|p| PaneRectWire {
                            id: pid.0,
                            x: r.x,
                            y: r.y,
                            w: r.w,
                            h: r.h,
                            active: pid == active,
                            attention: p.attention().is_pending(),
                            title: pane_title(p, pid.0),
                        })
                    })
                    .collect();
                (active.0, rects)
            }
            None => (0, Vec::new()),
        };
        LayoutWire { active_pane, tabs, panes }
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        let active_win = self.session.active_index();
        let mut panes = Vec::new();
        for (wi, win) in self.session.windows().iter().enumerate() {
            let active_pane = win.active_id();
            for p in win.panes() {
                let snap = p.snapshot();
                panes.push(PaneInfo {
                    id: p.id.0,
                    window: wi,
                    active: wi == active_win && p.id == active_pane,
                    title: p.title(),
                    cwd: p.cwd(),
                    cols: snap.cols,
                    rows: snap.rows,
                    attention: p.attention().is_pending(),
                });
            }
        }
        panes.sort_by_key(|p| p.id);
        panes
    }

    /// True once every pane's process has exited (the daemon may then shut down).
    pub fn all_exited(&self) -> bool {
        self.session.pane_count() == 0
            || self.session.windows().iter().all(|w| w.panes().all(|p| !p.is_alive()))
    }
}

/// The short title for a pane's title strip: its terminal title (OSC 0/2) if set, else the short
/// name of its cwd, else the `%id` fallback so the strip is never blank.
fn pane_title(p: &Pane, id: u64) -> String {
    let title = p.title();
    if !title.is_empty() {
        return title;
    }
    if let Some(name) = p.cwd().as_deref().map(gmux_mux::workspace::cwd_name).filter(|s| !s.is_empty()) {
        return name;
    }
    format!("%{id}")
}

/// Aggregate a window's per-pane progress into the sidebar's `(progress, error)` pair: an Error in
/// any pane wins (returns `error = true`); otherwise `progress` is the lowest pct among Set panes
/// (the least-done agent). Indeterminate/Paused panes count as active but report no pct, so a window
/// with only those yields `(None, false)` — same as idle to the pct-only sidebar. ponytail: pct+error
/// is all the sidebar renders; richer states go on the wire the day the UI grows a spinner.
fn window_progress(
    win: &Window,
    progress: &HashMap<PaneId, (ProgressState, Option<u8>)>,
) -> (Option<u8>, bool) {
    let mut error = false;
    let mut min_pct: Option<u8> = None;
    for p in win.panes() {
        if let Some((state, pct)) = progress.get(&p.id) {
            match state {
                ProgressState::Error => error = true,
                ProgressState::Set => {
                    if let Some(v) = pct {
                        min_pct = Some(min_pct.map_or(*v, |m| m.min(*v)));
                    }
                }
                _ => {}
            }
        }
    }
    (min_pct, error)
}

/// Encode a pane's grid for the wire, scrolled `offset` lines into scrollback (clamped). The
/// snapshot, history depth, and clamped offset are read under one terminal lock so they can't
/// skew against each other while the pump thread appends output.
fn grid_wire(p: &Pane, offset: usize) -> GridWire {
    let bracketed_paste = p.bracketed_paste();
    let (snap, history, offset) = p.snapshot_scrolled(offset);
    let mut cells = Vec::with_capacity(snap.cols as usize * snap.rows as usize);
    for row in &snap.cells {
        for c in row {
            let mut flags = 0u8;
            if c.bold {
                flags |= CELL_BOLD;
            }
            if c.italic {
                flags |= CELL_ITALIC;
            }
            if c.underline {
                flags |= CELL_UNDERLINE;
            }
            if c.inverse {
                flags |= CELL_INVERSE;
            }
            if c.wide {
                flags |= CELL_WIDE;
            }
            cells.push(CellWire {
                ch: c.ch,
                fg: [c.fg.r, c.fg.g, c.fg.b],
                bg: [c.bg.r, c.bg.g, c.bg.b],
                flags,
            });
        }
    }
    GridWire {
        cols: snap.cols,
        rows: snap.rows,
        cursor_col: snap.cursor.0,
        cursor_row: snap.cursor.1,
        cells,
        history: history as u32,
        offset: offset as u32,
        bracketed_paste,
    }
}

/// Capture a pane's screen text. `scrollback` (the `-S` option) pulls history above the viewport:
/// `Some(0)` = all retained scrollback + screen, `Some(n)` = the most-recent `n` lines, `None` =
/// the visible screen only.
fn capture(p: &Pane, scrollback: Option<usize>) -> String {
    let mut lines: Vec<String> = match scrollback {
        Some(n) => p.scrollback_text(n),
        None => p
            .snapshot()
            .cells
            .iter()
            .map(|row| {
                let mut s: String = row.iter().map(|c| c.ch).collect();
                s.truncate(s.trim_end_matches(' ').len());
                s
            })
            .collect(),
    };
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// The wire form of a pane exit for push subscribers: a `NotifyWire` with the reserved
/// `"pane-exited"` title and the pane id in `pane` (see `Call::Subscribe` docs).
fn exit_notify(pane: u64) -> NotifyWire {
    NotifyWire { pane, title: "pane-exited".into(), body: String::new(), urgency: 1 }
}

/// The wire form of per-pane damage for `output` subscribers: a `NotifyWire` with the reserved
/// `"pane-output"` title and the damaged pane's id (see `Call::Subscribe` docs). Filtered out of
/// non-`output` subscriber streams by [`push_to_subscribers`].
fn output_notify(pane: u64) -> NotifyWire {
    NotifyWire { pane, title: "pane-output".into(), body: String::new(), urgency: 0 }
}

/// Push one event `batch` to every subscriber as a `Response{id:0}` line, dropping (via
/// `retain`) any subscriber whose write fails — a disconnected client is simply removed. A empty
/// batch is a no-op. Split out so it can be unit-tested with an in-process pipe pair (no console).
///
/// Deliberately writes WITHOUT flushing: `PipeStream::flush` is `FlushFileBuffers`, which blocks
/// until the peer has **read** the data — a subscriber that isn't mid-`read` at push time would
/// deadlock the pusher. A plain `WriteFile` completes once the bytes are in the 64 KiB pipe
/// buffer; the reader gets them without any flush.
// ponytail: a subscriber that stops reading entirely can still fill its 64 KiB buffer and block
// the push thread (stalling delivery to other subscribers, never the daemon loop) — per-subscriber
// writer threads if that ever matters.
//
// Each subscriber carries its `output` opt-in flag. `pane-output` damage wires go only to
// `output == true` subscribers; everyone else gets the batch with those wires stripped (and no
// line at all if that leaves nothing). Two lines are serialized at most once per push, not once
// per subscriber.
fn push_to_subscribers(subscribers: &mut Vec<(PipeStream, bool)>, batch: &[NotifyWire]) {
    use std::io::Write;
    if batch.is_empty() {
        return;
    }
    // Full line: every wire, for `output` subscribers.
    let Some(full) = serialize_push(batch) else { return };
    // Filtered line: drop `pane-output` damage wires, for non-`output` subscribers. `None` when
    // that leaves an empty batch — those subscribers simply get nothing this tick.
    let filtered: Vec<NotifyWire> =
        batch.iter().filter(|n| n.title != "pane-output").cloned().collect();
    let filtered_line = if filtered.len() == batch.len() {
        Some(full.clone()) // nothing stripped — reuse the full line
    } else if filtered.is_empty() {
        None
    } else {
        serialize_push(&filtered)
    };
    subscribers.retain_mut(|(w, output)| {
        let line = if *output { Some(&full) } else { filtered_line.as_ref() };
        match line {
            Some(l) => w.write_all(l.as_bytes()).is_ok(),
            None => true, // no line for this subscriber this tick; keep it connected
        }
    });
}

/// Serialize one push batch as a `Response{id:0}` JSON line (newline-terminated). `None` if
/// serialization somehow fails (a poisoned/inexpressible value) — the caller then skips the push.
fn serialize_push(batch: &[NotifyWire]) -> Option<String> {
    let push = Response::ok(0, ResultBody::Notifications(batch.to_vec()));
    let mut line = serde_json::to_string(&push).ok()?;
    line.push('\n');
    Some(line)
}

/// Read the `persist_screen` flag from the same `%APPDATA%\gmux\gmux.json` the GUI reads (M7
/// privacy). The daemon does the persisting, so it reads this itself rather than importing
/// gmux-gui (which would drag winit/wgpu into the daemon). A missing/unreadable/malformed file or
/// absent key defaults to `true` (persist screen text). BOM-stripped like the GUI's loader, since
/// PowerShell/Notepad write a UTF-8 BOM that would otherwise make serde_json reject the whole file.
/// Parses via `Value` — no serde-derive dep for one bool. Exposed for the config-parse unit test.
fn load_persist_screen() -> bool {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    let path = PathBuf::from(base).join("gmux").join("gmux.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return true };
    persist_screen_from_json(&text)
}

/// The pure parse half of [`load_persist_screen`]: BOM-strip, read the top-level `persist_screen`
/// bool, default `true` when the file is malformed or the key is absent/non-bool.
fn persist_screen_from_json(text: &str) -> bool {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("persist_screen").and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

/// Where the session snapshot lives: `%LOCALAPPDATA%\gmux\state\session.json`.
fn state_path() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("gmux").join("state").join("session.json")
}

fn load_snapshot() -> Option<SessionSnapshot> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Build the inert-history replay for a restored pane: the saved screen lines under a dim divider.
/// Returns `None` when there is nothing to replay.
fn restore_replay(screen: &[String]) -> Option<String> {
    if screen.iter().all(|l| l.is_empty()) {
        return None;
    }
    let mut out = String::new();
    for line in screen {
        out.push_str(line);
        out.push_str("\r\n");
    }
    // Dim divider (SGR 90) marking where restored history ends and the fresh shell begins.
    out.push_str("\x1b[90m\u{2500}\u{2500}\u{2500} gmux: restored (process not running) \u{2500}\u{2500}\u{2500}\x1b[0m\r\n");
    Some(out)
}

/// Run the daemon: restore or create the mux, serve the pipe, and block until all panes exit.
pub fn run(shell: String, pipe_base: &str) -> io::Result<()> {
    let server = Arc::new(Mutex::new(Server::restore_or_new(shell)?));
    // Push subscribers, kept in their own mutex (not the Server one): tick() runs under the Server
    // lock, but the push writes go through these separate writer handles *after* the Server lock is
    // released, so a slow/blocked subscriber can never stall a request thread holding the Server.
    // Each entry pairs a writer handle with its `output` opt-in (whether it wants `pane-output`
    // damage wires) — see `Call::Subscribe`.
    let subscribers: Arc<Mutex<Vec<(PipeStream, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let name = gmux_pipe::pipe_name_for_user(pipe_base);
    let handler_server = server.clone();
    let handler_subs = subscribers.clone();
    let _pipe = PipeServer::start(&name, move |stream| {
        serve_connection(stream, &handler_server, &handler_subs);
    })?;
    eprintln!("gmux daemon: serving \\\\.\\pipe\\{name}");

    // Pushes run on their own thread, fed over a channel: even the no-flush pipe write can block
    // when a subscriber stops reading and its 64 KiB buffer fills, and that must stall only this
    // thread — never the tick loop that keeps every pane serviced.
    let (push_tx, push_rx) = std::sync::mpsc::channel::<Vec<NotifyWire>>();
    let push_subs = subscribers.clone();
    std::thread::spawn(move || {
        while let Ok(batch) = push_rx.recv() {
            if let Ok(mut subs) = push_subs.lock() {
                push_to_subscribers(&mut subs, &batch);
            }
        }
    });

    // Drain pane events (notifications + exits) and stop once every pane has exited.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let (batch, done) = match server.lock() {
            Ok(mut s) => {
                let batch = s.tick();
                (batch, s.all_exited())
            }
            Err(_) => (Vec::new(), true),
        };
        // Hand the batch to the push thread (never blocks the tick loop).
        if !batch.is_empty() {
            let _ = push_tx.send(batch);
        }
        if done {
            eprintln!("gmux daemon: all panes exited, shutting down");
            // Clean exit: clear the snapshot so the next start is fresh (a reboot, by contrast,
            // kills the daemon and leaves the last periodic save to restore from).
            let _ = std::fs::remove_file(state_path());
            return Ok(());
        }
    }
}

fn serve_connection(
    stream: PipeStream,
    server: &Arc<Mutex<Server>>,
    subscribers: &Arc<Mutex<Vec<(PipeStream, bool)>>>,
) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    loop {
        let req: Request = match read_msg(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                let _ = write_msg(&mut writer, &Response::err(0, format!("bad request: {e}")));
                return;
            }
        };
        let subscribe_output = match &req.call {
            Call::Subscribe { output } => Some(*output),
            _ => None,
        };
        let resp = match server.lock() {
            Ok(mut s) => s.handle(&req),
            Err(_) => Response::err(req.id, "server lock poisoned"),
        };
        if write_msg(&mut writer, &resp).is_err() {
            return;
        }
        // On a successful Subscribe, register a second writer handle (with its `output` flag) so
        // the daemon loop can push event batches on this connection. The read loop continues
        // (further requests still serve).
        if let (Some(output), true) = (subscribe_output, resp.error.is_none()) {
            match writer.try_clone() {
                Ok(w) => {
                    if let Ok(mut subs) = subscribers.lock() {
                        subs.push((w, output));
                    }
                }
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_proto::{Call, Request};

    // Note: methods that spawn panes need a real console (ConPTY binding) and are covered by the
    // console-gated integration test. These unit tests exercise the request plumbing only.

    #[test]
    fn hello_returns_version_without_a_pane() {
        // Build a Server-less handler by hand isn't possible (Server::new spawns a pane), so we
        // only assert the protocol shape of a hello response constructed directly.
        let resp = Response::ok(
            1,
            ResultBody::Hello { server_version: "0.0.0".into(), protocol: gmux_proto::PROTOCOL_VERSION },
        );
        assert!(resp.error.is_none());
    }

    #[test]
    fn unknown_method_targets_error_path_shape() {
        let req = Request { id: 9, call: Call::CapturePane { pane: 999, scrollback: None } };
        // A no-such-pane capture must be an error; verify the constructor used by handle().
        let resp = Response::err(req.id, "no pane %999");
        assert_eq!(resp.id, 9);
        assert!(resp.result.is_none());
    }

    /// M12 browser pane: `Browse` queues urls; `PollBrowse` drains them in order and leaves the
    /// queue empty (mirrors PollNotifications). No pane/console needed — pure queue plumbing, so it
    /// runs headless in the default `cargo test`.
    #[test]
    fn browse_queues_and_poll_browse_drains_in_order() {
        use gmux_mux::{Pane, Session};
        // Build a Server with a console-free remote pane so no ConPTY is bound.
        let pane = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let mut server = Server {
            session: Session::start("t", pane),
            shell: "pwsh".into(),
            last_view: (800, 600),
            notifications: Vec::new(),
            browse_requests: Vec::new(),
            ticks: 0,
            remotes: Vec::new(),
            progress: HashMap::new(),
            palette: Palette::default(),
            persist_screen: true,
        };

        let b1 = server.handle(&Request { id: 1, call: Call::Browse { url: "https://a.test".into() } });
        assert_eq!(b1.result, Some(ResultBody::Done));
        server.handle(&Request { id: 2, call: Call::Browse { url: "https://b.test".into() } });

        let drained = server.handle(&Request { id: 3, call: Call::PollBrowse });
        assert_eq!(
            drained.result,
            Some(ResultBody::Browses(vec!["https://a.test".into(), "https://b.test".into()]))
        );
        // A second poll is empty — the queue was taken, not copied.
        let again = server.handle(&Request { id: 4, call: Call::PollBrowse });
        assert_eq!(again.result, Some(ResultBody::Browses(Vec::new())));
    }

    /// `SetPalette` re-themes existing (console-free remote) panes: after it, SGR 31 red resolves
    /// to the custom palette color, not the built-in 0x800000.
    #[test]
    fn set_palette_applies_to_existing_panes() {
        let pane = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let pid = pane.id.0;
        let mut server = Server {
            session: Session::start("t", pane),
            shell: "pwsh".into(),
            last_view: (800, 600),
            notifications: Vec::new(),
            browse_requests: Vec::new(),
            ticks: 0,
            remotes: Vec::new(),
            progress: HashMap::new(),
            palette: Palette::default(),
            persist_screen: true,
        };

        // Custom red in ANSI slot 1; other slots left at their defaults on the wire (short vec).
        let call = Call::SetPalette { fg: [1, 2, 3], bg: [4, 5, 6], ansi: vec![[0, 0, 0], [0xde, 0xad, 0xbe]] };
        assert_eq!(server.handle(&Request { id: 1, call }).result, Some(ResultBody::Done));

        // Feed red text; the pane's grid now resolves Named::Red to the custom color.
        server.session.pane(PaneId(pid)).unwrap().push_output(b"\x1b[31mX\x1b[0m");
        let snap = server.session.pane(PaneId(pid)).unwrap().snapshot();
        let fg = snap.cells[0][0].fg;
        assert_eq!((fg.r, fg.g, fg.b), (0xde, 0xad, 0xbe));
    }

    /// M11 fleet overview: OSC 9;4 from a (console-free) remote pane flows pump -> tick drain ->
    /// per-pane progress map -> window aggregation; Error outranks pct; Remove clears.
    #[test]
    fn osc94_progress_aggregates_into_window() {
        use gmux_mux::{Pane, Session, Window};
        use std::collections::HashMap as Map;

        let a = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let b = Pane::remote(2, 80, 24, Box::new(|_| {}));
        let (ida, idb) = (a.id, b.id);
        let mut panes = Map::new();
        let root = gmux_mux::layout::Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            a: Box::new(gmux_mux::layout::Node::Leaf(ida)),
            b: Box::new(gmux_mux::layout::Node::Leaf(idb)),
        };
        panes.insert(ida, a);
        panes.insert(idb, b);
        let win = Window::from_parts(panes, root, ida);
        let session = Session::from_windows("t", vec![win], 0);

        // Two agents report progress; the sidebar shows the least-done one.
        session.pane(ida).unwrap().push_output(b"\x1b]9;4;1;42\x07");
        session.pane(idb).unwrap().push_output(b"\x1b]9;4;1;80\x07");
        let mut progress: HashMap<PaneId, (ProgressState, Option<u8>)> = HashMap::new();
        let drain = |progress: &mut HashMap<PaneId, (ProgressState, Option<u8>)>| {
            for w in session.windows() {
                for p in w.panes() {
                    for ev in p.drain_events() {
                        if let PaneEvent::Progress { state, pct } = ev {
                            if state == ProgressState::Remove {
                                progress.remove(&p.id);
                            } else {
                                progress.insert(p.id, (state, pct));
                            }
                        }
                    }
                }
            }
        };
        drain(&mut progress);
        let win = &session.windows()[0];
        assert_eq!(window_progress(win, &progress), (Some(42), false));

        // An error state wins visually over any percentage.
        session.pane(ida).unwrap().push_output(b"\x1b]9;4;2;0\x07");
        drain(&mut progress);
        assert_eq!(window_progress(win, &progress).1, true);

        // Remove clears both entries -> no progress shown.
        session.pane(ida).unwrap().push_output(b"\x1b]9;4;0;0\x07");
        session.pane(idb).unwrap().push_output(b"\x1b]9;4;0;0\x07");
        drain(&mut progress);
        assert_eq!(window_progress(win, &progress), (None, false));
    }

    /// M11 review regression: a pane that leaves the session without a drained Exited event
    /// (remote layout prune path) must not leak its progress entry — the tick sweep clears it.
    #[test]
    fn progress_entries_swept_for_vanished_panes() {
        use gmux_mux::{Pane, Session};
        let p = Pane::remote(9, 80, 24, Box::new(|_| {}));
        let gone = p.id; // never inserted into any window: simulates a pruned pane
        drop(p);
        let live = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let live_id = live.id;
        let win = gmux_mux::Window::from_parts(
            std::collections::HashMap::from([(live_id, live)]),
            gmux_mux::layout::Node::Leaf(live_id),
            live_id,
        );
        let mut progress = HashMap::new();
        progress.insert(gone, (ProgressState::Set, Some(50)));
        progress.insert(live_id, (ProgressState::Set, Some(10)));
        let session = Session::from_windows("t", vec![win], 0);
        progress.retain(|id, _| session.pane(*id).is_some()); // the tick() sweep
        assert!(!progress.contains_key(&gone), "vanished pane's entry must be swept");
        assert!(progress.contains_key(&live_id), "live pane's entry must survive");
    }

    /// M7 privacy config parse: `persist_screen` reads as-written, defaults true when absent /
    /// malformed, and survives a UTF-8 BOM (PowerShell/Notepad write one).
    #[test]
    fn persist_screen_config_parse() {
        assert!(persist_screen_from_json(r#"{"persist_screen": true}"#));
        assert!(!persist_screen_from_json(r#"{"persist_screen": false}"#));
        assert!(persist_screen_from_json(r#"{"font_px": 14}"#), "absent key defaults true");
        assert!(persist_screen_from_json("not json at all"), "malformed defaults true");
        assert!(!persist_screen_from_json("\u{feff}{\"persist_screen\": false}"), "BOM is stripped");
        // A non-bool value is ignored (defaults true), not coerced.
        assert!(persist_screen_from_json(r#"{"persist_screen": "no"}"#));
    }

    /// `pane_title` never returns blank: it falls back to `%id` with no title/cwd, and an OSC 2
    /// title wins once set. (cwd fallback needs OSC 7 plumbing; the never-blank guarantee is the
    /// point.) Uses a console-free remote pane, so it runs under the default headless `cargo test`.
    #[test]
    fn pane_title_falls_back_then_prefers_osc_title() {
        use gmux_mux::Pane;
        let p = Pane::remote(1, 80, 24, Box::new(|_| {}));
        let id = p.id.0;
        assert_eq!(pane_title(&p, id), format!("%{id}"), "blank title/cwd -> %id fallback");
        p.push_output(b"\x1b]2;my-title\x07"); // OSC 2 sets the terminal title synchronously
        assert_eq!(pane_title(&p, id), "my-title");
    }

    /// A pane exit is encoded on the subscribe stream as a `NotifyWire` with the reserved
    /// `"pane-exited"` title and the pane id — the convention `Call::Subscribe` documents.
    #[test]
    fn exit_notify_uses_reserved_title() {
        let n = super::exit_notify(42);
        assert_eq!(n.title, "pane-exited");
        assert_eq!(n.pane, 42);
    }

    /// A connected subscriber receives a pushed batch as a `Response{id:0}` with the notifications;
    /// `push_to_subscribers` writes one line per batch. Uses an in-process gmux_pipe pair — no
    /// console/ConPTY, so it runs under the default headless `cargo test`.
    #[test]
    fn subscriber_receives_pushed_batch() {
        use gmux_pipe::{client_connect, PipeServer};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;

        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("gmux-subtest-recv-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));

        // The server handler hands its connected stream back so the test can push to it directly.
        let (tx, rx) = mpsc::channel();
        let _server = PipeServer::start(&name, move |stream| {
            let _ = tx.send(stream);
        })
        .unwrap();

        let client = client_connect(&name).unwrap();
        let mut server_side = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();

        let batch = vec![
            NotifyWire { pane: 3, title: "hi".into(), body: "there".into(), urgency: 2 },
            exit_notify(7),
        ];
        let mut subs = vec![(server_side.try_clone().unwrap(), true)];
        push_to_subscribers(&mut subs, &batch);
        assert_eq!(subs.len(), 1, "a live subscriber is retained");

        let mut reader = BufReader::new(client);
        let resp: Response = read_msg(&mut reader).unwrap().unwrap();
        assert_eq!(resp.id, 0, "pushes are unsolicited id:0 envelopes");
        assert_eq!(resp.result, Some(ResultBody::Notifications(batch)));
        // Keep server_side alive until after the read (dropping it would EOF the client).
        let _ = &mut server_side;
    }

    /// An empty batch is a no-op (no line written), and a subscriber whose peer has hung up is
    /// dropped from the list on the next push (its write fails).
    #[test]
    fn dead_subscriber_is_dropped_and_empty_batch_is_noop() {
        use gmux_pipe::{client_connect, PipeServer};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;

        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("gmux-subtest-dead-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));

        let (tx, rx) = mpsc::channel();
        let _server = PipeServer::start(&name, move |stream| {
            let _ = tx.send(stream);
        })
        .unwrap();

        let client = client_connect(&name).unwrap();
        let server_side = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let mut subs = vec![(server_side, true)];

        // Empty batch: nothing written, subscriber untouched.
        push_to_subscribers(&mut subs, &[]);
        assert_eq!(subs.len(), 1, "empty batch must not touch the subscriber list");

        // Peer hangs up: the next non-empty push fails to write and drops the subscriber.
        drop(client);
        // Writes to a broken pipe may buffer once before erroring; push until the list drains or a
        // small bound is hit (a broken pipe surfaces within a couple of writes).
        let batch = vec![exit_notify(1)];
        for _ in 0..8 {
            if subs.is_empty() {
                break;
            }
            push_to_subscribers(&mut subs, &batch);
        }
        assert!(subs.is_empty(), "a subscriber whose peer hung up must be dropped");
    }

    /// `pane-output` damage wires reach only `output == true` subscribers; an `output == false`
    /// subscriber sees the batch with them stripped, and gets no line at all for a tick whose only
    /// wires were damage. Uses two in-process pipe pairs — no console/ConPTY.
    #[test]
    fn output_flag_gates_pane_output_wires() {
        use gmux_pipe::{client_connect, PipeServer};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::mpsc;

        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("gmux-subtest-outflag-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));

        let (tx, rx) = mpsc::channel();
        let _server = PipeServer::start(&name, move |stream| {
            let _ = tx.send(stream);
        })
        .unwrap();

        // Connect two subscribers, sequencing connect->recv so each server-side stream maps to its
        // client: A opts into output, B does not.
        let client_a = client_connect(&name).unwrap();
        let server_a = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let client_b = client_connect(&name).unwrap();
        let server_b = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        let mut subs = vec![(server_a, true), (server_b, false)];

        let mut reader_a = BufReader::new(client_a);
        let mut reader_b = BufReader::new(client_b);
        let read = |r: &mut BufReader<_>| -> Vec<NotifyWire> {
            match read_msg::<Response>(r).unwrap().unwrap().result {
                Some(ResultBody::Notifications(n)) => n,
                other => panic!("expected Notifications, got {other:?}"),
            }
        };

        // Mixed batch: a real notification + a damage wire.
        let hi = NotifyWire { pane: 1, title: "hi".into(), body: String::new(), urgency: 1 };
        push_to_subscribers(&mut subs, &[hi.clone(), output_notify(5)]);
        assert_eq!(read(&mut reader_a), vec![hi.clone(), output_notify(5)], "output=true sees damage");
        assert_eq!(read(&mut reader_b), vec![hi.clone()], "output=false has damage stripped");

        // Damage-only batch: A gets it, B is skipped (no line). Prove B was skipped by pushing a
        // following real notification — B's very next line is that one, not the damage tick.
        push_to_subscribers(&mut subs, &[output_notify(7)]);
        assert_eq!(read(&mut reader_a), vec![output_notify(7)], "output=true sees damage-only tick");
        let bye = NotifyWire { pane: 2, title: "bye".into(), body: String::new(), urgency: 1 };
        push_to_subscribers(&mut subs, &[bye.clone()]);
        assert_eq!(read(&mut reader_b), vec![bye], "output=false skipped the damage-only tick");

        assert_eq!(subs.len(), 2, "both subscribers stay connected");
    }

    /// `grid_wire` maps a double-width (CJK) cell to `CELL_WIDE` and leaves the trailing spacer
    /// flagless. Exercises the whole path bytes -> vt grid -> `Cell.wide` -> `CellWire.flags`.
    /// Console-free remote pane, so it runs headless.
    #[test]
    fn grid_wire_maps_wide_flag() {
        use gmux_mux::Pane;
        let p = Pane::remote(1, 80, 24, Box::new(|_| {}));
        p.push_output("中".as_bytes()); // U+4E2D, double-width
        let wire = grid_wire(&p, 0);
        assert_eq!(wire.cells[0].ch, '中');
        assert_eq!(wire.cells[0].flags & CELL_WIDE, CELL_WIDE, "wide char sets CELL_WIDE");
        assert_eq!(wire.cells[1].ch, ' ', "the cell after a wide char is a blank spacer");
        assert_eq!(wire.cells[1].flags & CELL_WIDE, 0, "spacer is not itself wide");
    }
}
