//! Console-gated integration test for the headless server: prove it owns a real pane and services
//! the automation protocol end to end. Run via `scripts/console-tests.ps1 gmux-server daemon`.

use gmux_proto::{Call, Request, ResultBody};
use gmux_server::Server;
use std::time::{Duration, Instant};

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
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-server daemon"]
fn server_owns_pane_and_serves_protocol() {
    let mut s = Server::new("cmd.exe".to_string()).expect("server");

    // list-panes shows exactly the one initial pane.
    let panes = match s.handle(&Request { id: 1, call: Call::ListPanes }).result {
        Some(ResultBody::Panes(p)) => p,
        other => panic!("expected panes, got {other:?}"),
    };
    assert_eq!(panes.len(), 1);
    let pane = panes[0].id;

    // split-pane creates a second pane.
    let new_pane = match s.handle(&Request { id: 2, call: Call::SplitPane { dir: "h".into(), command: Some("cmd.exe".into()) } }).result {
        Some(ResultBody::PaneId(p)) => p,
        other => panic!("expected pane id, got {other:?}"),
    };
    assert_ne!(new_pane, pane);
    assert_eq!(count_panes(&mut s), 2);

    // GetLayout returns both pane rects (side by side) + one tab.
    let layout = match s.handle(&Request { id: 10, call: Call::GetLayout { w: 1000, h: 400 } }).result {
        Some(ResultBody::Layout(l)) => l,
        other => panic!("expected layout, got {other:?}"),
    };
    assert_eq!(layout.panes.len(), 2, "split should yield two rects");
    assert_eq!(layout.tabs.len(), 1);
    assert!(layout.panes.iter().any(|r| r.active), "one pane must be active");

    // GetGrid returns a full grid for a pane.
    let grid = match s.handle(&Request { id: 11, call: Call::GetGrid { pane, offset: 0 } }).result {
        Some(ResultBody::Grid(g)) => g,
        other => panic!("expected grid, got {other:?}"),
    };
    assert_eq!(grid.cells.len(), grid.cols as usize * grid.rows as usize, "grid cell count");

    // ResizeView + FocusPane are accepted.
    assert!(s.handle(&Request { id: 12, call: Call::ResizeView { w: 800, h: 400, cell_w: 9, cell_h: 18 } }).error.is_none());
    assert!(s.handle(&Request { id: 13, call: Call::FocusPane { dir: "right".into() } }).error.is_none());

    // send-keys into the original pane, then capture its screen.
    std::thread::sleep(Duration::from_millis(400));
    let sk = s.handle(&Request {
        id: 3,
        call: Call::SendKeys { pane, text: "echo daemon-marker-77".into(), enter: true },
    });
    assert!(sk.error.is_none(), "send-keys errored: {:?}", sk.error);

    let found = wait_until(Duration::from_secs(6), || capture_contains(&mut s, pane, "daemon-marker-77"));
    assert!(found, "captured screen never showed the echoed marker");
}

#[test]
#[ignore = "requires a real console; run via scripts/console-tests.ps1 gmux-server daemon"]
fn snapshot_capture_and_restore_rebuilds_layout() {
    use gmux_mux::{Pane, PtySize, SessionSnapshot};

    let mut s = Server::new("cmd.exe".to_string()).expect("server");
    // Split so there are two panes in a tree.
    s.handle(&Request { id: 1, call: Call::SplitPane { dir: "h".into(), command: Some("cmd.exe".into()) } });
    let snap = SessionSnapshot::capture(&s.session);
    assert_eq!(snap.windows.len(), 1);
    assert_eq!(snap.windows[0].panes.len(), 2, "snapshot must capture both panes");

    // Restore into a brand-new session (as a fresh daemon would after reboot).
    let restored = snap
        .restore("gmux", |rec| {
            Pane::spawn_in("cmd.exe", PtySize { cols: 80, rows: 24 }, rec.cwd.as_deref(), None)
        })
        .expect("restore");
    assert_eq!(restored.pane_count(), 2, "restore must rebuild both panes");
    assert_eq!(restored.windows().len(), 1);
    assert_eq!(restored.active_window().unwrap().pane_count(), 2);
}

fn count_panes(s: &mut Server) -> usize {
    match s.handle(&Request { id: 100, call: Call::ListPanes }).result {
        Some(ResultBody::Panes(p)) => p.len(),
        _ => 0,
    }
}

fn capture_contains(s: &mut Server, pane: u64, needle: &str) -> bool {
    match s.handle(&Request { id: 101, call: Call::CapturePane { pane, scrollback: None } }).result {
        Some(ResultBody::Text(t)) => t.contains(needle),
        _ => false,
    }
}
