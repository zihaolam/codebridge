//! Host terminal color detection.
//!
//! Coding agents such as Codex derive UI shades (e.g. the input box background)
//! from the terminal's default background, which they discover by sending an
//! `OSC 11 ;?` query. Because the daemon's embedded libghostty terminal has no
//! idea what the *outer* terminal's background is, it would answer that query
//! with its own built-in default and the agent's derived shade would not
//! contrast with what Codebridge actually shows. To fix that, the client queries
//! the real terminal for its foreground/background once at startup, then the
//! daemon feeds those colors into each session's terminal as an `OSC 10/11` set
//! so libghostty answers the agent's query with the host's real colors.
//!
//! This mirrors Herdr's terminal-theme handling.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TerminalTheme {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<RgbColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<RgbColor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultColorKind {
    Foreground,
    Background,
}

/// Query the host terminal for its default foreground (OSC 10) and background
/// (OSC 11). Terminals reply with `\x1b]1{0,1};rgb:rrrr/gggg/bbbb\x1b\\`.
pub const HOST_COLOR_QUERY_SEQUENCE: &str = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\";

impl TerminalTheme {
    pub fn with_color(mut self, kind: DefaultColorKind, color: RgbColor) -> Self {
        match kind {
            DefaultColorKind::Foreground => self.foreground = Some(color),
            DefaultColorKind::Background => self.background = Some(color),
        }
        self
    }

    pub fn is_empty(self) -> bool {
        self.foreground.is_none() && self.background.is_none()
    }

    /// The `OSC 10`/`OSC 11` set sequences that make an embedded terminal adopt
    /// these colors as its defaults (so it answers agent queries with them).
    pub fn set_sequences(self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if let Some(color) = self.foreground {
            bytes.extend_from_slice(
                osc_set_default_color_sequence(DefaultColorKind::Foreground, color).as_bytes(),
            );
        }
        if let Some(color) = self.background {
            bytes.extend_from_slice(
                osc_set_default_color_sequence(DefaultColorKind::Background, color).as_bytes(),
            );
        }
        bytes
    }
}

/// Parse a single `OSC 10`/`OSC 11` color report from a terminal.
pub fn parse_default_color_response(sequence: &str) -> Option<(DefaultColorKind, RgbColor)> {
    let body = sequence.strip_prefix("\x1b]")?;
    let body = body
        .strip_suffix("\x1b\\")
        .or_else(|| body.strip_suffix('\u{7}'))?;
    let (command, value) = body.split_once(';')?;
    let kind = match command {
        "10" => DefaultColorKind::Foreground,
        "11" => DefaultColorKind::Background,
        _ => return None,
    };
    Some((kind, parse_rgb_color(value)?))
}

pub fn osc_set_default_color_sequence(kind: DefaultColorKind, color: RgbColor) -> String {
    let command = match kind {
        DefaultColorKind::Foreground => 10,
        DefaultColorKind::Background => 11,
    };
    format!(
        "\x1b]{command};rgb:{:02x}/{:02x}/{:02x}\x1b\\",
        color.r, color.g, color.b
    )
}

/// Scan a raw byte buffer for complete `OSC 10`/`OSC 11` reports and fold each
/// into `theme`. Returns how many bytes were consumed so far so a caller can
/// keep an incomplete trailing sequence buffered.
pub fn absorb_color_responses(buffer: &[u8], theme: &mut TerminalTheme) -> usize {
    let text = String::from_utf8_lossy(buffer);
    let mut consumed = 0;
    let mut rest = text.as_ref();
    while let Some(start) = rest.find("\x1b]") {
        let after = &rest[start..];
        // A complete OSC sequence ends with ST (ESC \) or BEL.
        let end = match find_osc_terminator(after) {
            Some(end) => end,
            None => break, // incomplete tail; stop and keep it buffered
        };
        let sequence = &after[..end];
        if let Some((kind, color)) = parse_default_color_response(sequence) {
            *theme = theme.with_color(kind, color);
        }
        let advance = start + end;
        consumed += advance;
        rest = &rest[advance..];
    }
    consumed.min(buffer.len())
}

/// Byte length of a complete OSC sequence starting at the beginning of `text`,
/// including its ST (`ESC \`) or BEL terminator, or None if it is incomplete.
fn find_osc_terminator(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 2; // skip the leading ESC ]
    while index < bytes.len() {
        match bytes[index] {
            0x07 => return Some(index + 1),
            0x1b if bytes.get(index + 1) == Some(&b'\\') => return Some(index + 2),
            _ => index += 1,
        }
    }
    None
}

