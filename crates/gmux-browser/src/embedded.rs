//! M12 stage 2: a WebView2 hosted **on the GUI's own thread**, parented straight to the winit
//! window — the browser panel.
//!
//! Round 44 tried the obvious port of the stage-1 design: keep the browser on its own thread and
//! give it a child window of the GUI's window. That fails. The child is created correctly (right
//! parent, right rect, WebView2 processes spawn) but never takes `WS_VISIBLE` — measured at
//! `0x44000000` after four different show paths, including `ShowWindow` from the parent's own
//! thread — and nothing paints.
//!
//! The reason is WebView2's threading model: the controller must live on the UI thread that owns
//! its parent `HWND`, and it manages its own child windows. So this module creates **no window at
//! all**. It hands `CreateCoreWebView2Controller` the winit `HWND` and lets WebView2 do the
//! parenting, on the winit thread.
//!
//! **Nothing here may block.** The stage-1 code could pump messages inside
//! `wait_for_async_operation` because it owned its thread; blocking the winit loop would freeze the
//! terminal. Environment and controller creation are therefore fire-and-forget: the completion
//! handlers store the controller in a shared slot, and every operation issued before it lands is
//! remembered (`pending_url`, `bounds`, `visible`) and applied when it does. COM callbacks arrive
//! through the same message queue winit is already pumping, so no extra pump is needed.
//!
//! [`EmbeddedBrowser`] is deliberately **not** `Send`: it is only ever touched from the GUI thread.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2Environment, ICoreWebView2Controller, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
};
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CreateCoreWebView2ControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL, VK_SHIFT};

/// A WebView2 embedded in the GUI's window. Create once, then drive it with [`navigate`],
/// [`set_bounds`], and [`set_visible`] — all safe to call before the WebView2 has finished
/// initializing (they are replayed when it does).
///
/// [`navigate`]: EmbeddedBrowser::navigate
/// [`set_bounds`]: EmbeddedBrowser::set_bounds
/// [`set_visible`]: EmbeddedBrowser::set_visible
pub struct EmbeddedBrowser {
    inner: Rc<Inner>,
}

struct Inner {
    /// The winit window the panel is parented to. Keyboard focus is handed back to it after the
    /// controller lands — WebView2 grabs focus on creation, which would silently redirect the
    /// user's typing from their terminal into the page.
    parent: isize,
    /// `None` until the async controller creation completes.
    controller: RefCell<Option<ICoreWebView2Controller>>,
    /// The URL to show once the controller lands (also the "navigate before ready" queue).
    pending_url: RefCell<Option<String>>,
    /// Latest requested rect, applied on every change and again when the controller arrives.
    bounds: Cell<RECT>,
    visible: Cell<bool>,
    /// Set when Ctrl+Shift+B is pressed WHILE THE PAGE HAS FOCUS. Once the user clicks into the
    /// panel, every keystroke goes to WebView2 and the app's own keymap never sees the toggle —
    /// so the panel could be opened but not closed. The app polls this each event-loop pass.
    toggle_requested: Cell<bool>,
    /// What the address bar shows: the page's current URI, updated by `SourceChanged` so links
    /// clicked inside the page are reflected too, not just app-driven navigations.
    url: RefCell<String>,
    /// Whether back/forward can act, from `HistoryChanged` — the bar dims dead buttons.
    can_back: Cell<bool>,
    can_fwd: Cell<bool>,
}

