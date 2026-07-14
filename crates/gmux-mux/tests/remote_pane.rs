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

/// The search-offset contract: the offset returned for a match, fed straight to `GetGrid`
/// (`snapshot_at`), scrolls that match into the viewport — and equals its distance above the live
/// bottom. Pushes 40 numbered lines (no trailing newline) into an 80x24 remote pane so `row{i}`
/// lands on absolute line `i`; the live bottom is `row39`, so `row30` sits 9 lines up.
#[test]
fn search_offset_scrolls_match_into_view() {
    let pane = remote_pane();
    let mut feed = String::new();
    for i in 0..40 {
        if i > 0 {
            feed.push_str("\r\n");
        }
        feed.push_str(&format!("row{i}"));
    }
    pane.push_output(feed.as_bytes());

    // "row30" is unique (no other line contains it) and 9 lines above the live bottom (row39).
    let hits = pane.search("row30");
    assert_eq!(hits, vec![9], "row30 is 9 lines above the live bottom");

    // The pinned contract: GetGrid at the returned offset shows the marker in the viewport.
    let snap = pane.snapshot_at(hits[0] as usize);
    assert!(
        snapshot_text_at(&snap).iter().any(|l| l.contains("row30")),
        "marker not in viewport at the returned offset: {:?}",
        snapshot_text_at(&snap)
    );

    // Nearest-to-bottom first: consecutive lines yield ascending offsets 0 (row39) .. 9 (row30).
    let seq = pane.search("row3");
    assert_eq!(seq.first(), Some(&0), "row39 (contains 'row3') is the live bottom -> offset 0");
    assert!(seq.windows(2).all(|w| w[0] < w[1]), "offsets ascend (nearest-to-bottom first): {seq:?}");

    // Case-insensitive substring; empty query is empty.
    assert_eq!(pane.search("ROW30"), vec![9], "search is case-insensitive");
    assert!(pane.search("").is_empty(), "empty query -> no matches");
    assert!(pane.search("nope").is_empty(), "a miss -> no matches");
}

/// The 500-match cap: 700 lines all containing the query yield exactly 500 offsets, the 500 nearest
/// the bottom (offsets 0..=499, ascending).
#[test]
fn search_caps_at_500_matches() {
    let pane = Pane::remote(1, 80, 24, Box::new(|_| {}));
    let mut feed = String::new();
    for i in 0..700 {
        if i > 0 {
            feed.push_str("\r\n");
        }
        feed.push_str(&format!("hit{i}"));
    }
    pane.push_output(feed.as_bytes());
    let hits = pane.search("hit");
    assert_eq!(hits.len(), 500, "capped at 500");
    assert_eq!(hits.first(), Some(&0), "nearest-to-bottom first");
    assert_eq!(hits.last(), Some(&499), "the 500 nearest the bottom, ascending");
}

fn snapshot_text_at(snap: &gmux_mux::PaneSnapshot) -> Vec<String> {
    snap.cells
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
fn resize_touches_the_grid_only() {
    // Transport-side resize is the transport's job; locally only the terminal grid changes.
    let pane = remote_pane();
    pane.resize(PtySize { cols: 40, rows: 10 }).unwrap();
    let snap = pane.snapshot();
    assert_eq!((snap.cols, snap.rows), (40, 10));
}
