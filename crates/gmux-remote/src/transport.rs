//! The tmux control-mode transport: a child process (production: `ssh -tt host -- tmux -CC
//! new -As gmux`; tests: any stub that speaks the same bytes) with piped stdio, a reader
//! thread that strips the `-CC` DCS wrapper and feeds [`gmux_tmux::Parser`], and an ordered
//! command pipe on stdin.
//!
//! Reply correlation is positional: control mode answers commands strictly in send order, so
//! the Nth [`gmux_tmux::Event::Reply`] drained answers the Nth [`RemoteTmux::send_command`].
//! (The `num` inside a `Reply` is tmux's own command counter — it does NOT equal the local
//! sequence number returned by `send_command`.) The one exception is the **attach greeting**:
//! on attach, control mode emits an unsolicited `%begin`/`%end` block (the reply to the
//! command on the client's own command line, e.g. `new -As gmux`) before the client sends
//! anything. The reader surfaces that first block as [`TransportEvent::Greeting`], so it never
//! shifts the positional correlation.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use gmux_tmux::{Event, Notification, Parser};

/// One event drained from the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// The unsolicited attach-greeting reply block (the first `Reply` on the stream). Not
    /// counted against [`RemoteTmux::send_command`]'s positional correlation.
    Greeting(Event),
    /// A parsed control-mode event (notification or reply).
    Ctrl(Event),
    /// The child's stdout closed (ssh died, remote tmux exited, or [`RemoteTmux::kill`]).
    /// Emitted exactly once, after every `Ctrl` event.
    Eof,
}

/// Don't create a console window for the child (`CREATE_NO_WINDOW`); ssh runs headless under
/// the gmux GUI.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A live control-mode connection. Dropping it kills the child's process tree and joins the
/// reader threads.
pub struct RemoteTmux {
    child: Child,
    /// Job object holding the child's whole tree (`KILL_ON_JOB_CLOSE`), so [`RemoteTmux::kill`]
    /// takes down grandchildren too — `cmd /c ssh …` leaves the pipe write end with the
    /// grandchild, and killing only the direct child would leave the reader blocked in `read`
    /// forever (the same teardown deadlock gmux-pty fixed for ConPTY).
    job: isize,
    /// `None` once closed — closing stdin is the polite way to end an ssh session.
    stdin: Option<ChildStdin>,
    queue: Arc<Mutex<VecDeque<TransportEvent>>>,
    /// Reply events observed by the reader thread (greeting excluded); `pending_len` =
    /// sent − seen.
    replies_seen: Arc<AtomicU64>,
    /// Set by the reader thread when stdout closes (after it queued `Eof`).
    eof: Arc<AtomicBool>,
    /// Set when `%exit` was seen — the remote ended control mode deliberately.
    detached: Arc<AtomicBool>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    /// Local send counter; the value *before* increment is each command's sequence number.
    sent: u64,
}

