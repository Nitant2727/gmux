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
    Paste,
    OpenSettings,
    Search,
    ZoomIn,
    ZoomOut,
    ZoomReset,
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
            "paste" => Action::Paste,
            "open_settings" => Action::OpenSettings,
            "search" => Action::Search,
            "zoom_in" => Action::ZoomIn,
            "zoom_out" => Action::ZoomOut,
            "zoom_reset" => Action::ZoomReset,
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
    ("paste", "ctrl+shift+v", Action::Paste),
    ("open_settings", "ctrl+,", Action::OpenSettings),
    ("search", "ctrl+shift+f", Action::Search),
    ("zoom_in", "ctrl+=", Action::ZoomIn),
    ("zoom_out", "ctrl+-", Action::ZoomOut),
    ("zoom_reset", "ctrl+0", Action::ZoomReset),
];

#[derive(Debug, Default, Deserialize)]
pub struct Theme {
    pub fg: Option<String>,
    pub bg: Option<String>,
    /// Path to a Windows Terminal color-scheme JSON fragment (`{ "background":"#0c0c0c",
    /// "foreground":..., "black":..., ... "brightWhite":... }`). Its 18 well-known keys map to the
    /// full palette; missing keys keep the built-in defaults. A relative path resolves against
    /// the config directory (`%APPDATA%\gmux\`), not the process cwd.
    pub scheme: Option<PathBuf>,
    /// Non-WT inline path: 16 `"#rrggbb"` strings for the ANSI 0..=15 system colors. Applied over
    /// the defaults (and over `scheme`, if both are given); entries past 16 or bad hex are ignored.
    pub ansi: Option<Vec<String>>,
}

/// A resolved palette for the wire (`Call::SetPalette`): default fg/bg + the 16 system colors,
/// each `[r, g, b]`. Mirrors `gmux_vt::Palette` without depending on it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub ansi: [[u8; 3]; 16],
}

/// The gmux defaults, byte-identical to `gmux_vt::Palette::default()`.
const DEFAULT_PALETTE: Palette = Palette {
    fg: [0xcc, 0xcc, 0xcc],
    bg: [0x11, 0x11, 0x11],
    ansi: [
        [0x00, 0x00, 0x00], [0x80, 0x00, 0x00], [0x00, 0x80, 0x00], [0x80, 0x80, 0x00],
        [0x00, 0x00, 0x80], [0x80, 0x00, 0x80], [0x00, 0x80, 0x80], [0xc0, 0xc0, 0xc0],
        [0x80, 0x80, 0x80], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
        [0x00, 0x00, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
    ],
};

/// The 16 ANSI slots keyed by their Windows Terminal scheme names (index = ANSI color number).
const WT_ANSI_KEYS: [&str; 16] = [
    "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
    "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightPurple",
    "brightCyan", "brightWhite",
];

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

    /// Resolve the full terminal palette from `theme`, layering (in order) over the gmux defaults:
    /// (1) a Windows Terminal `scheme` file's 18 well-known keys, (2) an inline `ansi` array of 16
    /// hex strings, (3) the standalone `fg`/`bg` hex overrides. Missing keys / bad hex keep the
    /// underlying value; a `scheme` path that can't be read logs and is skipped (defaults stand).
    /// Returns [`DEFAULT_PALETTE`] when no theme customizes any color, so callers always have a
    /// concrete palette to send.
    pub fn palette(&self) -> Palette {
        let mut p = DEFAULT_PALETTE;
        let Some(theme) = self.theme.as_ref() else { return p };
        if let Some(path) = &theme.scheme {
            if let Some(map) = load_scheme(path) {
                if let Some(c) = map.get("foreground").and_then(|s| parse_hex(s)) {
                    p.fg = c;
                }
                if let Some(c) = map.get("background").and_then(|s| parse_hex(s)) {
                    p.bg = c;
                }
                for (i, key) in WT_ANSI_KEYS.iter().enumerate() {
                    if let Some(c) = map.get(*key).and_then(|s| parse_hex(s)) {
                        p.ansi[i] = c;
                    }
                }
            }
        }
        if let Some(inline) = &theme.ansi {
            for (slot, s) in p.ansi.iter_mut().zip(inline) {
                if let Some(c) = parse_hex(s) {
                    *slot = c;
                }
            }
        }
        // The standalone fg/bg keys win over a scheme's foreground/background.
        p.fg = theme.fg.as_deref().and_then(parse_hex).unwrap_or(p.fg);
        p.bg = theme.bg.as_deref().and_then(parse_hex).unwrap_or(p.bg);
        p
    }
}

