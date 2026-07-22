//! gmux-pty — ConPTY-backed pseudo-terminal for a single pane.
//!
//! Productizes the M0 spike ([`spikes/conpty_osc`]) into a reusable type: [`Pty::spawn`] creates a
//! pseudoconsole, launches a shell attached to it inside a kill-on-close [job object], and streams
//! the child's output over an mpsc channel. Write input with [`Pty::write`], resize with
//! [`Pty::resize`]; dropping the [`Pty`] tears down the pseudoconsole and kills the process tree.
//!
//! ## The console requirement (learned in M0)
//!
//! A ConPTY child only binds its stdio to the pseudoconsole if the creating process has a real
//! console. A GUI/daemon process launched without one (windows subsystem, or under a CI/agent
//! harness with pipe stdio) must first obtain a console — [`ensure_console`] does this once:
//! `AllocConsole`, hide its window, and repoint our std handles at `CONOUT$`/`CONIN$` so the child
//! inherits console-backed stdio rather than our pipes.

use std::ffi::c_void;
use std::io;
use std::iter::once;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Once;
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    AllocConsole, AttachConsole, ClosePseudoConsole, CreatePseudoConsole, GetConsoleMode,
    GetConsoleWindow, GetStdHandle, ResizePseudoConsole, SetStdHandle, ATTACH_PARENT_PROCESS,
    COORD, HPCON, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOEXW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Pseudoconsole dimensions in character cells.
#[derive(Clone, Copy, Debug)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

impl PtySize {
    fn coord(self) -> COORD {
        COORD { X: self.cols.max(1) as i16, Y: self.rows.max(1) as i16 }
    }
}

/// A running pseudoconsole + its attached child process tree.
pub struct Pty {
    hpc: HPCON,
    input_write: HANDLE,
    process: HANDLE,
    thread: HANDLE,
    job: HANDLE,
    reader: Option<JoinHandle<()>>,
}

// The contained HANDLEs are owned by this Pty; access is synchronized by &self/Drop.
unsafe impl Send for Pty {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(once(0)).collect()
}

/// Build a UTF-16 environment block = this process's env with `additions` applied, double-null
/// terminated and sorted case-insensitively (as `CreateProcessW` expects with
/// `CREATE_UNICODE_ENVIRONMENT`).
fn build_env_block(additions: &[(String, String)]) -> Vec<u16> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, String> = std::env::vars().collect();
    for (k, v) in additions {
        map.insert(k.clone(), v.clone());
    }
    let mut entries: Vec<(String, String)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.to_uppercase().cmp(&b.0.to_uppercase()));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in entries {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0); // final double-null terminator
    block
}

fn last_err(context: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{context} (GetLastError={})", unsafe {
        GetLastError()
    }))
}

static CONSOLE_INIT: Once = Once::new();

/// Ensure this process has a console so ConPTY children bind their stdio to the pseudoconsole.
///
/// Idempotent and safe to call from any process. If we already have a console this is a no-op.
/// Otherwise it allocates one, hides the window, and repoints std handles at the console.
pub fn ensure_console() {
    CONSOLE_INIT.call_once(|| unsafe {
        if !GetConsoleWindow().is_null() {
            // A console exists, but the std handles may not point at it — a parent that spawned
            // us with redirected/null stdio (CREATE_NO_WINDOW still creates a console) leaves
            // stdout as a pipe/NUL, and ConPTY children only bind their stdio when the host's
            // stdout IS a console (the M0 finding). Repair the handles instead of trusting them.
            let mut mode = 0u32;
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            if stdout != INVALID_HANDLE_VALUE && GetConsoleMode(stdout, &mut mode) != 0 {
                return; // stdout really is a console: nothing to do
            }
            repoint_std_handles();
            return;
        }
        if AllocConsole() == 0 {
            return; // best effort; spawn may still work if a console appears elsewhere
        }
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            ShowWindow(hwnd as _, SW_HIDE);
        }
        repoint_std_handles();
    });
}

/// Attach to the parent process's console, if it has one — how a `windows`-subsystem gmux gets
/// its CLI output back to the terminal the user typed into. A GUI-subsystem process starts with
/// no console at all, so `gmux list-panes` would otherwise print into the void.
///
/// Std handles that are already valid are left alone: a redirected handle (`gmux list-panes |
/// findstr x`, an agent capturing output) is a pipe the caller set up on purpose, and repointing
/// it at the console would steal the output back from them. Only missing handles are filled in.
/// No parent console (an Explorer double-click) is the GUI path: the call fails and nothing
/// happens, which is exactly right — there is nowhere to print to.
pub fn attach_parent_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let missing = |h: HANDLE| h.is_null() || h == INVALID_HANDLE_VALUE;
        if missing(GetStdHandle(STD_OUTPUT_HANDLE)) || missing(GetStdHandle(STD_ERROR_HANDLE)) {
            let conout = CreateFileW(
                wide("CONOUT$").as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                share,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if conout != INVALID_HANDLE_VALUE {
                if missing(GetStdHandle(STD_OUTPUT_HANDLE)) {
                    SetStdHandle(STD_OUTPUT_HANDLE, conout);
                }
                if missing(GetStdHandle(STD_ERROR_HANDLE)) {
                    SetStdHandle(STD_ERROR_HANDLE, conout);
                }
            }
        }
        if missing(GetStdHandle(STD_INPUT_HANDLE)) {
            let conin = CreateFileW(
                wide("CONIN$").as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                share,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            );
            if conin != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_INPUT_HANDLE, conin);
            }
        }
    }
}