impl RemoteTmux {
    /// Launch `command_line` with piped stdio and start the reader threads.
    ///
    /// The first whitespace-delimited token is the program; the remainder is passed verbatim
    /// as the raw command-line tail ([`CommandExt::raw_arg`]). This mirrors how gmux-pty's
    /// `Pty::spawn` handles command lines — the whole line goes to `CreateProcessW`
    /// unparsed — so quoting reaches the child untouched (`ssh -tt host -- tmux -CC new -As
    /// gmux`, `powershell -Command "..."`).
    pub fn spawn(command_line: &str) -> io::Result<RemoteTmux> {
        let trimmed = command_line.trim();
        let program = trimmed.split_whitespace().next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty command line")
        })?;
        let rest = trimmed[program.len()..].trim_start();
        let mut cmd = Command::new(program);
        if !rest.is_empty() {
            cmd.raw_arg(rest);
        }
        let mut child = cmd
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Put the child (and any grandchildren it spawns) in a kill-on-close job so `kill`
        // reliably closes the pipe write ends and the reader threads can be joined.
        let job = job_for(&child);

        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");

        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let replies_seen = Arc::new(AtomicU64::new(0));
        let eof = Arc::new(AtomicBool::new(false));
        let detached = Arc::new(AtomicBool::new(false));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));

        let reader = {
            let queue = Arc::clone(&queue);
            let replies_seen = Arc::clone(&replies_seen);
            let eof = Arc::clone(&eof);
            let detached = Arc::clone(&detached);
            std::thread::spawn(move || {
                let mut filter = DcsFilter::new();
                let mut parser = Parser::new();
                let mut buf = [0u8; 8192];
                let mut greeted = false;
                let mut exited = false;
                'read: loop {
                    let n = match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let clean = filter.feed(&buf[..n]);
                    let mut q = queue.lock().expect("event queue poisoned");
                    for event in parser.feed(&clean) {
                        // `%exit` is the last control-mode output; what follows (the ST and
                        // restored terminal noise) is not protocol — stop parsing entirely.
                        if matches!(
                            event,
                            Event::Notification(Notification::Exit { .. })
                        ) {
                            detached.store(true, Ordering::SeqCst);
                            q.push_back(TransportEvent::Ctrl(event));
                            exited = true;
                            drop(q);
                            break 'read;
                        }
                        if matches!(event, Event::Reply { .. }) {
                            if !greeted {
                                // The unsolicited attach greeting: surface it distinctly and
                                // keep it out of the positional reply count.
                                greeted = true;
                                q.push_back(TransportEvent::Greeting(event));
                                continue;
                            }
                            replies_seen.fetch_add(1, Ordering::SeqCst);
                        }
                        q.push_back(TransportEvent::Ctrl(event));
                    }
                }
                if exited {
                    // Drain the remaining stdout to EOF so the child never blocks on a full
                    // pipe, discarding post-exit terminal noise.
                    while matches!(stdout.read(&mut buf), Ok(n) if n > 0) {}
                }
                queue.lock().expect("event queue poisoned").push_back(TransportEvent::Eof);
                eof.store(true, Ordering::SeqCst);
            })
        };

        // Drain stderr so the child never blocks on a full pipe; keep it for diagnostics
        // (ssh prints auth/host-key failures there). Appended chunk-by-chunk so a LIVE child's
        // output is visible — exactly the ssh-stuck-at-a-prompt case needs it.
        let stderr_reader = {
            let stderr_buf = Arc::clone(&stderr_buf);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => stderr_buf
                            .lock()
                            .expect("stderr buffer poisoned")
                            .extend_from_slice(&buf[..n]),
                    }
                }
            })
        };

        Ok(RemoteTmux {
            child,
            job,
            stdin,
            queue,
            replies_seen,
            eof,
            detached,
            stderr_buf,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            sent: 0,
        })
    }

    /// Drain all queued events (non-blocking). The final event ever produced is
    /// [`TransportEvent::Eof`], exactly once.
    ///
    /// The queue is unbounded **by design**: the daemon drains it every tick (~100 ms), so
    /// realistic growth is one tick's worth of output. A consumer that stops draining while
    /// the remote streams will grow memory; tmux's `%pause`-based flow control is the proper
    /// backpressure and is wired up with the stage-2c client.
    pub fn drain_events(&self) -> Vec<TransportEvent> {
        self.queue.lock().expect("event queue poisoned").drain(..).collect()
    }

    /// Write `cmd` + `\n` to the child's stdin and return this command's local sequence
    /// number (0, 1, 2, …). Replies are positional: the Nth `Reply` event answers the Nth
    /// `send_command`. A write failure is deliberately swallowed — a dead peer already
    /// surfaces as [`TransportEvent::Eof`], and a command sent into a dying pipe can never
    /// be answered either way.
    pub fn send_command(&mut self, cmd: &str) -> u64 {
        let seq = self.sent;
        self.sent += 1;
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(cmd.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
        }
        seq
    }

    /// Commands sent whose reply has not yet been parsed. With positional correlation this
    /// is all the bookkeeping needed: the next `Reply` drained answers the oldest
    /// outstanding command.
    pub fn pending_len(&self) -> u64 {
        self.sent.saturating_sub(self.replies_seen.load(Ordering::SeqCst))
    }

    /// Send literal `bytes` to remote pane `%pane`. Uses `send-keys -H` (hex bytes) so no
    /// byte ever needs tmux quoting: `send_keys(5, b"hi\n")` sends
    /// `send-keys -t %5 -H 68 69 0a`.
    pub fn send_keys(&mut self, pane: u64, bytes: &[u8]) -> u64 {
        use std::fmt::Write as _;
        let mut cmd = format!("send-keys -t %{pane} -H");
        for b in bytes {
            let _ = write!(cmd, " {b:02x}");
        }
        self.send_command(&cmd)
    }

    /// Tell the remote tmux this client is `w`×`h` cells (`refresh-client -C WxH`), so it
    /// lays windows out at gmux's real size.
    pub fn resize_client(&mut self, w: u16, h: u16) -> u64 {
        self.send_command(&format!("refresh-client -C {w}x{h}"))
    }

    /// Split remote pane `%pane`: `-h` puts the new pane beside it, `-v` below it.
    pub fn split_pane(&mut self, pane: u64, horizontal: bool) -> u64 {
        let flag = if horizontal { "-h" } else { "-v" };
        self.send_command(&format!("split-window -t %{pane} {flag}"))
    }

    /// Kill remote pane `%pane`.
    pub fn kill_pane(&mut self, pane: u64) -> u64 {
        self.send_command(&format!("kill-pane -t %{pane}"))
    }

    /// Create a new remote window.
    pub fn new_window(&mut self) -> u64 {
        self.send_command("new-window")
    }

    /// Make remote pane `%pane` the active pane of its window.
    pub fn select_pane(&mut self, pane: u64) -> u64 {
        self.send_command(&format!("select-pane -t %{pane}"))
    }

    /// Close the child's stdin (EOF). For ssh this is the graceful goodbye; the remote side
    /// then ends the session and [`TransportEvent::Eof`] follows.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Whether the transport is still delivering: false once the child's stdout has closed.
    /// This is stream liveness, not a process poll — exactly the condition that matters,
    /// since a child with a closed stdout can never produce another event.
    pub fn is_alive(&self) -> bool {
        !self.eof.load(Ordering::SeqCst)
    }

    /// Whether `%exit` was seen — the remote side ended control mode deliberately
    /// (detach/exit) rather than the pipe just breaking. (`%exit` is the protocol's own
    /// goodbye; the ST that follows it is stripped as trailing noise, never scanned for
    /// mid-stream — reply bodies may legitimately contain raw `ESC \`, e.g. OSC 8 hyperlinks
    /// in `capture-pane -e` output.)
    pub fn detached(&self) -> bool {
        self.detached.load(Ordering::SeqCst)
    }

    /// Everything the child wrote to stderr so far (ssh diagnostics).
    pub fn stderr_output(&self) -> Vec<u8> {
        self.stderr_buf.lock().expect("stderr buffer poisoned").clone()
    }

    /// Kill the child's whole process tree and join the reader threads. Idempotent; also runs
    /// on drop. Queued events (and the trailing `Eof`) remain drainable afterwards.
    ///
    /// Terminating the *job* (not just the direct child) is what makes the joins safe: with a
    /// command line like `cmd /c ssh …`, the grandchild holds the pipe write ends, and killing
    /// only `cmd.exe` would leave the reader threads blocked in `read` forever.
    pub fn kill(&mut self) {
        self.stdin = None;
        unsafe {
            if self.job != 0 {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job as *mut core::ffi::c_void,
                    0,
                );
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_reader.take() {
            let _ = h.join();
        }
        unsafe {
            if self.job != 0 {
                windows_sys::Win32::Foundation::CloseHandle(self.job as *mut core::ffi::c_void);
                self.job = 0;
            }
        }
    }
}

