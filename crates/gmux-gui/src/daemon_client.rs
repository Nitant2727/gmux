//! Blocking client to the gmux daemon over the named pipe. The thin-client GUI uses this to fetch
//! layout/grids for rendering and to send input/control. If no daemon is answering, it spawns one
//! (`gmux --daemon`) as a `DETACHED_PROCESS` that inherits no handles and starts with `SW_HIDE`, so
//! `ensure_console` allocates it a console whose host window never appears — the console itself is
//! required for its ConPTY panes to bind their stdio (the M0 console-binding finding; see
//! `spawn_daemon` for why the obvious alternatives are fatal).
//!
//! If the daemon dies mid-session (crash, upgrade, killed), [`DaemonClient::call`] reconnects
//! transparently — plain reconnect first (the daemon may have restarted on its own), respawn if
//! nothing is listening — and retries the request once before surfacing the error.

use std::collections::HashSet;
use std::io::{self, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gmux_pipe::PipeStream;
use gmux_proto::{read_msg, write_msg, Call, NotifyWire, Request, Response, ResultBody};
use winit::event_loop::EventLoopProxy;

pub struct DaemonClient {
    reader: BufReader<PipeStream>,
    writer: PipeStream,
    next_id: u64,
    /// Full pipe name (`<base>.<user>`), kept so a broken connection can be re-established.
    pipe_name: String,
}

impl DaemonClient {
    /// Connect to `\\.\pipe\<base>.<user>`, spawning `gmux --daemon` if nothing is listening yet.
    pub fn connect_or_spawn(pipe_base: &str) -> io::Result<DaemonClient> {
        let name = gmux_pipe::pipe_name_for_user(pipe_base);
        let stream = connect_or_spawn_stream(&name)?;
        let writer = stream.try_clone()?;
        Ok(DaemonClient { reader: BufReader::new(stream), writer, next_id: 1, pipe_name: name })
    }

    /// Send one request and await its response.
    ///
    /// On an I/O failure (daemon died / pipe broken) this reconnects — respawning the daemon if
    /// a plain reconnect fails — and retries the request once, but **only for idempotent calls**:
    /// a state-changing call (send-keys, split, close, …) may already have been applied before the
    /// connection broke, and re-sending it would double-apply (type twice, close two panes). For
    /// those the connection is still healed so the *next* call works, but this one returns `Err`.
    /// At most one reconnect (and thus one respawn) is attempted per call, so a persistently dead
    /// daemon errors out rather than looping. Daemon-reported errors (`Response::error`) never
    /// trigger a reconnect.
    pub fn call(&mut self, call: Call) -> Result<ResultBody, String> {
        let resp = match self.roundtrip(&call) {
            Ok(r) => r,
            Err(e) => {
                self.reconnect().map_err(|e| e.to_string())?;
                if !idempotent(&call) {
                    return Err(format!("connection to daemon lost mid-call (reconnected; not retrying a state-changing call): {e}"));
                }
                self.roundtrip(&call).map_err(|e| e.to_string())?
            }
        };
        if let Some(e) = resp.error {
            return Err(e);
        }
        resp.result.ok_or_else(|| "empty response".to_string())
    }

    /// One request/response exchange on the current connection. All I/O-level failures —
    /// including the daemon hanging up mid-read (EOF) — surface as `Err` so `call` can attempt
    /// recovery. The id counter only ever increments, keeping ids monotonic across reconnects.
    fn roundtrip(&mut self, call: &Call) -> io::Result<Response> {
        let id = self.next_id;
        self.next_id += 1;
        write_msg(&mut self.writer, &Request { id, call: call.clone() })?;
        match read_msg::<Response>(&mut self.reader)? {
            Some(r) => Ok(r),
            None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "daemon disconnected")),
        }
    }

    /// Replace the broken connection (discarding any half-read buffer): plain reconnect first —
    /// the daemon may have restarted on its own — falling back to the spawn-and-poll dance.
    fn reconnect(&mut self) -> io::Result<()> {
        let stream = connect_or_spawn_stream(&self.pipe_name)?;
        self.writer = stream.try_clone()?;
        self.reader = BufReader::new(stream);
        Ok(())
    }

    /// Fire-and-forget a control call, ignoring the (Done) result.
    pub fn control(&mut self, call: Call) {
        let _ = self.call(call);
    }
}

