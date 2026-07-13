//! gmux-vt — terminal state + OSC notification extraction.
//!
//! Wraps [`alacritty_terminal`]'s `Term` for grid / cursor / SGR state, and runs a **separate**
//! side [`vte::Parser`] for all OSC + BEL event extraction (per gmux ADR-003: alacritty's ansi
//! layer silently drops OSC 9/99/777, so gmux owns OSC parsing). In [`Terminal::advance`] the same
//! PTY bytes are fed to *both*: (a) alacritty's ansi `Processor` driving the `Term` (for the grid),
//! and (b) our side parser (for events). The side parser is stateful across calls, so OSC
//! sequences split across two `advance()` calls are still recognized.

mod osc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb as AnsiRgb};

use osc::OscState;

// ---------------------------------------------------------------------------
// Public contract (gmux-mux + renderer depend on these EXACT names/shapes).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyKind {
    Osc9,
    Osc777,
    Osc99,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotifyKind,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    Remove,
    Set,
    Error,
    Indeterminate,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    PromptStart,
    CommandStart,
    CommandExecuted,
    CommandFinished(Option<i32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    Damage,
    Bell,
    Title(String),
    Notification(Notification),
    Progress { state: ProgressState, pct: Option<u8> },
    Cwd(String),
    PromptMark(PromptMark),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    /// A double-width (CJK/emoji) char: it occupies this cell plus the next spacer cell (which
    /// carries `ch == ' '`). From alacritty's `Flags::WIDE_CHAR`.
    pub wide: bool,
}

// ---------------------------------------------------------------------------
// Default palette colors.
// ---------------------------------------------------------------------------

/// Default foreground: light gray.
const DEFAULT_FG: Rgb = Rgb { r: 0xcc, g: 0xcc, b: 0xcc };
/// Default background: near-black.
const DEFAULT_BG: Rgb = Rgb { r: 0x11, g: 0x11, b: 0x11 };

/// The default 16 system colors (indices 0..=15), matching the historical hardcoded xterm table.
const DEFAULT_ANSI: [Rgb; 16] = {
    const C: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), // 0  black
        (0x80, 0x00, 0x00), // 1  red
        (0x00, 0x80, 0x00), // 2  green
        (0x80, 0x80, 0x00), // 3  yellow
        (0x00, 0x00, 0x80), // 4  blue
        (0x80, 0x00, 0x80), // 5  magenta
        (0x00, 0x80, 0x80), // 6  cyan
        (0xc0, 0xc0, 0xc0), // 7  white
        (0x80, 0x80, 0x80), // 8  bright black
        (0xff, 0x00, 0x00), // 9  bright red
        (0x00, 0xff, 0x00), // 10 bright green
        (0xff, 0xff, 0x00), // 11 bright yellow
        (0x00, 0x00, 0xff), // 12 bright blue
        (0xff, 0x00, 0xff), // 13 bright magenta
        (0x00, 0xff, 0xff), // 14 bright cyan
        (0xff, 0xff, 0xff), // 15 bright white
    ];
    let mut out = [Rgb { r: 0, g: 0, b: 0 }; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = Rgb { r: C[i].0, g: C[i].1, b: C[i].2 };
        i += 1;
    }
    out
};

/// Runtime terminal color palette: default fg/bg and the 16 system colors (indices 0..=15).
/// The 216-color cube and grayscale ramp (indices 16..=255) stay computed and are not themeable.
/// `Default` reproduces gmux's historical hardcoded colors byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Palette { fg: DEFAULT_FG, bg: DEFAULT_BG, ansi: DEFAULT_ANSI }
    }
}

// ---------------------------------------------------------------------------
// Dimensions helper (alacritty's own TermSize is test-only / not exported).
// ---------------------------------------------------------------------------

struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

// ---------------------------------------------------------------------------
// Terminal.
// ---------------------------------------------------------------------------

