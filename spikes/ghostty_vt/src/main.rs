//! SPIKE 4 — libghostty-vt 0.2.0 build + behavior on Windows (gmux ADR-003).
//!
//! Empirically answers: does libghostty-vt build on Windows x64 with Zig 0.15.2,
//! does it surface OSC 9 / 777 / 99 to the embedder, does reflow work, and are
//! there any Windows-specific breakages?
//!
//! The key gmux requirement is: surface OSC 9 (notification), OSC 777 (notify),
//! and OSC 99 (kitty notification) to the embedder so hooks can fire.
//!
//! ============================ EMPIRICAL RESULTS ============================
//! Env: Windows 11 x64, Rust 1.96.1 (MSVC), Zig 0.15.2, libghostty-vt 0.2.0,
//!      libghostty-vt-sys 0.2.0 (vendors ghostty git commit fdbf9ff3, static lib).
//!
//! (a) BUILD: SUCCEEDS on Windows x64. sys build.rs fetches ghostty, builds the
//!     VT core with the local Zig, links ghostty-vt-static.lib. ~4 min cold.
//!
//! (b) RUNTIME crash #1 (Zig Debug mode == cargo `dev` profile default):
//!     the FIRST `vt_write` call segfaults with STATUS_ACCESS_VIOLATION
//!     (0xC0000005). Setting LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast makes the
//!     crash disappear and everything below works. So a plain `cargo build`
//!     produces a binary that crashes on the first byte fed in; you MUST force
//!     a Release-mode Zig build via that env var.
//!
//! (c) OSC surfacing to the embedder: there is NO on_desktop_notification /
//!     OSC-9 / OSC-777 / OSC-99 Terminal callback. Full effect set is
//!     on_pty_write, on_bell, on_enquiry, on_xtversion, on_title_changed,
//!     on_pwd_changed, on_size, on_color_scheme, on_device_attributes.
//!     Through vt_write, OSC 9/777/99 are silently consumed (OSC 9;9;<path>
//!     would fire on_pwd_changed; OSC 9;<msg> notification does not).
//!     The ONLY way to see them is the standalone osc::Parser, which yields
//!     CommandType::ShowDesktopNotification -- a UNIT variant with NO title/body
//!     payload, and OSC 9 vs OSC 777 are indistinguishable.
//!
//! (d) RUNTIME crash #2 (osc::Parser + OSC 99): OscParser::end() returns a null
//!     command for any OSC 99 sequence (any terminator), which the safe wrapper
//!     turns into `panic!("command must not be null: OutOfMemory")`
//!     (osc.rs:83). OSC 9 and OSC 777 parse fine. So the standalone-parser
//!     path -- the only path that surfaces notifications -- panics on OSC 99.
//!
//! (e) REFLOW: WORKS WELL. 20x4 -> 10x6 -> 50x4 all reflow wrapped content
//!     correctly. This is a genuine advantage over alacritty_terminal.
//!
//! (f) fmt::Formatter(Plain) reads back the visible grid correctly.
//!
//! CONCLUSION: partial. Builds and (in Release) mostly runs on Windows, and
//! reflow is a real win, but it does NOT deliver the gmux killer feature:
//! OSC 9/777/99 are not surfaced with usable payload, OSC 99 panics the parser,
//! and the default cargo profile crashes at runtime. gmux stays on
//! alacritty_terminal + a side vte OSC parser for M0.
//! ==========================================================================

use std::cell::RefCell;

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::osc::Parser as OscParser;
use libghostty_vt::terminal::Options;
use libghostty_vt::Terminal;

/// Dump the terminal's active screen as plain text via the fmt::Formatter.
fn dump_screen(term: &Terminal, trim: bool) -> String {
    let opts = FormatterOptions::new()
        .with_format(Format::Plain)
        .with_trim(trim);
    let mut f = Formatter::new(term, opts).expect("formatter new");
    let bytes = f.format_alloc(None).expect("format_alloc");
    String::from_utf8_lossy(bytes.as_ref()).into_owned()
}

