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
