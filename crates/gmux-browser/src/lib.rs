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
//! `eval_js` (M12 stage 2a) runs a script in the pane's WebView2 and routes the JSON result back:
//! since the controller is thread-affine, the GUI thread posts an [`Cmd::Eval`] carrying the
//! script + a reply channel; the browser thread calls `ExecuteScript` with a completion handler
//! that sends the result string back, and `eval_js` blocks on the reply with a ~10 s timeout.
//!
//! It is deliberately **not** exposed over the automation pipe. Eval needs a synchronous reply and
//! the WebView2 lives in the GUI process, not the daemon that serves the pipe — a `browser-eval`
//! call would require a daemon↔GUI RPC bridge, which is out of scope. `eval_js` stays a crate API
//! with real plumbing plus an `#[ignore]`d manual test; pipe-exposed eval waits for that bridge.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Controller, CreateCoreWebView2Environment,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    ExecuteScriptCompletedHandler,
};
use windows::core::{w, Error as WinError, Result as WinResult, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM, E_POINTER};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, PostMessageW, RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    CW_USEDEFAULT, GWLP_USERDATA, MSG, SW_SHOW, SW_SHOWNA, WM_APP, WM_DESTROY, WM_SIZE, WNDCLASSW,
    WS_CHILD, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// A command sent from the GUI thread to a browser pane's own thread.
enum Cmd {
    Navigate(String),
    /// Run `script` in the WebView2 and send the JSON result (or an error) back over `reply`.
    Eval { script: String, reply: Sender<Result<String, String>> },
    /// Move/resize an embedded pane's host window (client coords of the parent) and refit the
    /// WebView2 to it. Ignored by a top-level pane, which the user positions themselves.
    Bounds { x: i32, y: i32, w: i32, h: i32 },
    /// Show or hide an embedded pane without tearing down the WebView2 (a toggle keeps the page,
    /// its scroll position, and any login session alive).
    Visible(bool),
    Close,
}