/// Run a byte stream that is a single OSC body (without the leading ESC ] and
/// without the terminator) through a standalone osc::Parser and report the
/// CommandType. `payload` is everything between "ESC ]" and the terminator.
fn classify_osc(payload: &[u8], terminator: u8) -> String {
    let mut p = OscParser::new().expect("osc parser new");
    for &b in payload {
        p.next_byte(b);
    }
    let cmd = p.end(terminator);
    format!("{:?}", cmd.command_type())
}

fn main() {
    println!("=== SPIKE 4: libghostty-vt 0.2.0 on Windows x64 ===\n");
    println!(
        "build_info: version_string = {:?}",
        libghostty_vt::build_info::version_string()
    );
    println!();

    // ----------------------------------------------------------------------
    // 1. Create the Terminal and register EVERY available effect callback so we
    //    can observe exactly which OSCs are surfaced to the embedder.
    // ----------------------------------------------------------------------
    let bell_count = RefCell::new(0usize);
    let title_events = RefCell::new(Vec::<String>::new());
    let pwd_events = RefCell::new(Vec::<String>::new());
    let pty_writes = RefCell::new(Vec::<Vec<u8>>::new());

    let mut term = Terminal::new(Options {
        cols: 40,
        rows: 8,
        max_scrollback: 1000,
    })
    .expect("terminal new");

    term.on_bell({
        let bell_count = &bell_count;
        move |_t| *bell_count.borrow_mut() += 1
    })
    .expect("on_bell")
    .on_title_changed({
        let title_events = &title_events;
        move |t| {
            let title = t.title().unwrap_or("").to_owned();
            title_events.borrow_mut().push(title);
        }
    })
    .expect("on_title_changed")
    .on_pwd_changed({
        let pwd_events = &pwd_events;
        move |t| {
            let pwd = t.pwd().unwrap_or("").to_owned();
            pwd_events.borrow_mut().push(pwd);
        }
    })
    .expect("on_pwd_changed")
    .on_pty_write({
        let pty_writes = &pty_writes;
        move |_t, data| pty_writes.borrow_mut().push(data.to_vec())
    })
    .expect("on_pty_write");

    // NOTE: There is intentionally NO `on_desktop_notification` (OSC 9/777) or
    // `on_kitty_notification` (OSC 99) callback in the libghostty-vt 0.2.0 API.
    // The full callback set is: on_pty_write, on_bell, on_enquiry, on_xtversion,
    // on_title_changed, on_pwd_changed, on_size, on_color_scheme,
    // on_device_attributes. This is the crux of the ADR finding.

    // ----------------------------------------------------------------------
    // 2. Feed a representative byte stream through vt_write.
    // ----------------------------------------------------------------------
    println!("--- feeding vt_write stream (plain text, OSC 2 title, OSC 9/777/99, SGR) ---");

    // Plain text line
    term.vt_write(b"plain line one\r\n");

    // OSC 2 - set window title (KNOWN to be surfaced via on_title_changed)
    term.vt_write(b"\x1b]2;My Title\x1b\\");

    // OSC 9 - desktop notification: ESC ] 9 ; hello BEL
    term.vt_write(b"\x1b]9;hello\x07");

    // OSC 777 - notify;Title;Body : ESC ] 777 ; notify ; Title ; Body BEL
    term.vt_write(b"\x1b]777;notify;Title;Body\x07");

    // OSC 99 - kitty notification: ESC ] 99 ; ; kitty ESC \
    term.vt_write(b"\x1b]99;;kitty\x1b\\");

    // OSC 7 - set pwd (to confirm on_pwd_changed works, contrast w/ OSC 9)
    term.vt_write(b"\x1b]7;file://localhost/tmp/gmux\x1b\\");

    // A couple of SGR-colored lines
    term.vt_write(b"\x1b[1;32mgreen bold\x1b[0m\r\n");
    term.vt_write(b"\x1b[38;2;255;0;0mtruecolor red\x1b[0m\r\n");
    term.vt_write(b"last visible line");

    println!();
    println!("Observed effect callbacks after vt_write stream:");
    println!("  on_bell count      : {}", bell_count.borrow());
    println!("  on_title_changed   : {:?}", title_events.borrow());
    println!("  on_pwd_changed     : {:?}", pwd_events.borrow());
    println!(
        "  on_pty_write count : {} (responses back to pty)",
        pty_writes.borrow().len()
    );
    println!("  term.title()       : {:?}", term.title());
    println!("  term.pwd()         : {:?}", term.pwd());
    println!();
    println!(
        ">>> OSC 9 / 777 / 99 produced NO dedicated embedder callback\n    (there is no on_desktop_notification in the 0.2.0 Terminal API).\n    They are consumed by vt_write and, unless they also change title/pwd,\n    are NOT surfaced to gmux via the Terminal effect API.\n"
    );

    // ----------------------------------------------------------------------
    // 3. Read back the visible grid text via fmt::Formatter (Plain).
    // ----------------------------------------------------------------------
    println!("--- visible grid (Plain, trimmed) at 40x8 ---");
    let grid = dump_screen(&term, true);
    for (i, line) in grid.lines().enumerate() {
        println!("  row {i:>2}: {line:?}");
    }
    println!();

    // ----------------------------------------------------------------------
    // 4. Exercise reflow: resize smaller then larger. Write a long wrapping
    //    line so we can observe reflow behavior.
    // ----------------------------------------------------------------------
    println!("--- reflow test ---");
    let mut rterm = Terminal::new(Options {
        cols: 20,
        rows: 4,
        max_scrollback: 1000,
    })
    .expect("terminal new (reflow)");
    // Enable wraparound (default on) and write a line longer than 20 cols.
    rterm.vt_write(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789wraps-around-here");
    println!("  initial 20x4:");
    for (i, line) in dump_screen(&rterm, true).lines().enumerate() {
        println!("    row {i:>2}: {line:?}");
    }

    // Resize smaller (narrower) -> should reflow the wrapped content.
    rterm.resize(10, 6, 8, 16).expect("resize smaller");
    println!("  after resize to 10x6:");
    println!(
        "    cols={:?} rows={:?}",
        rterm.cols(),
        rterm.rows()
    );
    for (i, line) in dump_screen(&rterm, true).lines().enumerate() {
        println!("    row {i:>2}: {line:?}");
    }

    // Resize larger (wider) -> should reflow back to fewer, longer lines.
    rterm.resize(50, 4, 8, 16).expect("resize larger");
    println!("  after resize to 50x4:");
    println!(
        "    cols={:?} rows={:?}",
        rterm.cols(),
        rterm.rows()
    );
    for (i, line) in dump_screen(&rterm, true).lines().enumerate() {
        println!("    row {i:>2}: {line:?}");
    }
    println!();

    // ----------------------------------------------------------------------
    // 5. THE KEY TEST: run the standalone osc::Parser directly on each OSC body
    //    to see what CommandType it yields. This is the ONLY path by which gmux
    //    could surface OSC 9 / 777 / 99 with libghostty-vt.
    // ----------------------------------------------------------------------
    println!("--- standalone osc::Parser classification (payload = bytes after 'ESC ]') ---");
    // For each: payload excludes the leading "ESC ]" and the terminator.
    let cases: &[(&str, &[u8], u8)] = &[
        ("OSC 2  (title)       ", b"2;My Title", 0x07),
        ("OSC 9  (notify)      ", b"9;hello", 0x07),
        ("OSC 777 (notify;T;B) ", b"777;notify;Title;Body", 0x07),
        ("OSC 99 (kitty notif) ", b"99;;kitty", 0x1b), // ST = ESC \, terminator byte reported as ESC
        ("OSC 7  (pwd)         ", b"7;file://localhost/tmp/gmux", 0x07),
        ("OSC 8  (hyperlink)   ", b"8;;https://example.com", 0x07),
        ("OSC 9;4 (conemu prog)", b"9;4;1;50", 0x07),
    ];
    for (label, payload, term_byte) in cases {
        let kind = classify_osc(payload, *term_byte);
        println!("  {label}-> {kind}");
    }
    println!();
    println!(
        ">>> osc::Parser DOES parse OSC 9/777/99, but CommandType::ShowDesktopNotification\n    is a UNIT variant: the notification title/body payload is NOT exposed by the\n    safe 0.2.0 wrapper, and OSC 9 vs OSC 777 are indistinguishable at this layer.\n"
    );

    println!("=== SPIKE 4 complete: built and ran on Windows x64 ===");
}
