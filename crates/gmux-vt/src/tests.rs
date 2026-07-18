//! Tests for gmux-vt. Covers the M0 notification sequences, split-across-advance state, ST/BEL
//! terminator variants, OSC 9;4 progress, OSC 9;9 cwd, OSC 777 body-with-semicolons, OSC 99
//! multi-chunk reassembly, OSC 133 prompt marks, SGR color, and cursor tracking.

use super::*;

/// Collect only the notification/progress/cwd/title/mark/bell events (drop the trailing Damage)
/// for terser assertions.
fn non_damage(evs: Vec<TermEvent>) -> Vec<TermEvent> {
    evs.into_iter().filter(|e| !matches!(e, TermEvent::Damage)).collect()
}

fn first_notification(evs: &[TermEvent]) -> &Notification {
    evs.iter()
        .find_map(|e| match e {
            TermEvent::Notification(n) => Some(n),
            _ => None,
        })
        .expect("expected a Notification event")
}

// ---------------------------------------------------------------------------
// (1) The exact three M0 sequences.
// ---------------------------------------------------------------------------

#[test]
fn m0_osc9_notification() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]9;gmux osc9 message\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc9);
    assert_eq!(n.title, "gmux osc9 message");
    assert_eq!(n.body, "");
}

#[test]
fn m0_osc777_notification() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]777;notify;T;B\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc777);
    assert_eq!(n.title, "T");
    assert_eq!(n.body, "B");
}

#[test]
fn m0_osc99_notification() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]99;i=1:p=title;hi\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc99);
    assert_eq!(n.title, "hi");
    assert_eq!(n.id.as_deref(), Some("1"));
}

// ---------------------------------------------------------------------------
// (2) OSC split across two advance() calls.
// ---------------------------------------------------------------------------

#[test]
fn osc_split_across_advance() {
    let mut t = Terminal::new(80, 24);
    let first = non_damage(t.advance(b"\x1b]9;hel"));
    // No complete OSC yet -> no notification.
    assert!(first.iter().all(|e| !matches!(e, TermEvent::Notification(_))));
    let second = non_damage(t.advance(b"lo\x07"));
    let n = first_notification(&second);
    assert_eq!(n.kind, NotifyKind::Osc9);
    assert_eq!(n.title, "hello");
}

// ---------------------------------------------------------------------------
// (3) ST terminator variant (ESC-backslash) for each.
// ---------------------------------------------------------------------------

#[test]
fn st_terminator_osc9() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]9;st term msg\x1b\\"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc9);
    assert_eq!(n.title, "st term msg");
}

#[test]
fn st_terminator_osc777() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]777;notify;Title;Body\x1b\\"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc777);
    assert_eq!(n.title, "Title");
    assert_eq!(n.body, "Body");
}

#[test]
fn st_terminator_osc99() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]99;i=7:p=title;kittytitle\x1b\\"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc99);
    assert_eq!(n.title, "kittytitle");
    assert_eq!(n.id.as_deref(), Some("7"));
}

// ---------------------------------------------------------------------------
// (4) OSC 9;4 progress.
// ---------------------------------------------------------------------------

#[test]
fn osc9_4_progress_set_with_pct() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]9;4;1;42\x07"));
    assert!(evs.contains(&TermEvent::Progress { state: ProgressState::Set, pct: Some(42) }));
}

#[test]
fn osc9_4_progress_remove() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]9;4;0\x07"));
    assert!(evs.contains(&TermEvent::Progress { state: ProgressState::Remove, pct: None }));
}

// ---------------------------------------------------------------------------
// (5) OSC 9;9 cwd (Windows-style quoted path).
// ---------------------------------------------------------------------------

#[test]
fn osc9_9_cwd() {
    let mut t = Terminal::new(80, 24);
    // \x1b]9;9;"C:\repo"\x07  (the shell would send literal backslashes)
    let evs = non_damage(t.advance(b"\x1b]9;9;\"C:\\repo\"\x07"));
    assert!(evs.contains(&TermEvent::Cwd("C:\\repo".to_string())));
}

// ---------------------------------------------------------------------------
// (6) OSC 777 body containing semicolons kept intact.
// ---------------------------------------------------------------------------

#[test]
fn osc777_body_with_semicolons() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]777;notify;MyTitle;part1;part2;part3\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.kind, NotifyKind::Osc777);
    assert_eq!(n.title, "MyTitle");
    assert_eq!(n.body, "part1;part2;part3");
}

// ---------------------------------------------------------------------------
// (7) OSC 99 two-chunk reassembly.
// ---------------------------------------------------------------------------

