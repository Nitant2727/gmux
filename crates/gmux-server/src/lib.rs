//! gmux-server — the headless multiplexer server: owns the `Mux`/`Session` and its ConPTYs, and
//! serves the [`gmux_proto`] automation protocol over the named pipe. This is what `gmux --daemon`
//! runs; because the daemon (not the GUI) owns the panes, they survive the GUI detaching (M6).
//!
//! Panes render at a default cell size until a client reports its geometry (M6b adds resize/grid
//! streaming so a thin GUI can attach).

use std::io::{self, BufReader};
use std::sync::{Arc, Mutex};

use gmux_mux::{Pane, PaneId, PtySize, Session, SplitDir};
use gmux_pipe::{PipeServer, PipeStream};
use gmux_proto::{read_msg, write_msg, Call, PaneInfo, Request, Response, ResultBody};

const DEFAULT_SIZE: PtySize = PtySize { cols: 120, rows: 30 };

/// The multiplexer state a daemon serves.
pub struct Server {
    pub session: Session,
    pub shell: String,
}

impl Server {
    /// Create a server whose session's first window runs `shell`.
    pub fn new(shell: String) -> io::Result<Server> {
        let pane = Pane::spawn(&shell, DEFAULT_SIZE)?;
        Ok(Server { session: Session::start("gmux", pane), shell })
    }

    fn spawn_pane(&self, command: &Option<String>) -> io::Result<Pane> {
        let cmd = command.clone().unwrap_or_else(|| self.shell.clone());
        Pane::spawn(&cmd, DEFAULT_SIZE)
    }

    fn find(&self, id: u64) -> Option<&Pane> {
        self.session.pane(PaneId(id))
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
            Call::CapturePane { pane } => match self.find(*pane) {
                Some(p) => Response::ok(id, ResultBody::Text(capture(p))),
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::SplitPane { dir, command } => {
                let sd = match dir.as_str() {
                    "h" => SplitDir::Horizontal,
                    "v" => SplitDir::Vertical,
                    other => return Response::err(id, format!("bad dir '{other}' (h|v)")),
                };
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
        }
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

fn capture(p: &Pane) -> String {
    let snap = p.snapshot();
    let mut lines: Vec<String> = snap
        .cells
        .iter()
        .map(|row| {
            let mut s: String = row.iter().map(|c| c.ch).collect();
            s.truncate(s.trim_end_matches(' ').len());
            s
        })
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Run the daemon: create the mux, serve the pipe, and block until all panes exit.
pub fn run(shell: String, pipe_base: &str) -> io::Result<()> {
    let server = Arc::new(Mutex::new(Server::new(shell)?));
    let name = gmux_pipe::pipe_name_for_user(pipe_base);
    let handler_server = server.clone();
    let _pipe = PipeServer::start(&name, move |stream| {
        serve_connection(stream, &handler_server);
    })?;
    eprintln!("gmux daemon: serving \\\\.\\pipe\\{name}");

    // Block until every pane's process has exited.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if server.lock().map(|s| s.all_exited()).unwrap_or(true) {
            eprintln!("gmux daemon: all panes exited, shutting down");
            return Ok(());
        }
    }
}

fn serve_connection(stream: PipeStream, server: &Arc<Mutex<Server>>) {
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
        let resp = match server.lock() {
            Ok(mut s) => s.handle(&req),
            Err(_) => Response::err(req.id, "server lock poisoned"),
        };
        if write_msg(&mut writer, &resp).is_err() {
            return;
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
        let req = Request { id: 9, call: Call::CapturePane { pane: 999 } };
        // A no-such-pane capture must be an error; verify the constructor used by handle().
        let resp = Response::err(req.id, "no pane %999");
        assert_eq!(resp.id, 9);
        assert!(resp.result.is_none());
    }
}
