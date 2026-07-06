//! gmux-browser — M12 stage 1: a flag-gated WebView2 browser pane.
//!
//! **Scope (deliberately small).** A [`BrowserPane`] hosts a WebView2 in its **own top-level Win32
//! window** titled `gmux browser`. True split-embedding inside the winit/wgpu surface (reparenting
//! the WebView2 controller into a rect of the main window, damage-synced with the terminal panes)
//! is the M12 stage-2 job and is **not** done here — this proves the runtime, the COM projection,
//! the daemon→GUI Browse path, and `open`/`navigate`/`close` end to end.
//!
//! **Threading.** WebView2's controller is thread-affine and needs a message pump. The GUI's event
//! loop (winit `about_to_wait`) must not block, so each `BrowserPane` runs its window + WebView2 on
//! its **own thread** with its own `GetMessage` loop. The GUI drives it over a channel; a posted
//! `WM_APP` kicks the pump so queued commands are handled promptly. `open` blocks only until the
//! window exists (so a failure to create it surfaces synchronously); WebView2 creation completes
//! asynchronously on the browser thread.
//!
//! `eval_js` is intentionally a stub returning a clear error — script execution + result plumbing
//! lands with stage 2 (it needs the async `ExecuteScript` result routed back over a channel).

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Controller, CreateCoreWebView2Environment,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::{w, Error as WinError, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM, E_POINTER};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, PostMessageW, RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    CW_USEDEFAULT, GWLP_USERDATA, MSG, SW_SHOW, WM_APP, WM_DESTROY, WM_SIZE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};

/// A command sent from the GUI thread to a browser pane's own thread.
enum Cmd {
    Navigate(String),
    Close,
}

/// A handle to a browser pane hosted on its own thread. Dropping it (or calling [`close`]) tears
/// down the window and the WebView2. `open`/`navigate`/`close` are the M12 stage-1 API.
///
/// [`close`]: BrowserPane::close
pub struct BrowserPane {
    hwnd: HwndSend,
    /// Shared with the browser thread; commands land here and a `WM_APP` post kicks the pump.
    queue: Arc<Mutex<Vec<Cmd>>>,
}

/// `HWND` is a raw pointer (not `Send`); we only ever use it as a `PostMessageW` target from the
/// GUI thread, which is sound — the window itself is owned and mutated solely by the browser thread.
#[derive(Clone, Copy)]
struct HwndSend(HWND);
unsafe impl Send for HwndSend {}

impl BrowserPane {
    /// Open a browser pane on `url` in a new top-level `gmux browser` window. Blocks until the
    /// window is created (so window-creation failure is reported here); the WebView2 finishes
    /// initializing and navigates asynchronously on the browser thread.
    pub fn open(url: &str) -> Result<BrowserPane, String> {
        let queue: Arc<Mutex<Vec<Cmd>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<HwndSend, String>>();
        let url = url.to_string();
        let thread_queue = Arc::clone(&queue);

        std::thread::Builder::new()
            .name("gmux-browser".into())
            .spawn(move || browser_thread(url, thread_queue, ready_tx))
            .map_err(|e| format!("spawn browser thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(BrowserPane { hwnd, queue }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("browser thread exited before signalling readiness".into()),
        }
    }

    /// Navigate the existing pane to `url`.
    pub fn navigate(&self, url: &str) {
        self.post(Cmd::Navigate(url.to_string()));
    }

    /// Close the pane (destroys the window and ends its thread).
    pub fn close(&self) {
        self.post(Cmd::Close);
    }

    /// M12 stage 2. `ExecuteScript` is async and its result must be routed back over a channel;
    /// stage 1 ships the window + navigation only.
    pub fn eval_js(&self, _script: &str) -> Result<String, String> {
        Err("eval_js is not implemented in M12 stage 1 (WebView2 window + navigation only)".into())
    }

    /// Queue a command and kick the browser thread's message pump so it is handled promptly. The
    /// shared queue is the source of truth; `WM_APP` is only the wake — the wndproc drains the queue.
    fn post(&self, cmd: Cmd) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(cmd);
        }
        unsafe {
            let _ = PostMessageW(Some(self.hwnd.0), WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for BrowserPane {
    fn drop(&mut self) {
        self.close();
    }
}

/// State stashed behind the window's `GWLP_USERDATA` so the wndproc can reach the controller and
/// the command queue. Boxed and leaked into the window; reclaimed and dropped on `WM_DESTROY`.
struct WindowState {
    controller: Option<ICoreWebView2Controller>,
    queue: Arc<Mutex<Vec<Cmd>>>,
}

const CLASS_NAME: PCWSTR = w!("gmux-browser");

/// The browser thread: init COM, create the window + WebView2, navigate, then run the message loop.
fn browser_thread(
    url: String,
    queue: Arc<Mutex<Vec<Cmd>>>,
    ready_tx: Sender<Result<HwndSend, String>>,
) {
    unsafe {
        // WebView2 requires an STA (single-threaded apartment) on its host thread.
        if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            let _ = ready_tx.send(Err(format!("CoInitializeEx failed: {e}")));
            return;
        }

        let hwnd = match create_window(&queue) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        // Window exists: unblock BrowserPane::open. WebView2 creation (below) may still fail, but
        // the window is up and the pump will run either way.
        let _ = ready_tx.send(Ok(HwndSend(hwnd)));

        let _ = ShowWindow(hwnd, SW_SHOW);

        // Create the WebView2 environment then a controller parented to our window. Both are async
        // COM calls; `wait_for_async_operation` pumps messages until each completes.
        match create_webview(hwnd) {
            Ok(controller) => {
                resize_to_client(&controller, hwnd);
                if let Ok(wv) = controller.CoreWebView2() {
                    let target = pcwstr(&url);
                    let _ = wv.Navigate(target.as_pcwstr());
                }
                // Stash the live controller so WM_APP command handling can drive it.
                if let Some(state) = window_state(hwnd) {
                    state.controller = Some(controller);
                }
            }
            Err(e) => {
                eprintln!("gmux browser: WebView2 init failed: {e}");
                // Leave the empty window up so the failure is visible rather than a silent no-op.
            }
        }

        run_message_loop();
    }
}

/// Register the class (idempotent — a duplicate registration just fails harmlessly) and create the
/// top-level `gmux browser` window, stashing `WindowState` behind `GWLP_USERDATA`.
unsafe fn create_window(queue: &Arc<Mutex<Vec<Cmd>>>) -> Result<HWND, String> {
    let hinstance: HINSTANCE = GetModuleHandleW(None)
        .map(|h| HINSTANCE(h.0))
        .map_err(|e| format!("GetModuleHandleW: {e}"))?;

    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: CLASS_NAME,
        hInstance: hinstance,
        ..Default::default()
    };
    // RegisterClassW returns 0 if the class already exists; that's fine on a second pane.
    RegisterClassW(&class);

    let state = Box::new(WindowState { controller: None, queue: Arc::clone(queue) });
    let state_ptr = Box::into_raw(state);

    let hwnd = CreateWindowExW(
        Default::default(),
        CLASS_NAME,
        w!("gmux browser"),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        1024,
        768,
        None,
        None,
        Some(hinstance),
        Some(state_ptr as *const _),
    )
    .map_err(|e| {
        // Reclaim the leaked state on failure.
        drop(Box::from_raw(state_ptr));
        format!("CreateWindowExW: {e}")
    })?;

    SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    Ok(hwnd)
}

/// Synchronously (message-pumped) create the WebView2 environment + a controller parented to `hwnd`.
unsafe fn create_webview(hwnd: HWND) -> Result<ICoreWebView2Controller, String> {
    // 1) Environment.
    let (env_tx, env_rx) = mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(|handler| CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError)),
        Box::new(move |code, environment| {
            code?;
            let env = environment.ok_or_else(|| WinError::from(E_POINTER))?;
            let _ = env_tx.send(env);
            Ok(())
        }),
    )
    .map_err(|e| format!("create environment: {e}"))?;
    let environment = env_rx.recv().map_err(|_| "environment channel closed".to_string())?;

    // 2) Controller parented to our window.
    let (ctl_tx, ctl_rx) = mpsc::channel();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            environment
                .CreateCoreWebView2Controller(hwnd, &handler)
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, controller| {
            code?;
            let ctl = controller.ok_or_else(|| WinError::from(E_POINTER))?;
            let _ = ctl_tx.send(ctl);
            Ok(())
        }),
    )
    .map_err(|e| format!("create controller: {e}"))?;
    let controller = ctl_rx.recv().map_err(|_| "controller channel closed".to_string())?;

    unsafe {
        controller.SetIsVisible(true).map_err(|e| format!("SetIsVisible: {e}"))?;
    }
    Ok(controller)
}