/// Read a Windows Terminal color-scheme JSON fragment into a `key -> hex` map. BOM-stripped like
/// [`Config::load`]. A missing / unreadable / non-object file logs and yields `None` (defaults
/// stand). We deserialize to `HashMap<String, serde_json::Value>` and keep only the string values,
/// so a full `settings.json` fragment with nested objects doesn't blow up.
fn load_scheme(path: &std::path::Path) -> Option<HashMap<String, String>> {
    // A relative path resolves against the config dir (%APPDATA%\gmux\), NOT the process cwd —
    // Explorer launches have an unpredictable cwd, which would make the theme silently vanish.
    let resolved = if path.is_relative() {
        config_path().parent().map(|d| d.join(path)).unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let path = &resolved;
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gmux: cannot read theme scheme {}: {e}", path.display());
            return None;
        }
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("gmux: ignoring invalid theme scheme {}: {e}", path.display());
            return None;
        }
    };
    Some(
        raw.into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect(),
    )
}

/// `%APPDATA%\gmux\gmux.json` (ARCHITECTURE-specified location; falls back to `.` if unset).
pub fn config_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("gmux").join("gmux.json")
}

/// A fully-populated `gmux.json`, written on the first "open settings" so the file itself documents
/// the schema — JSON has no comments, so every setting appears with its default value. The `keys`
/// block is generated from [`DEFAULTS`], so it always lists every action + its default chord and
/// can't drift. `persist_screen` is read by the daemon (not this struct); the rest map to [`Config`].
/// Parses back cleanly via [`Config::load`] (extra keys like `persist_screen` are ignored here).
pub fn default_template() -> String {
    let keys = DEFAULTS
        .iter()
        .map(|(name, chord, _)| format!("    \"{name}\": \"{chord}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{
  \"font_px\": {font},
  \"persist_screen\": true,
  \"theme\": {{
    \"fg\": \"#{fg:02x}{fg1:02x}{fg2:02x}\",
    \"bg\": \"#{bg:02x}{bg1:02x}{bg2:02x}\",
    \"scheme\": null,
    \"ansi\": null
  }},
  \"keys\": {{
{keys}
  }}
}}
"
        ,
        font = 18.0f32, // ponytail: mirror of app::DEFAULT_FONT_PX (config cannot import app)
        fg = DEFAULT_PALETTE.fg[0], fg1 = DEFAULT_PALETTE.fg[1], fg2 = DEFAULT_PALETTE.fg[2],
        bg = DEFAULT_PALETTE.bg[0], bg1 = DEFAULT_PALETTE.bg[1], bg2 = DEFAULT_PALETTE.bg[2],
    )
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
    fn open_settings_chord_and_name() {
        // `ctrl+,` parses (the ',' single-char token becomes a Key::Character) and binds OpenSettings.
        assert_eq!(
            parse_chord("ctrl+,"),
            Some((ModifiersState::CONTROL, Key::Character(",".into())))
        );
        let km = Keymap::default();
        let (m, k) = parse_chord("ctrl+,").unwrap();
        assert_eq!(km.action(m, &k), Some(Action::OpenSettings));
        assert_eq!(Action::from_name("open_settings"), Some(Action::OpenSettings));
    }

    #[test]
    fn template_parses_and_covers_every_action() {
        let t = default_template();
        // Round-trips through serde as a Config (the on-disk load path).
        let cfg: Config = serde_json::from_str(&t).expect("template is valid config JSON");
        assert_eq!(cfg.font_px, Some(18.0));
        assert!(cfg.theme.is_some(), "template carries a theme block");
        let keys = cfg.keys.as_ref().expect("template has a keys block");
        // Every action name appears with its default chord.
        for (name, chord, _) in DEFAULTS {
            assert_eq!(keys.get(*name).map(String::as_str), Some(*chord), "template missing {name}");
        }
        assert_eq!(keys.len(), DEFAULTS.len(), "template lists exactly the default actions");
        // The generated keys parse back into the same Keymap the defaults build.
        let km = Keymap::build(&cfg);
        for (_, chord, action) in DEFAULTS {
            let (m, k) = parse_chord(chord).unwrap();
            assert_eq!(km.action(m, &k), Some(*action));
        }
    }

    #[test]
    fn paste_action_default_chord_and_name() {
        // Ctrl+Shift+V resolves to Paste out of the box.
        let km = Keymap::default();
        let (m, k) = parse_chord("ctrl+shift+v").unwrap();
        assert_eq!(km.action(m, &k), Some(Action::Paste));
        // The action name maps both ways (config `keys` uses these names).
        assert_eq!(Action::from_name("paste"), Some(Action::Paste));
    }

    #[test]
    fn zoom_chords_parse_and_bind() {
        // The three zoom chords use single-char tokens (`=`, `-`, `0`) that go through named_key's
        // Key::Character fallback — no chord-parser change needed.
        assert_eq!(
            parse_chord("ctrl+="),
            Some((ModifiersState::CONTROL, Key::Character("=".into())))
        );
        assert_eq!(
            parse_chord("ctrl+-"),
            Some((ModifiersState::CONTROL, Key::Character("-".into())))
        );
        assert_eq!(
            parse_chord("ctrl+0"),
            Some((ModifiersState::CONTROL, Key::Character("0".into())))
        );
        let km = Keymap::default();
        for (chord, action) in [
            ("ctrl+=", Action::ZoomIn),
            ("ctrl+-", Action::ZoomOut),
            ("ctrl+0", Action::ZoomReset),
        ] {
            let (m, k) = parse_chord(chord).unwrap();
            assert_eq!(km.action(m, &k), Some(action), "{chord} binds {action:?}");
        }
        // The action names round-trip (config `keys` uses these).
        assert_eq!(Action::from_name("zoom_in"), Some(Action::ZoomIn));
        assert_eq!(Action::from_name("zoom_out"), Some(Action::ZoomOut));
        assert_eq!(Action::from_name("zoom_reset"), Some(Action::ZoomReset));
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
            theme: Some(Theme { fg: Some("#abcdef".into()), bg: None, ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(cfg.fg([0, 0, 0]), [0xab, 0xcd, 0xef]);
        assert_eq!(cfg.bg([9, 9, 9]), [9, 9, 9]); // no bg -> default
    }

    // -- palette / Windows Terminal scheme import (M10) --

    /// A unique temp path for a scheme fixture (scratch under the OS temp dir).
    fn temp_scheme_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gmux-scheme-{}-{}.json", std::process::id(), tag))
    }

    #[test]
    fn no_theme_yields_default_palette() {
        assert_eq!(Config::default().palette(), DEFAULT_PALETTE);
    }

    #[test]
    fn wt_scheme_fragment_maps_keys_and_defaults_missing() {
        // A partial Campbell-ish fragment: fg/bg + a couple ANSI slots, plus a nested key we must
        // ignore rather than choke on. `red`/`brightPurple` exercise the WT->ANSI name mapping.
        let path = temp_scheme_path("wt");
        std::fs::write(
            &path,
            r##"{
                "name": "Test",
                "background": "#0c0c0c",
                "foreground": "#cccccc",
                "red": "#c50f1f",
                "brightPurple": "#b4009e",
                "nested": { "ignored": true }
            }"##,
        )
        .unwrap();
        let cfg = Config {
            theme: Some(Theme { scheme: Some(path.clone()), ..Default::default() }),
            ..Default::default()
        };
        let p = cfg.palette();
        assert_eq!(p.bg, [0x0c, 0x0c, 0x0c]);
        assert_eq!(p.fg, [0xcc, 0xcc, 0xcc]);
        assert_eq!(p.ansi[1], [0xc5, 0x0f, 0x1f]); // red (ANSI 1)
        assert_eq!(p.ansi[13], [0xb4, 0x00, 0x9e]); // brightPurple -> ANSI 13 (bright magenta)
        // Keys absent from the fragment keep the built-in defaults.
        assert_eq!(p.ansi[0], DEFAULT_PALETTE.ansi[0]); // black untouched
        assert_eq!(p.ansi[2], DEFAULT_PALETTE.ansi[2]); // green untouched
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn garbage_scheme_file_falls_back_to_defaults() {
        let path = temp_scheme_path("garbage");
        std::fs::write(&path, "{ this is not json").unwrap();
        let cfg = Config {
            theme: Some(Theme { scheme: Some(path.clone()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(cfg.palette(), DEFAULT_PALETTE, "bad scheme -> defaults, no panic");
        let _ = std::fs::remove_file(&path);

        // A missing file likewise falls back.
        let cfg = Config {
            theme: Some(Theme {
                scheme: Some(temp_scheme_path("does-not-exist")),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(cfg.palette(), DEFAULT_PALETTE);
    }

    #[test]
    fn inline_ansi_overrides_scheme_and_fg_bg_win() {
        // Inline `ansi` applies over defaults; standalone fg/bg override the scheme's fg/bg.
        let cfg = Config {
            theme: Some(Theme {
                fg: Some("#010203".into()),
                bg: Some("#040506".into()),
                ansi: Some(vec!["#111111".into(), "#deadbe".into()]), // slots 0,1
                scheme: None,
            }),
            ..Default::default()
        };
        let p = cfg.palette();
        assert_eq!(p.fg, [0x01, 0x02, 0x03]);
        assert_eq!(p.bg, [0x04, 0x05, 0x06]);
        assert_eq!(p.ansi[0], [0x11, 0x11, 0x11]);
        assert_eq!(p.ansi[1], [0xde, 0xad, 0xbe]);
        assert_eq!(p.ansi[2], DEFAULT_PALETTE.ansi[2]); // slot past the inline list keeps default
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
