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
    /// Select the Nth sidebar tab (1-based `n`, from alt+1..alt+9); dispatched as a 0-based
    /// `SelectWindow { index: n - 1 }`.
    SelectTab(u8),
    /// Jump the active pane's viewport to the previous / next command prompt (OSC 133 marks from
    /// the shell-integration snippet).
    PrevPrompt,
    NextPrompt,
    /// Open the command palette (fuzzy action + tab switcher overlay).
    CommandPalette,
    /// Write the active pane's full scrollback to a timestamped file in Downloads.
    ExportScrollback,
    /// Enter keyboard copy mode (move with arrows/hjkl, mark with v, copy with y/Enter).
    CopyMode,
    /// Show/hide the embedded browser panel (needs a `--features browser` build).
    ToggleBrowser,
    /// Pick a directory and open it as a new workspace (every pane in it starts there).
    OpenWorkspace,
    /// Pick a directory holding several projects and open a workspace for each one inside it.
    ImportWorkspaces,
    /// Filter the sidebar by typing (Esc clears, Enter selects the first match).
    FilterWorkspaces,
    /// Rename the active workspace inline (same editor a sidebar double-click opens).
    RenameWorkspace,
    /// Close the active workspace (asks first when it has running children).
    CloseWorkspace,
    /// Nudge the active pane's split divider by a small fraction (keyboard resize).
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
}