/// Terminal state: an alacritty `Term` (grid/cursor/SGR) plus a side vte OSC parser (events).
pub struct Terminal {
    term: Term<VoidListener>,
    /// alacritty's ansi processor — drives the grid from raw bytes.
    ansi: Processor,
    /// Our side vte parser — stateful across `advance()` for split-sequence support.
    osc_parser: vte::Parser,
    /// Cross-call OSC state (OSC 99 chunk reassembly buffers).
    osc_state: OscState,
    /// Runtime color palette used when resolving grid cells to `Rgb`.
    palette: Palette,
    cols: u16,
    rows: u16,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let size = GridSize { cols: cols as usize, rows: rows as usize };
        let term = Term::new(Config::default(), &size, VoidListener);
        Terminal {
            term,
            ansi: Processor::new(),
            osc_parser: vte::Parser::new(),
            osc_state: OscState::default(),
            palette: Palette::default(),
            cols,
            rows,
        }
    }

    /// Replace the color palette used to resolve grid cells (fg/bg, the 16 system colors, cursor).
    /// Takes effect on the next `visible_cells`/`cells_at_offset` — the grid stores logical colors
    /// (Named/Indexed), so re-theming is instant with no reparse.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Feed raw PTY bytes; drive both the grid and the side event parser. Returns the events seen
    /// in this chunk (plus a single `Damage` if the chunk was non-empty).
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<TermEvent> {
        // (a) Drive alacritty's Term for the grid/cursor/SGR state.
        self.ansi.advance(&mut self.term, bytes);

        // (b) Drive our side parser for OSC/BEL events.
        let mut events = Vec::new();
        self.osc_state.advance(&mut self.osc_parser, bytes, &mut events);

        // Emit one Damage per non-empty advance (simplest correct signal for the renderer).
        if !bytes.is_empty() {
            events.push(TermEvent::Damage);
        }
        events
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let size = GridSize { cols: cols as usize, rows: rows as usize };
        self.term.resize(size);
        self.cols = cols;
        self.rows = rows;
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Cursor position as `(col, row)`, 0-based, clamped to the visible grid.
    pub fn cursor(&self) -> (u16, u16) {
        let point: Point = self.term.grid().cursor.point;
        let col = point.column.0 as u16;
        // Cursor line is relative to the top of the viewport (line 0) when display_offset == 0.
        let row = point.line.0.max(0) as u16;
        (col.min(self.cols.saturating_sub(1)), row.min(self.rows.saturating_sub(1)))
    }

    /// Visible grid as plain text rows, trailing blanks trimmed.
    pub fn visible_text(&self) -> Vec<String> {
        self.visible_cells()
            .into_iter()
            .map(|row| {
                let mut s: String = row.iter().map(|c| c.ch).collect();
                let trimmed = s.trim_end_matches(' ').len();
                s.truncate(trimmed);
                s
            })
            .collect()
    }

    /// Visible grid, row-major, with fg/bg resolved to `Rgb` for the renderer.
    pub fn visible_cells(&self) -> Vec<Vec<Cell>> {
        self.cells_at_offset(0)
    }

    /// Whether the application enabled bracketed paste (DECSET 2004) — pasted text should then
    /// be wrapped in `ESC[200~` / `ESC[201~` so the shell treats it as literal input.
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE)
    }

    /// The application's mouse-reporting mode as a bitfield (0 = wants no mouse):
    /// 1 = clicks (DECSET 1000), 2 = button-drag (1002), 4 = any-motion (1003), 8 = SGR encoding
    /// (1006). Mirrors alacritty's `TermMode` mouse flags onto the gmux-proto contract so the GUI
    /// knows whether to forward mouse events (and how to encode them) instead of doing selection.
    pub fn mouse_mode(&self) -> u8 {
        use alacritty_terminal::term::TermMode as M;
        let m = self.term.mode();
        (m.contains(M::MOUSE_REPORT_CLICK) as u8)
            | (m.contains(M::MOUSE_DRAG) as u8) << 1
            | (m.contains(M::MOUSE_MOTION) as u8) << 2
            | (m.contains(M::SGR_MOUSE) as u8) << 3
    }

    /// Number of scrollback (history) lines currently retained above the viewport.
    pub fn history_len(&self) -> usize {
        self.term.grid().history_size()
    }

    /// The viewport scrolled `offset` lines up into scrollback, row-major with resolved colors.
    /// `offset == 0` is the live screen (identical to [`visible_cells`]); `offset` is clamped to the
    /// available history so callers can over-scroll harmlessly. Used by the GUI scrollback viewport.
    pub fn cells_at_offset(&self, offset: usize) -> Vec<Vec<Cell>> {
        let grid = self.term.grid();
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let offset = offset.min(grid.history_size()) as i32;
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            // Top visible row is `offset` lines above screen line 0 (negative == history).
            let line = Line(r as i32 - offset);
            out.push(self.row_cells(grid, line, cols));
        }
        out
    }

    /// Resolve one grid line into a row of renderer [`Cell`]s.
    fn row_cells(
        &self,
        grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
        line: Line,
        cols: usize,
    ) -> Vec<Cell> {
        let row = &grid[line];
        let mut cells = Vec::with_capacity(cols);
        for c in 0..cols {
            let cell = &row[Column(c)];
            let flags = cell.flags;
            let inverse = flags.contains(Flags::INVERSE);
            let bold = flags.contains(Flags::BOLD);
            let mut fg = resolve_color(cell.fg, &self.palette, self.palette.fg, bold);
            let mut bg = resolve_color(cell.bg, &self.palette, self.palette.bg, false);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            cells.push(Cell {
                ch: cell.c,
                fg,
                bg,
                bold,
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                inverse,
                wide: flags.contains(Flags::WIDE_CHAR),
            });
        }
        cells
    }

    /// Scrollback + visible content as plain text lines (oldest first), trailing blanks trimmed.
    /// Returns at most `max_lines` of the *most recent* content (history nearest the viewport plus
    /// the live screen); `max_lines == 0` means "all retained lines". Fully blank lines at the top
    /// of the returned range are dropped so restored/idle terminals don't emit a wall of emptiness.
    /// Backs the automation `capture-pane -S` scrollback query and session-restore replay.
    pub fn scrollback_text(&self, max_lines: usize) -> Vec<String> {
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let top = grid.topmost_line().0; // negative: -history_size
        let bottom = grid.bottommost_line().0; // screen_lines - 1
        let total = (bottom - top + 1) as usize;
        let take = if max_lines == 0 { total } else { max_lines.min(total) };
        let start = bottom - (take as i32) + 1; // most-recent `take` lines
        let mut lines: Vec<String> = Vec::with_capacity(take);
        for l in start..=bottom {
            let row = &grid[Line(l)];
            let mut s: String = (0..cols).map(|c| row[Column(c)].c).collect();
            let trimmed = s.trim_end_matches(' ').len();
            s.truncate(trimmed);
            lines.push(s);
        }
        // Drop leading fully-blank lines (common when scrollback isn't full yet).
        let first_content = lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len());
        lines.drain(0..first_content);
        lines
    }
}

