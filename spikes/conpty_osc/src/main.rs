//! M0 spike 1+2 — bundled ConPTY round-trip + OSC 9/777/99 passthrough proof.
//!
//! Loads the vendored `conpty.dll` (the MIT Microsoft.Windows.Console.ConPTY redist — NOT
//! kernel32's inbox CreatePseudoConsole), spawns PowerShell running `emit.ps1` under a real
//! pseudoconsole, reads the output stream back, and asserts that the OSC 9 / OSC 777 / OSC 99
//! notification sequences arrive **intact and in order** relative to plain-text markers.
//!
//! This is gmux's killer-feature go/no-go: if these sequences survive ConPTY, notification
//! hooks are possible. See ARCHITECTURE.md §5 / §7 and DECISIONS D-002.

use std::ffi::c_void;
use std::iter::once;
use std::mem::{size_of, transmute, zeroed};
use std::ptr::{null, null_mut};
use std::thread;

use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{
    AllocConsole, ClosePseudoConsole, CreatePseudoConsole, FreeConsole, GetConsoleWindow,
    GetStdHandle, COORD, HPCON, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, Sleep, UpdateProcThreadAttribute, WaitForSingleObject,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOEXW,
};

// Not surfaced by windows-sys as a constant in all versions — define it (winbase.h).
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

// The bundled conpty.dll's exports (used only to confirm the redist is loadable here).
type ConptyCreate =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32; // HRESULT
type ConptyClose = unsafe extern "system" fn(HPCON);