/// Fit the WebView2 controller to the window's client rect.
unsafe fn resize_to_client(controller: &ICoreWebView2Controller, hwnd: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_ok() {
        let _ = controller.SetBounds(rect);
    }
}

/// Blocking `GetMessage` pump for this thread's window. Ends when the window is destroyed
/// (`GetMessageW` returns 0 on `WM_QUIT`, which `DestroyWindow`→`WM_DESTROY` posts).
unsafe fn run_message_loop() {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// Reach the `WindowState` stashed behind `GWLP_USERDATA`.
unsafe fn window_state<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    ptr.as_mut()
}

/// Window procedure: drains queued commands on `WM_APP`, resizes the WebView2 on `WM_SIZE`, and
/// reclaims `WindowState` on `WM_DESTROY`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_APP => {
            if let Some(state) = window_state(hwnd) {
                let cmds: Vec<Cmd> =
                    state.queue.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                for cmd in cmds {
                    match cmd {
                        Cmd::Navigate(u) if !u.is_empty() => {
                            if let Some(ctl) = &state.controller {
                                if let Ok(wv) = ctl.CoreWebView2() {
                                    let target = pcwstr(&u);
                                    let _ = wv.Navigate(target.as_pcwstr());
                                }
                            }
                        }
                        Cmd::Navigate(_) => {} // the keep-alive hint from post(); ignore
                        Cmd::Close => {
                            let _ = DestroyWindow(hwnd);
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(state) = window_state(hwnd) {
                if let Some(ctl) = &state.controller {
                    resize_to_client(ctl, hwnd);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Reclaim and drop the boxed state (releasing the controller COM ref).
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            // End this thread's message loop.
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// A NUL-terminated UTF-16 buffer whose `PCWSTR` stays valid while the buffer lives.
struct WideStr(Vec<u16>);
impl WideStr {
    fn as_pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}
fn pcwstr(s: &str) -> WideStr {
    WideStr(s.encode_utf16().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one bit of non-trivial pure logic here: UTF-16 conversion must NUL-terminate so the
    /// `PCWSTR` handed to `Navigate` is a valid C wide string. (The COM/window path needs a live
    /// WebView2 runtime + desktop, exercised by demos/m12-browser.ps1, not a unit test.)
    #[test]
    fn pcwstr_is_nul_terminated_utf16() {
        let w = pcwstr("hi");
        assert_eq!(w.0, vec![b'h' as u16, b'i' as u16, 0]);
        assert_eq!(*w.0.last().unwrap(), 0, "must be NUL-terminated");
        // Round-trips (minus the terminator) back to the source string.
        let back = String::from_utf16(&w.0[..w.0.len() - 1]).unwrap();
        assert_eq!(back, "hi");
    }
}
