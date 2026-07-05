# Windows Toast Notifications for gmux — Research

**Date:** 2026-07-04
**Scope:** Toast delivery options for an UNPACKAGED, non-elevated, Rust-based Win32 desktop app (gmux); activation/focus behavior; suppression detection; complementary attention channels.
**Confidence legend:** [WV] = web-verified this session against a primary source; [MK] = model knowledge, not re-verified; [UNC] = uncertain/conflicting.

---

## 1. Executive summary

- The classic WinRT `ToastNotificationManager` path is the right one for gmux. The old "must install a Start Menu shortcut with an AppUserModelID" rule is **dead** for practical purposes: an unpackaged app registers by writing `HKCU\Software\Classes\AppUserModelId\<AUMID>` registry values (`DisplayName`, `IconUri`, optional `CustomActivator`) — no shortcut, no elevation, no MSIX. [WV]
- Windows App SDK `AppNotificationManager` **works unpackaged but drags in the Windows App Runtime** (framework + *Singleton* MSIX packages) as a machine-wide dependency; self-contained deployment explicitly does *not* cover the notification APIs (they depend on the Singleton package), and a bug open through WinAppSDK 1.8 (Dec 2025) shows `Register()` failing in self-contained unpackaged apps unless the runtime is installed. Not worth it for a Rust app. [WV]
- `Shell_NotifyIcon` balloons (NIF_INFO) still exist, render as toast-styled popups, but are legacy: on Windows 11 they don't persist into Notification Center, have queuing quirks, support no buttons/arguments, and give no activation payload. Use a tray icon for *presence*, not for notifications. [WV]
- **Activation:** while gmux is running (the normal case for a terminal multiplexer), the in-process `ToastNotification.Activated` event delivers the click with arguments — no COM registration needed. For click-after-exit you need the `INotificationActivationCallback` COM LocalServer (CLSID in `CustomActivator` + `HKCU\Software\Classes\CLSID\{...}\LocalServer32`). Both paths hit the same trap: **the activated process frequently does not get foreground rights** (ShellExperienceHost owns the foreground), so `SetForegroundWindow` may silently degrade to a taskbar flash. Plan mitigations. [WV]
- **Suppression:** Do-Not-Disturb/Focus state is detectable via `ToastNotificationManagerForUser.NotificationMode` (+ `NotificationModeChanged` event) **only on Windows 11** (UniversalApiContract v15, introduced build 10.0.23504). On Windows 10 21H2 the only option is the undocumented WNF query (`WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED` via `NtQueryWnfStateData`). Per-app disablement is detectable via `ToastNotifier.Setting`. [WV]
- **Rust:** `windows` crate (0.62.x as of this session) has full projections for `Windows.UI.Notifications` *and* `INotificationActivationCallback` (`windows::Win32::UI::Notifications`). `tauri-winrt-notification` 0.7.3 (released 2026-07-02, ~6M downloads) is alive and has `on_activated`/`on_dismissed` callbacks. Recommended: thin in-house layer over `windows`, optionally cribbing from tauri-winrt-notification. [WV]
- Toasts alone are not enough; the reliable "you WILL notice" stack is: in-app pane indicator (always) → `FlashWindowEx` + `ITaskbarList3::SetOverlayIcon` badge (when unfocused) → toast (when unfocused, suppressible) → `ITaskbarList3::SetProgressState` driven by ConEmu `OSC 9;4` (ambient state, pairs perfectly with agent progress). [MK/WV mixed, see §7]

---

## 2. Options for an unpackaged Win32 app

### 2.1 Classic WinRT `ToastNotificationManager` + AUMID (RECOMMENDED)