fn parse_rgb_color(value: &str) -> Option<RgbColor> {
    if let Some(rgb) = value.strip_prefix("rgb:") {
        let mut parts = rgb.split('/');
        // Consume exactly three components in order, then reject any trailing
        // one. Kept as sequential lets (not `Some(..).filter(..)`) so the
        // trailing-part check runs *after* r/g/b are parsed off `parts`.
        let r = parse_hex_component(parts.next()?)?;
        let g = parse_hex_component(parts.next()?)?;
        let b = parse_hex_component(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some(RgbColor { r, g, b });
    }

    if let Some(hex) = value.strip_prefix('#') {
        let digits = hex.len() / 3;
        if !matches!(digits, 1..=4) || hex.len() != digits * 3 {
            return None;
        }
        return Some(RgbColor {
            r: parse_hex_component(&hex[..digits])?,
            g: parse_hex_component(&hex[digits..digits * 2])?,
            b: parse_hex_component(&hex[digits * 2..])?,
        });
    }

    None
}

fn parse_hex_component(component: &str) -> Option<u8> {
    if component.is_empty()
        || component.len() > 4
        || !component.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return None;
    }
    let value = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (component.len() * 4)) - 1;
    Some(((value * 255 + (max / 2)) / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_st_terminated_rgb_response() {
        assert_eq!(
            parse_default_color_response("\x1b]10;rgb:cccc/dddd/eeee\x1b\\"),
            Some((
                DefaultColorKind::Foreground,
                RgbColor {
                    r: 0xcc,
                    g: 0xdd,
                    b: 0xee,
                },
            ))
        );
    }

    #[test]
    fn parses_bel_terminated_hash_response() {
        assert_eq!(
            parse_default_color_response("\x1b]11;#123456\u{7}"),
            Some((
                DefaultColorKind::Background,
                RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56,
                },
            ))
        );
    }

    #[test]
    fn scales_short_hex_components() {
        assert_eq!(parse_hex_component("f"), Some(255));
        assert_eq!(parse_hex_component("80"), Some(128));
        assert_eq!(parse_hex_component("800"), Some(128));
        assert_eq!(parse_hex_component("8000"), Some(128));
    }

    #[test]
    fn set_sequences_emit_both_channels() {
        let theme = TerminalTheme::default()
            .with_color(DefaultColorKind::Foreground, RgbColor { r: 1, g: 2, b: 3 })
            .with_color(
                DefaultColorKind::Background,
                RgbColor {
                    r: 0x28,
                    g: 0x2a,
                    b: 0x36,
                },
            );
        let bytes = theme.set_sequences();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b]10;rgb:01/02/03\x1b\\\x1b]11;rgb:28/2a/36\x1b\\"
        );
    }

    #[test]
    fn absorbs_both_responses_from_one_buffer() {
        let buffer = b"\x1b]10;rgb:6565/7b7b/8383\x1b\\\x1b]11;rgb:2424/2727/3a3a\x07";
        let mut theme = TerminalTheme::default();
        let consumed = absorb_color_responses(buffer, &mut theme);
        assert_eq!(consumed, buffer.len());
        assert_eq!(
            theme.foreground,
            Some(RgbColor {
                r: 0x65,
                g: 0x7b,
                b: 0x83
            })
        );
        assert_eq!(
            theme.background,
            Some(RgbColor {
                r: 0x24,
                g: 0x27,
                b: 0x3a
            })
        );
    }

    #[test]
    fn keeps_incomplete_trailing_sequence_buffered() {
        // Complete bg report followed by an incomplete fg report.
        let buffer = b"\x1b]11;rgb:2424/2727/3a3a\x1b\\\x1b]10;rgb:6565";
        let mut theme = TerminalTheme::default();
        let consumed = absorb_color_responses(buffer, &mut theme);
        assert_eq!(
            theme.background,
            Some(RgbColor {
                r: 0x24,
                g: 0x27,
                b: 0x3a
            })
        );
        assert_eq!(theme.foreground, None);
        // Only the complete leading report is consumed.
        assert_eq!(consumed, "\x1b]11;rgb:2424/2727/3a3a\x1b\\".len());
    }
}
