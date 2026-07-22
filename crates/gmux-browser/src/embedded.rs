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
    CreateCoreWebView2Environment, ICoreWebView2Controller,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, RECT};

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
    /// `None` until the async controller creation completes.
    controller: RefCell<Option<ICoreWebView2Controller>>,
    /// The URL to show once the controller lands (also the "navigate before ready" queue).
    pending_url: RefCell<Option<String>>,
    /// Latest requested rect, applied on every change and again when the controller arrives.
    bounds: Cell<RECT>,
    visible: Cell<bool>,
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
            controller: RefCell::new(None),
            pending_url: RefCell::new(Some(url.to_string())),
            bounds: Cell::new(RECT { left: x, top: y, right: x + w, bottom: y + h }),
            visible: Cell::new(true),
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
}

impl Inner {
    /// Install the freshly created controller and replay everything requested while it was pending.
    fn install(&self, controller: ICoreWebView2Controller) {
        unsafe {
            let _ = controller.SetBounds(self.bounds.get());
            let _ = controller.SetIsVisible(self.visible.get());
        }
        if let Some(url) = self.pending_url.borrow_mut().take() {
            navigate_controller(&controller, &url);
        }
        *self.controller.borrow_mut() = Some(controller);
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

fn navigate_controller(controller: &ICoreWebView2Controller, url: &str) {
    unsafe {
        if let Ok(wv) = controller.CoreWebView2() {
            let wide: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = wv.Navigate(PCWSTR(wide.as_ptr()));
        }
    }
}
