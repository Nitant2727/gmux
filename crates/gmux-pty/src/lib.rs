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
    AllocConsole, ClosePseudoConsole, CreatePseudoConsole, GetConsoleWindow, ResizePseudoConsole,
    SetStdHandle, COORD, HPCON, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
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
            return; // already have a console
        }
        if AllocConsole() == 0 {
            return; // best effort; spawn may still work if a console appears elsewhere
        }
        let hwnd = GetConsoleWindow();
        if !hwnd.is_null() {
            ShowWindow(hwnd as _, SW_HIDE);
        }
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
    });
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
        ensure_console();
        // Keep the env block alive for the whole CreateProcessW call.
        let mut env_block = if env.is_empty() { Vec::new() } else { build_env_block(env) };
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
                null(),
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
            // Close input so the child sees EOF, tear down the pseudoconsole (build 26100+ returns
            // immediately), which closes the output write end -> reader hits EOF and exits.
            CloseHandle(self.input_write);
            ClosePseudoConsole(self.hpc);
            if let Some(r) = self.reader.take() {
                let _ = r.join();
            }
            // Closing the last job handle kills any surviving process tree.
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