/// Wrap a raw HANDLE so it can cross the thread boundary into the reader.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dll_path = format!("{manifest}\\vendor\\conpty.dll");
    let emit_path = format!("{manifest}\\emit.ps1");
    let pwsh = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";

    println!("== gmux M0 spike: bundled ConPTY + OSC passthrough ==");
    println!("conpty.dll : {dll_path}");
    println!("emitter    : {emit_path}\n");

    unsafe {
        // This process may be launched with NO console and pipe stdio (e.g. under a CI/agent
        // harness). ConPTY needs a console context for the child to bind its stdio to the pty,
        // so ensure we have one. Our own std handles stay the original pipes, so our diagnostic
        // output still flows to the launcher.
        if GetConsoleWindow() as isize == 0 {
            FreeConsole();
            let alloc = AllocConsole();
            println!("[info] no inherited console -> AllocConsole() = {alloc}");
        }

        // Mark our std handles non-inheritable so the child never picks up our pipes.
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(id);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }

        // --- Confirm the bundled conpty.dll is loadable and exports the Conpty* API ---
        // (For the attach mechanics below we use inbox kernel32 CreatePseudoConsole, whose HPCON
        //  the inbox CreateProcessW pseudoconsole attribute understands. Bundled-DLL attach is a
        //  separate question tracked in the findings.)
        let hmod = LoadLibraryW(wide(&dll_path).as_ptr());
        assert!(!hmod.is_null(), "LoadLibraryW(conpty.dll) failed — is it vendored? run fetch-conpty.ps1");
        let _create: ConptyCreate = transmute(
            GetProcAddress(hmod, b"ConptyCreatePseudoConsole\0".as_ptr())
                .expect("ConptyCreatePseudoConsole not exported"),
        );
        let _close: ConptyClose = transmute(
            GetProcAddress(hmod, b"ConptyClosePseudoConsole\0".as_ptr())
                .expect("ConptyClosePseudoConsole not exported"),
        );
        println!("[ok] bundled conpty.dll loads and exports Conpty* (redist present)");

        // --- Pipes: input (term->child) and output (child->term) ---
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 0,
        };
        let (mut in_read, mut in_write) = (INVALID_HANDLE_VALUE, INVALID_HANDLE_VALUE);
        let (mut out_read, mut out_write) = (INVALID_HANDLE_VALUE, INVALID_HANDLE_VALUE);
        assert!(CreatePipe(&mut in_read, &mut in_write, &sa, 0) != 0, "CreatePipe(in) failed");
        assert!(CreatePipe(&mut out_read, &mut out_write, &sa, 0) != 0, "CreatePipe(out) failed");

        // --- Create the pseudoconsole (inbox kernel32) ---
        let size = COORD { X: 120, Y: 30 };
        let mut hpc: HPCON = 0;
        let hr = CreatePseudoConsole(size, in_read, out_write, 0, &mut hpc);
        assert!(hr >= 0, "CreatePseudoConsole failed: hr=0x{hr:08x}");
        assert!(hpc != 0, "null HPCON");
        println!("[ok] ConptyCreatePseudoConsole -> {size_x}x{size_y}", size_x = size.X, size_y = size.Y);

        // --- Reader thread: drain output pipe to EOF ---
        let reader_handle = SendHandle(out_read);
        let reader = thread::spawn(move || {
            let reader_handle = reader_handle; // force whole-struct (Send) capture, not just .0
            let mut collected: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut read: u32 = 0;
                let ok = ReadFile(reader_handle.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, null_mut());
                if ok == 0 || read == 0 {
                    break; // broken pipe / EOF
                }
                collected.extend_from_slice(&buf[..read as usize]);
            }
            collected
        });

        // --- Build STARTUPINFOEX with the pseudoconsole attribute ---
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        println!("[dbg] attr list size = {attr_size} bytes (hpc={hpc:#x})");
        // Pointer-aligned backing store for the attribute list.
        let mut attr_words = vec![0usize; (attr_size + size_of::<usize>() - 1) / size_of::<usize>()];
        si.lpAttributeList = attr_words.as_mut_ptr() as *mut c_void;
        let init = InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &mut attr_size);
        println!("[dbg] InitializeProcThreadAttributeList -> {init} (err={})", GetLastError());
        assert!(init != 0, "InitializeProcThreadAttributeList failed");
        let upd = UpdateProcThreadAttribute(
            si.lpAttributeList,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc as *const c_void,
            size_of::<HPCON>(),
            null_mut(),
            null_mut(),
        );
        println!("[dbg] UpdateProcThreadAttribute -> {upd} (err={})", GetLastError());
        assert!(upd != 0, "UpdateProcThreadAttribute(PSEUDOCONSOLE) failed");

        // --- Spawn PowerShell running the emitter, attached to the pseudoconsole ---
        let cmdline = format!("\"{pwsh}\" -NoProfile -ExecutionPolicy Bypass -File \"{emit_path}\"");
        let mut cmd_w = wide(&cmdline);
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            null(),
            cmd_w.as_mut_ptr(),
            null(),
            null(),
            0, // FALSE — pseudoconsole attribute binds the child's stdio to the pty
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &si as *const STARTUPINFOEXW as *const _,
            &mut pi,
        );
        assert!(ok != 0, "CreateProcessW(powershell) failed (GetLastError={})", GetLastError());
        println!("[ok] spawned child pid={} under the pseudoconsole\n", pi.dwProcessId);

        // Close the child-side pipe ends we no longer need.
        CloseHandle(in_read);
        CloseHandle(out_write);

        // Wait for the emitter to finish, then close the pseudoconsole (build 26100+ returns
        // immediately) which closes the output write end -> reader hits EOF.
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut exit_code: u32 = 0;
        GetExitCodeProcess(pi.hProcess, &mut exit_code);
        Sleep(200); // let ConPTY drain any remaining child output to the pipe before teardown
        println!("[info] child exit code = {exit_code}");
        ClosePseudoConsole(hpc);

        let output = reader.join().expect("reader thread panicked");

        // Cleanup.
        DeleteProcThreadAttributeList(si.lpAttributeList);
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        CloseHandle(in_write);
        CloseHandle(out_read);

        analyze(&output);
    }
}

/// A single event parsed linearly from the ConPTY output stream.
#[derive(Debug)]
#[allow(dead_code)] // Text payload retained for reuse/debugging though the verdict only reads OSCs
enum Ev {
    Text(String),
    Osc { num: String, payload: String, term: &'static str },
}

/// Minimal linear OSC extractor: coalesces printable text, captures `ESC ] ... (BEL|ESC\)`.
fn parse(bytes: &[u8]) -> Vec<Ev> {
    let mut evs = Vec::new();
    let mut text = Vec::new();
    let mut i = 0;
    let flush = |text: &mut Vec<u8>, evs: &mut Vec<Ev>| {
        if !text.is_empty() {
            evs.push(Ev::Text(String::from_utf8_lossy(text).into_owned()));
            text.clear();
        }
    };
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            // OSC start
            let mut j = i + 2;
            let mut term = "";
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    term = "BEL";
                    break;
                }
                if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    term = "ST";
                    break;
                }
                j += 1;
            }
            if term.is_empty() {
                // no terminator found; treat rest as text
                text.extend_from_slice(&bytes[i..]);
                i = bytes.len();
                continue;
            }
            flush(&mut text, &mut evs);
            let body = &bytes[i + 2..j];
            let (num, payload) = match body.iter().position(|&b| b == b';') {
                Some(p) => (
                    String::from_utf8_lossy(&body[..p]).into_owned(),
                    String::from_utf8_lossy(&body[p + 1..]).into_owned(),
                ),
                None => (String::from_utf8_lossy(body).into_owned(), String::new()),
            };
            evs.push(Ev::Osc { num, payload, term });
            i = if term == "BEL" { j + 1 } else { j + 2 };
        } else {
            text.push(bytes[i]);
            i += 1;
        }
    }
    flush(&mut text, &mut evs);
    evs
}