impl Action {
    fn from_name(s: &str) -> Option<Action> {
        // "select_tab_1".."select_tab_9" -> SelectTab(1..=9) (a 1-based tab number).
        if let Some(n) = s.strip_prefix("select_tab_") {
            let n: u8 = n.parse().ok()?;
            return (1..=9).contains(&n).then_some(Action::SelectTab(n));
        }
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
            "prev_prompt" => Action::PrevPrompt,
            "next_prompt" => Action::NextPrompt,
            "command_palette" => Action::CommandPalette,
            "export_scrollback" => Action::ExportScrollback,
            "copy_mode" => Action::CopyMode,
            "toggle_browser" => Action::ToggleBrowser,
            "open_workspace" => Action::OpenWorkspace,
            "import_workspaces" => Action::ImportWorkspaces,
            "filter_workspaces" => Action::FilterWorkspaces,
            "rename_workspace" => Action::RenameWorkspace,
            "close_workspace" => Action::CloseWorkspace,
            "resize_left" => Action::ResizeLeft,
            "resize_right" => Action::ResizeRight,
            "resize_up" => Action::ResizeUp,
            "resize_down" => Action::ResizeDown,
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
    // Browser-style tab cycling: ctrl+pageup/pagedown (freed ctrl+shift+n/p — N opens a new
    // window in most apps' muscle memory, and P is the command palette's industry-wide chord).
    ("next_window", "ctrl+pagedown", Action::NextWindow),
    ("prev_window", "ctrl+pageup", Action::PrevWindow),
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
    ("select_tab_1", "alt+1", Action::SelectTab(1)),
    ("select_tab_2", "alt+2", Action::SelectTab(2)),
    ("select_tab_3", "alt+3", Action::SelectTab(3)),
    ("select_tab_4", "alt+4", Action::SelectTab(4)),
    ("select_tab_5", "alt+5", Action::SelectTab(5)),
    ("select_tab_6", "alt+6", Action::SelectTab(6)),
    ("select_tab_7", "alt+7", Action::SelectTab(7)),
    ("select_tab_8", "alt+8", Action::SelectTab(8)),
    ("select_tab_9", "alt+9", Action::SelectTab(9)),
    ("prev_prompt", "ctrl+up", Action::PrevPrompt),
    ("next_prompt", "ctrl+down", Action::NextPrompt),
    ("command_palette", "ctrl+shift+p", Action::CommandPalette),
    ("export_scrollback", "ctrl+shift+s", Action::ExportScrollback),
    ("copy_mode", "ctrl+shift+m", Action::CopyMode),
    ("toggle_browser", "ctrl+shift+b", Action::ToggleBrowser),
    ("open_workspace", "ctrl+shift+o", Action::OpenWorkspace),
    ("import_workspaces", "ctrl+shift+i", Action::ImportWorkspaces),
    ("filter_workspaces", "ctrl+shift+k", Action::FilterWorkspaces),
    // Not F2 (the usual rename key): a bare key would be swallowed before reaching the pane, and
    // TUIs like htop/mc bind the F-keys. A chord keeps the pane's keyboard intact.
    ("rename_workspace", "ctrl+shift+r", Action::RenameWorkspace),
    ("close_workspace", "ctrl+shift+q", Action::CloseWorkspace),
    ("resize_left", "alt+shift+left", Action::ResizeLeft),
    ("resize_right", "alt+shift+right", Action::ResizeRight),
    ("resize_up", "alt+shift+up", Action::ResizeUp),
    ("resize_down", "alt+shift+down", Action::ResizeDown),
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
    /// A built-in colour scheme by name (see [`PRESETS`]) — the coarsest layer, so `scheme`,
    /// `ansi`, and `fg`/`bg` still refine it. Unset or unknown = the gmux defaults.
    pub preset: Option<String>,
    /// Non-WT inline path: 16 `"#rrggbb"` strings for the ANSI 0..=15 system colors. Applied over
    /// the defaults (and over `scheme`, if both are given); entries past 16 or bad hex are ignored.
    pub ansi: Option<Vec<String>>,
    /// Chrome accent (selected tab fill, active pane border, focus glow, highlights). Unset = the
    /// built-in cmux blue; `"system"` follows the user's Windows accent color; any `#rrggbb` pins
    /// that color.
    pub accent: Option<String>,
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

/// The built-in colour schemes, each the scheme's own published palette. Selected by name through
/// `theme.preset` and cycled by the settings panel; the order here is the order the panel cycles.
/// "default" is deliberately absent — it means *no* preset, so [`DEFAULT_PALETTE`] stands.
pub const PRESETS: &[(&str, Palette)] = &[
    ("campbell", Palette {
        fg: [0xcc, 0xcc, 0xcc], bg: [0x0c, 0x0c, 0x0c],
        ansi: [
            [0x0c, 0x0c, 0x0c], [0xc5, 0x0f, 0x1f], [0x13, 0xa1, 0x0e], [0xc1, 0x9c, 0x00],
            [0x00, 0x37, 0xda], [0x88, 0x17, 0x98], [0x3a, 0x96, 0xdd], [0xcc, 0xcc, 0xcc],
            [0x76, 0x76, 0x76], [0xe7, 0x48, 0x56], [0x16, 0xc6, 0x0c], [0xf9, 0xf1, 0xa5],
            [0x3b, 0x78, 0xff], [0xb4, 0x00, 0x9e], [0x61, 0xd6, 0xd6], [0xf2, 0xf2, 0xf2],
        ],
    }),
    ("one-dark", Palette {
        fg: [0xab, 0xb2, 0xbf], bg: [0x28, 0x2c, 0x34],
        ansi: [
            [0x28, 0x2c, 0x34], [0xe0, 0x6c, 0x75], [0x98, 0xc3, 0x79], [0xe5, 0xc0, 0x7b],
            [0x61, 0xaf, 0xef], [0xc6, 0x78, 0xdd], [0x56, 0xb6, 0xc2], [0xab, 0xb2, 0xbf],
            [0x5c, 0x63, 0x70], [0xe0, 0x6c, 0x75], [0x98, 0xc3, 0x79], [0xe5, 0xc0, 0x7b],
            [0x61, 0xaf, 0xef], [0xc6, 0x78, 0xdd], [0x56, 0xb6, 0xc2], [0xff, 0xff, 0xff],
        ],
    }),
    ("gruvbox-dark", Palette {
        fg: [0xeb, 0xdb, 0xb2], bg: [0x28, 0x28, 0x28],
        ansi: [
            [0x28, 0x28, 0x28], [0xcc, 0x24, 0x1d], [0x98, 0x97, 0x1a], [0xd7, 0x99, 0x21],
            [0x45, 0x85, 0x88], [0xb1, 0x62, 0x86], [0x68, 0x9d, 0x6a], [0xa8, 0x99, 0x84],
            [0x92, 0x83, 0x74], [0xfb, 0x49, 0x34], [0xb8, 0xbb, 0x26], [0xfa, 0xbd, 0x2f],
            [0x83, 0xa5, 0x98], [0xd3, 0x86, 0x9b], [0x8e, 0xc0, 0x7c], [0xeb, 0xdb, 0xb2],
        ],
    }),
    ("nord", Palette {
        fg: [0xd8, 0xde, 0xe9], bg: [0x2e, 0x34, 0x40],
        ansi: [
            [0x3b, 0x42, 0x52], [0xbf, 0x61, 0x6a], [0xa3, 0xbe, 0x8c], [0xeb, 0xcb, 0x8b],
            [0x81, 0xa1, 0xc1], [0xb4, 0x8e, 0xad], [0x88, 0xc0, 0xd0], [0xe5, 0xe9, 0xf0],
            [0x4c, 0x56, 0x6a], [0xbf, 0x61, 0x6a], [0xa3, 0xbe, 0x8c], [0xeb, 0xcb, 0x8b],
            [0x81, 0xa1, 0xc1], [0xb4, 0x8e, 0xad], [0x8f, 0xbc, 0xbb], [0xec, 0xef, 0xf4],
        ],
    }),
    ("catppuccin-mocha", Palette {
        fg: [0xcd, 0xd6, 0xf4], bg: [0x1e, 0x1e, 0x2e],
        ansi: [
            [0x45, 0x47, 0x5a], [0xf3, 0x8b, 0xa8], [0xa6, 0xe3, 0xa1], [0xf9, 0xe2, 0xaf],
            [0x89, 0xb4, 0xfa], [0xf5, 0xc2, 0xe7], [0x94, 0xe2, 0xd5], [0xba, 0xc2, 0xde],
            [0x58, 0x5b, 0x70], [0xf3, 0x8b, 0xa8], [0xa6, 0xe3, 0xa1], [0xf9, 0xe2, 0xaf],
            [0x89, 0xb4, 0xfa], [0xf5, 0xc2, 0xe7], [0x94, 0xe2, 0xd5], [0xa6, 0xad, 0xc8],
        ],
    }),
    ("tokyo-night", Palette {
        fg: [0xc0, 0xca, 0xf5], bg: [0x1a, 0x1b, 0x26],
        ansi: [
            [0x15, 0x16, 0x1e], [0xf7, 0x76, 0x8e], [0x9e, 0xce, 0x6a], [0xe0, 0xaf, 0x68],
            [0x7a, 0xa2, 0xf7], [0xbb, 0x9a, 0xf7], [0x7d, 0xcf, 0xff], [0xa9, 0xb1, 0xd6],
            [0x41, 0x48, 0x68], [0xf7, 0x76, 0x8e], [0x9e, 0xce, 0x6a], [0xe0, 0xaf, 0x68],
            [0x7a, 0xa2, 0xf7], [0xbb, 0x9a, 0xf7], [0x7d, 0xcf, 0xff], [0xc0, 0xca, 0xf5],
        ],
    }),
    ("solarized-dark", Palette {
        fg: [0x83, 0x94, 0x96], bg: [0x00, 0x2b, 0x36],
        ansi: [
            [0x07, 0x36, 0x42], [0xdc, 0x32, 0x2f], [0x85, 0x99, 0x00], [0xb5, 0x89, 0x00],
            [0x26, 0x8b, 0xd2], [0xd3, 0x36, 0x82], [0x2a, 0xa1, 0x98], [0xee, 0xe8, 0xd5],
            [0x00, 0x2b, 0x36], [0xcb, 0x4b, 0x16], [0x58, 0x6e, 0x75], [0x65, 0x7b, 0x83],
            [0x83, 0x94, 0x96], [0x6c, 0x71, 0xc4], [0x93, 0xa1, 0xa1], [0xfd, 0xf6, 0xe3],
        ],
    }),
];

/// Look a [`PRESETS`] entry up by name, case-insensitively. `None` for an unknown name.
pub fn preset_palette(name: &str) -> Option<Palette> {
    PRESETS.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, p)| *p)
}