/// Spawn a background thread that opens its OWN pipe connection, registers as an output-carrying
/// push subscriber (`Subscribe { output: true }`), and streams event batches forever. Per batch it:
/// forwards real notifications to `toasts` (for the main thread to render), records the ids of
/// panes that emitted output (`title == "pane-output"`) into `damaged`, and wakes the winit loop
/// via `proxy`. The returned flag flips to `false` when the thread exits (pipe EOF / error), so the
/// caller can respawn it after a daemon reconnect. ponytail: no join handle — a dead thread is
/// detected by the flag and replaced; the OS reaps it.
pub fn spawn_output_subscriber(
    proxy: EventLoopProxy<()>,
    damaged: Arc<Mutex<HashSet<u64>>>,
    toasts: Sender<NotifyWire>,
) -> Arc<AtomicBool> {
    let alive = Arc::new(AtomicBool::new(true));
    let flag = alive.clone();
    std::thread::spawn(move || {
        run_subscriber(&proxy, &damaged, &toasts);
        flag.store(false, Ordering::Relaxed); // EOF/error: signal the main loop to respawn us
    });
    alive
}

/// The subscriber read loop; returns on pipe EOF or any protocol/I/O error.
fn run_subscriber(proxy: &EventLoopProxy<()>, damaged: &Mutex<HashSet<u64>>, toasts: &Sender<NotifyWire>) {
    let name = gmux_pipe::pipe_name_for_user("gmux");
    let Ok(stream) = gmux_pipe::client_connect(&name) else { return };
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    if write_msg(&mut writer, &Request { id: 1, call: Call::Subscribe { output: true } }).is_err() {
        return;
    }
    loop {
        let resp = match read_msg::<Response>(&mut reader) {
            Ok(Some(r)) => r,
            _ => return, // clean EOF or error: die; the caller respawns us
        };
        if let Some(ResultBody::Notifications(notes)) = resp.result {
            let mut hits: Vec<u64> = Vec::new();
            for n in notes {
                if n.title == "pane-output" {
                    hits.push(n.pane); // a damage wire: this pane produced output
                } else {
                    let _ = toasts.send(n); // a real notification: toast it on the main thread
                }
            }
            if !hits.is_empty() {
                if let Ok(mut d) = damaged.lock() {
                    d.extend(hits);
                }
            }
        }
        // Wake the main loop for every batch (damage or notification) so it re-renders / toasts.
        if proxy.send_event(()).is_err() {
            return; // the event loop is gone
        }
    }
}

/// Whether re-sending `call` after an ambiguous failure is safe. Read-only queries and
/// absolute-geometry reports are; anything that types, creates, closes, toggles, or moves focus
/// is not — the daemon may have applied the first send before the connection broke.
fn idempotent(call: &Call) -> bool {
    matches!(
        call,
        Call::Hello { .. }
            | Call::ListPanes
            | Call::CapturePane { .. }
            | Call::GetLayout { .. }
            | Call::GetGrid { .. }
            | Call::ResizeView { .. }
            | Call::PollNotifications
            | Call::SetPalette { .. }
            | Call::SearchPane { .. }
            | Call::PromptOffsets { .. }
    )
}

/// Connect to `\\.\pipe\<name>`; if nothing is listening, spawn the daemon and poll until it is.
fn connect_or_spawn_stream(name: &str) -> io::Result<PipeStream> {
    if let Ok(stream) = gmux_pipe::client_connect(name) {
        return Ok(stream);
    }
    spawn_daemon();
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(150));
        if let Ok(stream) = gmux_pipe::client_connect(name) {
            return Ok(stream);
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "could not reach or start the gmux daemon"))
}

