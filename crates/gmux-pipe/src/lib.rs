//! gmux-pipe — a blocking Windows named-pipe server + client for the gmux automation API
//! (ARCHITECTURE §10 / D-005). Thread-per-connection; byte mode; same-user access only.
//!
//! # Implementation rules (from ARCHITECTURE §10 / docs/research/mux-architecture.md §e)
//! - Server instances: `CreateNamedPipeW(\\.\pipe\<name>, PIPE_ACCESS_DUPLEX [| FILE_FLAG_FIRST_PIPE_INSTANCE on the FIRST instance only], PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, PIPE_UNLIMITED_INSTANCES, 64*1024, 64*1024, 0, &sa)`.
//! - **Security descriptor — never NULL** (default DACL grants Everyone read): build one from SDDL
//!   `D:P(A;;GA;;;SY)(A;;GA;;;<current-user-SID>)` via `ConvertStringSecurityDescriptorToSecurityDescriptorW`
//!   (revision SDDL_REVISION_1 = 1). Get the current user SID string via
//!   `GetTokenInformation(TokenUser)` on the process token + `ConvertSidToStringSidW`. Free
//!   LocalAlloc'd memory with `LocalFree`.
//! - Accept loop: `ConnectNamedPipe(h, null)`; treat `ERROR_PIPE_CONNECTED` (535) as success
//!   (client connected between create and connect), and `ERROR_NO_DATA` (232) too (client
//!   connected *and closed* in that window — its buffered bytes must still reach the handler).
//!   On success, wrap the handle in `PipeStream`, spawn a handler thread, and create the next
//!   instance. On failure, close and retry (bounded backoff; do not spin).
//! - Client: `CreateFileW(\\.\pipe\<name>, GENERIC_READ|GENERIC_WRITE, 0, null, OPEN_EXISTING, 0, null)`.
//!   If `ERROR_PIPE_BUSY` (231), `WaitNamedPipeW` up to ~2s then retry once.
//! - `PipeStream`: Read = `ReadFile` (0 bytes read or broken pipe => Ok(0) EOF); Write = `WriteFile`
//!   in a loop; `flush` = `FlushFileBuffers`. `Drop` closes the handle. It is `Send`.
//! - No panics on I/O paths; map errors via `io::Error::from_raw_os_error(GetLastError())`.

use std::ffi::c_void;
use std::io;
use std::iter::once;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

fn pipe_path(name: &str) -> Vec<u16> {
    wide(&format!(r"\\.\pipe\{name}"))
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

/// The per-user pipe name: `<base>.<username>` with the username sanitized to
/// `[A-Za-z0-9_-]` (other chars become `_`). E.g. `"gmux"` -> `"gmux.Jeevan"`.
pub fn pipe_name_for_user(base: &str) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    let sanitized: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    format!("{base}.{sanitized}")
}

/// A connected duplex pipe stream (server or client side). Blocking I/O.
///
/// Owns the pipe `HANDLE`; the handle is closed on `Drop`.
#[derive(Debug)]
pub struct PipeStream {
    handle: HANDLE,
}

// The HANDLE is exclusively owned by this stream; access goes through &mut self / Drop.
unsafe impl Send for PipeStream {}

impl io::Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let mut read: u32 = 0;
        let ok = unsafe { ReadFile(self.handle, buf.as_mut_ptr(), len, &mut read, null_mut()) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_BROKEN_PIPE {
                return Ok(0); // peer closed the pipe: EOF
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        Ok(read as usize)
    }
}

impl io::Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut off = 0usize;
        while off < buf.len() {
            let chunk = (buf.len() - off).min(u32::MAX as usize) as u32;
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(self.handle, buf[off..].as_ptr(), chunk, &mut written, null_mut())
            };
            if ok == 0 {
                return Err(last_error());
            }
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "WriteFile wrote 0 bytes"));
            }
            off += written as usize;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if unsafe { FlushFileBuffers(self.handle) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

impl PipeStream {
    /// Duplicate the underlying handle so reads and writes can proceed through separate objects
    /// (e.g. a `BufReader` owns one side while responses are written through the other).
    pub fn try_clone(&self) -> io::Result<PipeStream> {
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        let mut new_handle: HANDLE = null_mut();
        let proc = unsafe { GetCurrentProcess() };
        let ok = unsafe {
            DuplicateHandle(proc, self.handle, proc, &mut new_handle, 0, 0, DUPLICATE_SAME_ACCESS)
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(PipeStream { handle: new_handle })
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// String form of a SID ("S-1-5-21-...") via `ConvertSidToStringSidW` (buffer LocalFree'd).
///
/// # Safety
/// `sid` must point to a valid SID.
unsafe fn string_sid(sid: PSID) -> io::Result<String> {
    let mut sid_w: *mut u16 = null_mut();
    if ConvertSidToStringSidW(sid, &mut sid_w) == 0 {
        return Err(last_error());
    }
    let mut n = 0usize;
    while *sid_w.add(n) != 0 {
        n += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_w, n));
    LocalFree(sid_w as HLOCAL);
    Ok(s)
}

/// String SID of the user this process runs as (via the process token).
fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_error());
        }
        let mut len: u32 = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        if len == 0 {
            let e = last_error();
            CloseHandle(token);
            return Err(e);
        }
        // u64-backed buffer so the TOKEN_USER view is properly aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut c_void, len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(last_error());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        string_sid(user.User.Sid)
    }
}

