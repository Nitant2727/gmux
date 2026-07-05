//! Tests for gmux-tmux. Covers every notification variant, %output unescaping (CR, backslash,
//! raw UTF-8 high bytes, invalid escapes), reply assembly (including %-prefixed body lines and
//! %error), split-across-feed byte-at-a-time parsing, CRLF tolerance, id sigil stripping, and
//! the layout-string grammar (leaf, {} / [] splits, mixed nesting, malformed input).

use super::*;

fn feed_all(input: &[u8]) -> Vec<Event> {
    Parser::new().feed(input)
}

fn feed_byte_at_a_time(input: &[u8]) -> Vec<Event> {
    let mut p = Parser::new();
    let mut events = Vec::new();
    for &b in input {
        events.extend(p.feed(&[b]));
    }
    events
}

fn note(n: Notification) -> Event {
    Event::Notification(n)
}

// ---------------------------------------------------------------------------
// (1) Notification parsing — every variant.
// ---------------------------------------------------------------------------

#[test]
fn output_basic() {
    assert_eq!(
        feed_all(b"%output %5 hello world\n"),
        vec![note(Notification::Output { pane: 5, data: b"hello world".to_vec() })],
    );
}

#[test]
fn layout_change() {
    let events = feed_all(b"%layout-change @1 dead,80x24,0,0,0\n");
    assert_eq!(
        events,
        vec![note(Notification::LayoutChange {
            window: 1,
            layout: Layout {
                checksum: 0xdead,
                root: Cell::Leaf { w: 80, h: 24, x: 0, y: 0, pane: 0 },
            },
        })],
    );
}

#[test]
fn layout_change_ignores_extra_fields() {
    // tmux >= 3.2 appends the visible layout and window flags.
    let events = feed_all(b"%layout-change @2 dead,80x24,0,0,3 beef,80x24,0,0,3 *\n");
    match &events[..] {
        [Event::Notification(Notification::LayoutChange { window: 2, layout })] => {
            assert_eq!(layout.checksum, 0xdead);
            assert_eq!(layout.root, Cell::Leaf { w: 80, h: 24, x: 0, y: 0, pane: 3 });
        }
        other => panic!("expected LayoutChange, got {other:?}"),
    }
}

#[test]
fn window_add() {
    assert_eq!(
        feed_all(b"%window-add @42\n"),
        vec![note(Notification::WindowAdd { window: 42 })],
    );
}

#[test]
fn window_close() {
    assert_eq!(
        feed_all(b"%window-close @7\n"),
        vec![note(Notification::WindowClose { window: 7 })],
    );
}

#[test]
fn window_renamed_name_keeps_spaces() {
    assert_eq!(
        feed_all(b"%window-renamed @3 build logs\n"),
        vec![note(Notification::WindowRenamed { window: 3, name: "build logs".into() })],
    );
}

#[test]
fn session_changed() {
    assert_eq!(
        feed_all(b"%session-changed $1 work\n"),
        vec![note(Notification::SessionChanged { session: 1, name: "work".into() })],
    );
}

#[test]
fn sessions_changed() {
    assert_eq!(feed_all(b"%sessions-changed\n"), vec![note(Notification::SessionsChanged)]);
}

#[test]
fn pane_mode_changed() {
    assert_eq!(
        feed_all(b"%pane-mode-changed %2\n"),
        vec![note(Notification::PaneModeChanged { pane: 2 })],
    );
}

#[test]
fn pause_and_continue() {
    assert_eq!(
        feed_all(b"%pause %4\n%continue %4\n"),
        vec![
            note(Notification::Pause { pane: 4 }),
            note(Notification::Continue { pane: 4 }),
        ],
    );
}

#[test]
fn exit_without_reason() {
    assert_eq!(feed_all(b"%exit\n"), vec![note(Notification::Exit { reason: None })]);
}

