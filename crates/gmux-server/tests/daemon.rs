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

fn count_panes(s: &mut Server) -> usize {
    match s.handle(&Request { id: 100, call: Call::ListPanes }).result {
        Some(ResultBody::Panes(p)) => p.len(),
        _ => 0,
    }
}

fn capture_contains(s: &mut Server, pane: u64, needle: &str) -> bool {
    match s.handle(&Request { id: 101, call: Call::CapturePane { pane } }).result {
        Some(ResultBody::Text(t)) => t.contains(needle),
        _ => false,
    }
}