#[test]
fn osc99_two_chunk() {
    let mut t = Terminal::new(80, 24);
    // Chunk 1: title, d=0 (more chunks follow).
    let a = non_damage(t.advance(b"\x1b]99;i=1:d=0;the title\x07"));
    assert!(a.iter().all(|e| !matches!(e, TermEvent::Notification(_))));
    // Chunk 2: body, d=1 (commit).
    let b = non_damage(t.advance(b"\x1b]99;i=1:p=body:d=1;the body\x07"));
    let n = first_notification(&b);
    assert_eq!(n.kind, NotifyKind::Osc99);
    assert_eq!(n.title, "the title");
    assert_eq!(n.body, "the body");
    assert_eq!(n.id.as_deref(), Some("1"));
}

// ---------------------------------------------------------------------------
// (8) OSC 133 prompt marks -> the four variants.
// ---------------------------------------------------------------------------

#[test]
fn osc133_prompt_marks() {
    let mut t = Terminal::new(80, 24);
    let a = non_damage(t.advance(b"\x1b]133;A\x07"));
    assert!(a.contains(&TermEvent::PromptMark(PromptMark::PromptStart)));

    let b = non_damage(t.advance(b"\x1b]133;B\x07"));
    assert!(b.contains(&TermEvent::PromptMark(PromptMark::CommandStart)));

    let c = non_damage(t.advance(b"\x1b]133;C\x07"));
    assert!(c.contains(&TermEvent::PromptMark(PromptMark::CommandExecuted)));

    let d = non_damage(t.advance(b"\x1b]133;D;0\x07"));
    assert!(d.contains(&TermEvent::PromptMark(PromptMark::CommandFinished(Some(0)))));
}

// ---------------------------------------------------------------------------
// (9) Plain text + SGR: red foreground.
// ---------------------------------------------------------------------------

#[test]
fn sgr_red_text() {
    let mut t = Terminal::new(80, 24);
    t.advance(b"\x1b[31mred\x1b[0m");
    let text = t.visible_text();
    assert!(text[0].starts_with("red"), "row0 = {:?}", text[0]);
    let cells = t.visible_cells();
    let fg = cells[0][0].fg;
    // Red-ish: red channel dominant.
    assert!(fg.r > fg.g && fg.r > fg.b, "expected red-ish fg, got {:?}", fg);
    assert!(fg.r >= 0x80, "expected strong red channel, got {:?}", fg);
}

// ---------------------------------------------------------------------------
// (9b) Runtime palette: set_palette re-themes named colors; default stays byte-identical.
// ---------------------------------------------------------------------------

#[test]
fn default_palette_matches_old_constants() {
    // The default palette must reproduce the historical hardcoded colors byte-for-byte.
    let p = Palette::default();
    assert_eq!(p.fg, Rgb { r: 0xcc, g: 0xcc, b: 0xcc });
    assert_eq!(p.bg, Rgb { r: 0x11, g: 0x11, b: 0x11 });
    assert_eq!(p.ansi[1], Rgb { r: 0x80, g: 0x00, b: 0x00 }); // red
    assert_eq!(p.ansi[15], Rgb { r: 0xff, g: 0xff, b: 0xff }); // bright white

    // SGR 31 with the default palette resolves to the old red constant exactly.
    let mut t = Terminal::new(80, 24);
    t.advance(b"\x1b[31mred\x1b[0m");
    assert_eq!(t.visible_cells()[0][0].fg, Rgb { r: 0x80, g: 0x00, b: 0x00 });
}

#[test]
fn set_palette_changes_named_color_resolution() {
    let mut t = Terminal::new(80, 24);
    let mut ansi = Palette::default().ansi;
    ansi[1] = Rgb { r: 0xde, g: 0xad, b: 0xbe }; // custom "red" (index 1)
    t.set_palette(Palette { fg: Rgb { r: 1, g: 2, b: 3 }, bg: Rgb { r: 4, g: 5, b: 6 }, ansi });

    // SGR 31 (Named::Red) now resolves to the custom red, not the default 0x800000.
    t.advance(b"\x1b[31mX\x1b[0m");
    assert_eq!(t.visible_cells()[0][0].fg, Rgb { r: 0xde, g: 0xad, b: 0xbe });
    // Computed 256-color entries (16..=255) stay untouched by the palette.
    t.advance(b"\x1b[38;5;196mY\x1b[0m");
    assert_eq!(t.visible_cells()[0][1].fg, Rgb { r: 0xff, g: 0x00, b: 0x00 });
}

// ---------------------------------------------------------------------------
// (10) Cursor position after writing text.
// ---------------------------------------------------------------------------