#[test]
fn exit_with_reason() {
    assert_eq!(
        feed_all(b"%exit detached\n"),
        vec![note(Notification::Exit { reason: Some("detached".into()) })],
    );
}

// ---------------------------------------------------------------------------
// (2) Unknown lines never error.
// ---------------------------------------------------------------------------

#[test]
fn unknown_notification_preserved() {
    // A real notification this parser does not (yet) understand.
    assert_eq!(
        feed_all(b"%subscription-changed name $1 @2 - - : value\n"),
        vec![note(Notification::Unknown {
            line: "%subscription-changed name $1 @2 - - : value".into(),
        })],
    );
}

#[test]
fn malformed_known_notification_is_unknown() {
    // Wrong sigil / missing args must not panic or drop the line.
    assert_eq!(
        feed_all(b"%window-add %5\n"),
        vec![note(Notification::Unknown { line: "%window-add %5".into() })],
    );
    assert_eq!(
        feed_all(b"%output nope\n"),
        vec![note(Notification::Unknown { line: "%output nope".into() })],
    );
}

#[test]
fn stray_non_percent_line_is_unknown() {
    assert_eq!(
        feed_all(b"stray text\n"),
        vec![note(Notification::Unknown { line: "stray text".into() })],
    );
}

// ---------------------------------------------------------------------------
// (3) %output unescaping through the parser.
// ---------------------------------------------------------------------------

#[test]
fn output_unescapes_cr_and_backslash() {
    // \015 = CR, \134 = backslash (the two escapes tmux always emits).
    assert_eq!(
        feed_all(b"%output %0 a\\015b\\134c\n"),
        vec![note(Notification::Output { pane: 0, data: b"a\rb\\c".to_vec() })],
    );
}

#[test]
fn output_preserves_raw_utf8_high_bytes() {
    let line = "%output %9 caf\u{e9} \u{1f980}\n".as_bytes();
    assert_eq!(
        feed_all(line),
        vec![note(Notification::Output {
            pane: 9,
            data: "caf\u{e9} \u{1f980}".as_bytes().to_vec(),
        })],
    );
}

#[test]
fn output_invalid_escapes_pass_through() {
    // \9 is not octal; a trailing lone backslash has no digits.
    assert_eq!(
        feed_all(b"%output %1 a\\9b\\\n"),
        vec![note(Notification::Output { pane: 1, data: b"a\\9b\\".to_vec() })],
    );
}

#[test]
fn output_empty_data() {
    assert_eq!(
        feed_all(b"%output %3 \n"),
        vec![note(Notification::Output { pane: 3, data: vec![] })],
    );
    assert_eq!(
        feed_all(b"%output %3\n"),
        vec![note(Notification::Output { pane: 3, data: vec![] })],
    );
}

// ---------------------------------------------------------------------------
// (4) unescape() unit tests.
// ---------------------------------------------------------------------------

#[test]
fn unescape_octal_boundaries() {
    assert_eq!(unescape(b"\\000"), vec![0x00]);
    assert_eq!(unescape(b"\\015"), vec![0x0d]);
    assert_eq!(unescape(b"\\134"), vec![0x5c]);
    assert_eq!(unescape(b"\\377"), vec![0xff]);
}

#[test]
fn unescape_invalid_sequences_literal() {
    assert_eq!(unescape(b"\\"), b"\\".to_vec()); // lone trailing backslash
    assert_eq!(unescape(b"\\01"), b"\\01".to_vec()); // too few digits
    assert_eq!(unescape(b"\\08x"), b"\\08x".to_vec()); // non-octal digit
    assert_eq!(unescape(b"\\777"), b"\\777".to_vec()); // value > 0xff
}

#[test]
fn unescape_consecutive_and_embedded() {
    assert_eq!(unescape(b"\\033\\133m"), vec![0x1b, 0x5b, b'm']); // ESC [ m
    assert_eq!(unescape(b"x\\134134y"), b"x\\134y".to_vec()); // decoded '\' is not re-scanned
}