impl Drop for RemoteTmux {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Create a kill-on-close job object and put `child`'s tree in it. Returns 0 (and the
/// transport degrades to direct-child kill) if any step fails — same policy as gmux-pty.
fn job_for(child: &Child) -> isize {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        AssignProcessToJobObject(job, child.as_raw_handle());
        job as isize
    }
}

// ---------------------------------------------------------------------------
// DCS wrapper stripping.
// ---------------------------------------------------------------------------

/// The `-CC` DCS introducer tmux prints before the first control-mode line.
const DCS_INTRO: &[u8] = b"\x1bP1000p";

/// Strips the `-CC` DCS introducer [`DCS_INTRO`] at stream start, recognized across chunk
/// boundaries; a stream-start prefix that only *looks* like the introducer (then diverges) is
/// replayed as data. Deliberately does NOT scan for ST (`ESC \`): reply bodies may contain
/// raw `ESC \` legitimately (OSC 8 hyperlinks in `capture-pane -e` output), and the protocol's
/// real goodbye is the `%exit` notification — the reader stops parsing there, so the trailing
/// ST never reaches the parser as anything but ignored post-exit noise.
struct DcsFilter {
    /// Bytes of [`DCS_INTRO`] matched so far; only meaningful while `intro_active`.
    intro_matched: usize,
    /// Still at stream start, matching the introducer.
    intro_active: bool,
}

impl DcsFilter {
    fn new() -> Self {
        DcsFilter { intro_matched: 0, intro_active: true }
    }