#[test]
fn cursor_after_text() {
    let mut t = Terminal::new(80, 24);
    t.advance(b"hello");
    // After writing "hello" the cursor sits at column 5, row 0.
    assert_eq!(t.cursor(), (5, 0));
}

// ---------------------------------------------------------------------------
// Extra coverage: bell, title, plain OSC 9 vs numeric disambiguation, indeterminate progress.
// ---------------------------------------------------------------------------

#[test]
fn bare_bell_event() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x07"));
    assert!(evs.contains(&TermEvent::Bell));
}

#[test]
fn osc2_title() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]2;gmux - claude\x07"));
    assert!(evs.contains(&TermEvent::Title("gmux - claude".to_string())));
}

#[test]
fn osc9_unknown_numeric_swallowed() {
    let mut t = Terminal::new(80, 24);
    // 9;5 is ConEmu "wait for keypress" — an unknown-to-us numeric subcommand: swallow.
    let evs = non_damage(t.advance(b"\x1b]9;5\x07"));
    assert!(evs.iter().all(|e| !matches!(e, TermEvent::Notification(_))));
}

#[test]
fn osc9_prompt_mark_12() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]9;12\x07"));
    assert!(evs.contains(&TermEvent::PromptMark(PromptMark::PromptStart)));
}

#[test]
fn osc7_cwd_file_uri() {
    let mut t = Terminal::new(80, 24);
    let evs = non_damage(t.advance(b"\x1b]7;file://HOST/C:/repo/sub\x1b\\"));
    assert!(evs.contains(&TermEvent::Cwd("C:\\repo\\sub".to_string())), "evs = {:?}", evs);
}

