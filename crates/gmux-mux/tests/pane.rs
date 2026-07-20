//! Integration tests for gmux-mux: a live pane wiring ConPTY + terminal + attention.
//! Console-gated (ConPTY child binding) — run via `scripts/console-tests.ps1 gmux-mux pane`.

use gmux_mux::{Attention, Pane, PaneEvent, PtySize};
use std::time::{Duration, Instant};

fn snapshot_text(pane: &Pane) -> Vec<String> {
    pane.snapshot()
        .cells
        .iter()
        .map(|row| {
            let mut s: String = row.iter().map(|c| c.ch).collect();
            let end = s.trim_end_matches(' ').len();
            s.truncate(end);
            s
        })
        .collect()
}

fn wait_until(cap: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + cap;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    pred()
}

#[test]
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-mux pane"]
fn pane_runs_shell_and_snapshots() {
    let pane =
        Pane::spawn("cmd.exe /c echo gmux-mux-grid-marker", PtySize { cols: 120, rows: 30 }).unwrap();
    let found = wait_until(Duration::from_secs(6), || {
        snapshot_text(&pane).iter().any(|line| line.contains("gmux-mux-grid-marker"))
    });
    assert!(found, "marker not rendered into the pane grid: {:?}", snapshot_text(&pane));
}

#[test]
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-mux pane"]
fn injects_gmux_pane_env() {
    let pane = Pane::spawn("cmd.exe /c echo PANE=%GMUX_PANE%", PtySize { cols: 120, rows: 30 }).unwrap();
    let found = wait_until(Duration::from_secs(6), || {
        snapshot_text(&pane).iter().any(|l| l.contains("PANE=%") && !l.contains("GMUX_PANE%"))
    });
    assert!(found, "GMUX_PANE not injected/expanded: {:?}", snapshot_text(&pane));
}

#[test]
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-mux pane"]
fn typed_command_executes_in_first_and_later_panes() {
    // Regression (round 23): interactive input — a written command line must EXECUTE (CR
    // submits), and it must work for panes spawned at any point, not just the process's first
    // (the daemon bug hid because every probe targeted pane %0).
    let run = |pane: &Pane, marker: &str| {
        // Wait for an interactive prompt, then type an echo and press Enter.
        assert!(
            wait_until(Duration::from_secs(8), || {
                snapshot_text(pane).iter().any(|l| l.ends_with('>'))
            }),
            "no cmd prompt appeared: {:?}",
            snapshot_text(pane)
        );
        pane.write(format!("echo {marker}\r").as_bytes()).unwrap();
        // The OUTPUT line is the marker alone; the echoed input line contains "echo ".
        assert!(
            wait_until(Duration::from_secs(8), || {
                snapshot_text(pane).iter().any(|l| l.trim() == marker)
            }),
            "typed command did not execute: {:?}",
            snapshot_text(pane)
        );
    };
    let first = Pane::spawn("cmd.exe", PtySize { cols: 120, rows: 30 }).unwrap();
    run(&first, "typed-exec-first");
    let later = Pane::spawn("cmd.exe", PtySize { cols: 120, rows: 30 }).unwrap();
    run(&later, "typed-exec-later");
}

#[test]
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-mux pane"]
fn osc9_sets_attention_and_emits_event() {
    let cmd = r#"powershell -NoProfile -Command "[Console]::Out.Write([char]27 + ']9;agent needs input' + [char]7); Start-Sleep -Milliseconds 500""#;
    let pane = Pane::spawn(cmd, PtySize { cols: 120, rows: 30 }).unwrap();

    let pending = wait_until(Duration::from_secs(10), || pane.attention() == Attention::Pending);
    assert!(pending, "OSC 9 from the pane did not raise attention to Pending");

    // A Notification pane-event should also have been emitted.
    let saw_notification = pane
        .drain_events()
        .iter()
        .any(|e| matches!(e, PaneEvent::Notification(n) if n.title.contains("agent needs input")));
    assert!(saw_notification, "expected a PaneEvent::Notification carrying the message");

    // Focusing the pane clears attention.
    pane.focus();
    assert_eq!(pane.attention(), Attention::Quiet);
}