/// The settings panel's cycle order: "default" (no preset) followed by every [`PRESETS`] name, so
/// the panel and the resolver can't list different schemes.
pub fn preset_names() -> Vec<&'static str> {
    std::iter::once("default").chain(PRESETS.iter().map(|(n, _)| *n)).collect()
}

/// Eight colours previewing what a scheme *name* looks like: background, the six ANSI hues you
/// actually see in a terminal (red..cyan), foreground. `"default"` previews the built-ins; an
/// unknown name has nothing to preview and yields an empty ribbon rather than a misleading one.
pub fn preset_swatch(name: &str) -> Vec<[u8; 3]> {
    let p = if name.eq_ignore_ascii_case("default") {
        Some(DEFAULT_PALETTE)
    } else {
        preset_palette(name)
    };
    p.map_or_else(Vec::new, |p| {
        let mut v = vec![p.bg];
        v.extend_from_slice(&p.ansi[1..7]);
        v.push(p.fg);
        v
    })
}

/// The 16 ANSI slots keyed by their Windows Terminal scheme names (index = ANSI color number).
const WT_ANSI_KEYS: [&str; 16] = [
    "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
    "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue", "brightPurple",
    "brightCyan", "brightWhite",
];

/// What the config asks the chrome accent to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccentChoice {
    /// The built-in accent (cmux blue).
    Default,
    /// Follow the Windows accent color.
    System,
    /// A pinned `#rrggbb`.
    Fixed([u8; 3]),
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub font_px: Option<f32>,
    pub theme: Option<Theme>,
    /// action name -> chord string, e.g. `"split_h": "ctrl+alt+2"`.
    pub keys: Option<HashMap<String, String>>,
    /// Hovering a pane focuses it without a click (X11-style). Default off.
    pub focus_follows_mouse: Option<bool>,
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

    /// How `theme.accent` resolves: a pinned color, the Windows accent (`"system"`), or the
    /// built-in default when unset or unparseable.
    pub fn accent(&self) -> AccentChoice {
        match self.theme.as_ref().and_then(|t| t.accent.as_deref()) {
            Some(s) if s.eq_ignore_ascii_case("system") => AccentChoice::System,
            Some(s) => parse_hex(s).map_or(AccentChoice::Default, AccentChoice::Fixed),
            None => AccentChoice::Default,
        }
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
        // A named preset is the coarsest layer: it replaces the whole palette, and everything
        // below refines it. An unknown name is logged and skipped rather than silently ignored —
        // a typo'd preset would otherwise look like the feature doesn't work.
        if let Some(name) = theme.preset.as_deref() {
            match preset_palette(name) {
                Some(preset) => p = preset,
                None if name.eq_ignore_ascii_case("default") => {}
                None => eprintln!("gmux: unknown theme preset {name:?}, using the defaults"),
            }
        }
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
    \"preset\": null,
    \"fg\": null,
    \"bg\": null,
    \"scheme\": null,
    \"ansi\": null,
    \"accent\": null
  }},
  \"keys\": {{
{keys}
  }}
}}
"
        ,
        font = 18.0f32, // ponytail: mirror of app::DEFAULT_FONT_PX (config cannot import app)
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

