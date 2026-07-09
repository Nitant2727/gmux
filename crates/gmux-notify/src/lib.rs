//! gmux-notify — turns pane attention into real Windows attention: toast notifications,
//! taskbar flash, and taskbar progress. This productizes the M0 toast spike
//! (`spikes/toast/src/main.rs`) per ARCHITECTURE.md §7.3 and docs/research/windows-toasts.md.
//!
//! # PUBLIC CONTRACT (implement exactly — gmux-gui and the future CLI depend on these)
//!
//! ```ignore
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Urgency { Low, Normal, Critical }
//! #[derive(Debug, Clone, Copy, PartialEq, Eq)]
//! pub enum ProgressState { None, Normal, Error, Indeterminate, Paused }
//!
//! #[derive(Debug, Clone)]
//! pub struct ToastRequest {
//!     pub tag: String,        // unique per pane e.g. "pane-5"; same tag REPLACES in place (never stacks)
//!     pub group: String,      // session id e.g. "sess-0"
//!     pub title: String,
//!     pub body: String,
//!     pub urgency: Urgency,
//!     pub launch_arg: String, // returned verbatim on click, e.g. "pane=5;action=focus"
//! }
//!
//! pub struct Notifier { /* AUMID + in-proc activation queue (Arc<Mutex<Vec<String>>>) */ }
//! impl Notifier {
//!     // Registers the AUMID via HKCU registry (no shortcut/MSIX/elevation) + wires
//!     // ToastNotification.Activated. aumid e.g. "com.gmux.app", display e.g. "gmux".
//!     pub fn new(aumid: &str, display_name: &str) -> std::io::Result<Notifier>;
//!     pub fn show(&self, req: &ToastRequest) -> std::io::Result<()>;
//!     pub fn clear(&self, tag: &str, group: &str);
//!     pub fn poll_activations(&self) -> Vec<String>; // drains launch args from clicks
//! }
//!
//! pub fn flash_window(hwnd: isize, on: bool); // FlashWindowEx TRAY|TIMERNOFG / STOP
//!
//! pub struct Taskbar { /* ITaskbarList3 + hwnd */ }
//! impl Taskbar {
//!     pub fn new(hwnd: isize) -> Option<Taskbar>;
//!     pub fn set_progress(&self, state: ProgressState, pct: Option<u8>);
//!     pub fn clear_progress(&self);
//! }
//! ```
//!
//! # Implementation rules (from the research)
//! - Model the toast XML on the M0 spike: ToastGeneric binding with two text nodes (title, body), a
//!   launch attribute on the toast element, and one action ("Focus pane", arguments=launch_arg).
//!   SetTag(tag) + SetGroup(group) so a re-show replaces. Urgency::Critical => scenario="urgent".
//! - SANITIZE title/body before building XML: strip C0 (0x00-0x1F) and C1 (0x80-0x9F) control chars,
//!   then XML-escape & < > " '. Factor the XML-string building into a PURE function so it is unit
//!   testable without WinRT.
//! - Do NOT gate on ToastNotifier.Setting() before the first Show() (fresh AUMID returns 0x80070490
//!   on run 1 - treat a Setting() error as first-run, not disabled).
//! - clear() => ToastNotificationManager::History().Remove(tag, group, aumid).

use std::sync::{Arc, Mutex};

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastNotification, ToastNotificationManager,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::{
    ITaskbarList3, SetCurrentProcessExplicitAppUserModelID, TaskbarList, TBPF_ERROR,
    TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FlashWindowEx, FLASHWINFO, FLASHW_STOP, FLASHW_TIMERNOFG, FLASHW_TRAY,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// How loud a notification should be. `Critical` maps to the toast
/// `scenario="urgent"` attribute so it can break through Do-Not-Disturb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// Taskbar-button progress fill state, mirroring `ITaskbarList3::SetProgressState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    None,
    Normal,
    Error,
    Indeterminate,
    Paused,
}

/// A single toast to show. `tag` is unique per pane; a re-show with the same
/// `tag`/`group` replaces the previous toast in place instead of stacking.
#[derive(Debug, Clone)]
pub struct ToastRequest {
    /// Unique per pane e.g. "pane-5"; same tag REPLACES in place (never stacks).
    pub tag: String,
    /// Session id e.g. "sess-0".
    pub group: String,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    /// Returned verbatim on click, e.g. "pane=5;action=focus".
    pub launch_arg: String,
}