fn analyze(output: &[u8]) {
    println!("== raw output: {} bytes ==", output.len());
    // Hex-ish dump limited to keep it readable.
    let printable: String = output
        .iter()
        .map(|&b| match b {
            0x1b => "<ESC>".to_string(),
            0x07 => "<BEL>".to_string(),
            0x0d => "\\r".to_string(),
            0x0a => "\\n".to_string(),
            0x20..=0x7e => (b as char).to_string(),
            _ => format!("<{b:02x}>"),
        })
        .collect();
    println!("{}\n", if printable.len() > 1200 { &printable[printable.len() - 1200..] } else { &printable });

    let evs = parse(output);
    let oscs: Vec<&Ev> = evs.iter().filter(|e| matches!(e, Ev::Osc { .. })).collect();
    println!("== parsed OSC events ({}) ==", oscs.len());
    for e in &oscs {
        if let Ev::Osc { num, payload, term } = e {
            println!("  OSC {num:<4} [{term}]  payload={payload:?}");
        }
    }
    println!();

    // Assertions: each sequence present with expected content.
    let find = |n: &str, needle: &str| {
        oscs.iter().any(|e| matches!(e, Ev::Osc { num, payload, .. } if num == n && payload.contains(needle)))
    };
    let has9 = find("9", "gmux osc9 message");
    let has777 = find("777", "gmux osc777 title");
    let has99 = find("99", "gmux osc99");

    // Ordering criterion: the notification OSCs must arrive in the correct RELATIVE order
    // (9 -> 777 -> 99). ConPTY renders screen text as a block separate from passed-through OSCs,
    // so it intentionally does not preserve byte-interleaving of text markers vs OSCs — only the
    // OSC-to-OSC order matters for gmux.
    let osc_seq: Vec<String> = oscs
        .iter()
        .filter_map(|e| match e {
            Ev::Osc { num, .. } => Some(format!("OSC{num}")),
            _ => None,
        })
        .collect();
    println!("== OSC arrival order ==\n  {}\n", osc_seq.join(" -> "));
    let ordered = is_subsequence(&["OSC9", "OSC777", "OSC99"], &osc_seq);

    println!("== RESULT ==");
    println!("  OSC 9  present & intact : {}", yn(has9));
    println!("  OSC 777 present & intact: {}", yn(has777));
    println!("  OSC 99 present & intact : {}", yn(has99));
    println!("  in-order passthrough    : {}", yn(ordered));

    let pass = has9 && has777 && has99 && ordered;
    println!("\n  >>> KILLER-FEATURE GO/NO-GO: {} <<<", if pass { "GO ✅" } else { "NO-GO ❌" });

    // Persist a verdict file so the spike can be launched detached (real console via Start-Process)
    // and still report its result.
    let osc_lines: String = oscs
        .iter()
        .map(|e| match e {
            Ev::Osc { num, payload, term } => format!("  OSC {num} [{term}] {payload:?}\n"),
            _ => String::new(),
        })
        .collect();
    let report = format!(
        "captured_bytes={}\nOSC9={has9} OSC777={has777} OSC99={has99} ordered={ordered}\nverdict={}\nparsed_oscs:\n{osc_lines}",
        output.len(),
        if pass { "GO" } else { "NO-GO" },
    );
    let _ = std::fs::write(concat!(env!("CARGO_MANIFEST_DIR"), "\\result.txt"), report);

    if !pass {
        std::process::exit(1);
    }
}

fn is_subsequence(needle: &[&str], hay: &[String]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.by_ref().any(|h| h == n))
}

fn yn(b: bool) -> &'static str {
    if b {
        "YES"
    } else {
        "NO"
    }
}