/// Point the std handles at the process's console (`CONOUT$`/`CONIN$`).
unsafe fn repoint_std_handles() {
    unsafe {
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let conout = CreateFileW(
            wide("CONOUT$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            share,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        let conin = CreateFileW(
            wide("CONIN$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            share,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        if conout != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, conout);
            SetStdHandle(STD_ERROR_HANDLE, conout);
        }
        if conin != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, conin);
        }
    }
}

impl Pty {
    /// Spawn `command_line` attached to a new pseudoconsole of `size`, inheriting this process's
    /// environment. See [`Pty::spawn_with_env`] to add variables.
    pub fn spawn(command_line: &str, size: PtySize) -> io::Result<(Pty, Receiver<Vec<u8>>)> {
        Self::spawn_with_env(command_line, size, &[])
    }

    /// Spawn `command_line` with `env` variables added to (or overriding) the inherited environment
    /// — e.g. `GMUX_PANE`. Returns the handle plus a channel yielding raw output chunks until the
    /// child exits (the channel closes on EOF).
    pub fn spawn_with_env(
        command_line: &str,
        size: PtySize,
        env: &[(String, String)],
    ) -> io::Result<(Pty, Receiver<Vec<u8>>)> {
        Self::spawn_full(command_line, size, env, None)
    }