// ---------------------------------------------------------------------------
// Pure, WinRT-free helpers (unit-testable)
// ---------------------------------------------------------------------------

/// Sanitize and XML-escape a string for embedding as text/attribute content.
///
/// 1. Strip C0 (`0x00..=0x1F`) and C1 (`0x80..=0x9F`) control characters.
/// 2. XML-escape the five special characters: `&`, `<`, `>`, `"`, `'`.
///
/// Pure so it can be unit-tested without WinRT.
pub fn sanitize_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        // Strip C0 and C1 control characters.
        if c <= 0x1F || (0x80..=0x9F).contains(&c) {
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Map an [`Urgency`] to the toast `scenario` attribute value, if any.
/// Only `Critical` produces a scenario (`"urgent"`); others get the default.
fn scenario_for(urgency: Urgency) -> Option<&'static str> {
    match urgency {
        Urgency::Critical => Some("urgent"),
        Urgency::Low | Urgency::Normal => None,
    }
}

/// Build the toast content XML for a request, as a pure string.
///
/// Models the M0 spike: a `ToastGeneric` binding with two `<text>` nodes
/// (title, body), a `launch` attribute on the `<toast>` element, and a single
/// "Focus pane" `<action>` whose `arguments` echo the launch arg. All
/// user-supplied text is sanitized + XML-escaped via [`sanitize_escape`].
///
/// Pure so it can be unit-tested without WinRT.
pub fn build_toast_xml_string(req: &ToastRequest) -> String {
    let title = sanitize_escape(&req.title);
    let body = sanitize_escape(&req.body);
    let launch = sanitize_escape(&req.launch_arg);
    let scenario_attr = match scenario_for(req.urgency) {
        Some(s) => format!(r#" scenario="{s}""#),
        None => String::new(),
    };

    format!(
        r#"<toast launch="{launch}" activationType="foreground"{scenario_attr}>
  <visual>
    <binding template="ToastGeneric">
      <text>{title}</text>
      <text>{body}</text>
    </binding>
  </visual>
  <actions>
    <action content="Focus pane" arguments="{launch}" activationType="foreground"/>
  </actions>
</toast>"#
    )
}

/// Encode a Rust `&str` as a NUL-terminated UTF-16 buffer for the Win32 -W APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// AUMID registration
// ---------------------------------------------------------------------------

/// Write `HKCU\Software\Classes\AppUserModelId\<aumid>` with a `DisplayName`
/// value. This is the only registration an unpackaged app needs for the shell
/// to accept `CreateToastNotifier(aumid)` — no shortcut, MSIX, or elevation.
fn register_aumid(aumid: &str, display_name: &str) -> std::io::Result<()> {
    let subkey = wide(&format!(r"Software\Classes\AppUserModelId\{aumid}"));
    let mut hkey = HKEY::default();

    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .ok()
        .map_err(to_io)?;

        let value_name = wide("DisplayName");
        let value_data = wide(display_name);
        let data_bytes: &[u8] = std::slice::from_raw_parts(
            value_data.as_ptr() as *const u8,
            value_data.len() * std::mem::size_of::<u16>(),
        );

        let rc = RegSetValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            REG_SZ,
            Some(data_bytes),
        );

        let _ = RegCloseKey(hkey);

        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::other(format!(
                "RegSetValueExW(DisplayName) returned {rc:?}"
            )));
        }
    }

    Ok(())
}

