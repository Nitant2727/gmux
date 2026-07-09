//! Minimal isolation harness for the Windows STATUS_ACCESS_VIOLATION.
//! Run stages one at a time via arg to find exactly what crashes.
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::terminal::Options;
use libghostty_vt::Terminal;

fn dump(term: &Terminal) -> String {
    let opts = FormatterOptions::new().with_format(Format::Plain).with_trim(true);
    let mut f = Formatter::new(term, opts).expect("formatter new");
    let bytes = f.format_alloc(None).expect("format_alloc");
    String::from_utf8_lossy(bytes.as_ref()).into_owned()
}

fn main() {
    let stage: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    eprintln!("stage {stage}: start");

    eprintln!("  creating terminal");
    let mut term = Terminal::new(Options { cols: 40, rows: 8, max_scrollback: 1000 }).expect("new");
    eprintln!("  terminal created; cols={:?} rows={:?}", term.cols(), term.rows());

    if stage >= 1 {
        eprintln!("  vt_write plain");
        term.vt_write(b"plain line one\r\n");
        eprintln!("  vt_write plain OK");
    }
    if stage >= 2 {
        eprintln!("  vt_write OSC2 title");
        term.vt_write(b"\x1b]2;My Title\x1b\\");
        eprintln!("  OSC2 OK; title={:?}", term.title());
    }
    if stage >= 3 {
        eprintln!("  vt_write OSC9");
        term.vt_write(b"\x1b]9;hello\x07");
        eprintln!("  OSC9 OK");
    }
    if stage >= 4 {
        eprintln!("  vt_write OSC777");
        term.vt_write(b"\x1b]777;notify;Title;Body\x07");
        eprintln!("  OSC777 OK");
    }
    if stage >= 5 {
        eprintln!("  vt_write OSC99");
        term.vt_write(b"\x1b]99;;kitty\x1b\\");
        eprintln!("  OSC99 OK");
    }
    if stage >= 6 {
        eprintln!("  vt_write SGR");
        term.vt_write(b"\x1b[1;32mgreen\x1b[0m\r\n");
        eprintln!("  SGR OK");
    }
    if stage >= 7 {
        eprintln!("  dump_screen via Formatter");
        let g = dump(&term);
        eprintln!("  dump OK ({} bytes)", g.len());
        for (i, l) in g.lines().enumerate() { eprintln!("    row {i}: {l:?}"); }
    }
    if stage >= 8 {
        eprintln!("  resize 10x6");
        term.resize(10, 6, 8, 16).expect("resize");
        eprintln!("  resize OK; cols={:?} rows={:?}", term.cols(), term.rows());
    }
    eprintln!("stage {stage}: DONE");
}
