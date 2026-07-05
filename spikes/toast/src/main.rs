//! SPIKE 3 — Unpackaged Windows toast from Rust via registry AUMID.
//!
//! Proves ADR-006: an unpackaged Rust process can
//!   1. claim an AppUserModelID purely by writing a HKCU registry key
//!      (no Start-Menu shortcut, no MSIX, no elevation),
//!   2. fire a real ToastGeneric toast with a launch arg + an <action> button,
//!   3. wire the in-process ToastNotification.Activated / Dismissed / Failed
//!      events to receive the click-back argument.
//!
//! Run it, then click the toast (or its "Focus pane" button) within ~25 s.

use std::sync::mpsc;
use std::time::Duration;

use windows::core::{h, Interface, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    NotificationSetting, ToastActivatedEventArgs, ToastDismissedEventArgs, ToastNotification,
    ToastNotificationManager,
};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

const AUMID: &str = "com.gmux.spike";
const DISPLAY_NAME: &str = "gmux (M0 spike)";

/// Encode a Rust &str as a NUL-terminated UTF-16 buffer for the Win32 -W APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Write HKCU\Software\Classes\AppUserModelId\<AUMID> with a DisplayName value.
/// This is the ONLY registration an unpackaged app needs for the shell to
/// accept CreateToastNotifier(AUMID). No shortcut, no MSIX, no elevation.
fn register_aumid() -> windows::core::Result<()> {
    let subkey = wide(&format!(
        r"Software\Classes\AppUserModelId\{AUMID}"
    ));
    let mut hkey = HKEY::default();

    unsafe {
        // RegCreateKeyExW creates the key if absent, opens it if present.
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
        .ok()?;

        let value_name = wide("DisplayName");
        let value_data = wide(DISPLAY_NAME);
        // Byte length INCLUDING the terminating NUL (RegSetValueExW wants bytes).
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
            eprintln!("[warn] RegSetValueExW(DisplayName) returned {:?}", rc);
        }
        // IconUri intentionally omitted — optional per ADR-006. A real gmux build
        // would set an IconUri value pointing at a bundled .ico/.png file path.
    }

    Ok(())
}

fn build_toast_xml() -> windows::core::Result<XmlDocument> {
    // ToastGeneric template. launch= is delivered when the toast BODY is clicked;
    // the <action> arguments= is delivered when that button is clicked.
    let xml = r#"<toast launch="pane=5;action=focus" activationType="foreground">
  <visual>
    <binding template="ToastGeneric">
      <text>gmux: agent needs attention</text>
      <text>Pane 5 finished. Click to focus.</text>
    </binding>
  </visual>
  <actions>
    <action content="Focus pane"
            arguments="pane=5;action=focus"
            activationType="foreground"/>
  </actions>
</toast>"#;

    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    Ok(doc)
}

fn main() -> windows::core::Result<()> {
    println!("== SPIKE 3: unpackaged toast via registry AUMID ==");

    // 1. Claim the AUMID for THIS process so the shell attributes the toast to us.
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(h!("com.gmux.spike"))?;
    }
    println!("[ok] SetCurrentProcessExplicitAppUserModelID(com.gmux.spike)");

    // 2. Register the AUMID in HKCU (DisplayName).
    register_aumid()?;
    println!(r"[ok] wrote HKCU\Software\Classes\AppUserModelId\com.gmux.spike\DisplayName");

    // 3. Build the notifier.
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
    println!("[ok] CreateToastNotifierWithId(com.gmux.spike)");

    // Report whether notifications are enabled for this AUMID.
    match notifier.Setting() {
        Ok(setting) => {
            let enabled = setting == NotificationSetting::Enabled;
            println!(
                "[info] notifier.Setting() = {:?} (enabled = {})",
                setting, enabled
            );
        }
        Err(e) => println!("[warn] notifier.Setting() failed: {e}"),
    }

    // 4. Build the toast + register handlers BEFORE Show().
    let xml = build_toast_xml()?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;

    // Channel so the event thread can tell main() a click landed.
    let (tx, rx) = mpsc::channel::<String>();

    let tx_act = tx.clone();
    toast.Activated(&TypedEventHandler::<ToastNotification, windows::core::IInspectable>::new(
        move |_sender, args| {
            // args is IInspectable; cast to ToastActivatedEventArgs for .Arguments().
            if let Some(args) = args.as_ref() {
                if let Ok(act) = args.cast::<ToastActivatedEventArgs>() {
                    let arguments = act.Arguments().unwrap_or_default();
                    println!("[EVENT] Activated  arguments = {:?}", arguments.to_string());
                    let _ = tx_act.send(format!("Activated: {}", arguments));
                } else {
                    println!("[EVENT] Activated (could not cast args)");
                    let _ = tx_act.send("Activated (uncastable args)".into());
                }
            } else {
                println!("[EVENT] Activated (null args)");
                let _ = tx_act.send("Activated (null args)".into());
            }
            Ok(())
        },
    ))?;

    toast.Dismissed(&TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(
        move |_sender, args| {
            if let Some(args) = args.as_ref() {
                if let Ok(reason) = args.Reason() {
                    println!("[EVENT] Dismissed reason = {:?}", reason);
                }
            }
            Ok(())
        },
    ))?;

    toast.Failed(&TypedEventHandler::<ToastNotification, windows::UI::Notifications::ToastFailedEventArgs>::new(
        move |_sender, args| {
            if let Some(args) = args.as_ref() {
                if let Ok(err) = args.ErrorCode() {
                    println!("[EVENT] Failed errorcode = {:?}", err);
                }
            }
            Ok(())
        },
    ))?;

    println!("[ok] registered Activated / Dismissed / Failed handlers");

    // 5. Show it.
    match notifier.Show(&toast) {
        Ok(()) => println!("[ok] notifier.Show(toast) returned Ok"),
        Err(e) => {
            println!("[FAIL] notifier.Show(toast) errored: {e}");
            return Err(e);
        }
    }

    println!("[info] toast shown. Click it (or 'Focus pane') within 25 s...");

    // Keep the process alive so the in-process Activated handler can fire.
    // Poll the channel so we print promptly if a click lands.
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(msg) => println!("[main] received from event thread: {msg}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("[done] 25 s window elapsed; exiting.");
    Ok(())
}
