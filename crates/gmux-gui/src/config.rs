//! User configuration: `%APPDATA%\gmux\gmux.json`. Holds font size, an optional fg/bg theme, and
//! keybinding overrides. Everything is optional and forgiving — a missing or malformed file falls
//! back to [`Config::default`] (an eprintln notes the parse error) and never panics. The GUI stats
//! the file's mtime periodically and hot-reloads on change.
//!
//! Theme note: cell colors come from the daemon's grid (gmux-vt owns the palette), so `theme.bg`
//! only drives the GUI's clear/background color and `theme.fg` the sidebar text color. Full
//! palette theming is deferred (it would mean threading a theme through gmux-vt).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Everything the GUI can drive: one variant per gmux keybinding action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    SplitH,
    SplitV,
    ClosePane,
    ToggleZoom,
    NewWindow,
    NextWindow,
    PrevWindow,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ScrollPageUp,
    ScrollPageDown,
}

impl Action {
    fn from_name(s: &str) -> Option<Action> {
        Some(match s {
            "split_h" => Action::SplitH,
            "split_v" => Action::SplitV,
            "close_pane" => Action::ClosePane,
            "toggle_zoom" => Action::ToggleZoom,
            "new_window" => Action::NewWindow,
            "next_window" => Action::NextWindow,
            "prev_window" => Action::PrevWindow,
            "focus_left" => Action::FocusLeft,
            "focus_right" => Action::FocusRight,
            "focus_up" => Action::FocusUp,
            "focus_down" => Action::FocusDown,
            "scroll_page_up" => Action::ScrollPageUp,
            "scroll_page_down" => Action::ScrollPageDown,
            _ => return None,
        })
    }
}

/// The current hardcoded chords (see `app::App::try_shortcut`), as `(name, chord, action)`.
const DEFAULTS: &[(&str, &str, Action)] = &[
    ("split_h", "ctrl+shift+d", Action::SplitH),
    ("split_v", "ctrl+shift+e", Action::SplitV),
    ("close_pane", "ctrl+shift+w", Action::ClosePane),
    ("toggle_zoom", "ctrl+shift+z", Action::ToggleZoom),
    ("new_window", "ctrl+shift+t", Action::NewWindow),
    ("next_window", "ctrl+shift+n", Action::NextWindow),
    ("prev_window", "ctrl+shift+p", Action::PrevWindow),
    ("focus_left", "alt+left", Action::FocusLeft),
    ("focus_right", "alt+right", Action::FocusRight),
    ("focus_up", "alt+up", Action::FocusUp),
    ("focus_down", "alt+down", Action::FocusDown),
    ("scroll_page_up", "shift+pageup", Action::ScrollPageUp),
    ("scroll_page_down", "shift+pagedown", Action::ScrollPageDown),
];

#[derive(Debug, Default, Deserialize)]
pub struct Theme {
    pub fg: Option<String>,
    pub bg: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub font_px: Option<f32>,
    pub theme: Option<Theme>,
    /// action name -> chord string, e.g. `"split_h": "ctrl+alt+2"`.
    pub keys: Option<HashMap<String, String>>,
}

impl Config {
    /// Load from [`config_path`]. Missing file or parse error -> [`Config::default`] (never panics).
    pub fn load() -> Config {
        let path = config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return Config::default(), // no file yet is normal, stay quiet
        };
        // PowerShell's Set-Content and Notepad write a UTF-8 BOM by default; serde_json rejects
        // it, which would silently discard the whole config on the most common Windows edit path.
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        match serde_json::from_str(text) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gmux: ignoring invalid config {}: {e}", path.display());
                Config::default()
            }
        }
    }

    /// Resolve `theme.fg`/`theme.bg` hex strings into `[r, g, b]`, falling back to `default`.
    pub fn fg(&self, default: [u8; 3]) -> [u8; 3] {
        self.theme.as_ref().and_then(|t| t.fg.as_deref()).and_then(parse_hex).unwrap_or(default)
    }
    pub fn bg(&self, default: [u8; 3]) -> [u8; 3] {
        self.theme.as_ref().and_then(|t| t.bg.as_deref()).and_then(parse_hex).unwrap_or(default)
    }
}

/// `%APPDATA%\gmux\gmux.json` (ARCHITECTURE-specified location; falls back to `.` if unset).
pub fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("gmux").join("gmux.json")
}

/// `"#rrggbb"` (or `"rrggbb"`) -> `[r, g, b]`; anything else -> `None`.
fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// Parse a chord like `"ctrl+shift+d"`, `"alt+left"`, `"shift+pageup"` into `(mods, key)`.
/// Case-insensitive. Returns `None` on an unknown key token, and on a chord with **no
/// modifier** — a bare key binding would consume that key before it reaches the pane (every
/// `q` closing a pane instead of typing).
fn parse_chord(s: &str) -> Option<(ModifiersState, Key)> {
    let mut mods = ModifiersState::empty();
    let mut key: Option<Key> = None;
    for tok in s.split('+') {
        let t = tok.trim().to_ascii_lowercase();
        match t.as_str() {
            "ctrl" | "control" => mods |= ModifiersState::CONTROL,
            "shift" => mods |= ModifiersState::SHIFT,
            "alt" => mods |= ModifiersState::ALT,
            "super" | "win" | "cmd" | "meta" => mods |= ModifiersState::SUPER,
            "" => return None,
            other => key = Some(named_key(other)?),
        }
    }
    if mods.is_empty() {
        return None;
    }
    Some((mods, key?))
}