/// How long [`BrowserPane::eval_js`] waits for the async `ExecuteScript` result before giving up.
const EVAL_TIMEOUT: Duration = Duration::from_secs(10);

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

    /// Open a browser pane **embedded in `parent`** (the GUI's window) at client-rect `(x, y, w, h)`
    /// — the M12 stage-2 panel. The WebView2 lives in a child window, so it composites over the
    /// wgpu surface without the renderer having to know anything about it; the caller reserves the
    /// rect in its own layout and calls [`set_bounds`] whenever that rect moves.
    ///
    /// Threading is unchanged from [`open`]: the child window and its message pump live on the
    /// browser thread. A child owned by another thread has its input queue attached to the parent's,
    /// which is why the GUI loop must never block for long (it doesn't — it is a poll loop).
    ///
    /// [`set_bounds`]: BrowserPane::set_bounds
    /// [`open`]: BrowserPane::open
    pub fn embed(parent: isize, x: i32, y: i32, w: i32, h: i32, url: &str) -> Result<BrowserPane, String> {
        let queue: Arc<Mutex<Vec<Cmd>>> = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<HwndSend, String>>();
        let url = url.to_string();
        let thread_queue = Arc::clone(&queue);
        let host = HostRect { parent, x, y, w, h };

        std::thread::Builder::new()
            .name("gmux-browser".into())
            .spawn(move || browser_thread_in(url, thread_queue, ready_tx, Some(host)))
            .map_err(|e| format!("spawn browser thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(BrowserPane { hwnd, queue }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("browser thread exited before signalling readiness".into()),
        }
    }

    /// Move/resize an embedded pane (client coords of its parent). No-op once the window is gone.
    pub fn set_bounds(&self, x: i32, y: i32, w: i32, h: i32) {
        self.post(Cmd::Bounds { x, y, w, h });
    }

    /// Show the embedded child **from the caller's thread**, which must be the thread that owns the
    /// parent window.
    ///
    /// This is not redundant with the `ShowWindow` the browser thread already does: a child window
    /// created on a different thread from its parent does not reliably take `WS_VISIBLE` from that
    /// thread (measured — the style stayed `WS_CHILD|WS_CLIPSIBLINGS`, no `WS_VISIBLE`, and the
    /// panel never appeared). Showing it from the parent's own thread is what actually applies.
    /// `SW_SHOWNA` so the panel never steals focus from the terminal.
    pub fn show_from_parent_thread(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNA};
        unsafe {
            let _ = ShowWindow(self.hwnd.0, SW_SHOWNA);
        }
    }

    /// Show or hide an embedded pane, keeping the page (and its session) loaded.
    pub fn set_visible(&self, visible: bool) {
        self.post(Cmd::Visible(visible));
    }

    /// Navigate the existing pane to `url`. Returns `false` if the pane's window is gone (the user
    /// closed it) — the caller should drop this handle and [`open`] a fresh pane, because a post to
    /// a destroyed window is a silent no-op and every later browse would vanish.
    ///
    /// [`open`]: BrowserPane::open
    #[must_use]
    pub fn navigate(&self, url: &str) -> bool {
        if !self.is_alive() {
            return false;
        }
        self.post(Cmd::Navigate(url.to_string()));
        true
    }

    /// Whether the pane's window still exists. False once the user closes it (its thread has ended).
    pub fn is_alive(&self) -> bool {
        unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(self.hwnd.0)).as_bool() }
    }

    /// Close the pane (destroys the window and ends its thread).
    pub fn close(&self) {
        self.post(Cmd::Close);
    }

    /// Run `script` in the pane's WebView2 and return its JSON-encoded result (WebView2's
    /// `ExecuteScript` returns `"null"` for a script with no value, `"\"text\""` for a string, etc).
    ///
    /// Blocks until the async `ExecuteScript` completion handler fires on the browser thread, up to
    /// [`EVAL_TIMEOUT`]. Errors if the WebView2 has not finished initializing yet (its creation is
    /// asynchronous — call after the page has had a moment to load), or if the browser thread is gone.
    pub fn eval_js(&self, script: &str) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        self.post(Cmd::Eval { script: script.to_string(), reply });
        match rx.recv_timeout(EVAL_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err("eval_js timed out".into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("browser thread gone before eval_js replied".into())
            }
        }
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
    /// A URL that arrived while the WebView2 was still initializing. `create_webview` is async and
    /// pumps messages while it runs, so a `Navigate` posted in that window would otherwise be
    /// drained against a `None` controller and silently dropped — one of the two ways the pane
    /// "did nothing with no error". Navigated as soon as the controller lands.
    pending: Option<String>,
}

const CLASS_NAME: PCWSTR = w!("gmux-browser");

/// Where an embedded pane's child window goes: a parent HWND plus a client-coords rect.
#[derive(Clone, Copy)]
struct HostRect {
    parent: isize,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}
// Same reasoning as `HwndSend`: the raw parent handle is only ever passed to CreateWindowExW on
// the browser thread. The window it names is owned by the GUI thread and never dereferenced here.
unsafe impl Send for HostRect {}

/// The browser thread for a top-level pane (see [`browser_thread_in`]).
fn browser_thread(
    url: String,
    queue: Arc<Mutex<Vec<Cmd>>>,
    ready_tx: Sender<Result<HwndSend, String>>,
) {
    browser_thread_in(url, queue, ready_tx, None)
}

