//! A [`Pane`] = one [`Pty`] + one [`Terminal`] + a background pump that feeds PTY output into the
//! terminal, updates attention state, and forwards notable events over a channel.

use std::io;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use gmux_pty::{Pty, PtySize};
use gmux_vt::{Cell, Notification, ProgressState, TermEvent, Terminal};

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
    /// The child process exited (PTY reached EOF).
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

/// A live terminal pane.
pub struct Pane {
    pub id: PaneId,
    pty: Pty,
    terminal: Arc<Mutex<Terminal>>,
    events: Receiver<PaneEvent>,
    attention: Arc<Mutex<Attention>>,
    title: Arc<Mutex<String>>,
    cwd: Arc<Mutex<Option<String>>>,
    _pump: Option<JoinHandle<()>>,
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
                let evs = {
                    let mut term = t.lock().unwrap();
                    term.advance(&chunk)
                };
                let mut damaged = false;
                for ev in evs {
                    match ev {
                        TermEvent::Damage => damaged = true,
                        TermEvent::Notification(n) => {
                            a.lock().unwrap().set_pending();
                            let _ = tx.send(PaneEvent::Notification(n));
                        }
                        TermEvent::Bell => {
                            a.lock().unwrap().set_pending();
                            let _ = tx.send(PaneEvent::Bell);
                        }
                        TermEvent::Title(s) => {
                            *ti.lock().unwrap() = s.clone();
                            let _ = tx.send(PaneEvent::Title(s));
                        }
                        TermEvent::Cwd(p) => {
                            *cw.lock().unwrap() = Some(p.clone());
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
            let _ = tx.send(PaneEvent::Exited);
        });

        Ok(Pane {
            id,
            pty,
            terminal,
            events,
            attention,
            title,
            cwd,
            _pump: Some(pump),
        })
    }

    /// Write raw input (keystrokes / VT) to the child.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        self.pty.write(data)
    }

    /// Resize both the pseudoconsole and the terminal grid. No-op when the size is unchanged, so
    /// periodic geometry heartbeats don't spam `ResizePseudoConsole`.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        {
            let mut term = self.terminal.lock().unwrap();
            if term.cols() == size.cols && term.rows() == size.rows {
                return Ok(());
            }
            term.resize(size.cols, size.rows);
        }
        self.pty.resize(size)
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

    /// Drain any pending pane events (non-blocking).
    pub fn drain_events(&self) -> Vec<PaneEvent> {
        self.events.try_iter().collect()
    }

    /// Block for the next pane event (used by tests / event loops).
    pub fn recv_event(&self) -> Option<PaneEvent> {
        self.events.recv().ok()
    }

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
        self.pty.is_alive()
    }
}

// On drop, fields drop in order: `pty` first (ClosePseudoConsole -> reader EOF -> the pump's input
// channel closes -> the pump loop ends), then the `pump` JoinHandle drops and detaches the finished
// thread. Joining here would deadlock, since `pty` is still open while this runs.

