//! tmux layout-string parsing (the `#{window_layout}` format, also carried by
//! `%layout-change` notifications).
//!
//! Grammar (mirrors tmux's `layout_dump`/`layout_parse`):
//!
//! ```text
//! layout := checksum ',' cell
//! cell   := WxH ',' X ',' Y ( ',' pane-id
//!                           | '{' cell (',' cell)* '}'    horizontal split (side by side)
//!                           | '[' cell (',' cell)* ']' )  vertical split (stacked)
//! ```
//!
//! The checksum is tmux's 16-bit rolling checksum printed as hex (`%04x`); gmux parses but does
//! not verify it. Example: `bb62,159x48,0,0{79x48,0,0,1,79x48,80,0,2}` — a 159x48 window split
//! into two side-by-side panes `%1` and `%2` (pane ids in layout strings carry no `%` sigil).

/// A parsed tmux window layout: checksum plus the root cell of the split tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub checksum: u16,
    pub root: Cell,
}

/// One node of the layout tree: a pane, or a horizontal/vertical split of child cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Leaf { w: u32, h: u32, x: u32, y: u32, pane: u64 },
    Split { w: u32, h: u32, x: u32, y: u32, horizontal: bool, children: Vec<Cell> },
}

impl std::str::FromStr for Layout {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        parse_layout(s)
    }
}

/// Parse a full tmux layout string. Malformed input yields `Err` with a byte-offset message;
/// this never panics.
pub fn parse_layout(s: &str) -> Result<Layout, String> {
    let (cksum, _) = s
        .split_once(',')
        .ok_or_else(|| format!("layout {s:?} has no ',' after checksum"))?;
    let checksum = u16::from_str_radix(cksum, 16)
        .map_err(|_| format!("bad layout checksum {cksum:?} (want 16-bit hex)"))?;
    let mut cursor = Cursor { bytes: s.as_bytes(), pos: cksum.len() + 1 };
    let root = cursor.cell(0)?;
    if cursor.pos != s.len() {
        return Err(format!("trailing garbage at byte {} of layout {s:?}", cursor.pos));
    }
    Ok(Layout { checksum, root })
}

/// Deepest split nesting accepted. Real layouts are a handful of levels; the cap exists because
/// layout strings arrive from the remote peer and the parser recurses per level — without a cap a
/// ~16 KB string of nested `{` overflows the stack (an uncatchable process abort, not an `Err`).
const MAX_DEPTH: usize = 64;

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {} of layout", b as char, self.pos))
        }
    }

    fn number<T: std::str::FromStr>(&mut self) -> Result<T, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(format!("expected a number at byte {start} of layout"));
        }
        // Only ASCII digits were consumed, so the slice is valid UTF-8.
        std::str::from_utf8(&self.bytes[start..self.pos])
            .unwrap()
            .parse()
            .map_err(|_| format!("number out of range at byte {start} of layout"))
    }

    fn cell(&mut self, depth: usize) -> Result<Cell, String> {
        if depth > MAX_DEPTH {
            return Err(format!("layout nesting deeper than {MAX_DEPTH} at byte {}", self.pos));
        }
        let w: u32 = self.number()?;
        self.expect(b'x')?;
        let h: u32 = self.number()?;
        self.expect(b',')?;
        let x: u32 = self.number()?;
        self.expect(b',')?;
        let y: u32 = self.number()?;
        match self.peek() {
            Some(b',') => {
                self.pos += 1;
                let pane: u64 = self.number()?;
                Ok(Cell::Leaf { w, h, x, y, pane })
            }
            Some(open) if open == b'{' || open == b'[' => {
                let horizontal = open == b'{';
                let close = if horizontal { b'}' } else { b']' };
                self.pos += 1;
                let mut children = vec![self.cell(depth + 1)?];
                loop {
                    match self.peek() {
                        Some(b',') => {
                            self.pos += 1;
                            children.push(self.cell(depth + 1)?);
                        }
                        Some(b) if b == close => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return Err(format!(
                                "expected ',' or '{}' at byte {} of layout",
                                close as char, self.pos
                            ));
                        }
                    }
                }
                Ok(Cell::Split { w, h, x, y, horizontal, children })
            }
            _ => Err(format!("expected ',', '{{' or '[' at byte {} of layout", self.pos)),
        }
    }
}
