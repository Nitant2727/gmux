//! Local crash reports: a panic hook that appends a timestamped report (message, location,
//! backtrace) to `%LOCALAPPDATA%/gmux/crash/<component>-<pid>.txt`. Reports never leave the
//! machine; the previous hook still runs so the default stderr output is preserved.

use std::backtrace::Backtrace;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Install the crash-report panic hook for this process. `component` names the role
/// ("daemon" | "gui") in the report file. The hook itself must never panic, so all
/// io errors are ignored.
pub fn install(component: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = crash_dir() {
            let message = panic_message(info);
            let location = info.location().map(|l| l.to_string()).unwrap_or_default();
            let _ = write_report(&dir, component, &message, &location);
        }
        previous(info);
    }));
}

/// Append one report to `<dir>/<component>-<pid>.txt`, creating dirs as needed.
fn write_report(dir: &Path, component: &str, message: &str, location: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{component}-{}.txt", std::process::id()));
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writeln!(f, "--- gmux {} {component} panic (unix {ts}) ---", env!("CARGO_PKG_VERSION"))?;
    writeln!(f, "message:  {message}")?;
    writeln!(f, "location: {location}")?;
    writeln!(f, "{}", Backtrace::force_capture())
}

fn crash_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("gmux").join("crash"))
}

/// The panic payload as text (it's a `&str` or `String` for all `panic!`-family macros).
fn panic_message(info: &std::panic::PanicHookInfo) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("gmux-crash-test-{}", std::process::id()));
        let unique = base.join(format!("{:?}", std::time::SystemTime::now()).replace([' ', ':'], "_"));
        fs::create_dir_all(&unique).unwrap();
        unique
    }

    #[test]
    fn write_report_appends_message_location_and_backtrace() {
        let dir = tmp_dir();
        write_report(&dir, "test", "boom", "src/main.rs:1:1").unwrap();
        let path = dir.join(format!("test-{}.txt", std::process::id()));
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("message:  boom"), "{out}");
        assert!(out.contains("location: src/main.rs:1:1"), "{out}");
        assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
        // a second panic in the same process appends rather than truncates
        write_report(&dir, "test", "boom again", "loc").unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("boom") && out.contains("boom again"), "{out}");
    }
}
