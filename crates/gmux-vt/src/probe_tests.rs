//! TEMPORARY probe tests — deleted before finishing.
use super::*;

fn nd(evs: Vec<TermEvent>) -> Vec<TermEvent> {
    evs.into_iter().filter(|e| !matches!(e, TermEvent::Damage)).collect()
}
fn notif(evs: &[TermEvent]) -> Option<&Notification> {
    evs.iter().find_map(|e| match e { TermEvent::Notification(n) => Some(n), _ => None })
}

#[test]
fn probe_9_4_progress_not_notification() {
    let mut t = Terminal::new(80, 24);
    let evs = nd(t.advance(b"\x1b]9;4;1;50\x07"));
    println!("9;4;1;50 => {:?}", evs);
    assert!(notif(&evs).is_none(), "9;4 progress must not be a notification");
}

#[test]
fn probe_9_space_tasks_done_is_notification() {
    let mut t = Terminal::new(80, 24);
    let evs = nd(t.advance(b"\x1b]9;9 tasks done\x07"));
    println!("9;'9 tasks done' => {:?}", evs);
    let n = notif(&evs).expect("prose starting with digit+space IS a notification");
    assert_eq!(n.title, "9 tasks done");
}

#[test]
fn probe_systemd_bare_indeterminate() {
    let mut t = Terminal::new(80, 24);
    let evs = nd(t.advance(b"\x1b]9;4;3\x07"));
    println!("9;4;3 => {:?}", evs);
    assert!(notif(&evs).is_none(), "systemd bare 9;4;3 must not be a toast");
    assert!(evs.contains(&TermEvent::Progress { state: ProgressState::Indeterminate, pct: None }));
}

#[test]
fn probe_unknown_osc99_meta_key_ignored() {
    let mut t = Terminal::new(80, 24);
    // z=whatever is unknown; must be ignored, not fatal.
    let evs = nd(t.advance(b"\x1b]99;i=5:z=foo:p=title;hey\x07"));
    println!("osc99 unknown key => {:?}", evs);
    let n = notif(&evs).expect("unknown osc99 meta key must be ignored, notification still emitted");
    assert_eq!(n.title, "hey");
    assert_eq!(n.id.as_deref(), Some("5"));
}

#[test]
fn probe_split_mid_metadata() {
    let mut t = Terminal::new(80, 24);
    // Split right in the middle of the metadata "i=1:p=ti" | "tle;body-title"
    let a = nd(t.advance(b"\x1b]99;i=9:p=ti"));
    println!("split-meta first => {:?}", a);
    assert!(notif(&a).is_none());
    let b = nd(t.advance(b"tle;the title\x07"));
    println!("split-meta second => {:?}", b);
    let n = notif(&b).expect("split mid-metadata must still parse");
    assert_eq!(n.title, "the title");
    assert_eq!(n.id.as_deref(), Some("9"));
}

#[test]
fn probe_split_mid_terminator_esc_then_backslash() {
    let mut t = Terminal::new(80, 24);
    // vte dispatches the OSC when it sees the terminating ESC; '\' just consumes the escape.
    let a = nd(t.advance(b"\x1b]9;split ST\x1b"));
    println!("split-term first => {:?}", a);
    let n = notif(&a).expect("OSC dispatches on terminating ESC");
    assert_eq!(n.title, "split ST");
    let b = nd(t.advance(b"\\"));
    println!("split-term second => {:?}", b);
    assert!(notif(&b).is_none(), "trailing backslash must not double-dispatch");
}

#[test]
fn probe_split_full_st_next_chunk() {
    let mut t = Terminal::new(80, 24);
    let a = nd(t.advance(b"\x1b]777;notify;Ti;Bo"));
    assert!(notif(&a).is_none());
    let b = nd(t.advance(b"\x1b\\"));
    let n = notif(&b).expect("whole ST in next chunk terminates");
    assert_eq!(n.title, "Ti");
    assert_eq!(n.body, "Bo");
    println!("full-st-next-chunk OK");
}

#[test]
fn probe_utf8_multibyte_split() {
    let mut t = Terminal::new(80, 24);
    // 'é' = 0xC3 0xA9. Split between the two bytes across advance() calls.
    t.advance(&[0xC3]);
    t.advance(&[0xA9]);
    let text = t.visible_text();
    println!("utf8 split row0 = {:?}", text[0]);
    assert_eq!(text[0], "é", "multibyte char split across advance must render");
}

#[test]
fn probe_long_osc_no_terminator_bounded() {
    let mut t = Terminal::new(80, 24);
    // 5 MB of OSC data with NO terminator — must not hang or OOM/panic.
    let mut payload = Vec::with_capacity(5_000_000 + 8);
    payload.extend_from_slice(b"\x1b]9;");
    payload.resize(payload.len() + 5_000_000, b'A');
    let evs = t.advance(&payload);
    println!("long-osc event count = {}", evs.len());
    // No notification (never terminated). Just must return.
    assert!(evs.iter().all(|e| !matches!(e, TermEvent::Notification(_))));
}