impl EmbeddedBrowser {
    /// Start creating a WebView2 parented to `hwnd`, sized to `(x, y, w, h)` in the window's client
    /// coords, showing `url`. Returns immediately — creation finishes asynchronously on this
    /// thread's message loop.
    ///
    /// # Safety contract
    /// Must be called on the thread that owns `hwnd` (the winit thread), which must keep pumping
    /// messages. Both hold for gmux's event loop.
    pub fn new(hwnd: isize, x: i32, y: i32, w: i32, h: i32, url: &str) -> Result<EmbeddedBrowser, String> {
        let inner = Rc::new(Inner {
            parent: hwnd,
            controller: RefCell::new(None),
            pending_url: RefCell::new(Some(url.to_string())),
            bounds: Cell::new(RECT { left: x, top: y, right: x + w, bottom: y + h }),
            visible: Cell::new(true),
            toggle_requested: Cell::new(false),
            url: RefCell::new(url.to_string()),
            can_back: Cell::new(false),
            can_fwd: Cell::new(false),
        });
        let for_env = Rc::clone(&inner);
        // `CreateCoreWebView2Environment` is async; its handler then starts the controller, whose
        // handler installs it. Neither step blocks the caller.
        let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
            move |code, environment| {
                code?;
                let Some(env) = environment else { return Ok(()) };
                let for_ctl = Rc::clone(&for_env);
                let ctl_handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
                    move |code, controller| {
                        code?;
                        if let Some(controller) = controller {
                            for_ctl.install(controller);
                        }
                        Ok(())
                    },
                ));
                unsafe { env.CreateCoreWebView2Controller(HWND(hwnd as *mut _), &ctl_handler)? };
                Ok(())
            },
        ));
        unsafe { CreateCoreWebView2Environment(&handler) }
            .map_err(|e| format!("CreateCoreWebView2Environment: {e}"))?;
        Ok(EmbeddedBrowser { inner })
    }

    /// Point the panel at `url` (queued if the WebView2 is still initializing).
    pub fn navigate(&self, url: &str) {
        match self.inner.controller.borrow().as_ref() {
            Some(c) => navigate_controller(c, url),
            None => *self.inner.pending_url.borrow_mut() = Some(url.to_string()),
        }
    }

    /// Move/resize the panel within the window's client area.
    pub fn set_bounds(&self, x: i32, y: i32, w: i32, h: i32) {
        let rect = RECT { left: x, top: y, right: x + w, bottom: y + h };
        self.inner.bounds.set(rect);
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                let _ = c.SetBounds(rect);
            }
        }
    }

    /// Show/hide the panel, keeping the page (and any login session) loaded.
    pub fn set_visible(&self, visible: bool) {
        self.inner.visible.set(visible);
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                let _ = c.SetIsVisible(visible);
            }
        }
    }

    /// Whether the WebView2 has finished initializing (for status/diagnostics).
    pub fn is_ready(&self) -> bool {
        self.inner.controller.borrow().is_some()
    }

    /// Whether Ctrl+Shift+B was pressed inside the page since the last call. Clears the flag.
    pub fn take_toggle_request(&self) -> bool {
        self.inner.toggle_requested.replace(false)
    }

    /// The page's current URI — what the address bar shows. Tracks in-page navigation too.
    pub fn current_url(&self) -> String {
        self.inner.url.borrow().clone()
    }

    /// `(can_go_back, can_go_forward)` for the nav buttons.
    pub fn nav_state(&self) -> (bool, bool) {
        (self.inner.can_back.get(), self.inner.can_fwd.get())
    }

    pub fn go_back(&self) {
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                if let Ok(wv) = c.CoreWebView2() {
                    let _ = wv.GoBack();
                }
            }
        }
    }

    pub fn go_forward(&self) {
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                if let Ok(wv) = c.CoreWebView2() {
                    let _ = wv.GoForward();
                }
            }
        }
    }

    pub fn reload(&self) {
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                if let Ok(wv) = c.CoreWebView2() {
                    let _ = wv.Reload();
                }
            }
        }
    }
}