/// The browser thread: init COM, create the window (top-level, or a child of `host`) + WebView2,
/// navigate, then run the message loop.
fn browser_thread_in(
    url: String,
    queue: Arc<Mutex<Vec<Cmd>>>,
    ready_tx: Sender<Result<HwndSend, String>>,
    host: Option<HostRect>,
) {
    unsafe {
        // WebView2 requires an STA (single-threaded apartment) on its host thread.
        if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            let _ = ready_tx.send(Err(format!("CoInitializeEx failed: {e}")));
            return;
        }

        let hwnd = match create_window(&queue, host) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        // Window exists: unblock BrowserPane::open. WebView2 creation (below) may still fail, but
        // the window is up and the pump will run either way.
        let _ = ready_tx.send(Ok(HwndSend(hwnd)));

        // An embedded child needs SW_SHOWNA: SW_SHOW activates, which would steal focus from the
        // terminal every time a `gmux browse` lands.
        let _ = ShowWindow(hwnd, if host.is_some() { SW_SHOWNA } else { SW_SHOW });

        // Create the WebView2 environment then a controller parented to our window. Both are async
        // COM calls; `wait_for_async_operation` pumps messages until each completes.
        match create_webview(hwnd) {
            Ok(controller) => {
                resize_to_client(&controller, hwnd);
                // Stash the live controller FIRST: from here on any WM_APP navigates directly, so
                // there is no gap where a command could land against a `None` controller.
                let pending = if let Some(state) = window_state(hwnd) {
                    state.controller = Some(controller.clone());
                    state.pending.take()
                } else {
                    None
                };
                if let Ok(wv) = controller.CoreWebView2() {
                    // A URL queued during initialization supersedes the one we opened with.
                    let shown = pending.as_deref().unwrap_or(&url);
                    let target = pcwstr(shown);
                    let _ = wv.Navigate(target.as_pcwstr());
                    set_title(hwnd, shown);
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
/// pane's window, stashing `WindowState` behind `GWLP_USERDATA`. With `host`, that is a CHILD of
/// the GUI's window at the given client rect (the embedded panel); without, a top-level
/// `gmux browser` window.
unsafe fn create_window(queue: &Arc<Mutex<Vec<Cmd>>>, host: Option<HostRect>) -> Result<HWND, String> {
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

    let state = Box::new(WindowState { controller: None, queue: Arc::clone(queue), pending: None });
    let state_ptr = Box::into_raw(state);

    let (style, x, y, w_, h_, parent) = match host {
        // The panel: a child clipped to the GUI's client area. WS_CLIPSIBLINGS keeps it from
        // fighting other children for the same pixels.
        Some(r) => (
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            r.x,
            r.y,
            r.w,
            r.h,
            Some(HWND(r.parent as *mut _)),
        ),
        None => (WS_OVERLAPPEDWINDOW, CW_USEDEFAULT, CW_USEDEFAULT, 1024, 768, None),
    };
    let hwnd = CreateWindowExW(
        Default::default(),
        CLASS_NAME,
        w!("gmux browser"),
        style,
        x,
        y,
        w_,
        h_,
        parent,
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
    // Only the top-level window needs the DPI fix-up; the panel's rect comes from the GUI, which
    // already works in physical pixels.
    if host.is_none() {
        scale_to_dpi(hwnd);
    }
    Ok(hwnd)
}

/// `CreateWindowExW` takes physical pixels, so on a scaled display the 1024x768 we ask for lands as
/// a window that is *smaller* than intended — 571x402 client at 175%, and worse on a 4K panel (the
/// "collapsed window" this pane was reported with). Re-apply the size scaled by the window's DPI.
unsafe fn scale_to_dpi(hwnd: HWND) {
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOMOVE, SWP_NOZORDER};
    let dpi = GetDpiForWindow(hwnd);
    if dpi == 0 || dpi == 96 {
        return; // unscaled display (or the call failed): the created size is already right
    }
    let scale = |v: i32| v * dpi as i32 / 96;
    let _ = SetWindowPos(hwnd, None, 0, 0, scale(1024), scale(768), SWP_NOMOVE | SWP_NOZORDER);
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

/// Run `script` in the controller's WebView2, sending the JSON result (or an error) over `reply`.
/// Called on the browser thread; the completion handler fires later on this thread's message pump.
unsafe fn eval_on_thread(
    controller: &Option<ICoreWebView2Controller>,
    script: &str,
    reply: Sender<Result<String, String>>,
) {
    let wv = match controller.as_ref().and_then(|c| c.CoreWebView2().ok()) {
        Some(wv) => wv,
        None => {
            let _ = reply.send(Err("browser not ready (WebView2 still initializing)".into()));
            return;
        }
    };
    // The handler owns one clone; the dispatch-failure arm keeps its own so a failed ExecuteScript
    // still reports a clear error rather than surfacing as a channel disconnect. Only one send
    // reaches the single-value receiver; the loser is a harmless no-op.
    let dispatch_reply = reply.clone();
    // The macro hands the closure a converted `(Result<()>, String)`: `code` is the completion
    // HRESULT and `result` the JSON-encoded value (empty on an error HRESULT).
    let handler = ExecuteScriptCompletedHandler::create(Box::new(move |code: WinResult<()>, result: String| {
        let msg = match code {
            Ok(()) => Ok(result),
            Err(e) => Err(format!("ExecuteScript failed: {e}")),
        };
        let _ = reply.send(msg);
        Ok(())
    }));
    let target = pcwstr(script);
    if let Err(e) = wv.ExecuteScript(target.as_pcwstr(), &handler) {
        let _ = dispatch_reply.send(Err(format!("ExecuteScript dispatch failed: {e}")));
    }
}

/// Retitle the window `gmux browser - <url>`. The pane's whole state is otherwise invisible from
/// outside the process, which is what made the "it silently did nothing" bug so hard to pin down;
/// the title makes the current target observable to a human and to a test.
unsafe fn set_title(hwnd: HWND, url: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowLongW, SetWindowTextW, GWL_STYLE};
    // The title also carries the host geometry + style: the panel has no other externally visible
    // state, and this is what made the embed verifiable without a screenshot.
    let mut r = RECT::default();
    let _ = GetClientRect(hwnd, &mut r);
    let style = GetWindowLongW(hwnd, GWL_STYLE);
    let title = pcwstr(&format!(
        "gmux browser [{}x{} style=0x{:X}] - {url}",
        r.right - r.left,
        r.bottom - r.top,
        style
    ));
    let _ = SetWindowTextW(hwnd, title.as_pcwstr());
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
                        Cmd::Navigate(u) if !u.is_empty() => match &state.controller {
                            Some(ctl) => {
                                if let Ok(wv) = ctl.CoreWebView2() {
                                    let target = pcwstr(&u);
                                    let _ = wv.Navigate(target.as_pcwstr());
                                    set_title(hwnd, &u);
                                }
                            }
                            // Still initializing: hold the URL instead of dropping it.
                            None => state.pending = Some(u),
                        },
                        Cmd::Navigate(_) => {} // the keep-alive hint from post(); ignore
                        Cmd::Eval { script, reply } => eval_on_thread(&state.controller, &script, reply),
                        Cmd::Bounds { x, y, w, h } => {
                            use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
                            let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
                            // WM_SIZE refits the controller, but resize it here too: a move-only
                            // SetWindowPos sends no WM_SIZE.
                            if let Some(ctl) = &state.controller {
                                resize_to_client(ctl, hwnd);
                            }
                        }
                        Cmd::Visible(show) => {
                            use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNA};
                            let _ = ShowWindow(hwnd, if show { SW_SHOWNA } else { SW_HIDE });
                            // Tell WebView2 as well, so a hidden panel stops rendering entirely
                            // rather than just being covered.
                            if let Some(ctl) = &state.controller {
                                let _ = ctl.SetIsVisible(show);
                            }
                        }
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

    /// Manual (ignored): needs a live WebView2 runtime + an interactive desktop, so it can't run
    /// under headless `cargo test`. Run with `cargo test -p gmux-browser -- --ignored` on a desktop.
    /// Opens example.com, gives the page a moment to load, evals `1+1`, and expects the JSON `"2"`.
    #[test]
    #[ignore = "requires a live WebView2 runtime + interactive desktop"]
    fn eval_js_returns_json_result() {
        let pane = BrowserPane::open("https://example.com").expect("open browser pane");
        // WebView2 creation + first navigation are async; wait before evaluating.
        std::thread::sleep(Duration::from_secs(3));
        let out = pane.eval_js("1 + 1").expect("eval_js should return a result");
        assert_eq!(out, "2", "ExecuteScript returns the JSON-encoded value");
    }
}