/// Map a lowercase key token to a winit [`Key`]. Single chars -> `Key::Character`; a handful of
/// named keys we actually bind. Unknown -> `None`.
fn named_key(t: &str) -> Option<Key> {
    Some(match t {
        "left" => Key::Named(NamedKey::ArrowLeft),
        "right" => Key::Named(NamedKey::ArrowRight),
        "up" => Key::Named(NamedKey::ArrowUp),
        "down" => Key::Named(NamedKey::ArrowDown),
        "pageup" => Key::Named(NamedKey::PageUp),
        "pagedown" => Key::Named(NamedKey::PageDown),
        "home" => Key::Named(NamedKey::Home),
        "end" => Key::Named(NamedKey::End),
        _ if t.chars().count() == 1 => Key::Character(t.into()),
        _ => return None,
    })
}

/// Resolved keybindings: `(mods, key)` -> [`Action`], built from [`DEFAULTS`] plus overrides.
pub struct Keymap {
    map: HashMap<(ModifiersState, ChordKey), Action>,
}

/// A hashable wrapper for the parts of [`Key`] we care about (winit's `Key<SmolStr>` isn't `Hash`
/// for our purposes uniformly — `Character` holds a string, `Named` an enum).
#[derive(Clone, PartialEq, Eq, Hash)]
enum ChordKey {
    Char(char),
    Named(NamedKey),
}

impl ChordKey {
    fn from_key(k: &Key) -> Option<ChordKey> {
        match k {
            Key::Character(s) => s.chars().next().map(|c| ChordKey::Char(c.to_ascii_lowercase())),
            Key::Named(n) => Some(ChordKey::Named(*n)),
            _ => None,
        }
    }
}

impl Keymap {
    /// Build from the compiled-in defaults, then apply any `keys` overrides from `config`.
    /// Bad action names, bad chords, or unknown keys are skipped with an eprintln warning; the
    /// corresponding default binding is left in place.
    pub fn build(config: &Config) -> Keymap {
        let mut map = HashMap::new();
        for (_, chord, action) in DEFAULTS {
            if let Some((mods, key)) = parse_chord(chord) {
                if let Some(ck) = ChordKey::from_key(&key) {
                    map.insert((mods, ck), *action);
                }
            }
        }
        if let Some(overrides) = &config.keys {
            // Sorted by action name so two overrides claiming the same chord resolve the same
            // way every launch (HashMap iteration order would make the winner random).
            let mut entries: Vec<(&String, &String)> = overrides.iter().collect();
            entries.sort();
            for (name, chord) in entries {
                let Some(action) = Action::from_name(name) else {
                    eprintln!("gmux: unknown action '{name}' in config keys; ignoring");
                    continue;
                };
                let Some((mods, key)) = parse_chord(chord) else {
                    eprintln!("gmux: bad chord '{chord}' for action '{name}'; keeping default");
                    continue;
                };
                let Some(ck) = ChordKey::from_key(&key) else {
                    eprintln!("gmux: unusable key in chord '{chord}'; keeping default");
                    continue;
                };
                // Drop any default that maps this same action elsewhere, so a rebind *moves* it
                // rather than leaving the old chord live alongside the new one.
                map.retain(|_, a| *a != action);
                if let Some(shadowed) = map.insert((mods, ck), action) {
                    eprintln!(
                        "gmux: chord '{chord}' now runs '{name}', unbinding {shadowed:?} — \
                         rebind it to keep it"
                    );
                }
            }
        }
        Keymap { map }
    }

    /// Look up the action bound to `(mods, key)`, if any.
    pub fn action(&self, mods: ModifiersState, key: &Key) -> Option<Action> {
        let ck = ChordKey::from_key(key)?;
        self.map.get(&(mods, ck)).copied()
    }
}

