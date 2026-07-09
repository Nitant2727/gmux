//! Isolate the osc::Parser::end panic. The full run panicked with
//! "command must not be null: OutOfMemory" when end() was called with
//! terminator 0x1b (ESC, i.e. the start of an ST). Test which terminator byte
//! is the problem and whether a fresh parser per sequence matters.
use libghostty_vt::osc::Parser as OscParser;

fn classify(label: &str, payload: &[u8], terminator: u8) {
    // Fresh parser each call (matches the crate's intended one-shot usage).
    let mut p = OscParser::new().expect("osc parser new");
    for &b in payload {
        p.next_byte(b);
    }
    // Catch the panic so we can report per-case instead of aborting the process.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cmd = p.end(terminator);
        format!("{:?}", cmd.command_type())
    }));
    match res {
        Ok(kind) => println!("  {label} (term=0x{terminator:02x}) -> {kind}"),
        Err(_) => println!("  {label} (term=0x{terminator:02x}) -> PANIC (null command)"),
    }
}

fn main() {
    // Silence the default panic hook noise; we report ourselves.
    std::panic::set_hook(Box::new(|_| {}));

    println!("osc::Parser::end terminator matrix:");
    // 0x07 = BEL, 0x1b = ESC (start of ST), 0x5c = backslash (the ST final byte)
    classify("OSC 99 kitty  ", b"99;;kitty", 0x07);
    classify("OSC 99 kitty  ", b"99;;kitty", 0x1b);
    classify("OSC 99 kitty  ", b"99;;kitty", 0x5c);
    classify("OSC 2  title  ", b"2;My Title", 0x07);
    classify("OSC 2  title  ", b"2;My Title", 0x1b);
    classify("OSC 9  notify ", b"9;hello", 0x07);
    classify("OSC 9  notify ", b"9;hello", 0x1b);
    classify("OSC 777 notify", b"777;notify;T;B", 0x07);
    classify("OSC 777 notify", b"777;notify;T;B", 0x1b);
}
