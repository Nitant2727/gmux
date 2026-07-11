//! Minimal Windows clipboard get/set for `CF_UNICODETEXT`. ponytail: two hand-rolled calls over the
//! `windows` crate rather than a clipboard dependency; failures log and no-op (a clipboard hiccup
//! must never take the GUI down). `owner` is the window HWND (as `isize`); `0` means no owner.

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::Foundation::GlobalFree;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

const CF_UNICODE: u32 = CF_UNICODETEXT.0 as u32;

fn owner_hwnd(owner: isize) -> Option<HWND> {
    (owner != 0).then(|| HWND(owner as *mut core::ffi::c_void))
}

/// Write `text` to the clipboard as UTF-16 `CF_UNICODETEXT`. Logs and returns on any failure.
pub fn set_text(owner: isize, text: &str) {
    // UTF-16 with the required NUL terminator.
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    unsafe {
        if OpenClipboard(owner_hwnd(owner)).is_err() {
            eprintln!("gmux: clipboard: OpenClipboard failed");
            return;
        }
        // Everything past here must CloseClipboard before returning.
        let bytes = wide.len() * std::mem::size_of::<u16>();
        let hmem = match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("gmux: clipboard: GlobalAlloc failed: {e}");
                let _ = CloseClipboard();
                return;
            }
        };
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            eprintln!("gmux: clipboard: GlobalLock failed");
            let _ = GlobalFree(Some(hmem));
            let _ = CloseClipboard();
            return;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        // GlobalUnlock reports "still locked" via Err even on the normal fully-unlocked path, so
        // its result is intentionally ignored (the windows-rs BOOL-to-Result quirk).
        let _ = GlobalUnlock(hmem);
        if EmptyClipboard().is_err() {
            eprintln!("gmux: clipboard: EmptyClipboard failed");
            let _ = GlobalFree(Some(hmem));
            let _ = CloseClipboard();
            return;
        }
        // On success the system takes ownership of `hmem`; on failure it stays ours to free.
        if SetClipboardData(CF_UNICODE, Some(HANDLE(hmem.0))).is_err() {
            eprintln!("gmux: clipboard: SetClipboardData failed");
            let _ = GlobalFree(Some(hmem));
        }
        let _ = CloseClipboard();
    }
}

/// Read the clipboard's `CF_UNICODETEXT` as a `String`, or `None` if empty / unavailable.
pub fn get_text(owner: isize) -> Option<String> {
    unsafe {
        if OpenClipboard(owner_hwnd(owner)).is_err() {
            return None;
        }
        let text = read_unicode_text();
        let _ = CloseClipboard();
        text
    }
}

/// Read the currently-open clipboard's `CF_UNICODETEXT`. Caller owns Open/Close.
unsafe fn read_unicode_text() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODE).ok()?;
    let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
    if ptr.is_null() {
        return None;
    }
    // Walk to the NUL terminator, then copy out (String owns its bytes before we unlock).
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    let _ = GlobalUnlock(HGLOBAL(handle.0));
    Some(s)
}