#[test]
fn unescape_passes_high_bytes() {
    assert_eq!(unescape(&[0xc3, 0xa9, 0xf0, 0x9f, 0xa6, 0x80]), vec![0xc3, 0xa9, 0xf0, 0x9f, 0xa6, 0x80]);
}

// ---------------------------------------------------------------------------
// (5) Reply assembly.
// ---------------------------------------------------------------------------

#[test]
fn reply_basic() {
    let events = feed_all(b"%begin 1700000000 12 1\nline one\nline two\n%end 1700000000 12 1\n");
    assert_eq!(
        events,
        vec![Event::Reply {
            num: 12,
            body: vec!["line one".into(), "line two".into()],
            error: false,
        }],
    );
}

#[test]
fn reply_empty_body() {
    assert_eq!(
        feed_all(b"%begin 1700000000 7 1\n%end 1700000000 7 1\n"),
        vec![Event::Reply { num: 7, body: vec![], error: false }],
    );
}

#[test]
fn reply_error_sets_error_flag() {
    let events = feed_all(b"%begin 1700000000 3 1\nno such command\n%error 1700000000 3 1\n");
    assert_eq!(
        events,
        vec![Event::Reply { num: 3, body: vec!["no such command".into()], error: true }],
    );
}

#[test]
fn reply_body_percent_output_stays_in_body() {
    // A body line that looks exactly like a notification must NOT be surfaced as one.
    let events =
        feed_all(b"%begin 1700000000 5 1\n%output %1 fake\n%end 1700000000 5 1\n");
    assert_eq!(
        events,
        vec![Event::Reply { num: 5, body: vec!["%output %1 fake".into()], error: false }],
    );
}

#[test]
fn reply_end_correlates_by_command_number() {
    // An %end with a different command number is body; the matching one closes the block.
    let events =
        feed_all(b"%begin 1700000000 8 1\n%end 1700000000 9 1\n%end 1700000000 8 1\n");
    assert_eq!(
        events,
        vec![Event::Reply { num: 8, body: vec!["%end 1700000000 9 1".into()], error: false }],
    );
}

#[test]
fn notifications_flow_around_replies() {
    let events = feed_all(
        b"%window-add @1\n%begin 1700000000 2 1\nok\n%end 1700000000 2 1\n%window-close @1\n",
    );
    assert_eq!(
        events,
        vec![
            note(Notification::WindowAdd { window: 1 }),
            Event::Reply { num: 2, body: vec!["ok".into()], error: false },
            note(Notification::WindowClose { window: 1 }),
        ],
    );
}

// ---------------------------------------------------------------------------
// (6) Streaming: partial lines across feeds.
// ---------------------------------------------------------------------------

#[test]
fn partial_line_buffered_until_newline() {
    let mut p = Parser::new();
    assert_eq!(p.feed(b"%window-add"), vec![]);
    assert_eq!(p.feed(b" @1"), vec![]);
    assert_eq!(p.feed(b"\n"), vec![note(Notification::WindowAdd { window: 1 })]);
}

#[test]
fn byte_at_a_time_yields_identical_events() {
    let stream: &[u8] = b"%session-changed $0 main\n\
        %begin 1700000000 1 1\n\
        %output %1 not a notification\n\
        %end 1700000000 1 1\n\
        %output %1 real\\015output\n\
        %layout-change @1 bb62,159x48,0,0{79x48,0,0,1,79x48,80,0,2}\n\
        %mystery-line\n\
        %exit\n";
    let all_at_once = feed_all(stream);
    assert_eq!(all_at_once.len(), 6);
    assert_eq!(feed_byte_at_a_time(stream), all_at_once);
}

#[test]
fn crlf_line_endings_tolerated() {
    let events = feed_all(b"%window-add @1\r\n%begin 1 4 1\r\nbody\r\n%end 1 4 1\r\n");
    assert_eq!(
        events,
        vec![
            note(Notification::WindowAdd { window: 1 }),
            Event::Reply { num: 4, body: vec!["body".into()], error: false },
        ],
    );
}

