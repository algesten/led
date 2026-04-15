//! Translate a chord string like `"Ctrl-c"`, `"Up"`, `"Enter"`, `"a"`,
//! `"Ctrl-Left"`, `"Ctrl-Space"` into the byte sequence a terminal sends
//! in raw mode.
//!
//! Coverage:
//! - Plain printable chars (ASCII + UTF-8)
//! - Named control keys: Enter/Return, Esc/Escape, Tab, Backspace, Space,
//!   Up, Down, Left, Right, Home, End, PgUp/PageUp, PgDn/PageDown, Delete
//! - Modifier `Ctrl-` on letters (xterm: byte AND 0x1F)
//! - Modifier `Ctrl-` on Space (0x00) and on `/`, `_`, `7` (0x1F — the
//!   three aliases all collapse to the same byte at the terminal layer)
//! - Modifier `Ctrl-` and/or `Alt-` on arrow keys, Home, End, PgUp/Dn,
//!   Delete via the xterm CSI modifier sequences (`ESC [ 1 ; <m> X` for
//!   letter forms; `ESC [ N ; <m> ~` for tilde forms), where `<m>` is
//!   1=none, 2=Shift, 3=Alt, 5=Ctrl, 7=Ctrl+Alt
//! - `Alt-` on any base by ESC-prefixing the base sequence

