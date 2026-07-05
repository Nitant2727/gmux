//! Integration tests for remote-backed panes: a terminal fed by `push_output` with no ConPTY, no
//! pump thread — and therefore no console requirement, so these run under plain `cargo test`
//! (that's the feature: remote mirrors need nothing from the local console stack).

use std::sync::{Arc, Mutex};

use gmux_mux::{Attention, Pane, PaneEvent, PtySize};

/// A remote pane whose input goes nowhere (for output-side tests).
fn remote_pane() -> Pane {
    Pane::remote(3, 80, 24, Box::new(|_| {}))
}

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

#[test]
fn push_output_renders_and_emits_output() {
    let pane = remote_pane();
    pane.push_output(b"hello");
    let text = snapshot_text(&pane);
    assert!(text.iter().any(|l| l.contains("hello")), "grid: {text:?}");
    let evs = pane.drain_events();
    assert!(evs.iter().any(|e| matches!(e, PaneEvent::Output)), "events: {evs:?}");
}

#[test]
fn osc777_over_remote_raises_attention_and_notification() {
    // The killer feature over the remote path: %output flows through the same OSC parser as
    // local PTY bytes, so a remote agent's OSC 777 lights up attention exactly like a local one.
    let pane = remote_pane();
    assert_eq!(pane.attention(), Attention::Quiet);
    pane.push_output(b"\x1b]777;notify;agent;needs input\x07");
    assert_eq!(pane.attention(), Attention::Pending, "OSC 777 must raise attention");
    let evs = pane.drain_events();
    assert!(
        evs.iter().any(
            |e| matches!(e, PaneEvent::Notification(n) if n.title == "agent" && n.body == "needs input")
        ),
        "expected a Notification event: {evs:?}"
    );
    // Focusing the pane clears attention, same as local.
    pane.focus();
    assert_eq!(pane.attention(), Attention::Quiet);
}

#[test]
fn write_reaches_input_closure() {
    let sent: Arc<Mutex<Vec<u8>>> = Arc::default();
    let s = sent.clone();
    let pane = Pane::remote(7, 80, 24, Box::new(move |b| s.lock().unwrap().extend_from_slice(b)));
    assert_eq!(pane.remote_id(), Some(7));
    pane.write(b"ls\r").unwrap();
    assert_eq!(sent.lock().unwrap().as_slice(), b"ls\r");
}

#[test]
fn mark_exited_flips_liveness_and_emits_exited() {
    let pane = remote_pane();
    assert!(pane.is_alive());
    pane.mark_exited();
    assert!(!pane.is_alive());
    let exited = pane.drain_events().iter().filter(|e| matches!(e, PaneEvent::Exited)).count();
    assert_eq!(exited, 1);
    // Idempotent: a second call must not emit a second Exited.
    pane.mark_exited();
    assert!(pane.drain_events().is_empty());
}

#[test]
fn resize_touches_the_grid_only() {
    // Transport-side resize is the transport's job; locally only the terminal grid changes.
    let pane = remote_pane();
    pane.resize(PtySize { cols: 40, rows: 10 }).unwrap();
    let snap = pane.snapshot();
    assert_eq!((snap.cols, snap.rows), (40, 10));
}