    /// Filter one chunk, returning the bytes to hand to the parser.
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !self.intro_active {
            return bytes.to_vec();
        }
        let mut out = Vec::with_capacity(bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            if b == DCS_INTRO[self.intro_matched] {
                self.intro_matched += 1;
                if self.intro_matched == DCS_INTRO.len() {
                    self.intro_active = false; // introducer fully stripped
                    out.extend_from_slice(&bytes[i + 1..]);
                    break;
                }
                continue;
            }
            // Divergence: the matched prefix was real data — replay it, then pass the rest.
            self.intro_active = false;
            out.extend_from_slice(&DCS_INTRO[..self.intro_matched]);
            out.extend_from_slice(&bytes[i..]);
            break;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests. No tmux/ssh exists on dev machines; every child is a stub process (`cmd /c type`
// replays canned control-mode bytes, `powershell` records stdin), which is exactly the
// injectable-command contract production relies on.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gmux_tmux::Notification;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// A collision-free temp path (tests run in parallel in one process).
    fn temp_path(tag: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("gmux-remote-test-{}-{n}-{tag}", std::process::id()))
    }

    /// Poll `drain_events` until `Eof` arrives, returning everything drained.
    fn drain_until_eof(rt: &RemoteTmux, timeout: Duration) -> Vec<TransportEvent> {
        let deadline = Instant::now() + timeout;
        let mut all = Vec::new();
        loop {
            all.extend(rt.drain_events());
            if all.iter().any(|e| matches!(e, TransportEvent::Eof)) {
                return all;
            }
            assert!(
                Instant::now() < deadline,
                "no Eof within {timeout:?}; events {all:?}; stderr {:?}",
                String::from_utf8_lossy(&rt.stderr_output()),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn ctrl(event: Event) -> TransportEvent {
        TransportEvent::Ctrl(event)
    }

    // -- DCS filter unit tests (chunk boundaries are the hard part) --

    #[test]
    fn dcs_filter_strips_intro_across_chunk_boundaries() {
        let mut f = DcsFilter::new();
        let mut out = Vec::new();
        out.extend(f.feed(b"\x1bP10")); // introducer split mid-way…
        out.extend(f.feed(b"00p%exit\n"));
        assert_eq!(out, b"%exit\n");
    }

    #[test]
    fn dcs_filter_passes_raw_st_through() {
        // Raw `ESC \` (e.g. an OSC 8 terminator inside a capture-pane body) must survive —
        // detach detection is the reader's `%exit` handling, not a byte scan.
        let mut f = DcsFilter::new();
        let mut out = Vec::new();
        out.extend(f.feed(b"\x1bP1000p"));
        out.extend(f.feed(b"body \x1b")); // ESC split from its backslash…
        out.extend(f.feed(b"\\ more\n"));
        assert_eq!(out, b"body \x1b\\ more\n");
    }

    #[test]
    fn dcs_filter_replays_diverging_intro_lookalike_as_data() {
        let mut f = DcsFilter::new();
        // Starts like the introducer (`ESC P 1`) then diverges: every byte is real data.
        let out = f.feed(b"\x1bP1x%output %1 a\n");
        assert_eq!(out, b"\x1bP1x%output %1 a\n");
    }

    // -- Canned control-mode stream through a stub child --

    #[test]
    fn canned_stream_yields_events_then_eof() {
        let path = temp_path("canned.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(DCS_INTRO);
        canned.extend_from_slice(b"%begin 1578920019 100 0\n");
        canned.extend_from_slice(b"%end 1578920019 100 0\n");
        canned.extend_from_slice(b"%output %1 hi\\015there\n"); // octal-escaped CR
        canned.extend_from_slice(b"%exit\n");
        canned.extend_from_slice(b"\x1b\\"); // ST detach marker
        std::fs::write(&path, &canned).unwrap();

        let mut rt = RemoteTmux::spawn(&format!("cmd.exe /c type {}", path.display())).unwrap();
        let events = drain_until_eof(&rt, Duration::from_secs(20));
        assert_eq!(
            events,
            vec![
                // The first reply block on the stream is the unsolicited attach greeting.
                TransportEvent::Greeting(Event::Reply { num: 100, body: vec![], error: false }),
                ctrl(Event::Notification(Notification::Output {
                    pane: 1,
                    data: b"hi\rthere".to_vec(),
                })),
                ctrl(Event::Notification(Notification::Exit { reason: None })),
                TransportEvent::Eof,
            ],
        );
        // The exact match above already proves neither the DCS intro nor the post-%exit ST
        // leaked; be explicit anyway.
        for e in &events {
            if let TransportEvent::Ctrl(Event::Notification(Notification::Unknown { line })) = e {
                panic!("wrapper bytes leaked into events: {line:?}");
            }
        }
        assert_eq!(rt.pending_len(), 0, "the greeting must not count as a reply");
        assert!(rt.detached(), "%exit must be noted as detach");
        assert!(!rt.is_alive(), "stdout closed => not alive");
        rt.kill();
        let _ = std::fs::remove_file(&path);
    }

    // -- stdin path: commands and hex-encoded keys reach the child verbatim --

    #[test]
    fn stdin_carries_commands_and_hex_send_keys() {
        let out = temp_path("stdin.txt");
        let cl = format!(
            "powershell -NoProfile -Command \"$input | Set-Content -Path {}\"",
            out.display(),
        );
        let mut rt = RemoteTmux::spawn(&cl).unwrap();
        assert_eq!(rt.send_command("list-panes"), 0);
        assert_eq!(rt.send_keys(5, b"hi\n"), 1);
        assert_eq!(rt.pending_len(), 2);
        rt.close_stdin(); // EOF ends the stub's $input pipeline; it writes the file and exits
        let events = drain_until_eof(&rt, Duration::from_secs(60));
        assert_eq!(events, vec![TransportEvent::Eof], "stub produced no control-mode output");
        assert_eq!(rt.pending_len(), 2, "nothing ever replied");
        rt.kill();
        let written = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines, ["list-panes", "send-keys -t %5 -H 68 69 0a"]);
        let _ = std::fs::remove_file(&out);
    }

    // -- Positional reply correlation --

    #[test]
    fn nth_reply_answers_nth_send_command_with_greeting_excluded() {
        let path = temp_path("replies.bin");
        let mut canned = Vec::new();
        canned.extend_from_slice(DCS_INTRO);
        // Production shape: attach emits an unsolicited greeting block FIRST (the reply to
        // the command line's own command). It must not shift positional correlation.
        canned.extend_from_slice(b"%begin 1000 6 1\n%end 1000 6 1\n");
        // tmux's own counter starts wherever it likes (here 7) — it is NOT the local seq.
        canned.extend_from_slice(b"%begin 1000 7 1\nline-a\n%end 1000 7 1\n");
        canned.extend_from_slice(b"%begin 1000 8 1\nline-b\n%error 1000 8 1\n");
        std::fs::write(&path, &canned).unwrap();

        let mut rt = RemoteTmux::spawn(&format!("cmd.exe /c type {}", path.display())).unwrap();
        let seq_a = rt.send_command("list-panes");
        let seq_b = rt.send_command("bogus-command");
        assert_eq!((seq_a, seq_b), (0, 1), "local sequence numbers are 0-based send order");
        assert!(rt.pending_len() <= 2, "never more pending than sent");

        let events = drain_until_eof(&rt, Duration::from_secs(20));
        assert!(
            matches!(events[0], TransportEvent::Greeting(Event::Reply { num: 6, .. })),
            "first block is the greeting: {events:?}",
        );
        let replies: Vec<&Event> = events
            .iter()
            .filter_map(|e| match e {
                TransportEvent::Ctrl(ev @ Event::Reply { .. }) => Some(ev),
                _ => None,
            })
            .collect();
        // Replies arrive in send order: replies[seq_a] answers the first command, etc.
        assert_eq!(
            replies,
            vec![
                &Event::Reply { num: 7, body: vec![b"line-a".to_vec()], error: false },
                &Event::Reply { num: 8, body: vec![b"line-b".to_vec()], error: true },
            ],
        );
        assert_eq!(rt.pending_len(), 0, "both commands answered; greeting not counted");
        assert!(!rt.detached(), "stream ended without %exit: a plain drop, not a detach");
        rt.kill();
        let _ = std::fs::remove_file(&path);
    }

    // -- Teardown: kill() must take down grandchildren, not just the direct child --

    /// The reviewer's deadlock repro: `cmd /c powershell …` leaves the pipe write ends with
    /// the grandchild; killing only cmd.exe left the reader threads blocked and kill()/Drop
    /// hanging until the grandchild exited on its own. The job object kills the whole tree.
    #[test]
    fn kill_returns_promptly_with_grandchild_holding_the_pipe() {
        let mut rt = RemoteTmux::spawn(
            "cmd.exe /c powershell -NoProfile -Command Start-Sleep -Seconds 25",
        )
        .unwrap();
        let start = Instant::now();
        rt.kill();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill() took {:?} — grandchild not terminated",
            start.elapsed(),
        );
        // Eof is drainable after the kill.
        let events = rt.drain_events();
        assert!(events.iter().any(|e| matches!(e, TransportEvent::Eof)), "{events:?}");
    }
}