pub fn chord_to_bytes(chord: &str) -> Option<Vec<u8>> {
    if chord.is_empty() {
        return None;
    }

    let mut ctrl = false;
    let mut alt = false;
    let mut rest = chord;
    loop {
        if let Some(r) = strip_prefix_ci(rest, "Ctrl-") {
            ctrl = true;
            rest = r;
        } else if let Some(r) = strip_prefix_ci(rest, "Alt-") {
            alt = true;
            rest = r;
        } else {
            break;
        }
    }

    // 1. CSI keys (arrows, Home, End, PgUp/Dn, Delete) — uniform handling
    // of Ctrl/Alt modifiers via the xterm modifier protocol.
    if let Some(csi) = csi_shape(rest) {
        let bytes = csi.encode(modifier_code(ctrl, alt));
        return Some(bytes);
    }

    // 2. Specific named keys with non-CSI encodings.
    let base: Vec<u8> = match rest {
        "Enter" | "Return" => vec![b'\r'],
        "Esc" | "Escape" => vec![0x1b],
        "Tab" => vec![b'\t'],
        "Backspace" => vec![0x7f],
        "Space" if ctrl => {
            // Ctrl-Space sends NUL.
            ctrl = false;
            return Some(if alt { vec![0x1b, 0x00] } else { vec![0x00] });
        }
        "Space" => vec![b' '],
        s if s.chars().count() == 1 => {
            let ch = s.chars().next().unwrap();
            if ctrl {
                let lower = (ch as u32 as u8).to_ascii_lowercase();
                let byte = if lower.is_ascii_alphabetic() {
                    lower & 0x1f
                } else if matches!(ch, '/' | '_' | '7') {
                    // The three undo-alias chords. All three collapse to
                    // 0x1F at the terminal byte layer; they're worth
                    // distinct goldens because chord-PARSING in led
                    // accepts each spelling, but the byte stream is the
                    // same.
                    0x1f
                } else if ch == ' ' {
                    0x00
                } else {
                    return None;
                };
                ctrl = false;
                if alt {
                    return Some(vec![0x1b, byte]);
                } else {
                    return Some(vec![byte]);
                }
            }
            let mut buf = [0u8; 4];
            ch.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        _ => return None,
    };

    // 3. Reject Ctrl on the named keys handled above (Enter/Esc/Tab/Backspace).
    // These have no widely-supported terminal encoding for the Ctrl modifier.
    if ctrl {
        return None;
    }

    if alt {
        let mut out = vec![0x1b];
        out.extend_from_slice(&base);
        Some(out)
    } else {
        Some(base)
    }
}

#[derive(Clone, Copy)]
enum CsiShape {
    /// Letter form: `ESC [ <prefix> ; <mod> <letter>` (or `ESC [ <letter>`
    /// when no modifier).
    Letter(u8),
    /// Tilde form: `ESC [ <num> ; <mod> ~` (or `ESC [ <num> ~` when no
    /// modifier).
    Tilde(u8),
}

impl CsiShape {
    fn encode(self, m: u8) -> Vec<u8> {
        let mut out = vec![0x1b, b'['];
        match self {
            CsiShape::Letter(letter) => {
                if m == 1 {
                    out.push(letter);
                } else {
                    out.extend_from_slice(format!("1;{m}").as_bytes());
                    out.push(letter);
                }
            }
            CsiShape::Tilde(num) => {
                if m == 1 {
                    out.extend_from_slice(format!("{num}").as_bytes());
                } else {
                    out.extend_from_slice(format!("{num};{m}").as_bytes());
                }
                out.push(b'~');
            }
        }
        out
    }
}

fn csi_shape(name: &str) -> Option<CsiShape> {
    Some(match name {
        "Up" => CsiShape::Letter(b'A'),
        "Down" => CsiShape::Letter(b'B'),
        "Right" => CsiShape::Letter(b'C'),
        "Left" => CsiShape::Letter(b'D'),
        "Home" => CsiShape::Letter(b'H'),
        "End" => CsiShape::Letter(b'F'),
        "PgUp" | "PageUp" => CsiShape::Tilde(5),
        "PgDn" | "PageDown" => CsiShape::Tilde(6),
        "Delete" => CsiShape::Tilde(3),
        _ => return None,
    })
}

/// xterm modifier code: 1 none, 2 Shift, 3 Alt, 4 Shift+Alt, 5 Ctrl,
/// 6 Ctrl+Shift, 7 Ctrl+Alt, 8 Ctrl+Shift+Alt.
fn modifier_code(ctrl: bool, alt: bool) -> u8 {
    match (ctrl, alt) {
        (false, false) => 1,
        (false, true) => 3,
        (true, false) => 5,
        (true, true) => 7,
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c() {
        assert_eq!(chord_to_bytes("Ctrl-c"), Some(vec![0x03]));
    }
    #[test]
    fn ctrl_x() {
        assert_eq!(chord_to_bytes("Ctrl-x"), Some(vec![0x18]));
    }
    #[test]
    fn ctrl_lower_equals_upper() {
        assert_eq!(chord_to_bytes("Ctrl-S"), chord_to_bytes("Ctrl-s"));
    }
    #[test]
    fn enter() {
        assert_eq!(chord_to_bytes("Enter"), Some(vec![b'\r']));
    }
    #[test]
    fn esc() {
        assert_eq!(chord_to_bytes("Esc"), Some(vec![0x1b]));
    }
    #[test]
    fn up() {
        assert_eq!(chord_to_bytes("Up"), Some(vec![0x1b, b'[', b'A']));
    }
    #[test]
    fn lower_a() {
        assert_eq!(chord_to_bytes("a"), Some(vec![b'a']));
    }
    #[test]
    fn alt_x() {
        assert_eq!(chord_to_bytes("Alt-x"), Some(vec![0x1b, b'x']));
    }
    #[test]
    fn ctrl_left() {
        assert_eq!(chord_to_bytes("Ctrl-Left"), Some(b"\x1b[1;5D".to_vec()));
    }
    #[test]
    fn ctrl_right() {
        assert_eq!(chord_to_bytes("Ctrl-Right"), Some(b"\x1b[1;5C".to_vec()));
    }
    #[test]
    fn ctrl_home() {
        assert_eq!(chord_to_bytes("Ctrl-Home"), Some(b"\x1b[1;5H".to_vec()));
    }
    #[test]
    fn ctrl_end() {
        assert_eq!(chord_to_bytes("Ctrl-End"), Some(b"\x1b[1;5F".to_vec()));
    }
    #[test]
    fn alt_up() {
        assert_eq!(chord_to_bytes("Alt-Up"), Some(b"\x1b[1;3A".to_vec()));
    }
    #[test]
    fn ctrl_pgup() {
        assert_eq!(chord_to_bytes("Ctrl-PgUp"), Some(b"\x1b[5;5~".to_vec()));
    }
    #[test]
    fn pgdn_plain() {
        assert_eq!(chord_to_bytes("PgDn"), Some(b"\x1b[6~".to_vec()));
    }
    #[test]
    fn delete_plain() {
        assert_eq!(chord_to_bytes("Delete"), Some(b"\x1b[3~".to_vec()));
    }
    #[test]
    fn ctrl_space_is_nul() {
        assert_eq!(chord_to_bytes("Ctrl-Space"), Some(vec![0x00]));
    }
    #[test]
    fn ctrl_slash_aliases() {
        assert_eq!(chord_to_bytes("Ctrl-/"), Some(vec![0x1f]));
        assert_eq!(chord_to_bytes("Ctrl-_"), Some(vec![0x1f]));
        assert_eq!(chord_to_bytes("Ctrl-7"), Some(vec![0x1f]));
    }
    #[test]
    fn ctrl_on_unsupported_named() {
        // No widely-supported encoding for these.
        assert_eq!(chord_to_bytes("Ctrl-Enter"), None);
        assert_eq!(chord_to_bytes("Ctrl-Esc"), None);
        assert_eq!(chord_to_bytes("Ctrl-Tab"), None);
    }
}