impl Default for Keymap {
    fn default() -> Keymap {
        Keymap::build(&Config::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_good_chords() {
        assert_eq!(
            parse_chord("ctrl+shift+d"),
            Some((ModifiersState::CONTROL | ModifiersState::SHIFT, Key::Character("d".into())))
        );
        assert_eq!(parse_chord("alt+left"), Some((ModifiersState::ALT, Key::Named(NamedKey::ArrowLeft))));
        assert_eq!(parse_chord("shift+pageup"), Some((ModifiersState::SHIFT, Key::Named(NamedKey::PageUp))));
        // case-insensitive
        assert_eq!(parse_chord("CTRL+SHIFT+D"), parse_chord("ctrl+shift+d"));
    }

    #[test]
    fn rejects_bad_chords() {
        assert_eq!(parse_chord("ctrl+shift+"), None); // trailing empty token
        assert_eq!(parse_chord("ctrl+boguskey"), None); // unknown named key
        assert_eq!(parse_chord("ctrl+shift"), None); // mods only, no key
    }

    #[test]
    fn default_map_is_complete() {
        let km = Keymap::default();
        // Every default action must resolve.
        for (_, chord, action) in DEFAULTS {
            let (mods, key) = parse_chord(chord).expect("default chord parses");
            assert_eq!(km.action(mods, &key), Some(*action), "missing default for {chord:?}");
        }
        assert_eq!(km.map.len(), DEFAULTS.len());
    }

    #[test]
    fn override_replaces_default() {
        let mut keys = HashMap::new();
        keys.insert("split_h".to_string(), "ctrl+alt+2".to_string());
        let cfg = Config { keys: Some(keys), ..Default::default() };
        let km = Keymap::build(&cfg);

        // New chord fires SplitH.
        let (m, k) = parse_chord("ctrl+alt+2").unwrap();
        assert_eq!(km.action(m, &k), Some(Action::SplitH));
        // Old default chord no longer maps SplitH (the rebind moved it).
        let (m0, k0) = parse_chord("ctrl+shift+d").unwrap();
        assert_ne!(km.action(m0, &k0), Some(Action::SplitH));
        // Unrelated defaults are untouched.
        let (m1, k1) = parse_chord("ctrl+shift+w").unwrap();
        assert_eq!(km.action(m1, &k1), Some(Action::ClosePane));
    }

    #[test]
    fn bad_override_keeps_default_and_warns() {
        let mut keys = HashMap::new();
        keys.insert("split_h".to_string(), "ctrl+bogus".to_string()); // unparseable
        keys.insert("not_an_action".to_string(), "ctrl+q".to_string()); // unknown action
        let cfg = Config { keys: Some(keys), ..Default::default() };
        let km = Keymap::build(&cfg);
        // The bad split_h override is dropped, default stays.
        let (m, k) = parse_chord("ctrl+shift+d").unwrap();
        assert_eq!(km.action(m, &k), Some(Action::SplitH));
        assert_eq!(km.map.len(), DEFAULTS.len());
    }

    #[test]
    fn invalid_json_falls_back_to_default() {
        let cfg: Config = match serde_json::from_str("{ not valid json") {
            Ok(c) => c,
            Err(_) => Config::default(),
        };
        assert!(cfg.font_px.is_none() && cfg.theme.is_none() && cfg.keys.is_none());
    }

    #[test]
    fn parses_theme_hex() {
        assert_eq!(parse_hex("#ff8800"), Some([0xff, 0x88, 0x00]));
        assert_eq!(parse_hex("00ff00"), Some([0x00, 0xff, 0x00]));
        assert_eq!(parse_hex("#fff"), None); // too short
        assert_eq!(parse_hex("#gggggg"), None); // non-hex
        let cfg = Config {
            theme: Some(Theme { fg: Some("#abcdef".into()), bg: None }),
            ..Default::default()
        };
        assert_eq!(cfg.fg([0, 0, 0]), [0xab, 0xcd, 0xef]);
        assert_eq!(cfg.bg([9, 9, 9]), [9, 9, 9]); // no bg -> default
    }

    // -- adversarial-review regressions (511967d review) --

    /// PowerShell/Notepad write a UTF-8 BOM; the config must still parse (it was silently
    /// discarded before).
    #[test]
    fn bom_prefixed_json_still_parses() {
        let json = "\u{feff}{\"font_px\": 20.0}";
        let stripped = json.strip_prefix('\u{feff}').unwrap_or(json);
        let cfg: Config = serde_json::from_str(stripped).unwrap();
        assert_eq!(cfg.font_px, Some(20.0));
    }

    /// A chord with no modifier would eat normal typing (every `q` closing a pane); it must be
    /// rejected at parse time.
    #[test]
    fn bare_key_chords_are_rejected() {
        assert!(parse_chord("q").is_none());
        assert!(parse_chord("pageup").is_none());
        assert!(parse_chord("ctrl+q").is_some());
        assert!(parse_chord("shift+pageup").is_some());
    }

    /// Two overrides claiming the same chord must resolve deterministically (sorted by action
    /// name) instead of varying with HashMap iteration order.
    #[test]
    fn colliding_overrides_resolve_deterministically() {
        let mut keys = std::collections::HashMap::new();
        keys.insert("split_h".to_string(), "ctrl+alt+9".to_string());
        keys.insert("split_v".to_string(), "ctrl+alt+9".to_string());
        let cfg = Config { keys: Some(keys), ..Default::default() };
        let expected = Keymap::build(&cfg)
            .action(ModifiersState::CONTROL | ModifiersState::ALT, &Key::Character("9".into()));
        for _ in 0..20 {
            let got = Keymap::build(&cfg).action(
                ModifiersState::CONTROL | ModifiersState::ALT,
                &Key::Character("9".into()),
            );
            assert_eq!(got, expected, "winner must not vary between builds");
        }
    }
}
