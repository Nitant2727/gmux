// What console/stdio situation does the harness give us? (Explains the ConPTY child-attach failure.)
use windows_sys::Win32::Storage::FileSystem::GetFileType;
use windows_sys::Win32::System::Console::{
    AllocConsole, FreeConsole, GetConsoleWindow, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE,
};
fn main() {
    unsafe {
        eprintln!("GetConsoleWindow = 0x{:x}", GetConsoleWindow() as isize);
        for (name, id) in [("in", STD_INPUT_HANDLE), ("out", STD_OUTPUT_HANDLE), ("err", STD_ERROR_HANDLE)] {
            let h = GetStdHandle(id);
            let ft = GetFileType(h); // 1=disk 2=char(console) 3=pipe 0=unknown
            eprintln!("std {name:3}: handle=0x{:x} GetFileType={ft} (2=console,3=pipe)", h as isize);
        }
        let freed = FreeConsole();
        let alloc = AllocConsole();
        eprintln!("FreeConsole={freed} AllocConsole={alloc}  err_after_alloc={}", 
            windows_sys::Win32::Foundation::GetLastError());
        eprintln!("after AllocConsole GetConsoleWindow = 0x{:x}", GetConsoleWindow() as isize);
    }
}