#[test]
fn osc99_base64_payload() {
    let mut t = Terminal::new(80, 24);
    // "hello" base64 = aGVsbG8=
    let evs = non_damage(t.advance(b"\x1b]99;i=2:e=1:p=title;aGVsbG8=\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.title, "hello");
}

#[test]
fn osc99_single_shot_no_id() {
    let mut t = Terminal::new(80, 24);
    // Minimal single-shot form: OSC 99 ; ; text  (empty metadata, text = title).
    let evs = non_damage(t.advance(b"\x1b]99;;Hello world\x07"));
    let n = first_notification(&evs);
    assert_eq!(n.title, "Hello world");
    assert_eq!(n.id, None);
}

#[test]
fn damage_emitted_once_per_nonempty_advance() {
    let mut t = Terminal::new(80, 24);
    let evs = t.advance(b"abc");
    assert_eq!(evs.iter().filter(|e| matches!(e, TermEvent::Damage)).count(), 1);
    // Empty advance -> no damage.
    let empty = t.advance(b"");
    assert!(empty.is_empty());
}

#[test]
fn resize_updates_dims() {
    let mut t = Terminal::new(80, 24);
    assert_eq!((t.cols(), t.rows()), (80, 24));
    t.resize(100, 40);
    assert_eq!((t.cols(), t.rows()), (100, 40));
    assert_eq!(t.visible_cells().len(), 40);
    assert_eq!(t.visible_cells()[0].len(), 100);
}

#[test]
fn newline_moves_cursor_down() {
    let mut t = Terminal::new(80, 24);
    t.advance(b"ab\r\ncd");
    // Second line "cd" -> cursor at col 2, row 1.
    assert_eq!(t.cursor(), (2, 1));
    let text = t.visible_text();
    assert_eq!(text[0], "ab");
    assert_eq!(text[1], "cd");
}

// ---------------------------------------------------------------------------
// Scrollback / history (M8 scrollback viewport + capture-pane -S).
// ---------------------------------------------------------------------------

/// Write more lines than fit on-screen and confirm the overflow lands in scrollback history,
/// retrievable via `scrollback_text` and re-viewable via `cells_at_offset`.
#[test]
fn scrollback_retains_scrolled_off_lines() {
    let mut t = Terminal::new(20, 5); // tiny 5-row viewport
    // 12 numbered lines: the first ~7 scroll off the top.
    let mut input = String::new();
    for i in 1..=12 {
        input.push_str(&format!("line{i}\r\n"));
    }
    t.advance(input.as_bytes());

    // History now holds the lines pushed above the 5-row viewport.
    assert!(t.history_len() >= 7, "expected >=7 history lines, got {}", t.history_len());

    // Full scrollback (max_lines = 0) contains both the earliest and the latest content.
    let all = t.scrollback_text(0);
    assert!(all.iter().any(|l| l == "line1"), "earliest line missing from scrollback: {all:?}");
    assert!(all.iter().any(|l| l == "line12"), "latest line missing from scrollback: {all:?}");

    // Capping returns only the most-recent N lines.
    let last3 = t.scrollback_text(3);
    assert!(!last3.iter().any(|l| l == "line1"), "cap should drop oldest: {last3:?}");
    assert!(last3.iter().any(|l| l == "line12"), "cap must keep newest: {last3:?}");
}

/// `cells_at_offset(0)` equals the live viewport; a positive offset reveals scrolled-off history.
#[test]
fn cells_at_offset_scrolls_into_history() {
    let mut t = Terminal::new(20, 5);
    let mut input = String::new();
    for i in 1..=12 {
        input.push_str(&format!("line{i}\r\n"));
    }
    t.advance(input.as_bytes());

    // offset 0 == the live screen.
    let live = t.cells_at_offset(0);
    assert_eq!(live.len(), 5);
    let row_text = |cells: &Vec<Cell>| -> String {
        let s: String = cells.iter().map(|c| c.ch).collect();
        s.trim_end_matches(' ').to_string()
    };
    let live_top = row_text(&live[0]);

    // Scrolling up reveals older content than the live viewport's top row.
    let scrolled = t.cells_at_offset(3);
    assert_eq!(scrolled.len(), 5);
    let scrolled_top = row_text(&scrolled[0]);
    assert_ne!(scrolled_top, live_top, "scrolling should change the top row");

    // Over-scrolling is clamped, not a panic/out-of-range.
    let clamped = t.cells_at_offset(9999);
    assert_eq!(clamped.len(), 5);
}

// ---------------------------------------------------------------------------
// Wide (double-width CJK) cells.
// ---------------------------------------------------------------------------

/// A double-width char (U+4E2D 中) sets `wide` on its cell and leaves the following spacer cell
/// blank (`ch == ' '`), so the renderer/wire can size it across two columns.
#[test]
fn wide_char_flags_cell_and_leaves_spacer() {
    let mut t = Terminal::new(80, 24);
    t.advance("中".as_bytes()); // UTF-8: E4 B8 AD
    let row = &t.visible_cells()[0];
    assert_eq!(row[0].ch, '中');
    assert!(row[0].wide, "double-width char must set Cell.wide");
    assert!(!row[1].wide, "spacer cell is not itself wide");
    assert_eq!(row[1].ch, ' ', "the cell after a wide char is a blank spacer");
    // The cursor advances two columns past a wide glyph.
    assert_eq!(t.cursor(), (2, 0));
}

// ---------------------------------------------------------------------------
// Mouse-reporting mode (DECSET/DECRST 1000/1002/1003/1006).
// ---------------------------------------------------------------------------

/// DECSET turns on each mouse-reporting mode (mapped to its contract bit) and DECRST clears it.
/// Note: alacritty treats clicks/drag/motion (1000/1002/1003) as mutually exclusive — the latest
/// wins — while SGR (1006) is an orthogonal encoding flag that composes with any of them.
#[test]
fn mouse_mode_tracks_decset_and_decrst() {
    let mut t = Terminal::new(80, 24);
    assert_eq!(t.mouse_mode(), 0, "no mouse reporting by default");

    t.advance(b"\x1b[?1000h"); // clicks
    assert_eq!(t.mouse_mode(), 1);
    t.advance(b"\x1b[?1002h"); // button-drag (replaces clicks)
    assert_eq!(t.mouse_mode(), 2);
    t.advance(b"\x1b[?1003h"); // any-motion (replaces drag)
    assert_eq!(t.mouse_mode(), 4);

    // SGR encoding composes with the active motion mode: 4 | 8 = 12.
    t.advance(b"\x1b[?1006h");
    assert_eq!(t.mouse_mode(), 12);

    // DECRST the motion mode, leaving only the SGR encoding bit.
    t.advance(b"\x1b[?1003l");
    assert_eq!(t.mouse_mode(), 8);
    t.advance(b"\x1b[?1006l");
    assert_eq!(t.mouse_mode(), 0);
}

// ---------------------------------------------------------------------------
// DECSCUSR cursor style (CSI Ps SP q).
// ---------------------------------------------------------------------------

/// The raw DECSCUSR Ps is recoverable: default 0, `Ps 4` steady underline reads back 4, and `Ps 0`
/// resets to the default. `Ps 6` steady bar reads back 6.
#[test]
fn cursor_style_tracks_decscusr() {
    let mut t = Terminal::new(80, 24);
    assert_eq!(t.cursor_style(), 0, "default cursor style is 0 (block)");

    t.advance(b"\x1b[4 q"); // steady underline
    assert_eq!(t.cursor_style(), 4);

    t.advance(b"\x1b[6 q"); // steady bar
    assert_eq!(t.cursor_style(), 6);

    t.advance(b"\x1b[0 q"); // reset to default
    assert_eq!(t.cursor_style(), 0);
}