/// LocalAlloc'd self-relative security descriptor granting GENERIC_ALL to SYSTEM and the
/// current user only (protected DACL — no inherited ACEs, nothing for Everyone).
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// The descriptor is an immutable LocalAlloc'd blob; only read by CreateNamedPipeW.
unsafe impl Send for SecurityDescriptor {}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

fn same_user_security_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid()?;
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})"));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    if ok == 0 || sd.is_null() {
        return Err(last_error());
    }
    Ok(SecurityDescriptor(sd))
}

fn create_instance(path: &[u16], sd: &SecurityDescriptor, first: bool) -> io::Result<HANDLE> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let h = unsafe {
        CreateNamedPipeW(
            path.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            &sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(h)
}

/// State moved into the accept-loop thread. Wrapped in one struct so the raw-pointer fields
/// (`sd`, `first`) travel inside a single `Send` value — Rust 2021 disjoint capture would
/// otherwise try to capture the non-`Send` pointers individually.
struct AcceptCtx {
    path: Vec<u16>,
    sd: SecurityDescriptor,
    first: HANDLE,
}

// `first` is exclusively owned by the accept loop; `sd` is Send in its own right.
unsafe impl Send for AcceptCtx {}

/// A named-pipe server accepting connections on `\\.\pipe\<name>`.
///
/// Holds the accept-loop thread handle; the thread itself is detached (never joined) and runs
/// until the process exits.
pub struct PipeServer {
    _accept: thread::JoinHandle<()>,
}

impl PipeServer {
    /// Serve `\\.\pipe\<name>`: create an instance, block on `ConnectNamedPipe`, then hand the
    /// connected stream to `handler` on a fresh thread and immediately create the next
    /// instance. Runs until the process exits (the accept thread is detached).
    pub fn start<F>(name: &str, handler: F) -> io::Result<PipeServer>
    where
        F: Fn(PipeStream) + Send + Sync + 'static,
    {
        let path = pipe_path(name);
        let sd = same_user_security_descriptor()?;
        // Create the first instance synchronously so name-in-use and ACL errors surface here.
        let first = create_instance(&path, &sd, true)?;
        let handler = Arc::new(handler);
        let ctx = AcceptCtx { path, sd, first };
        let accept = thread::spawn(move || accept_loop(ctx, handler));
        Ok(PipeServer { _accept: accept })
    }
}

fn accept_loop<F>(ctx: AcceptCtx, handler: Arc<F>)
where
    F: Fn(PipeStream) + Send + Sync + 'static,
{
    const BACKOFF_START: Duration = Duration::from_millis(10);
    const BACKOFF_MAX: Duration = Duration::from_secs(1);
    let mut backoff = BACKOFF_START;
    let mut handle = ctx.first;
    loop {
        if handle == INVALID_HANDLE_VALUE {
            match create_instance(&ctx.path, &ctx.sd, false) {
                Ok(h) => handle = h,
                Err(_) => {
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            }
        }
        let ok = unsafe { ConnectNamedPipe(handle, null_mut()) };
        // ERROR_PIPE_CONNECTED (535): a client connected between create and connect — success.
        // ERROR_NO_DATA (232): a client connected *and already closed* in that window; its
        // written bytes are still buffered in the pipe, so hand the stream to the handler,
        // which drains them and then sees EOF.
        let connected = ok != 0 || {
            let err = unsafe { GetLastError() };
            err == ERROR_PIPE_CONNECTED || err == ERROR_NO_DATA
        };
        if connected {
            let stream = PipeStream { handle };
            handle = INVALID_HANDLE_VALUE;
            backoff = BACKOFF_START;
            let f = Arc::clone(&handler);
            thread::spawn(move || f(stream));
        } else {
            unsafe { CloseHandle(handle) };
            handle = INVALID_HANDLE_VALUE;
            thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Client side: open an existing pipe by name (`CreateFileW` on `\\.\pipe\<name>`).
///
/// If all server instances are busy (`ERROR_PIPE_BUSY`), waits up to ~2s for a free instance
/// via `WaitNamedPipeW` and retries once. A nonexistent pipe fails immediately.
pub fn client_connect(name: &str) -> io::Result<PipeStream> {
    let path = pipe_path(name);
    let mut waited = false;
    loop {
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(PipeStream { handle: h });
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY && !waited {
            waited = true;
            if unsafe { WaitNamedPipeW(path.as_ptr(), 2000) } != 0 {
                continue; // an instance freed up: retry once
            }
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn unique_name(tag: &str) -> String {
        format!(
            "gmux-pipe-test-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Handler used by the echo tests: read one line, write it back uppercased.
    fn uppercase_line_handler(stream: PipeStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let mut stream = reader.into_inner();
            let _ = stream.write_all(line.to_uppercase().as_bytes());
        }
    }

    #[test]
    fn pipe_name_includes_sanitized_username() {
        let name = pipe_name_for_user("gmux");
        assert!(name.starts_with("gmux."), "expected base prefix, got {name}");
        let user_part = &name["gmux.".len()..];
        assert!(!user_part.is_empty(), "username part must not be empty");
        assert!(
            user_part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "illegal chars in {user_part}"
        );
        // The sanitized current username must appear verbatim.
        let raw = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());
        let sanitized: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        assert_eq!(user_part, sanitized);
    }

    #[test]
    fn echo_round_trip() {
        let name = unique_name("echo");
        let _server = PipeServer::start(&name, uppercase_line_handler).unwrap();
        let mut client = client_connect(&name).unwrap();
        client.write_all(b"hello gmux\n").unwrap();
        let mut echoed = String::new();
        client.read_to_string(&mut echoed).unwrap();
        assert_eq!(echoed, "HELLO GMUX\n");
    }

    #[test]
    fn two_concurrent_clients_get_own_echo() {
        let name = unique_name("multi");
        let _server = PipeServer::start(&name, uppercase_line_handler).unwrap();
        let mut clients = Vec::new();
        for i in 0..2 {
            let name = name.clone();
            clients.push(thread::spawn(move || {
                let mut client = client_connect(&name).unwrap();
                let msg = format!("client {i} says hi\n");
                client.write_all(msg.as_bytes()).unwrap();
                let mut echoed = String::new();
                client.read_to_string(&mut echoed).unwrap();
                assert_eq!(echoed, msg.to_uppercase());
            }));
        }
        for c in clients {
            c.join().unwrap();
        }
    }

    #[test]
    fn large_payload_round_trips_intact() {
        const N: usize = 1024 * 1024; // 1 MiB — far larger than the 64 KiB pipe buffers
        let name = unique_name("large");
        let _server = PipeServer::start(&name, |mut stream: PipeStream| {
            let mut buf = vec![0u8; N];
            if stream.read_exact(&mut buf).is_ok() {
                let _ = stream.write_all(&buf);
            }
        })
        .unwrap();
        let payload: Vec<u8> = (0..N).map(|i| (i % 251) as u8).collect();
        let mut client = client_connect(&name).unwrap();
        client.write_all(&payload).unwrap();
        let mut echoed = vec![0u8; N];
        client.read_exact(&mut echoed).unwrap();
        assert!(echoed == payload, "1 MiB payload corrupted in round-trip");
    }

    #[test]
    fn connect_to_nonexistent_pipe_errors() {
        let err = client_connect(&unique_name("nonexistent")).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(2), "expected ERROR_FILE_NOT_FOUND, got {err}");
    }

    /// SECURITY: the DACL actually applied to a created pipe instance must contain exactly
    /// two ACCESS_ALLOWED ACEs — SYSTEM (S-1-5-18) and the current user — and nothing else.
    ///
    /// This is the no-null-fallback proof: `PipeServer::start` propagates any descriptor
    /// build failure with `?` (there is no code path that passes NULL), and this test reads
    /// the DACL back off the kernel object. Had a NULL descriptor slipped through, the pipe
    /// would carry the OS default DACL (5 ACEs incl. Everyone/Anonymous read), failing the
    /// AceCount == 2 assertion. A same-user client connect must still succeed.
    #[test]
    fn pipe_dacl_is_system_and_current_user_only() {
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
        use windows_sys::Win32::Security::{GetAce, ACCESS_ALLOWED_ACE, ACL, DACL_SECURITY_INFORMATION};

        let name = unique_name("acl");
        let sd = same_user_security_descriptor().unwrap();
        assert!(!sd.0.is_null(), "security descriptor must never be null");
        let server = create_instance(&pipe_path(&name), &sd, true).unwrap();

        let mut dacl: *mut ACL = null_mut();
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = unsafe {
            GetSecurityInfo(
                server,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut psd,
            )
        };
        assert_eq!(rc, 0, "GetSecurityInfo failed with Win32 error {rc}");
        assert!(!dacl.is_null(), "pipe has a NULL DACL — that would grant everyone full access");

        let mut sids = Vec::new();
        unsafe {
            assert_eq!((*dacl).AceCount, 2, "expected exactly SYSTEM + current user ACEs");
            for i in 0..u32::from((*dacl).AceCount) {
                let mut ace_ptr: *mut c_void = null_mut();
                assert_ne!(GetAce(dacl, i, &mut ace_ptr), 0, "GetAce({i}) failed");
                let ace = &*(ace_ptr as *const ACCESS_ALLOWED_ACE);
                // 0 == ACCESS_ALLOWED_ACE_TYPE (constant lives behind an unused cargo feature).
                assert_eq!(ace.Header.AceType, 0, "ACE {i} is not ACCESS_ALLOWED");
                sids.push(string_sid(&ace.SidStart as *const u32 as PSID).unwrap());
            }
            LocalFree(psd as HLOCAL);
        }
        sids.sort();
        let mut expected = vec!["S-1-5-18".to_string(), current_user_sid().unwrap()];
        expected.sort();
        assert_eq!(sids, expected, "DACL must grant exactly SYSTEM and the current user");

        // The strict DACL must still admit the same user (the listening instance is openable).
        let client = client_connect(&name);
        unsafe { CloseHandle(server) };
        client.expect("same-user client must be admitted by the DACL");
    }

    /// Client writes then drops immediately: the handler must observe clean EOF (Ok) rather
    /// than an error or panic, and must have received the bytes written before the drop.
    #[test]
    fn handler_sees_eof_when_client_drops_after_write() {
        let name = unique_name("eof");
        let (tx, rx) = std::sync::mpsc::channel();
        let _server = PipeServer::start(&name, move |mut stream: PipeStream| {
            let mut buf = Vec::new();
            let res = stream.read_to_end(&mut buf);
            let _ = tx.send((res.map_err(|e| e.to_string()), buf));
        })
        .unwrap();

        {
            let mut client = client_connect(&name).unwrap();
            client.write_all(b"parting words").unwrap();
        } // client handle closed here

        let (res, buf) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("handler panicked or hung instead of seeing EOF");
        assert_eq!(res, Ok(b"parting words".len()), "broken pipe must read as EOF, not error");
        assert_eq!(buf, b"parting words");
    }

    /// Sequential reconnects: five clients one after another must all round-trip, proving the
    /// accept loop recreates a fresh listening instance after each hand-off.
    #[test]
    fn five_sequential_clients_all_succeed() {
        let name = unique_name("seq");
        let _server = PipeServer::start(&name, uppercase_line_handler).unwrap();
        for i in 0..5 {
            let mut client = client_connect(&name)
                .unwrap_or_else(|e| panic!("reconnect {i} failed: {e}"));
            let msg = format!("round trip {i}\n");
            client.write_all(msg.as_bytes()).unwrap();
            let mut echoed = String::new();
            client.read_to_string(&mut echoed).unwrap();
            assert_eq!(echoed, msg.to_uppercase(), "round {i} corrupted");
        }
    }

    /// FILE_FLAG_FIRST_PIPE_INSTANCE: a second server on the same name must fail with
    /// ERROR_ACCESS_DENIED instead of silently coexisting (pipe-squatting protection).
    #[test]
    fn second_server_on_same_name_fails() {
        let name = unique_name("dup");
        let _server = PipeServer::start(&name, |_stream| {}).unwrap();
        // No unwrap_err: PipeServer is deliberately not Debug (contract has no such bound).
        let err = match PipeServer::start(&name, |_stream| {}) {
            Ok(_second) => panic!("second server on the same name must fail, but started"),
            Err(e) => e,
        };
        assert_eq!(
            err.raw_os_error(),
            Some(5),
            "expected ERROR_ACCESS_DENIED from FILE_FLAG_FIRST_PIPE_INSTANCE, got {err}"
        );
    }
}