// ---------------------------------------------------------------------------
// (7) Id sigil stripping.
// ---------------------------------------------------------------------------

#[test]
fn id_sigils_stripped_per_kind() {
    let events = feed_all(b"%output %11 x\n%window-add @22\n%session-changed $33 s\n");
    assert_eq!(
        events,
        vec![
            note(Notification::Output { pane: 11, data: b"x".to_vec() }),
            note(Notification::WindowAdd { window: 22 }),
            note(Notification::SessionChanged { session: 33, name: "s".into() }),
        ],
    );
}

// ---------------------------------------------------------------------------
// (8) Layout parsing.
// ---------------------------------------------------------------------------

#[test]
fn layout_simple_leaf() {
    let layout = parse_layout("b25f,80x24,0,0,0").unwrap();
    assert_eq!(layout.checksum, 0xb25f);
    assert_eq!(layout.root, Cell::Leaf { w: 80, h: 24, x: 0, y: 0, pane: 0 });
}

#[test]
fn layout_horizontal_split() {
    // The doc example: two side-by-side panes.
    let layout = parse_layout("bb62,159x48,0,0{79x48,0,0,1,79x48,80,0,2}").unwrap();
    assert_eq!(layout.checksum, 0xbb62);
    assert_eq!(
        layout.root,
        Cell::Split {
            w: 159,
            h: 48,
            x: 0,
            y: 0,
            horizontal: true,
            children: vec![
                Cell::Leaf { w: 79, h: 48, x: 0, y: 0, pane: 1 },
                Cell::Leaf { w: 79, h: 48, x: 80, y: 0, pane: 2 },
            ],
        },
    );
}

#[test]
fn layout_vertical_split() {
    let layout = parse_layout("e3b2,80x24,0,0[80x12,0,0,3,80x11,0,13,4]").unwrap();
    assert_eq!(
        layout.root,
        Cell::Split {
            w: 80,
            h: 24,
            x: 0,
            y: 0,
            horizontal: false,
            children: vec![
                Cell::Leaf { w: 80, h: 12, x: 0, y: 0, pane: 3 },
                Cell::Leaf { w: 80, h: 11, x: 0, y: 13, pane: 4 },
            ],
        },
    );
}

#[test]
fn layout_mixed_nesting() {
    let layout =
        parse_layout("9f42,160x48,0,0{80x48,0,0,1,79x48,81,0[79x24,81,0,2,79x23,81,25{40x23,81,25,3,38x23,122,25,4}]}")
            .unwrap();
    let Cell::Split { horizontal: true, children, .. } = &layout.root else {
        panic!("expected horizontal root, got {:?}", layout.root);
    };
    assert_eq!(children[0], Cell::Leaf { w: 80, h: 48, x: 0, y: 0, pane: 1 });
    let Cell::Split { horizontal: false, children: inner, .. } = &children[1] else {
        panic!("expected vertical split, got {:?}", children[1]);
    };
    assert_eq!(inner[0], Cell::Leaf { w: 79, h: 24, x: 81, y: 0, pane: 2 });
    let Cell::Split { horizontal: true, children: innermost, .. } = &inner[1] else {
        panic!("expected horizontal split, got {:?}", inner[1]);
    };
    assert_eq!(innermost.len(), 2);
    assert_eq!(innermost[1], Cell::Leaf { w: 38, h: 23, x: 122, y: 25, pane: 4 });
}

#[test]
fn layout_via_fromstr() {
    let layout: Layout = "b25f,80x24,0,0,0".parse().unwrap();
    assert_eq!(layout.checksum, 0xb25f);
}