/// Convert a `windows::core::Error` into a `std::io::Error`.
fn to_io(e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

// ---------------------------------------------------------------------------
// Notifier
// ---------------------------------------------------------------------------

/// Owns the AUMID and an in-process activation queue. Each shown toast wires
/// its `Activated` event to push the click's launch arg onto the shared queue,
/// which callers drain via [`Notifier::poll_activations`].
pub struct Notifier {
    aumid: String,
    activations: Arc<Mutex<Vec<String>>>,
}

impl Notifier {
    /// Register the AUMID (HKCU registry, no shortcut/MSIX/elevation) and claim
    /// it for this process so the shell attributes toasts to it.
    ///
    /// `aumid` e.g. `"com.gmux.app"`, `display_name` e.g. `"gmux"`.
    pub fn new(aumid: &str, display_name: &str) -> std::io::Result<Notifier> {
        // Claim the AUMID for this process so taskbar grouping + toast
        // attribution agree on one identity.
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(aumid)).map_err(to_io)?;
        }
        register_aumid(aumid, display_name)?;

        Ok(Notifier {
            aumid: aumid.to_string(),
            activations: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Build and show a toast. Uses `SetTag`/`SetGroup` so a same-tag re-show
    /// replaces in place. Wires the `Activated` event before `Show()` so a
    /// click enqueues the launch arg for [`Notifier::poll_activations`].
    ///
    /// Does NOT gate on `ToastNotifier.Setting()` — a fresh AUMID returns an
    /// error on run 1, which means "first run", not "disabled".
    pub fn show(&self, req: &ToastRequest) -> std::io::Result<()> {
        let xml_str = build_toast_xml_string(req);

        let doc = XmlDocument::new().map_err(to_io)?;
        doc.LoadXml(&HSTRING::from(xml_str)).map_err(to_io)?;

        let toast = ToastNotification::CreateToastNotification(&doc).map_err(to_io)?;
        toast.SetTag(&HSTRING::from(&req.tag)).map_err(to_io)?;
        toast.SetGroup(&HSTRING::from(&req.group)).map_err(to_io)?;

        // Wire Activated -> push the click's launch arg onto the shared queue.
        let queue = Arc::clone(&self.activations);
        toast
            .Activated(&TypedEventHandler::<
                ToastNotification,
                windows::core::IInspectable,
            >::new(move |_sender, args| {
                if let Some(args) = args.as_ref() {
                    if let Ok(act) = args.cast::<ToastActivatedEventArgs>() {
                        let arguments = act.Arguments().unwrap_or_default();
                        if let Ok(mut q) = queue.lock() {
                            q.push(arguments.to_string());
                        }
                    }
                }
                Ok(())
            }))
            .map_err(to_io)?;

        let notifier =
            ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(&self.aumid))
                .map_err(to_io)?;

        // Deliberately do NOT gate on notifier.Setting(): a fresh AUMID returns
        // 0x80070490 on run 1, which is first-run, not disabled.
        notifier.Show(&toast).map_err(to_io)?;
        Ok(())
    }

    /// Remove a previously shown toast from the banner and Notification Center
    /// via `ToastNotificationManager::History().Remove(tag, group, aumid)`.
    /// Best-effort: errors are swallowed.
    pub fn clear(&self, tag: &str, group: &str) {
        if let Ok(history) = ToastNotificationManager::History() {
            let _ = history.RemoveGroupedTagWithId(
                &HSTRING::from(tag),
                &HSTRING::from(group),
                &HSTRING::from(&self.aumid),
            );
        }
    }

    /// Drain and return the launch args from any toast clicks since the last
    /// call. Empty if nothing was clicked.
    pub fn poll_activations(&self) -> Vec<String> {
        match self.activations.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Taskbar flash
// ---------------------------------------------------------------------------

/// Flash the window's taskbar button. `on == true` flashes (TRAY|TIMERNOFG,
/// i.e. until the window gains focus); `on == false` stops the flashing.
pub fn flash_window(hwnd: isize, on: bool) {
    let flags = if on {
        FLASHW_TRAY | FLASHW_TIMERNOFG
    } else {
        FLASHW_STOP
    };
    let info = FLASHWINFO {
        cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
        hwnd: HWND(hwnd as *mut _),
        dwFlags: flags,
        uCount: 0,
        dwTimeout: 0,
    };
    unsafe {
        let _ = FlashWindowEx(&info);
    }
}

// ---------------------------------------------------------------------------
// Taskbar progress
// ---------------------------------------------------------------------------

/// Wraps `ITaskbarList3` bound to a single window for taskbar-button progress.
pub struct Taskbar {
    list: ITaskbarList3,
    hwnd: HWND,
}

impl Taskbar {
    /// Create a taskbar-progress controller for `hwnd`. Best-effort
    /// `CoInitializeEx(APARTMENTTHREADED)` (a prior init on this thread with a
    /// compatible model is fine); returns `None` if the COM object cannot be
    /// created.
    pub fn new(hwnd: isize) -> Option<Taskbar> {
        unsafe {
            // Best-effort apartment init. RPC_E_CHANGED_MODE / S_FALSE mean the
            // thread is already initialized; both are acceptable.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let list: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_ALL).ok()?;
            Some(Taskbar {
                list,
                hwnd: HWND(hwnd as *mut _),
            })
        }
    }

    /// Set the progress state and, for `Normal`/`Error`/`Paused`, an optional
    /// percentage (0..=100). `Indeterminate` ignores `pct`. `None` clears.
    pub fn set_progress(&self, state: ProgressState, pct: Option<u8>) {
        let flag = match state {
            ProgressState::None => TBPF_NOPROGRESS,
            ProgressState::Normal => TBPF_NORMAL,
            ProgressState::Error => TBPF_ERROR,
            ProgressState::Indeterminate => TBPF_INDETERMINATE,
            ProgressState::Paused => TBPF_PAUSED,
        };
        unsafe {
            let _ = self.list.SetProgressState(self.hwnd, flag);
            // A value is only meaningful for the determinate states.
            if matches!(
                state,
                ProgressState::Normal | ProgressState::Error | ProgressState::Paused
            ) {
                if let Some(p) = pct {
                    let p = p.min(100) as u64;
                    let _ = self.list.SetProgressValue(self.hwnd, p, 100);
                }
            }
        }
    }

    /// Clear any taskbar progress (equivalent to `set_progress(None, None)`).
    pub fn clear_progress(&self) {
        self.set_progress(ProgressState::None, None);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_escapes_five_xml_specials() {
        let input = r#"a & b < c > d " e ' f"#;
        let out = sanitize_escape(input);
        assert_eq!(out, "a &amp; b &lt; c &gt; d &quot; e &apos; f");
    }

    #[test]
    fn sanitize_strips_c0_control_chars() {
        // Include NUL, tab, newline, CR, and other C0 chars; none may survive.
        let input = "hel\u{0}lo\tworld\r\n\u{1}\u{1f}!";
        let out = sanitize_escape(input);
        assert_eq!(out, "helloworld!");
        assert!(!out.chars().any(|c| (c as u32) <= 0x1F));
    }

    #[test]
    fn sanitize_strips_c1_control_chars() {
        // 0x80..=0x9F must be stripped; 0xA0 (NBSP) and normal unicode kept.
        let input = "x\u{80}y\u{9f}z\u{a0}\u{2603}";
        let out = sanitize_escape(input);
        assert_eq!(out, "xyz\u{a0}\u{2603}");
        assert!(!out.chars().any(|c| (0x80..=0x9F).contains(&(c as u32))));
    }

    #[test]
    fn sanitize_neutralizes_metachars_and_control_bytes_together() {
        // A single string carrying ALL five XML metacharacters interleaved with
        // the specific control bytes called out: ESC (0x1b), newline (0x0a),
        // and BEL (0x07). Every metachar must become an entity and every
        // control byte must vanish.
        let input = "&\u{1b}<\u{0a}>\u{07}\"'";
        let out = sanitize_escape(input);
        assert_eq!(out, "&amp;&lt;&gt;&quot;&apos;");
        // No raw control byte of any kind survived (C0 or C1).
        assert!(
            !out.chars().any(|c| {
                let u = c as u32;
                u <= 0x1F || (0x80..=0x9F).contains(&u)
            }),
            "sanitized output still holds a raw control byte: {out:?}"
        );
        // Specifically none of the three named bytes remain.
        for bad in ['\u{1b}', '\u{0a}', '\u{07}'] {
            assert!(!out.contains(bad), "byte {:#x} survived", bad as u32);
        }
    }

    #[test]
    fn xml_title_with_metachars_and_control_bytes_is_neutralized() {
        // A hostile TITLE: every XML metacharacter (& < > " ') plus the three
        // named embedded control bytes (ESC 0x1b, newline 0x0a, BEL 0x07).
        let hostile_title = "a&b\u{1b}<c>\u{0a}\"d\u{07}'e";
        let req = ToastRequest {
            tag: "pane-9".into(),
            group: "sess-1".into(),
            title: hostile_title.into(),
            body: "clean body".into(),
            urgency: Urgency::Normal,
            launch_arg: "pane=9;action=focus".into(),
        };
        let xml = build_toast_xml_string(&req);

        // The title's contribution must be fully escaped, control bytes gone.
        // Expected: metachars -> entities, control bytes -> removed.
        let expected_title = "a&amp;b&lt;c&gt;&quot;d&apos;e";
        assert!(
            xml.contains(&format!("<text>{expected_title}</text>")),
            "escaped title not found in XML:\n{xml}"
        );

        // No RAW metacharacter from the title leaked in. The template itself
        // contains ", <, >, & only in known structural spots, so we assert the
        // hostile title's raw specials are absent as *content* by checking the
        // title node holds no bare metachar between its tags.
        let title_node = xml
            .split("<text>")
            .nth(1)
            .and_then(|s| s.split("</text>").next())
            .expect("title <text> node present");
        assert_eq!(title_node, expected_title);
        assert!(!title_node.contains('&') || title_node.contains("&amp;"));
        for raw in ['<', '>', '"', '\''] {
            assert!(
                !title_node.contains(raw),
                "raw metachar {raw:?} survived in title node: {title_node:?}"
            );
        }

        // The three named control bytes contributed by the title must not
        // appear anywhere in the produced XML. ESC (0x1b) and BEL (0x07) never
        // occur in the structural template, so a global check is exact.
        for bad in [0x1bu32, 0x07u32] {
            assert!(
                !xml.chars().any(|c| c as u32 == bad),
                "control byte {bad:#x} leaked into XML"
            );
        }
        // Newline (0x0a) DOES appear structurally in the template, so a raw
        // global check would be wrong. Instead prove the title node is free of
        // it (it must be a single line with no embedded 0x0a).
        assert!(
            !title_node.chars().any(|c| c as u32 == 0x0a),
            "newline leaked into the title node"
        );

        // Belt-and-suspenders: no C0 (except structural whitespace \n which the
        // author writes, never the user) originates from the title node, and no
        // C1 anywhere.
        assert!(
            !title_node.chars().any(|c| (c as u32) <= 0x1F),
            "a C0 control byte survived in the title node"
        );
        assert!(
            !xml.chars().any(|c| (0x80..=0x9F).contains(&(c as u32))),
            "a C1 control byte survived in the XML"
        );
    }

    fn sample_request(urgency: Urgency) -> ToastRequest {
        ToastRequest {
            tag: "pane-5".into(),
            group: "sess-0".into(),
            title: "claude <build> & \"deploy\"".into(),
            body: "line1\u{0}line2 <tag> ' & done".into(),
            urgency,
            launch_arg: "pane=5;action=focus".into(),
        }
    }

    #[test]
    fn xml_contains_escaped_title_and_body_and_launch_arg() {
        let xml = build_toast_xml_string(&sample_request(Urgency::Normal));

        // Escaped title present, raw special chars absent from the title text.
        assert!(xml.contains("claude &lt;build&gt; &amp; &quot;deploy&quot;"));
        // Body: NUL stripped, specials escaped.
        assert!(xml.contains("line1line2 &lt;tag&gt; &apos; &amp; done"));
        // Launch arg appears (on both the toast launch= and the action).
        assert!(xml.contains(r#"launch="pane=5;action=focus""#));
        assert!(xml.contains(r#"arguments="pane=5;action=focus""#));
    }

    #[test]
    fn xml_has_no_raw_control_chars() {
        let xml = build_toast_xml_string(&sample_request(Urgency::Normal));
        // No C0 chars except the structural whitespace we author (\n, spaces).
        // The sanitized user text must contribute none: assert no NUL / C1.
        assert!(!xml.chars().any(|c| c as u32 == 0x0));
        assert!(!xml
            .chars()
            .any(|c| (0x80..=0x9F).contains(&(c as u32))));
    }

    #[test]
    fn critical_maps_to_urgent_scenario() {
        let xml = build_toast_xml_string(&sample_request(Urgency::Critical));
        assert!(xml.contains(r#"scenario="urgent""#));
        assert_eq!(scenario_for(Urgency::Critical), Some("urgent"));
    }

    #[test]
    fn non_critical_has_no_scenario() {
        for u in [Urgency::Low, Urgency::Normal] {
            let xml = build_toast_xml_string(&sample_request(u));
            assert!(!xml.contains("scenario="));
            assert_eq!(scenario_for(u), None);
        }
    }

    /// Desktop-only smoke test: constructs a real Notifier and shows a toast.
    /// Requires an interactive desktop session; run with:
    ///   cargo test -p gmux-notify -- --ignored show_real_toast
    #[test]
    #[ignore = "requires desktop"]
    fn show_real_toast() {
        let notifier = Notifier::new("com.gmux.test", "gmux (test)")
            .expect("Notifier::new should register the AUMID");
        let req = ToastRequest {
            tag: "pane-5".into(),
            group: "sess-0".into(),
            title: "gmux: agent needs attention".into(),
            body: "Pane 5 finished. Click to focus.".into(),
            urgency: Urgency::Critical,
            launch_arg: "pane=5;action=focus".into(),
        };
        notifier.show(&req).expect("show should succeed on desktop");
    }
}