impl Inner {
    /// Install the freshly created controller and replay everything requested while it was pending.
    fn install(self: &Rc<Self>, controller: ICoreWebView2Controller) {
        unsafe {
            let _ = controller.SetBounds(self.bounds.get());
            let _ = controller.SetIsVisible(self.visible.get());
        }
        if let Some(url) = self.pending_url.borrow_mut().take() {
            navigate_controller(&controller, &url);
        }
        // Ctrl+Shift+B must keep meaning "toggle the panel" even when the page owns the keyboard,
        // or a click into the panel makes it uncloseable by key. WebView2 only reports keys to the
        // host through this accelerator event.
        let for_keys = Rc::clone(self);
        let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else { return Ok(()) };
            unsafe {
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN;
                let mut vk = 0u32;
                let _ = args.KeyEventKind(&mut kind);
                let _ = args.VirtualKey(&mut vk);
                let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
                let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN && vk == 0x42 && ctrl && shift {
                    for_keys.toggle_requested.set(true);
                    let _ = args.SetHandled(true);
                }
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe {
            let _ = controller.add_AcceleratorKeyPressed(&handler, &mut token);
        }
        // Track the page's URI and history state for the address bar. One updater serves both
        // events: `SourceChanged` fires on navigation (links included), `HistoryChanged` when
        // back/forward availability moves.
        if let Ok(wv) = unsafe { controller.CoreWebView2() } {
            let sync = {
                let inner = Rc::clone(self);
                move |wv: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2| unsafe {
                    let mut uri = windows::core::PWSTR::null();
                    if wv.Source(&mut uri).is_ok() && !uri.is_null() {
                        *inner.url.borrow_mut() = String::from_utf16_lossy(uri.as_wide());
                        // COM allocated the string; freeing it is on us.
                        windows::Win32::System::Com::CoTaskMemFree(Some(uri.0 as *const _));
                    }
                    let (mut back, mut fwd) = (windows::core::BOOL(0), windows::core::BOOL(0));
                    let _ = wv.CanGoBack(&mut back);
                    let _ = wv.CanGoForward(&mut fwd);
                    inner.can_back.set(back.as_bool());
                    inner.can_fwd.set(fwd.as_bool());
                }
            };
            let on_source = sync.clone();
            let src_handler = webview2_com::SourceChangedEventHandler::create(Box::new(
                move |sender, _| {
                    if let Some(wv) = sender {
                        on_source(&wv);
                    }
                    Ok(())
                },
            ));
            let hist_handler = webview2_com::HistoryChangedEventHandler::create(Box::new(
                move |sender, _| {
                    if let Some(wv) = sender {
                        sync(&wv);
                    }
                    Ok(())
                },
            ));
            let (mut t1, mut t2) = (0i64, 0i64);
            unsafe {
                let _ = wv.add_SourceChanged(&src_handler, &mut t1);
                let _ = wv.add_HistoryChanged(&hist_handler, &mut t2);
            }
        }
        *self.controller.borrow_mut() = Some(controller);
        // WebView2 takes keyboard focus as it comes up; give it back to the terminal — opening a
        // side panel must not redirect what the user is typing.
        unsafe {
            let _ = SetFocus(Some(HWND(self.parent as *mut _)));
        }
    }
}

impl Drop for EmbeddedBrowser {
    fn drop(&mut self) {
        // Closing the controller tears down WebView2's child windows; without it they outlive the
        // panel and keep painting over the terminal.
        if let Some(c) = self.inner.controller.borrow().as_ref() {
            unsafe {
                let _ = c.Close();
            }
        }
    }
}

/// The panel's start page, as a `data:` URL — what Ctrl+Shift+B shows before any page is loaded.
///
/// It used to be `about:blank`, which WebView2 renders honouring the system dark theme: a pure
/// black page inside a near-black app, i.e. the panel opened and looked exactly like it hadn't.
/// This page matches the chrome, says what it is, and carries a search box (a plain form posting
/// to DuckDuckGo), so the panel is usable without the CLI. Nothing loads over the network until
/// the user submits or `gmux browse --pane` navigates.
pub fn start_page_url() -> String {
    const HTML: &str = concat!(
        "<!doctype html><meta charset='utf-8'><title>gmux</title><style>",
        "body{background:#0b0b0d;color:#e8e8ec;font-family:Consolas,'Cascadia Mono',monospace;",
        "display:flex;align-items:center;justify-content:center;height:100vh;margin:0}",
        ".c{width:min(420px,80%)}h1{color:#3b8ae6;font-size:18px;margin:0 0 14px;font-weight:600}",
        "input{width:100%;box-sizing:border-box;background:#151518;color:#e8e8ec;",
        "border:1px solid #242429;border-radius:6px;padding:10px 12px;font:inherit;outline:none}",
        "input:focus{border-color:#3b8ae6}p{color:#7e8088;font-size:12px;line-height:1.7}",
        "code{color:#b8bac2}</style><div class='c'><h1>gmux browser</h1>",
        "<form action='https://duckduckgo.com/'>",
        "<input name='q' placeholder='search or paste a url' autocomplete='off'></form>",
        "<p>ctrl+shift+b hides this panel &middot; <code>gmux browse --pane &lt;url&gt;</code> ",
        "opens a page here &middot; hiding keeps the page loaded</p></div>",
    );
    // Percent-encode for a data: URL. Conservative: everything outside unreserved is encoded.
    let mut url = String::with_capacity(HTML.len() * 2 + 16);
    url.push_str("data:text/html,");
    for b in HTML.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                url.push(b as char)
            }
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

fn navigate_controller(controller: &ICoreWebView2Controller, url: &str) {
    unsafe {
        if let Ok(wv) = controller.CoreWebView2() {
            let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = wv.Navigate(PCWSTR(wide.as_ptr()));
        }
    }
}
