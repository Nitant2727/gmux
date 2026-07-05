//! Integration tests for gmux-pty against real ConPTY + real shells.
//!
//! These prove the pane works even when the test runner has no console (the M0 harness
//! condition) — `ensure_console()` must handle that.

use gmux_pty::{Pty, PtySize};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Collect output until the child exits (then drain), or `cap` elapses.
fn collect(pty: &Pty, rx: &Receiver<Vec<u8>>, cap: Duration) -> String {
    let mut out = Vec::new();
    let deadline = Instant::now() + cap;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(RecvTimeoutError::Timeout) => {
                if !pty.is_alive() {
                    while let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
                        out.extend_from_slice(&c);
                    }
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// The output-checking tests need the host process's stdout to be a real console (ConPTY child
// binding requirement — see lib.rs). `cargo test` under a pipe-stdio launcher does not provide
// one, so these are #[ignore]'d there and run via `test-conpty.ps1` (Start-Process, real console).
#[test]
#[ignore = "requires a real console; run via crates/gmux-pty/test-conpty.ps1"]
fn cmd_echo_roundtrips() {
    let (pty, rx) =
        Pty::spawn("cmd.exe /c echo gmux-pty-roundtrip-marker", PtySize { cols: 120, rows: 30 })
            .expect("spawn cmd");
    let out = collect(&pty, &rx, Duration::from_secs(6));
    assert!(
        out.contains("gmux-pty-roundtrip-marker"),
        "child output did not reach the host (console binding failed?). got: {out:?}"
    );
}

#[test]
#[ignore = "requires a real console; run via crates/gmux-pty/test-conpty.ps1"]
fn powershell_osc_passes_through() {
    // The killer-feature path at the crate level: an OSC 9 emitted by the child must reach us.
    let cmd = r#"powershell -NoProfile -Command "[Console]::Out.Write([char]27 + ']9;gmux-pane-osc' + [char]7)""#;
    let (pty, rx) = Pty::spawn(cmd, PtySize { cols: 120, rows: 30 }).expect("spawn powershell");
    let out = collect(&pty, &rx, Duration::from_secs(10));
    assert!(
        out.contains("\x1b]9;gmux-pane-osc\x07") || out.contains("]9;gmux-pane-osc"),
        "OSC 9 did not pass through ConPTY. got: {out:?}"
    );
}

#[test]
#[ignore = "requires a real console; run via crates/gmux-pty/test-conpty.ps1"]
fn write_and_read_interactive() {
    // Drive cmd interactively: write a command, read its echoed output.
    let (pty, rx) = Pty::spawn("cmd.exe", PtySize { cols: 120, rows: 30 }).expect("spawn cmd");
    std::thread::sleep(Duration::from_millis(400)); // let the prompt come up
    pty.write(b"echo interactive-marker-42\r\n").expect("write");
    pty.write(b"exit\r\n").expect("write exit");
    let out = collect(&pty, &rx, Duration::from_secs(8));
    assert!(out.contains("interactive-marker-42"), "interactive echo missing. got: {out:?}");
}

#[test]
fn resize_does_not_crash() {
    let (pty, _rx) = Pty::spawn("cmd.exe", PtySize { cols: 80, rows: 24 }).expect("spawn");
    std::thread::sleep(Duration::from_millis(200));
    pty.resize(PtySize { cols: 120, rows: 40 }).expect("resize");
    pty.resize(PtySize { cols: 100, rows: 30 }).expect("resize");
    // Dropping pty here must not hang.
}
