//! `gmux hooks setup <agent>` — configure AI coding agents so their notifications reach gmux.
//!
//! Each agent's default notification behaviour does little-to-nothing in an unrecognized terminal
//! (ARCHITECTURE D-011), so gmux configures them to emit OSC that gmux parses into toasts:
//! - **codex** — `~/.codex/config.toml`: `tui.notification_method = "osc9"`.
//! - **gemini** — `~/.gemini/settings.json`: `general.notificationMethod = "osc777"`.
//! - **claude-code** — `~/.claude/settings.json`: a `Notification` hook running `gmux _hook
//!   claude-code`, which emits an OSC 777 (see `internal_hook` in main).
//! - **aider** — `~/.aider.conf.yml`: `notifications-command: gmux notify ...`.
//!
//! All merges preserve existing content. The functions take a `home` dir so they're unit-testable
//! against a temp directory.

use std::fs;
use std::io;
use std::path::Path;

/// Run setup for `agent` ("codex" | "gemini" | "claude-code" | "aider" | "all") under `home`.
/// Returns human-readable descriptions of what was changed.
pub fn setup(agent: &str, home: &Path) -> io::Result<Vec<String>> {
    match agent {
        "codex" => Ok(vec![setup_codex(home)?]),
        "gemini" => Ok(vec![setup_gemini(home)?]),
        "claude-code" | "claude" => Ok(vec![setup_claude(home)?]),
        "aider" => Ok(vec![setup_aider(home)?]),
        "all" => Ok(vec![
            setup_claude(home)?,
            setup_codex(home)?,
            setup_gemini(home)?,
            setup_aider(home)?,
        ]),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown agent '{other}' (expected: claude-code, codex, gemini, aider, all)"),
        )),
    }
}

fn setup_codex(home: &Path) -> io::Result<String> {
    let path = home.join(".codex").join("config.toml");
    let mut doc = read_or_default(&path)?
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    doc["tui"]["notification_method"] = toml_edit::value("osc9");
    write_atomic(&path, doc.to_string().as_bytes())?;
    Ok(format!("codex: set tui.notification_method=\"osc9\" in {}", path.display()))
}

fn setup_gemini(home: &Path) -> io::Result<String> {
    let path = home.join(".gemini").join("settings.json");
    let mut root = read_json_object(&path)?;
    let general = root
        .entry("general")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(obj) = general.as_object_mut() {
        obj.insert("enableNotifications".into(), serde_json::json!(true));
        obj.insert("notificationMethod".into(), serde_json::json!("osc777"));
    }
    write_atomic(&path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(format!("gemini: set general.notificationMethod=\"osc777\" in {}", path.display()))
}

fn setup_claude(home: &Path) -> io::Result<String> {
    let path = home.join(".claude").join("settings.json");
    let mut root = read_json_object(&path)?;
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| bad("hooks is not an object"))?;
    let cmd = "gmux _hook claude-code";
    let entry = serde_json::json!({ "hooks": [ { "type": "command", "command": cmd } ] });
    let list = hooks.entry("Notification").or_insert_with(|| serde_json::json!([]));
    let arr = list.as_array_mut().ok_or_else(|| bad("hooks.Notification is not an array"))?;
    // Idempotent: only add if our command isn't already present.
    let present = arr.iter().any(|e| e.to_string().contains(cmd));
    if !present {
        arr.push(entry);
    }
    write_atomic(&path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(format!(
        "claude-code: {} Notification hook -> `{cmd}` in {}",
        if present { "kept existing" } else { "added" },
        path.display()
    ))
}

fn setup_aider(home: &Path) -> io::Result<String> {
    // Aider config is YAML; keep it dependency-free by appending our keys if absent.
    let path = home.join(".aider.conf.yml");
    let existing = read_or_default(&path)?;
    let mut out = existing.clone();
    if !existing.contains("notifications-command:") {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("notifications: true\n");
        out.push_str("notifications-command: gmux notify --title aider --body \"needs input\"\n");
        write_atomic(&path, out.as_bytes())?;
        Ok(format!("aider: added notifications-command in {}", path.display()))
    } else {
        Ok(format!("aider: notifications-command already present in {}", path.display()))
    }
}

// --- helpers ---

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn read_or_default(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn read_json_object(path: &Path) -> io::Result<serde_json::Map<String, serde_json::Value>> {
    let text = read_or_default(path)?;
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let value: serde_json::Value = serde_json::from_str(&text)?;
    match value {
        serde_json::Value::Object(m) => Ok(m),
        _ => Err(bad("expected a JSON object at the top level")),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("gmux.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("gmux-hooks-test-{}", std::process::id()));
        let unique = base.join(format!("{:?}", std::time::SystemTime::now()).replace([' ', ':'], "_"));
        fs::create_dir_all(&unique).unwrap();
        unique
    }

    #[test]
    fn codex_sets_osc9_and_preserves_existing() {
        let home = tmp_home();
        let cfg = home.join(".codex").join("config.toml");
        fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        fs::write(&cfg, "model = \"o1\"\n[tui]\ntheme = \"dark\"\n").unwrap();
        setup("codex", &home).unwrap();
        let out = fs::read_to_string(&cfg).unwrap();
        assert!(out.contains("notification_method = \"osc9\""), "{out}");
        assert!(out.contains("model = \"o1\""), "must preserve existing keys: {out}");
        assert!(out.contains("theme = \"dark\""), "{out}");
    }

    #[test]
    fn gemini_sets_method_in_fresh_file() {
        let home = tmp_home();
        setup("gemini", &home).unwrap();
        let out = fs::read_to_string(home.join(".gemini").join("settings.json")).unwrap();
        assert!(out.contains("\"notificationMethod\": \"osc777\""), "{out}");
    }

    #[test]
    fn claude_adds_notification_hook_idempotently() {
        let home = tmp_home();
        let first = setup("claude-code", &home).unwrap();
        assert!(first[0].contains("added"), "{first:?}");
        let path = home.join(".claude").join("settings.json");
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("gmux _hook claude-code"), "{out}");
        // second run must not duplicate
        let second = setup("claude-code", &home).unwrap();
        assert!(second[0].contains("kept existing"), "{second:?}");
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v["hooks"]["Notification"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "hook must not be duplicated");
    }

    #[test]
    fn claude_preserves_unrelated_settings() {
        let home = tmp_home();
        let path = home.join(".claude").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"model\":\"opus\",\"theme\":\"dark\"}").unwrap();
        setup("claude-code", &home).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["theme"], "dark");
        assert!(v["hooks"]["Notification"].is_array());
    }

    #[test]
    fn unknown_agent_errors() {
        assert!(setup("nope", &tmp_home()).is_err());
    }
}