// ---------------------------------------------------------------------------
// Color resolution.
// ---------------------------------------------------------------------------

fn ansi_to_rgb(c: AnsiRgb) -> Rgb {
    Rgb { r: c.r, g: c.g, b: c.b }
}

/// Resolve an alacritty [`Color`] to concrete [`Rgb`] against `palette`.
/// - `Spec` -> its bytes directly.
/// - `Named` -> the palette's 16 system colors / default fg/bg (bold promotes normal to bright).
/// - `Indexed` -> the 256-color palette (0..=15 from `palette.ansi`, 16..=255 computed).
fn resolve_color(color: Color, palette: &Palette, default: Rgb, bold: bool) -> Rgb {
    match color {
        Color::Spec(rgb) => ansi_to_rgb(rgb),
        Color::Indexed(i) => xterm_256(i, palette),
        Color::Named(named) => named_color(named, palette, default, bold),
    }
}

fn named_color(named: NamedColor, palette: &Palette, default: Rgb, bold: bool) -> Rgb {
    use NamedColor::*;
    // Standard xterm-ish 16-color palette (indices 0..=15).
    let idx = match named {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        White => 7,
        BrightBlack => 8,
        BrightRed => 9,
        BrightGreen => 10,
        BrightYellow => 11,
        BrightBlue => 12,
        BrightMagenta => 13,
        BrightCyan => 14,
        BrightWhite => 15,
        // Dim variants map to their base 0..=7 index.
        DimBlack => 0,
        DimRed => 1,
        DimGreen => 2,
        DimYellow => 3,
        DimBlue => 4,
        DimMagenta => 5,
        DimCyan => 6,
        DimWhite => 7,
        // Semantic colors resolve to the palette's default fg/bg.
        Foreground | BrightForeground | DimForeground => {
            return if named == Foreground && bold {
                // Bold text with the default fg brightens slightly (bright white).
                xterm_256(15, palette)
            } else {
                default
            };
        }
        Background => return default,
        Cursor => return palette.fg,
    };
    // Bold promotes a normal (0..=7) named color to its bright (8..=15) counterpart, matching the
    // common terminal convention.
    let idx = if bold && idx < 8 { idx + 8 } else { idx };
    xterm_256(idx, palette)
}

/// The xterm 256-color palette, with the 16 system colors sourced from `palette`.
/// 0..=15   : the 16 system colors (from `palette.ansi`).
/// 16..=231 : a 6x6x6 color cube (computed).
/// 232..=255: a 24-step grayscale ramp (computed).
fn xterm_256(i: u8, palette: &Palette) -> Rgb {
    match i {
        0..=15 => palette.ansi[i as usize],
        16..=231 => {
            let n = i - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            // xterm cube steps: 0 -> 0, 1..5 -> 55 + 40*level.
            let step = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + 40 * v
                }
            };
            Rgb { r: step(r), g: step(g), b: step(b) }
        }
        232..=255 => {
            let level = 8 + 10 * (i - 232);
            Rgb { r: level, g: level, b: level }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod probe_tests;