**API:** `Windows.UI.Notifications.ToastNotificationManager.CreateToastNotifier(aumid)` → `ToastNotifier.Show(toast)`, where `toast` is a `ToastNotification` built from an `XmlDocument` containing the [toast content XML schema](https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/app-notifications-content). Available since Windows 8; fully functional on Win10 21H2 and Win11, x64 and ARM64 (it's an OS inbox API, `windows.dll`). [WV for API existence; MK for ARM64 but this is inbox OS code]

**The shortcut requirement is history.** The legacy docs ([Quickstart: Sending a toast notification from the desktop, Win8-era](https://learn.microsoft.com/en-us/previous-versions/windows/desktop/legacy/hh802768(v=vs.85)) and [How to enable desktop toast notifications through an AppUserModelID](https://learn.microsoft.com/en-us/windows/win32/shell/enable-desktop-toast-with-appusermodelid)) required a Start-Menu `.lnk` carrying `System.AppUserModel.ID`. The modern documented path for "other types of unpackaged apps" ([send-local-toast-other-apps](https://github.com/MicrosoftDocs/windows-dev-docs/blob/docs/hub/apps/design/shell/tiles-and-notifications/send-local-toast-other-apps.md)) is **registry-only registration**: [WV]

```
HKCU\Software\Classes\AppUserModelId\<YOUR_AUMID>      (HKLM also works, machine-wide)
    DisplayName          REG_EXPAND_SZ   "gmux"
    IconUri              REG_EXPAND_SZ   "C:\Program Files\gmux\assets\toast.png"
    IconBackgroundColor  REG_SZ          "FF2B2B2B"        (AARRGGBB; optional)
    CustomActivator      REG_SZ          "{GUID-of-COM-activator}"   (optional, see §3)
```

Then call `CreateToastNotifier("<YOUR_AUMID>")` with exactly that AUMID string. HKCU registration needs **no elevation** (confirmed by community usage, e.g. [BurntToast #236](https://github.com/Windos/BurntToast/issues/236) — "Applications register with the Windows notification system by creating registry keys under HKCU\Software\Classes\AppUserModelId"). [WV]

This is the same mechanism the Windows Community Toolkit shipped in 7.0 as "no more shortcut needed" ([CommunityToolkit PR #3457](https://github.com/CommunityToolkit/WindowsCommunityToolkit/pull/3457)) — `ToastNotificationManagerCompat` auto-writes the AUMID + COM activator registration on first use. Firefox, Tailscale, and PowerShell's BurntToast all use registry-based AUMIDs today. [WV for PR; MK for Firefox/Tailscale specifics]

Also set the AUMID on your process/window (`SetCurrentProcessExplicitAppUserModelID` or per-window `System.AppUserModel.ID` prop) so taskbar grouping, the Settings per-app toggle, and toast attribution all agree on one identity. [MK — standard practice, see [Tailscale #8386](https://github.com/tailscale/tailscale/issues/8386) for a real-world example of why]

**Caveats:**
- A toast sent with an unregistered AUMID fails (`Show` silently no-ops or throws "element not found"). Register at first run, before the first `Show`. [MK]
- Elevated (admin) processes: app notifications are documented as unsupported for elevated apps in the WinAppSDK docs ("Show will fail silently"); the classic path is similarly unreliable when elevated. gmux should not run elevated anyway; if a user launches it elevated, degrade to FlashWindowEx/tray. [WV for WinAppSDK statement; UNC for exact classic-path behavior when elevated]

### 2.2 Windows App SDK `AppNotificationManager` (NOT recommended for gmux)

State as of mid-2026: WinAppSDK 2.0 stable released **2026-04-29** (servicing to 2027-04-29), patch 2.1.3 on 2026-05-21, and 2.2.0 released ~June 2026 ([release notes](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-notes/windows-app-sdk-2-0), [release channels](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-channels)). [WV]

- **Unpackaged support exists.** For unpackaged apps, `AppNotificationManager.Default.Register()` auto-registers the calling process as the COM server and pulls display name/icon from the shell — no manifest, no manual AUMID ([Register docs](https://learn.microsoft.com/en-us/windows/windows-app-sdk/api/winrt/microsoft.windows.appnotifications.appnotificationmanager.register), [console-app guide](https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/app-notifications-console)). [WV]
- **Register()/event semantics** (from the [quickstart](https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/app-notifications-quickstart), fetched this session): hook `NotificationInvoked` **before** calling `Register()`, otherwise a *new process* is launched to handle the click; call `Register()` **before** `AppInstance.GetActivatedEventArgs()`; if the app was dead, launch comes via COM activation with `ExtendedActivationKind.AppNotification` (or `Launch` + later `NotificationInvoked`); `activationType="background"` in the XML payload is **ignored** for desktop apps — you decide in code whether to show UI. Elevated apps unsupported ("Show will fail silently"). [WV]
- **The dependency problem:** `AppNotificationManager` depends on the Windows App Runtime **Singleton** MSIX package. The [self-contained deployment guide](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/self-contained-deploy/deploy-self-contained-apps) (updated 2026-05-28, fetched this session) explicitly lists app notifications as an API that "rel[ies] on additional MSIX packages" and tells you to gate on `AppNotificationManager.IsSupported()`, ship the MSIX dependency in your installer, or not use the API. [WV]
- **Known bug:** [WindowsAppSDK #6071](https://github.com/microsoft/WindowsAppSDK/issues/6071) — self-contained unpackaged app calling `Register()` throws "Unable to load resource dll. Microsoft.WindowsAppRuntime.Insights.Resource.dll" unless `Microsoft.WindowsAppSDK.Runtime` is installed machine-wide. Affects 1.8.260224000; still open/needs-triage as of Dec 2025. Unverified whether 2.x fixed it. [WV]
- **For Rust specifically:** WinAppSDK is consumed via C++/WinRT or C# projections; Rust bindings exist only as third-party experiments. Adding a machine-wide runtime installer requirement to a Rust terminal app to get the *same toasts* the inbox API already provides is a bad trade. [MK]

**Verdict:** skip. Everything gmux needs (buttons, hero image, progress, tag/group, urgent scenario) is in the inbox `Windows.UI.Notifications` XML schema.

### 2.3 `Shell_NotifyIcon` balloon (NIF_INFO) fallback

- Not formally deprecated as an API, but a legacy corner: `NOTIFYICONDATA.uTimeout` deprecated since Vista; balloons render in the toast visual style on Win10/11 ([NOTIFYICONDATAW docs](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-notifyicondataw)). [WV]
- Windows 11 regressions: balloon content **does not persist into Notification Center** and there are queuing/stuck-balloon bugs ([MS Q&A 1362368](https://learn.microsoft.com/en-us/answers/questions/1362368/how-to-show-balloon-tip-message-in-windows-notific), [MS Q&A 1377262](https://learn.microsoft.com/en-us/answers/questions/1377262/balloontip-(toast-notification)-does-not-disappear)). No action buttons, no argument payload, requires a tray icon to exist. [WV]
- Use only as a last-ditch fallback (e.g., toast registration failed) — or not at all.

---

## 3. Activation: getting the click back into gmux

### 3.1 In-process `Activated` event (covers gmux's main case)

`ToastNotification` exposes WinRT events: `Activated` (`TypedEventHandler<ToastNotification, IInspectable>` — cast args to `ToastActivatedEventArgs` for `.Arguments` and, on newer builds, `.UserInput`), `Dismissed` (`ToastDismissedEventArgs.Reason`: `UserCanceled`/`ApplicationHidden`/`TimedOut`), and `Failed`. These fire **in your process** as long as it is alive — no COM registration required. Since gmux *is* the terminal hosting the agent panes, it is by definition running when its toasts are clicked; this covers ~all real scenarios. Windows-docs-rs projection: [`ToastActivatedEventArgs`](https://microsoft.github.io/windows-docs-rs/doc/windows/UI/Notifications/struct.ToastActivatedEventArgs.html). [WV for API surface; MK for exact handler-arg casting details]

Encode the target in the launch/button arguments, e.g. `launch="pane=42;action=focus"` and `<action content="Focus pane" arguments="pane=42;action=focus"/>`.

### 3.2 COM activator (`INotificationActivationCallback`) — for click-after-exit and Action Center clicks

Documented in [Send a local toast from a WRL C++ desktop app](https://learn.microsoft.com/en-us/windows/apps/design/shell/tiles-and-notifications/send-local-toast-desktop-cpp-wrl) and the registry doc in §2.1: [WV]

1. Pick a CLSID GUID; write it to `CustomActivator` under your AppUserModelId key.
2. Register `HKCU\Software\Classes\CLSID\{GUID}\LocalServer32` = `"C:\path\gmux.exe" -ToastActivated` (any args; Windows appends `-Embedding` when COM-launching).
3. Implement `INotificationActivationCallback::Activate(LPCWSTR appUserModelId, LPCWSTR invokedArgs, const NOTIFICATION_USER_INPUT_DATA* data, ULONG count)` on an out-of-proc COM object, register the class object at startup (`CoRegisterClassObject`).
4. When the toast (or its buttons with `activationType="foreground"`/default) is clicked, Windows COM-activates the LocalServer if not running, or calls into the running instance's registered class object.

In Rust: `windows::Win32::UI::Notifications::INotificationActivationCallback` exists in the `windows` crate (0.62.x) ([windows-docs-rs](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Notifications/struct.INotificationActivationCallback.html)); implement with the `#[implement]` macro + `CoRegisterClassObject` from `Win32_System_Com`. [WV for interface presence; MK for implement-macro workflow]

Alternative: **protocol activation** (`activationType="protocol"` + `launch="gmux://focus/pane/42"` + an HKCU protocol handler). Simpler, no COM — but spawns a new process instance that must forward to the running one over the named pipe (which gmux has anyway: `\\.\pipe\gmux`). Also historically the workaround when COM activation misbehaves ([microsoft-ui-xaml #5499](https://github.com/microsoft/microsoft-ui-xaml/issues/5499)). [WV]

### 3.3 Foreground-rights gotcha (IMPORTANT)

When a toast is clicked, the foreground process is **ShellExperienceHost.exe**, not gmux. Windows' foreground-lock rules ([SetForegroundWindow docs](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow)) mean the activated app often lacks the right to take foreground; `SetForegroundWindow` then just flashes the taskbar button. Evidence: [microsoft-ui-xaml #5499](https://github.com/microsoft/microsoft-ui-xaml/issues/5499) (COM-activated app "activated in the background", `AttachThreadInput` to ShellExperienceHost impossible), [Mozilla bug 1863798](https://bugzilla.mozilla.org/show_bug.cgi?id=1863798) (Thunderbird/Firefox notification click doesn't foreground the window), [CommunityToolkit #4874](https://github.com/CommunityToolkit/WindowsCommunityToolkit/issues/4874). Foreground privilege can only be *given*, e.g. via `CoAllowSetForegroundWindow` from a process that has it ([docs](https://learn.microsoft.com/en-us/windows/win32/api/objbase/nf-objbase-coallowsetforegroundwindow), [Old New Thing](https://devblogs.microsoft.com/oldnewthing/20090220-00/?p=19083)) — but the shell doesn't reliably call it on your behalf. [WV]

Mitigation ladder for gmux's `focus_pane(pane_id)`:
1. `ShowWindow(SW_RESTORE)` if minimized, then `SetForegroundWindow` immediately on the activation callback thread (works in many cases — the shell grants foreground to COM-activated notification servers in the common path). [MK — empirically true in MS samples, not guaranteed]
2. If `GetForegroundWindow() != hwnd` afterwards: fall back to `FlashWindowEx` (honest, unobtrusive) — never inject input (`AttachThreadInput`/synthetic Alt keypress hacks are fragile and can be flagged). [MK/opinion]
3. Regardless of focus outcome, always select the target session/window/pane internally so the *next* click on the taskbar lands the user on the right pane. [design]

---

## 4. Rust ecosystem (mid-2026)

| Crate | Version / date | Status | Activation | Notes |
|---|---|---|---|---|
| `windows` (windows-rs) | 0.62.2 (latest on docs.rs this session) | First-party, active | Full: WinRT events + `INotificationActivationCallback` | Features: `UI_Notifications`, `Data_Xml_Dom`, `Foundation`, `Win32_UI_Notifications`, `Win32_System_Com`, `Win32_UI_Shell` [WV version; MK feature names] |
| `tauri-winrt-notification` | 0.7.3, **2026-07-02**, ~6.07M downloads | Maintained (tauri-apps) | `on_activated(Option<String>)` (button action string), `on_dismissed(ToastDismissalReason)` — in-proc only, no COM activator | Builder API: `hero()`, `image()`, `add_button()`, `duration()`, `scenario()`; `Toast::POWERSHELL_APP_ID` fallback AUMID ("toast will erroneously report its origin as powershell") — gmux must pass its own registered AUMID instead. No registry-registration helper. [WV] |
| `winrt-toast` (original) | stale | effectively unmaintained | `show_with_callbacks` | superseded by fork ↓ [WV] |
| `winrt-toast-reborn` | 0.3.8, 2025-09-01, ~15K downloads | fork of kdeconnect-rs/winrt-toast (AtifChy) | activated/dismissed/failed callbacks | has a `register()` helper that writes the AUMID registry keys (hive details unconfirmed) [WV version; UNC register() hive] |
| `win-toast-notify` | 0.1.6, 2024-08-01, ~10K downloads | stale (2 years) | limited | supports progress bars, buttons; low adoption [WV] |
| `notify-rust` | active | cross-platform facade | limited on Windows | Windows backend delegates to the winrt-notification lineage; abstraction hides tag/group/scenario control gmux needs [MK] |

**Recommendation:** write gmux's own ~300-line `toast.rs` on the `windows` crate: registry registration (HKCU AppUserModelId) at startup, XML builder for exactly the payloads gmux uses, in-proc `Activated`/`Dismissed` handlers, tag/group management. Adopt the COM activator later if click-after-exit matters (it barely does for a live multiplexer). `tauri-winrt-notification` is a fine bootstrap/reference but its `Option<String>` activation arg and lack of tag/group update control will pinch. [opinion grounded in WV facts]

## 5. C# path (for reference / if any companion tooling is C#)

- **`Microsoft.Toolkit.Uwp.Notifications` 7.1.3** — the classic `ToastContentBuilder`/`ToastNotificationManagerCompat` package. Repo **archived** ([WindowsCommunityToolkit](https://github.com/CommunityToolkit/WindowsCommunityToolkit)), package frozen since ~2022 but still functional and still the best C# option for *unpackaged, non-WinAppSDK* apps. `CommunityToolkit.WinUI.Notifications` is **deprecated on NuGet** ([7.1.2](https://www.nuget.org/packages/CommunityToolkit.WinUI.Notifications/)). [WV]
- **WinAppSDK `AppNotificationManager` + `AppNotificationBuilder`** — Microsoft's recommended path for new C#/WinUI apps ([notifications overview](https://learn.microsoft.com/en-us/windows/apps/develop/notifications/)); same runtime-dependency caveats as §2.2. [WV]

---

## 6. Behavior details gmux must handle

### 6.1 Do-Not-Disturb / Focus suppression — detection

- Windows 11 22H2 renamed Focus Assist to **Do not disturb** + **Focus sessions**. When DND is on, toasts are silently diverted to Notification Center (no banner, no sound). `scenario="urgent"` toasts can break through if the user's "priority notifications" settings allow ([MS Q&A on ToastNotificationMode/DND](https://learn.microsoft.com/en-us/answers/questions/3897802/relation-between-toastnotificationmode-and-do-not)). [WV]
- **Documented detection (Win11 only):** `ToastNotificationManager.GetDefault()`… → `ToastNotificationManagerForUser.NotificationMode` returning `ToastNotificationMode { Unrestricted, PriorityOnly, AlarmsOnly }`, plus `NotificationModeChanged` event. Requires **UniversalApiContract v15 / introduced build 10.0.23504** — i.e. Windows 11 23H2+; **not available on Windows 10 21H2** ([API docs, fetched this session](https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotificationmanagerforuser.notificationmode)). [WV]
- **Undocumented detection (Win10 + older Win11):** WNF query — `NtQueryWnfStateData` on `WNF_SHEL_QUIETHOURS_ACTIVE_PROFILE_CHANGED` `{0xA3BF1C75, 0x0D83063E}`; returned DWORD: 0 = off, 1 = priority-only, 2 = alarms-only ([riverar gist](https://gist.github.com/riverar/980120d7e3a13ed8b1d665cf974c8e31)). Undocumented, "not to be used in any serious manner," but widely copied (PowerToys uses WNF for this). [WV for gist; MK for PowerToys]
- **`SHQueryUserNotificationState`** (shellapi, documented): detects fullscreen/presentation states (`QUNS_BUSY`, `QUNS_RUNNING_D3D_FULL_SCREEN`, `QUNS_PRESENTATION_MODE`, `QUNS_QUIET_TIME`) — useful signal that a banner won't be seen; does **not** reflect Win11 DND toggle. [MK]
- **gmux design:** when suppression is detected (or toast `Show` succeeds but mode != Unrestricted), escalate the *non-toast* channels (badge + flash) so the attention signal survives DND. Never mark an agent-needs-input event as "delivered" just because `Show()` returned.

### 6.2 Per-app toggle in Settings

After the first toast, "gmux" (the AUMID's `DisplayName`/`IconUri`) appears under **Settings → System → Notifications**, where users can disable it, kill sound, or demote banners. Detect via `ToastNotifier.Setting` → `NotificationSetting { Enabled, DisabledForApplication, DisabledForUser, DisabledByGroupPolicy, DisabledByManifest }`. Check before each send; if disabled, rely on in-app + taskbar channels and (optionally, once) tell the user in-app. [MK — long-stable WinRT API]

### 6.3 Grouping, replacement, collapse

- `ToastNotification.Tag` (max ~64 chars) + `.Group`: a new toast with the same tag+group **replaces** the old one in place (banner and Notification Center). gmux should use `group = workspace/session id`, `tag = pane id` so "pane 3 needs input" never stacks duplicates — re-notify by re-showing the same tag (banner reappears). [MK — core documented behavior]
- Notification Center auto-collapses an app's notifications beyond the first ~3 behind "See more"; OS also caps retained notifications per app (historically 20). Don't rely on Action Center as a queue — gmux's own UI is the queue. [MK/UNC for exact caps]
- `ToastNotificationHistory` (`ToastNotificationManager.History`) allows programmatic removal: **when the user focuses the pane, remove its toast** (`History.Remove(tag, group, aumid)`) so Notification Center never shows stale "needs input" entries. [MK]

### 6.4 Expiry & duration

- Banner display: ~5–6 s default; `duration="long"` = 25 s; `scenario="reminder"` stays on screen until dismissed (requires at least one action button). [MK]
- Notification Center retention: default and maximum **72 h / 3 days**; `ToastNotification.ExpirationTime` can only shorten it ([ExpirationTime docs](https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotification.expirationtime), [windows-dev-docs #1606](https://github.com/MicrosoftDocs/windows-dev-docs/issues/1606)). [WV]
- `ExpiresOnReboot = true` — appropriate for gmux: an "agent waiting" toast is meaningless after reboot (session-restore will re-evaluate). [MK]

### 6.5 Scenarios & urgency

- `scenario` attribute: `default | reminder | alarm | incomingCall | urgent`. `urgent` (Win11 22H2+) renders with priority and can break through DND when the user permits "important notifications"; on older builds the attribute is ignored gracefully. Candidate for an opt-in "agent blocked > N minutes" escalation — default OFF to stay polite. [WV]

### 6.6 Buttons, inputs, images, progress

- Up to **5** `<action>` elements; each carries its own `arguments` string → e.g. `Focus pane`, `Dismiss`, `Kill agent`. `activationType` per button (`foreground` default / `protocol`). [MK — schema stable]
- `<input type="text">` quick-reply works for desktop apps via COM activation (`NOTIFICATION_USER_INPUT_DATA`) — a wild but real option: reply to an agent prompt from the toast. Requires the COM activator (§3.2). [MK]
- Images: `appLogoOverride` (with `hint-crop="circle"`), inline `image`, `placement="hero"` (large top banner). Local `file:///` paths work for unpackaged apps; keep assets on disk. [MK; hero/image builder verified present in tauri-winrt-notification 0.7.3 [WV]]
- **Progress bar:** `<progress value="0.6" status="Running…" title="claude: build" />` with data-binding — update in place via `ToastNotifier.Update(NotificationData, tag, group)` with incrementing `SequenceNumber`, no re-toast. Natural sink for agent task progress (see OSC 9;4 pairing, §7.3). [MK — documented "toast progress bar" feature]
- Audio: `<audio src="ms-winsoundevent:Notification.Default" />` or `silent="true"`. Default gmux to silent; sound is the user's choice in Settings anyway. [MK]

---

## 7. Complementary attention channels (the full stack)

### 7.1 `FlashWindowEx` — taskbar flash
`FLASHWINFO { FLASHW_TRAY | FLASHW_TIMERNOFG }` flashes the taskbar button amber until the window gains focus. Zero registration, works everywhere, cannot be disabled by DND, universally understood as "this app wants you." The single highest-value/lowest-cost channel. Stop condition automatic (`FLASHW_TIMERNOFG`). [MK — decades-stable Win32]

### 7.2 `ITaskbarList3::SetOverlayIcon` — badge
COM `ITaskbarList3` (CLSID `TaskbarList`), per-HWND 16×16 overlay icon bottom-right of the taskbar button — render a count badge ("3 panes waiting") or an attention glyph; clear with `NULL`. Must wait for `TaskbarButtonCreated` window message after explorer (re)starts. This is the persistent, glanceable "how many agents need me" indicator. [MK — documented, stable since Win7]

### 7.3 `ITaskbarList3::SetProgressState/SetProgressValue` — pairs with OSC 9;4
Taskbar-button progress fill: `TBPF_NORMAL / TBPF_ERROR (red) / TBPF_PAUSED (yellow) / TBPF_INDETERMINATE`. Windows Terminal already implements exactly the pipeline gmux needs: parse ConEmu **`ESC ] 9 ; 4 ; <st> ; <pr> BEL`** (st: 0=clear, 1=normal, 2=error, 3=indeterminate, 4=warning; pr: 0–100) from the PTY stream and forward to `ITaskbarList3` ([Windows Terminal docs](https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences), [terminal PR #8055](https://github.com/microsoft/terminal/pull/8055)). Agents/build tools increasingly emit OSC 9;4 (adopted by ConEmu, Windows Terminal, Ghostty). With multiple panes, aggregate: any-error → TBPF_ERROR; else any-indeterminate → indeterminate; else mean of normal progress. [WV]

### 7.4 Tray icon
`Shell_NotifyIcon` icon for presence + right-click menu ("panes waiting" jump list); optionally swap the icon glyph when attention is pending. Do **not** use its balloons (§2.3). Note taskbar *jump lists* (`ICustomDestinationList`) can also list waiting panes. [MK]

### 7.5 Recommended combination ("you WILL notice, without being obnoxious")

| App state | Channels fired on "agent needs input" |
|---|---|
| gmux focused, pane visible | in-app pane border/indicator only (no toast — never toast the focused app) |
| gmux focused, pane in another window/tab | in-app indicator + window/tab badge |
| gmux unfocused | toast (tag=pane, group=session) + `FlashWindowEx` + overlay badge count |
| gmux minimized/other virtual desktop | same as unfocused; toast is the primary channel |
| DND / Focus session active (detected) | skip/expect-suppressed toast; badge + flash carry the signal; optional `urgent` escalation if user opted in |
| toasts disabled per-app (`NotificationSetting != Enabled`) | badge + flash + tray glyph; one-time in-app hint |
| running elevated | badge + flash only (toasts unavailable) |
| agent working (no input needed) | OSC 9;4 → taskbar progress only; clear on completion; error state → TBPF_ERROR + optional toast |

Hygiene rules: replace-don't-stack (tag/group), auto-remove toast when pane is focused (`History.Remove`), silent audio default, `ExpiresOnReboot`, never re-toast more than once per pane per N minutes, all channels clear the moment the user looks at the pane.

---

## 8. Sources (primary, fetched/searched this session)

- https://learn.microsoft.com/en-us/windows/apps/develop/notifications/app-notifications/app-notifications-quickstart (fetched; WinAppSDK activation semantics, elevated-app limitation)
- https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/self-contained-deploy/deploy-self-contained-apps (fetched; Singleton dependency, IsSupported guidance)
- https://github.com/microsoft/WindowsAppSDK/issues/6071 (fetched; self-contained unpackaged Register() failure)
- https://learn.microsoft.com/en-us/windows/windows-app-sdk/api/winrt/microsoft.windows.appnotifications.appnotificationmanager.register
- https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/release-notes/windows-app-sdk-2-0 (2.0 GA 2026-04-29)
- https://github.com/MicrosoftDocs/windows-dev-docs/blob/docs/hub/apps/design/shell/tiles-and-notifications/send-local-toast-other-apps.md (registry AUMID registration)
- https://github.com/CommunityToolkit/WindowsCommunityToolkit/pull/3457 ("no more shortcut needed")
- https://learn.microsoft.com/en-us/windows/win32/shell/enable-desktop-toast-with-appusermodelid (legacy shortcut requirement)
- https://learn.microsoft.com/en-us/windows/apps/design/shell/tiles-and-notifications/send-local-toast-desktop-cpp-wrl (COM activator flow)
- https://github.com/microsoft/microsoft-ui-xaml/issues/5499 · https://bugzilla.mozilla.org/show_bug.cgi?id=1863798 · https://learn.microsoft.com/en-us/windows/win32/api/objbase/nf-objbase-coallowsetforegroundwindow · https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setforegroundwindow (foreground rights)
- https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotificationmanagerforuser.notificationmode (fetched; Win11-only DND API, contract v15)
- https://gist.github.com/riverar/980120d7e3a13ed8b1d665cf974c8e31 (fetched; WNF quiet-hours query)
- https://learn.microsoft.com/en-us/uwp/api/windows.ui.notifications.toastnotification.expirationtime · https://github.com/MicrosoftDocs/windows-dev-docs/issues/1606 (3-day cap)
- https://crates.io/api/v1/crates/tauri-winrt-notification (0.7.3, 2026-07-02) · https://crates.io/api/v1/crates/winrt-toast-reborn (0.3.8, 2025-09-01) · https://crates.io/api/v1/crates/win-toast-notify (0.1.6, 2024-08-01)
- https://docs.rs/tauri-winrt-notification/latest/tauri_winrt_notification/struct.Toast.html (fetched; callbacks, POWERSHELL_APP_ID)
- https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Notifications/struct.INotificationActivationCallback.html (windows-rs interface)
- https://www.nuget.org/packages/CommunityToolkit.WinUI.Notifications/ (deprecated) · https://github.com/CommunityToolkit/WindowsCommunityToolkit (archived)
- https://learn.microsoft.com/en-us/windows/terminal/tutorials/progress-bar-sequences · https://github.com/microsoft/terminal/pull/8055 (OSC 9;4)
- https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-notifyicondataw · https://learn.microsoft.com/en-us/answers/questions/1362368/how-to-show-balloon-tip-message-in-windows-notific (balloon status)
- https://learn.microsoft.com/en-us/answers/questions/3897802/relation-between-toastnotificationmode-and-do-not (urgent vs DND)