/// `SW_HIDE` in the daemon's `STARTUPINFO`. Windows passes the creating process's show-window
/// setting on to the console host it launches for that process — so the console `ensure_console`
/// allocates comes up hidden instead of as a second visible terminal.
///
/// This is the part that actually fixes it. `ShowWindow(GetConsoleWindow(), SW_HIDE)` inside the
/// daemon does not: on Windows 11, where Windows Terminal is the default console host, the console
/// window belongs to *Terminal*, and `GetConsoleWindow` hands back this process's legacy
/// pseudo-window — hiding which is invisible to the user, so every launch popped a terminal
/// titled `gmux.exe` alongside the app.
/// Start `gmux --daemon` detached, with a hidden console.
///
/// Uses `CreateProcessW` rather than `std::process::Command` for one reason: `STARTUPINFOW` is the
/// only way to set the show-window flag, and `Command::show_window` is still unstable. Handles are
/// deliberately NOT inherited — a GUI launched with redirected or broken stdio would otherwise pass
/// those handles to the daemon, whose ConPTY panes then die instantly (daemon exits "all panes
/// exited" -> GUI reconnect churn -> fatal exit).
///
/// `DETACHED_PROCESS` stays: the daemon starts console-less so `gmux_pty::ensure_console` takes its
/// `AllocConsole` path, which is the configuration the ConPTY binding was designed around.
/// `CREATE_NO_WINDOW` looks like the obvious way to suppress the window and is **fatal** — retried
/// in round 67 on the theory that `ensure_console`'s later handle-repair had made it safe, and the
/// daemon still died as its first pane spawned, respawning in a ~2s loop. What was missing was not
/// a different creation flag but the show-window field below.
fn spawn_daemon() {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let Ok(exe) = std::env::current_exe() else { return };
    // CreateProcessW may write into the command line buffer, so it must be owned and mutable.
    let mut cmdline: Vec<u16> =
        format!("\"{}\" --daemon", exe.display()).encode_utf16().chain(std::iter::once(0)).collect();

    let mut si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    si.dwFlags = STARTF_USESHOWWINDOW;
    si.wShowWindow = SW_HIDE.0 as u16;
    let mut pi = PROCESS_INFORMATION::default();

    let ok = unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false, // no handle inheritance
            DETACHED_PROCESS,
            None,
            None,
            &si,
            &mut pi,
        )
    };
    if ok.is_ok() {
        // We never wait on the daemon; drop our references so it isn't held open by this process.
        unsafe {
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
        }
    } else {
        eprintln!("gmux: could not start the daemon: {:?}", ok);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The daemon dies after every response; `call` must reconnect and retry transparently.
    ///
    /// An in-process `PipeServer` stands in for the daemon: each connection's handler answers
    /// exactly one request and then drops the stream (= daemon death). The accept loop keeps a
    /// fresh instance listening, so recovery takes the plain-reconnect path — no daemon (and no
    /// respawned test binary) is involved.
    #[test]
    fn call_reconnects_after_daemon_death_with_monotonic_ids() {
        let base = format!("gmux-gui-test-reconnect-{}", std::process::id());
        let name = gmux_pipe::pipe_name_for_user(&base);
        let seen_ids = Arc::new(Mutex::new(Vec::<u64>::new()));
        let ids = Arc::clone(&seen_ids);
        let _server = gmux_pipe::PipeServer::start(&name, move |stream| {
            let mut reader = BufReader::new(stream);
            if let Ok(Some(req)) = read_msg::<Request>(&mut reader) {
                ids.lock().unwrap().push(req.id);
                let mut writer = reader.into_inner();
                let _ = write_msg(&mut writer, &Response::ok(req.id, ResultBody::Done));
            }
            // Returning drops the stream: the "daemon" dies after one request.
        })
        .unwrap();

        let mut client = DaemonClient::connect_or_spawn(&base).unwrap();
        assert_eq!(client.call(Call::ListPanes), Ok(ResultBody::Done));
        // The server hung up after the first response; this call's first attempt fails at the
        // I/O layer and must be transparently retried on a new connection.
        assert_eq!(client.call(Call::ListPanes), Ok(ResultBody::Done));

        // The server saw the first call (id 1) and the retry (id 3); the failed attempt (id 2)
        // burned an id but never reached a handler. Ids stay monotonic across the reconnect.
        assert_eq!(*seen_ids.lock().unwrap(), vec![1, 3]);

        // A NON-idempotent call must not be retried: the first attempt fails on the dead
        // connection, the client reconnects (healing the pipe) but returns Err instead of
        // re-sending — the daemon might already have applied it.
        let sk = client.call(Call::SendKeys { pane: 0, text: "x".into(), enter: false });
        assert!(sk.is_err(), "state-changing call must error, not silently retry: {sk:?}");
        // ...and the healed connection serves the next idempotent call normally.
        assert_eq!(client.call(Call::ListPanes), Ok(ResultBody::Done));
        let ids = seen_ids.lock().unwrap().clone();
        assert!(!ids.contains(&4), "the SendKeys attempt (id 4) must never be re-sent: {ids:?}");
        assert_eq!(*ids.last().unwrap(), 5, "healed connection must serve the follow-up call");
    }
}
