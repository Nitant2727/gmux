//! A [`Pane`] = one [`Terminal`] fed by a [`Backend`]: either a local [`Pty`] with a background
//! pump that feeds ConPTY output into the terminal, or a remote tmux pane whose transport pushes
//! `%output` bytes in via [`Pane::push_output`]. Both paths funnel through the same
//! [`TermEvent`]-to-[`PaneEvent`] mapping, so OSC 9/777/99 from remote agents raise attention and
//! toasts exactly like local ones.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use gmux_pty::{Pty, PtySize};
use gmux_vt::{Cell, Notification, Palette, ProgressState, TermEvent, Terminal};

use crate::attention::Attention;
use crate::ids::PaneId;

/// A notable event observed on a pane (forwarded from the pump thread).
#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// The grid changed; re-render.
    Output,
    /// An agent notification (OSC 9/777/99).
    Notification(Notification),
    /// A bell (BEL) while the pane was not focused.
    Bell,
    /// Title changed (OSC 0/2).
    Title(String),
    /// Working directory changed (OSC 7 / 9;9).
    Cwd(String),
    /// Progress update (OSC 9;4).
    Progress { state: ProgressState, pct: Option<u8> },
    /// The child process exited (PTY reached EOF / the transport reported the remote pane gone).
    Exited,
}

/// An immutable view of a pane's visible grid, for rendering.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    pub cells: Vec<Vec<Cell>>,
    pub cursor: (u16, u16),
    pub cols: u16,
    pub rows: u16,
}

/// What produces a pane's bytes and consumes its input.
enum Backend {
    /// A local ConPTY child; output arrives via the pump thread.
    Local { pty: Pty },
    /// A mirror of a remote tmux pane (`%remote_id`); the real PTY lives on the remote server.
    /// Output arrives via [`Pane::push_output`]; `input` forwards keystrokes to the transport
    /// (which wraps them in `send-keys`). Holds the events `Sender` so `push_output` /
    /// `mark_exited` reach the same channel the local pump would — for local panes the sender is
    /// owned solely by the pump thread, preserving channel-disconnect semantics on exit.
    Remote {
        input: Box<dyn Fn(&[u8]) + Send + Sync>,
        alive: Arc<AtomicBool>,
        remote_id: u64,
        tx: Sender<PaneEvent>,
    },
}

/// A live terminal pane.
pub struct Pane {
    pub id: PaneId,
    backend: Backend,
    terminal: Arc<Mutex<Terminal>>,
    events: Receiver<PaneEvent>,
    attention: Arc<Mutex<Attention>>,
    title: Arc<Mutex<String>>,
    cwd: Arc<Mutex<Option<String>>>,
    _pump: Option<JoinHandle<()>>,
}

/// Advance `terminal` with `bytes` and forward the resulting [`TermEvent`]s as [`PaneEvent`]s:
/// attention on Notification/Bell, title/cwd tracking, and a single `Output` per damaged chunk.
/// The single funnel for pane output — the local pump thread and [`Pane::push_output`] both call
/// this, so remote `%output` hits the same OSC parser + attention path as local PTY output.
fn pump_bytes(
    terminal: &Mutex<Terminal>,
    attention: &Mutex<Attention>,
    title: &Mutex<String>,
    cwd: &Mutex<Option<String>>,
    tx: &Sender<PaneEvent>,
    bytes: &[u8],
) {
    let evs = terminal.lock().unwrap().advance(bytes);
    let mut damaged = false;
    for ev in evs {
        match ev {
            TermEvent::Damage => damaged = true,
            TermEvent::Notification(n) => {
                attention.lock().unwrap().set_pending();
                let _ = tx.send(PaneEvent::Notification(n));
            }
            TermEvent::Bell => {
                attention.lock().unwrap().set_pending();
                let _ = tx.send(PaneEvent::Bell);
            }
            TermEvent::Title(s) => {
                *title.lock().unwrap() = s.clone();
                let _ = tx.send(PaneEvent::Title(s));
            }
            TermEvent::Cwd(p) => {
                *cwd.lock().unwrap() = Some(p.clone());
                let _ = tx.send(PaneEvent::Cwd(p));
            }
            TermEvent::Progress { state, pct } => {
                let _ = tx.send(PaneEvent::Progress { state, pct });
            }
            TermEvent::PromptMark(_) => {}
        }
    }
    if damaged {
        let _ = tx.send(PaneEvent::Output);
    }
}