/// The compiled-in default bindings — `(action_name, chord, action)` — for the command palette's
/// action list. ponytail: the palette shows DEFAULT chords even when a user rebound one (the
/// action still runs; only the hint could be stale).
pub fn default_bindings() -> &'static [(&'static str, &'static str, Action)] {
    DEFAULTS
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

    // -- built-in colour schemes --

    #[test]
    fn every_preset_resolves_and_is_refined_by_the_finer_layers() {
        // The panel cycles exactly the presets that resolve, so a name it shows can't be one the
        // resolver rejects (which would look like picking a scheme did nothing).
        let names = preset_names();
        assert_eq!(names[0], "default", "the cycle starts at 'no preset'");
        assert_eq!(names.len(), PRESETS.len() + 1);
        for name in names.iter().skip(1) {
            let p = preset_palette(name).unwrap_or_else(|| panic!("{name} does not resolve"));
            assert_ne!(p.fg, p.bg, "{name} would render invisible text");
            // Case-insensitive, the way the config is read.
            assert_eq!(preset_palette(&name.to_uppercase()), Some(p));
        }
        assert_eq!(preset_palette("no-such-scheme"), None);

        // A preset replaces the whole palette...
        let cfg = Config {
            theme: Some(Theme { preset: Some("nord".into()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(cfg.palette(), preset_palette("nord").unwrap());

        // ...and every finer layer still overrides it: inline `ansi` beats the preset's slots, and
        // the standalone fg/bg beat the preset's fg/bg.
        let cfg = Config {
            theme: Some(Theme {
                preset: Some("nord".into()),
                ansi: Some(vec!["#010203".into()]), // slot 0 only
                fg: Some("#0a0b0c".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = cfg.palette();
        let nord = preset_palette("nord").unwrap();
        assert_eq!(p.ansi[0], [0x01, 0x02, 0x03], "inline ansi wins over the preset");
        assert_eq!(p.ansi[1], nord.ansi[1], "untouched slots keep the preset");
        assert_eq!(p.fg, [0x0a, 0x0b, 0x0c], "theme.fg wins over the preset");
        assert_eq!(p.bg, nord.bg, "no theme.bg -> the preset's background stands");
    }

    #[test]
    fn every_cycled_name_previews_the_palette_it_would_apply() {
        // The panel draws this ribbon next to the name, so it has to be the palette the same name
        // resolves to — a swatch that disagreed with what Enter applies is worse than none.
        for name in preset_names() {
            let s = preset_swatch(name);
            assert_eq!(s.len(), 8, "{name} previews bg + 6 hues + fg");
            let p = preset_palette(name).unwrap_or(DEFAULT_PALETTE);
            assert_eq!(s[0], p.bg);
            assert_eq!(s[1..7], p.ansi[1..7]);
            assert_eq!(s[7], p.fg);
        }
        assert_eq!(preset_swatch("DEFAULT"), preset_swatch("default"), "case-insensitive");
        assert!(preset_swatch("no-such-scheme").is_empty(), "nothing to preview -> no ribbon");
    }

    #[test]
    fn unknown_preset_leaves_the_defaults_alone() {
        let cfg = Config {
            theme: Some(Theme { preset: Some("garbage".into()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(cfg.palette(), DEFAULT_PALETTE, "a typo'd preset must not corrupt the palette");
    }

    #[test]
    fn the_template_pins_no_colors_of_its_own() {
        // The template is written on first open; if it pinned fg/bg (the LAST palette layer) a
        // preset picked later would render with the wrong background and look broken.
        let cfg: Config = serde_json::from_str(&default_template()).unwrap();
        let theme = cfg.theme.as_ref().unwrap();
        assert!(theme.fg.is_none() && theme.bg.is_none() && theme.preset.is_none());
        assert_eq!(cfg.palette(), DEFAULT_PALETTE);
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
    fn toggle_browser_binds_and_names() {
        // Ctrl+Shift+B toggles the embedded browser panel out of the box.
        let km = Keymap::default();
        let (m, k) = parse_chord("ctrl+shift+b").unwrap();
        assert_eq!(km.action(m, &k), Some(Action::ToggleBrowser));
        assert_eq!(Action::from_name("toggle_browser"), Some(Action::ToggleBrowser));
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
    fn select_tab_actions_parse_and_bind() {
        // Names round-trip to a 1-based SelectTab; alt+digit chords bind them out of the box.
        assert_eq!(Action::from_name("select_tab_1"), Some(Action::SelectTab(1)));
        assert_eq!(Action::from_name("select_tab_9"), Some(Action::SelectTab(9)));
        assert_eq!(Action::from_name("select_tab_0"), None); // out of 1..=9
        assert_eq!(Action::from_name("select_tab_10"), None);
        assert_eq!(Action::from_name("select_tab_x"), None);
        // "alt+1" parses (a digit falls through named_key to Key::Character) — no chord-parser change.
        assert_eq!(parse_chord("alt+1"), Some((ModifiersState::ALT, Key::Character("1".into()))));
        let km = Keymap::default();
        for n in 1..=9u8 {
            let (m, k) = parse_chord(&format!("alt+{n}")).unwrap();
            assert_eq!(km.action(m, &k), Some(Action::SelectTab(n)), "alt+{n} binds tab {n}");
        }
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
    fn accent_choice_resolves_three_ways() {
        let with = |v: &str| Config {
            theme: Some(Theme { accent: Some(v.into()), ..Default::default() }),
            ..Default::default()
        };
        // Unset, and any unparseable value, fall back to the built-in accent.
        assert_eq!(Config::default().accent(), AccentChoice::Default);
        assert_eq!(with("nope").accent(), AccentChoice::Default);
        // "system" (any casing) opts into the Windows accent color.
        assert_eq!(with("system").accent(), AccentChoice::System);
        assert_eq!(with("System").accent(), AccentChoice::System);
        // A hex pins that exact color.
        assert_eq!(with("#60cdff").accent(), AccentChoice::Fixed([0x60, 0xcd, 0xff]));
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
                preset: None,
                accent: None,
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
