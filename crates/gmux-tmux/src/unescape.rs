//! Octal unescaping for `%output` data.
//!
//! tmux control mode escapes every byte < 0x20 plus `\` itself as `\ooo` (exactly three octal
//! digits): `\` → `\134`, CR → `\015`. UTF-8 arrives as raw high bytes and must pass through
//! untouched. This module only decodes — gmux never needs the escape direction (commands sent
//! *to* tmux are plain text lines).

/// Decode tmux control-mode octal escapes in `%output` data.
///
/// A backslash followed by exactly three octal digits (`\000`..`\377`) decodes to that byte.
/// Anything else — a lone trailing backslash, too few octal digits, non-octal digits, or a
/// three-digit value above 0xff — passes through literally, byte for byte. Bytes >= 0x20
/// (including raw UTF-8 high bytes) are copied verbatim.
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\\' && i + 3 < data.len() {
            let d = &data[i + 1..i + 4];
            if d.iter().all(|&b| matches!(b, b'0'..=b'7')) {
                let value =
                    u16::from(d[0] - b'0') * 64 + u16::from(d[1] - b'0') * 8 + u16::from(d[2] - b'0');
                if value <= 0xff {
                    out.push(value as u8);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}