    /// Like [`Pty::spawn_with_env`] but also sets the child's working directory (`cwd`) — used by
    /// session restore to reopen shells in their saved directory.
    pub fn spawn_full(
        command_line: &str,
        size: PtySize,
        env: &[(String, String)],
        cwd: Option<&str>,
    ) -> io::Result<(Pty, Receiver<Vec<u8>>)> {
        ensure_console();
        // Keep the env block + cwd alive for the whole CreateProcessW call.
        let mut env_block = if env.is_empty() { Vec::new() } else { build_env_block(env) };
        let cwd_w = cwd.filter(|c| !c.is_empty()).map(wide);
        let cwd_ptr = cwd_w.as_ref().map(|v| v.as_ptr()).unwrap_or(null());
        let (env_ptr, env_flag) = if env_block.is_empty() {
            (null(), 0)
        } else {
            (env_block.as_mut_ptr() as *const c_void, CREATE_UNICODE_ENVIRONMENT)
        };
        unsafe {
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 0,
            };
            let (mut in_read, mut in_write) = (INVALID_HANDLE_VALUE, INVALID_HANDLE_VALUE);
            let (mut out_read, mut out_write) = (INVALID_HANDLE_VALUE, INVALID_HANDLE_VALUE);
            if windows_sys::Win32::System::Pipes::CreatePipe(&mut in_read, &mut in_write, &sa, 0) == 0 {
                return Err(last_err("CreatePipe(input)"));
            }
            if windows_sys::Win32::System::Pipes::CreatePipe(&mut out_read, &mut out_write, &sa, 0) == 0 {
                return Err(last_err("CreatePipe(output)"));
            }

            let mut hpc: HPCON = 0;
            let hr = CreatePseudoConsole(size.coord(), in_read, out_write, 0, &mut hpc);
            if hr < 0 || hpc == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, format!("CreatePseudoConsole hr=0x{hr:08x}")));
            }

            // Kill-on-close job so the whole child tree dies when this Pty drops.
            let job = CreateJobObjectW(null(), null());
            if !job.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }

            // STARTUPINFOEX carrying the pseudoconsole attribute.
            let mut si: STARTUPINFOEXW = zeroed();
            si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
            let mut attr_size: usize = 0;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
            let mut attr = vec![0usize; (attr_size + size_of::<usize>() - 1) / size_of::<usize>()];
            si.lpAttributeList = attr.as_mut_ptr() as *mut c_void;
            if InitializeProcThreadAttributeList(si.lpAttributeList, 1, 0, &mut attr_size) == 0 {
                return Err(last_err("InitializeProcThreadAttributeList"));
            }
            if UpdateProcThreadAttribute(
                si.lpAttributeList,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                hpc as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null_mut(),
            ) == 0
            {
                return Err(last_err("UpdateProcThreadAttribute(PSEUDOCONSOLE)"));
            }

            let mut cmd = wide(command_line);
            let mut pi: PROCESS_INFORMATION = zeroed();
            let ok = CreateProcessW(
                null(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                0, // no handle inheritance; pseudoconsole binds stdio
                EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | env_flag,
                env_ptr,
                cwd_ptr,
                &si as *const STARTUPINFOEXW as *const _,
                &mut pi,
            );
            if ok == 0 {
                let e = last_err("CreateProcessW");
                ClosePseudoConsole(hpc);
                return Err(e);
            }

            // Put the child in the job *before* it runs, then release it.
            if !job.is_null() {
                AssignProcessToJobObject(job, pi.hProcess);
            }
            ResumeThread(pi.hThread);

            DeleteProcThreadAttributeList(si.lpAttributeList);
            CloseHandle(in_read);
            CloseHandle(out_write);

            // Reader thread: drain output to the channel until EOF.
            let (tx, rx) = channel::<Vec<u8>>();
            let reader_handle = SendHandle(out_read);
            let reader = thread::spawn(move || {
                let reader_handle = reader_handle; // force whole-struct (Send) capture
                let mut buf = [0u8; 8192];
                loop {
                    let mut read: u32 = 0;
                    let ok = ReadFile(reader_handle.0, buf.as_mut_ptr(), buf.len() as u32, &mut read, null_mut());
                    if ok == 0 || read == 0 {
                        break;
                    }
                    if tx.send(buf[..read as usize].to_vec()).is_err() {
                        break; // receiver dropped
                    }
                }
                CloseHandle(reader_handle.0);
            });

            Ok((
                Pty { hpc, input_write: in_write, process: pi.hProcess, thread: pi.hThread, job, reader: Some(reader) },
                rx,
            ))
        }
    }

    /// Write raw bytes (keystrokes / VT input) to the child.
    pub fn write(&self, data: &[u8]) -> io::Result<()> {
        unsafe {
            let mut written: u32 = 0;
            let mut off = 0;
            while off < data.len() {
                let ok = WriteFile(
                    self.input_write,
                    data[off..].as_ptr(),
                    (data.len() - off) as u32,
                    &mut written,
                    null_mut(),
                );
                if ok == 0 {
                    return Err(last_err("WriteFile(pty input)"));
                }
                off += written as usize;
            }
        }
        Ok(())
    }

    /// Resize the pseudoconsole.
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let hr = unsafe { ResizePseudoConsole(self.hpc, size.coord()) };
        if hr < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, format!("ResizePseudoConsole hr=0x{hr:08x}")));
        }
        Ok(())
    }

    /// True while the child process is still running.
    pub fn is_alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 }
    }

    /// Live process count inside the kill-on-close job (the shell counts as 1, so `> 1` means the
    /// shell has running children — a build, an agent, anything worth a close confirmation).
    /// 0 when the job handle is unavailable (treated as not-busy by callers).
    pub fn process_count(&self) -> u32 {
        if self.job.is_null() {
            return 0;
        }
        #[repr(C)]
        #[derive(Default)]
        struct BasicAccounting {
            total_user_time: i64,
            total_kernel_time: i64,
            this_period_total_user_time: i64,
            this_period_total_kernel_time: i64,
            total_page_fault_count: u32,
            total_processes: u32,
            active_processes: u32,
            total_terminated_processes: u32,
        }
        // windows-sys exposes QueryInformationJobObject; JobObjectBasicAccountingInformation = 1.
        unsafe {
            let mut info = BasicAccounting::default();
            let ok = windows_sys::Win32::System::JobObjects::QueryInformationJobObject(
                self.job,
                1, // JobObjectBasicAccountingInformation
                &mut info as *mut _ as *mut core::ffi::c_void,
                core::mem::size_of::<BasicAccounting>() as u32,
                null_mut(),
            );
            if ok == 0 {
                return 0;
            }
            info.active_processes
        }
    }

    /// Block until the child exits and return its exit code.
    pub fn wait(&self) -> u32 {
        unsafe {
            WaitForSingleObject(self.process, INFINITE);
            let mut code: u32 = 0;
            GetExitCodeProcess(self.process, &mut code);
            code
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            // Kill the child process tree FIRST. On Win11 (26100+) ClosePseudoConsole returns
            // immediately without disconnecting a still-attached client, and the ConPTY host keeps
            // the output pipe open while any client lives — so tearing down the console before the
            // client is gone can leave the reader blocked in ReadFile forever (the reader.join()
            // below then never returns, and the job close that would have killed the client never
            // runs). Terminating the job up front guarantees the host drains, closes the pipe, and
            // the reader hits EOF, making the join safe.
            if !self.job.is_null() {
                TerminateJobObject(self.job, 0);
            } else {
                TerminateProcess(self.process, 0);
            }
            CloseHandle(self.input_write);
            ClosePseudoConsole(self.hpc);
            if let Some(r) = self.reader.take() {
                let _ = r.join();
            }
            if !self.job.is_null() {
                CloseHandle(self.job);
            }
            CloseHandle(self.process);
            CloseHandle(self.thread);
        }
    }
}

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

#[cfg(test)]
mod tests {
    use super::build_env_block;

    #[test]
    fn env_block_adds_vars_and_inherits() {
        let block = build_env_block(&[("GMUX_TEST_VAR".into(), "hello-42".into())]);
        let s = String::from_utf16_lossy(&block);
        assert!(s.contains("GMUX_TEST_VAR=hello-42"), "addition missing");
        assert!(s.to_uppercase().contains("PATH="), "did not inherit process env (PATH)");
        assert_eq!(*block.last().unwrap(), 0, "block must be null-terminated");
    }
}