impl Pane {
    /// Spawn `command_line` (a shell or program) in a new pseudoconsole of `size` and start pumping
    /// its output through a fresh terminal.
    pub fn spawn(command_line: &str, size: PtySize) -> io::Result<Pane> {
        Self::spawn_in(command_line, size, None, None)
    }

    /// Spawn `command_line` in working directory `cwd`, optionally pre-seeding the terminal with
    /// `replay` bytes (inert restored history) before the child's output starts. Used by session
    /// restore to reopen a shell in its saved directory beneath its previous screen contents.
    pub fn spawn_in(
        command_line: &str,
        size: PtySize,
        cwd: Option<&str>,
        replay: Option<&str>,
    ) -> io::Result<Pane> {
        let id = PaneId::alloc();
        // Inject self-addressing env so agent hooks and `gmux notify --pane` can target this pane,
        // and advertise gmux to terminal-aware tools.
        let env = vec![
            ("GMUX_PANE".to_string(), id.to_string()),
            ("TERM_PROGRAM".to_string(), "gmux".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ];
        let (pty, rx) = Pty::spawn_full(command_line, size, &env, cwd)?;
        let terminal = Arc::new(Mutex::new(Terminal::new(size.cols, size.rows)));
        // Replay saved history into the fresh terminal before the child's output arrives.
        if let Some(r) = replay {
            terminal.lock().unwrap().advance(r.as_bytes());
        }
        let attention = Arc::new(Mutex::new(Attention::default()));
        let title = Arc::new(Mutex::new(String::new()));
        let cwd = Arc::new(Mutex::new(None));

        let (tx, events) = channel::<PaneEvent>();
        let (t, a, ti, cw) = (terminal.clone(), attention.clone(), title.clone(), cwd.clone());
        let pump = thread::spawn(move || {
            while let Ok(chunk) = rx.recv() {
                pump_bytes(&t, &a, &ti, &cw, &tx, &chunk);
            }
            let _ = tx.send(PaneEvent::Exited);
        });

        Ok(Pane {
            id,
            backend: Backend::Local { pty },
            terminal,
            events,
            attention,
            title,
            cwd,
            _pump: Some(pump),
        })
    }

    /// Create a pane mirroring remote tmux pane `%remote_id`. No process and no pump thread: the
    /// transport pushes `%output` bytes in via [`Pane::push_output`] and keystrokes flow out
    /// through `input` (which the transport wraps in `send-keys`). The remote server owns the
    /// real PTY; this side owns only the terminal grid + attention state.
    pub fn remote(
        remote_id: u64,
        cols: u16,
        rows: u16,
        input: Box<dyn Fn(&[u8]) + Send + Sync>,
    ) -> Pane {
        let (tx, events) = channel::<PaneEvent>();
        Pane {
            id: PaneId::alloc(),
            backend: Backend::Remote {
                input,
                alive: Arc::new(AtomicBool::new(true)),
                remote_id,
                tx,
            },
            terminal: Arc::new(Mutex::new(Terminal::new(cols, rows))),
            events,
            attention: Arc::new(Mutex::new(Attention::default())),
            title: Arc::new(Mutex::new(String::new())),
            cwd: Arc::new(Mutex::new(None)),
            _pump: None,
        }
    }

    /// Feed remote output through the terminal — the exact path the local pump takes
    /// ([`pump_bytes`]), so OSC 9/777/99 from remote agents raise attention and emit the same
    /// events. Events land on the pane's channel; drain with [`Pane::drain_events`]. No-op on
    /// local panes (their PTY pump owns the terminal feed).
    pub fn push_output(&self, bytes: &[u8]) {
        if let Backend::Remote { tx, .. } = &self.backend {
            pump_bytes(&self.terminal, &self.attention, &self.title, &self.cwd, tx, bytes);
        }
    }

    /// The remote tmux pane id (the `N` of `%N`) backing this pane; `None` for local panes.
    pub fn remote_id(&self) -> Option<u64> {
        match &self.backend {
            Backend::Local { .. } => None,
            Backend::Remote { remote_id, .. } => Some(*remote_id),
        }
    }

    /// Mark a remote pane dead (the transport saw the tmux pane exit or the link drop): flips
    /// liveness and emits [`PaneEvent::Exited`] once. No-op on local panes — their pump emits
    /// `Exited` at PTY EOF.
    pub fn mark_exited(&self) {
        if let Backend::Remote { alive, tx, .. } = &self.backend {
            if alive.swap(false, Ordering::Relaxed) {
                let _ = tx.send(PaneEvent::Exited);
            }
        }
    }

    /// Write raw input (keystrokes / VT) to the child — directly for local panes, via the
    /// transport's input closure for remote ones.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        match &self.backend {
            Backend::Local { pty } => pty.write(data),
            Backend::Remote { input, .. } => {
                input(data);
                Ok(())
            }
        }
    }

    /// Resize the pseudoconsole and the terminal grid. No-op when the size is unchanged, so
    /// periodic geometry heartbeats don't spam `ResizePseudoConsole`. Remote panes resize the
    /// grid only — pushing the new size to the remote tmux is the transport's job.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        {
            let mut term = self.terminal.lock().unwrap();
            if term.cols() == size.cols && term.rows() == size.rows {
                return Ok(());
            }
            term.resize(size.cols, size.rows);
        }
        match &self.backend {
            Backend::Local { pty } => pty.resize(size),
            Backend::Remote { .. } => Ok(()),
        }
    }

    /// Snapshot the visible grid for rendering.
    pub fn snapshot(&self) -> PaneSnapshot {
        self.snapshot_at(0)
    }

    /// Snapshot the grid scrolled `offset` lines up into scrollback (0 = live screen; clamped to
    /// available history). Backs the GUI scrollback viewport via `GetGrid { offset }`.
    pub fn snapshot_at(&self, offset: usize) -> PaneSnapshot {
        self.snapshot_scrolled(offset).0
    }

    /// Like [`snapshot_at`], but also returns `(history, clamped_offset)` read under the **same**
    /// terminal lock, so the reported scroll position can't skew against the rendered cells when
    /// the pump thread mutates history concurrently.
    pub fn snapshot_scrolled(&self, offset: usize) -> (PaneSnapshot, usize, usize) {
        let term = self.terminal.lock().unwrap();
        let history = term.history_len();
        let offset = offset.min(history);
        let snap = PaneSnapshot {
            cells: term.cells_at_offset(offset),
            cursor: term.cursor(),
            cols: term.cols(),
            rows: term.rows(),
        };
        (snap, history, offset)
    }

    /// Scrollback + visible content as plain text lines (oldest first). `max_lines == 0` returns all
    /// retained history; otherwise the most-recent `max_lines`. Backs `capture-pane -S` and the
    /// snapshot screen capture used by session restore.
    pub fn scrollback_text(&self, max_lines: usize) -> Vec<String> {
        self.terminal.lock().unwrap().scrollback_text(max_lines)
    }

    /// Number of scrollback (history) lines currently retained above the viewport.
    pub fn history_len(&self) -> usize {
        self.terminal.lock().unwrap().history_len()
    }

    /// Re-theme this pane's terminal (fg/bg + the 16 system colors). Takes `&self` — the terminal
    /// is behind an `Arc<Mutex>`, so a shared pane ref suffices; the next snapshot reflects it.
    pub fn set_palette(&self, palette: Palette) {
        self.terminal.lock().unwrap().set_palette(palette);
    }

    /// Drain any pending pane events (non-blocking).
    pub fn drain_events(&self) -> Vec<PaneEvent> {
        self.events.try_iter().collect()
    }

    // NOTE: no blocking `recv_event` — a remote pane's backend holds a live event Sender for the
    // pane's lifetime, so a recv-until-disconnect loop would never terminate. Poll `drain_events`.

    pub fn attention(&self) -> Attention {
        *self.attention.lock().unwrap()
    }

    /// Focus the pane: clears attention.
    pub fn focus(&self) {
        self.attention.lock().unwrap().focus();
    }

    /// Externally request attention on this pane (e.g. the `notify` API method — equivalent to the
    /// pane emitting a notification itself).
    pub fn request_attention(&self) {
        self.attention.lock().unwrap().set_pending();
    }

    pub fn title(&self) -> String {
        self.title.lock().unwrap().clone()
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.lock().unwrap().clone()
    }

    pub fn is_alive(&self) -> bool {
        match &self.backend {
            Backend::Local { pty } => pty.is_alive(),
            Backend::Remote { alive, .. } => alive.load(Ordering::Relaxed),
        }
    }
}

// On drop, fields drop in order: `backend` (holding the `Pty`) before `_pump`, so for local panes
// ClosePseudoConsole -> reader EOF -> the pump's input channel closes -> the pump loop ends, then
// the `pump` JoinHandle drops and detaches the finished thread. Joining here would deadlock, since
// the `Pty` is still open while this runs. Remote panes have no pump; dropping the backend drops
// the input closure and the events sender.