#[test]
fn layout_malformed_errors_not_panics() {
    for bad in [
        "",                                // empty
        "b25f",                            // no comma after checksum
        "zzzz,80x24,0,0,0",                // non-hex checksum
        "b25f,80x24,0,0",                  // missing pane id / split
        "b25f,80x24,0,0,",                 // trailing comma without pane id
        "b25f,80x,0,0,0",                  // missing height
        "b25f,80x24,0,0{79x24,0,0,1",      // unterminated split
        "b25f,80x24,0,0{}",                // empty split
        "b25f,80x24,0,0{79x24,0,0,1]",     // mismatched close bracket
        "b25f,80x24,0,0,0garbage",         // trailing garbage
        "b25f,80x24,0,0,0,1x1,0,0,1",      // trailing sibling with no enclosing split
    ] {
        assert!(parse_layout(bad).is_err(), "expected Err for {bad:?}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial-review regressions (M9 stage 1 hardening).
// ---------------------------------------------------------------------------

/// A remote-deliverable deeply nested layout must return `Err`, not overflow the stack.
#[test]
fn layout_nesting_depth_is_capped() {
    let deep = format!("0000,{}1x1,0,0,1{}", "1x1,0,0{".repeat(10_000), "}".repeat(10_000));
    let err = parse_layout(&deep).unwrap_err();
    assert!(err.contains("nesting"), "{err}");
    // A realistically deep (but sane) layout still parses.
    let ok = format!("0000,{}1x1,0,0,1{}", "1x1,0,0{".repeat(20), "}".repeat(20));
    assert!(parse_layout(&ok).is_ok());
}

/// Guards are recognized only at column 0: an indented `%end` inside a body (e.g. captured pane
/// text) must stay in the body instead of closing the block early.
#[test]
fn indented_guard_lookalikes_stay_in_reply_body() {
    let events = feed_all(b"%begin 1700000000 2 1\n  %end 0 2\nreal body\n%end 1700000000 2 1\n");
    assert_eq!(
        events,
        vec![Event::Reply {
            num: 2,
            body: vec!["  %end 0 2".into(), "real body".into()],
            error: false
        }],
    );
    // ...and an indented %begin outside a block is just an Unknown notification.
    let events = feed_all(b"  %begin 1 3 1\n");
    assert_eq!(
        events,
        vec![Event::Notification(Notification::Unknown { line: "  %begin 1 3 1".into() })],
    );
}

/// Reply body lines are raw bytes: non-UTF-8 content (capture-pane of a binary-spewing pane)
/// survives verbatim instead of being replaced with U+FFFD.
#[test]
fn reply_body_preserves_raw_bytes() {
    let mut p = Parser::new();
    let mut input = b"%begin 1 5 1\nraw ".to_vec();
    input.push(0xFF);
    input.extend_from_slice(b" bytes\n%end 1 5 1\n");
    let events = p.feed(&input);
    let expected_line = {
        let mut l = b"raw ".to_vec();
        l.push(0xFF);
        l.extend_from_slice(b" bytes");
        l
    };
    assert_eq!(events, vec![Event::Reply { num: 5, body: vec![expected_line], error: false }]);
}

/// An unterminated line past MAX_LINE is discarded (bounded memory) and surfaced as a note once
/// its newline finally arrives; framing then resumes cleanly.
#[test]
fn overlong_unterminated_line_is_discarded_not_buffered() {
    let mut p = Parser::new();
    // Stream > MAX_LINE bytes with no newline, in chunks.
    let chunk = vec![b'a'; 256 * 1024];
    for _ in 0..5 {
        assert!(p.feed(&chunk).is_empty());
    }
    // The newline ends the overlong line -> one Unknown note; the next line parses normally.
    let events = p.feed(b"\n%window-add @9\n");
    assert_eq!(
        events,
        vec![
            Event::Notification(Notification::Unknown {
                line: "<gmux-tmux: overlong line discarded>".into()
            }),
            Event::Notification(Notification::WindowAdd { window: 9 }),
        ],
    );
}
