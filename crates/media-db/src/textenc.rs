//! Subtitle text decoding: UTF-8 (BOM stripped), UTF-16 via BOM, and
//! mostly-ASCII Windows-1252/Latin-1. Shared by subtitle embedding and
//! the WebVTT endpoint.

/// Decode subtitle bytes to UTF-8 text, or None if the encoding can't be
/// determined safely. Handles: UTF-8 (BOM stripped), UTF-16 LE/BE with a
/// BOM, and Windows-1252/Latin-1 when the content is >= 95% ASCII (the
/// English-subtitle case; a mostly-non-ASCII single-byte file could be any
/// codepage, so those are refused rather than guessed).
pub fn decode_subtitle_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() >= 2 && (bytes[..2] == [0xFF, 0xFE] || bytes[..2] == [0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return Some(String::from_utf16_lossy(&units));
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.strip_prefix('\u{feff}').unwrap_or(text).to_string());
    }
    let ascii = bytes.iter().filter(|b| b.is_ascii()).count();
    if (ascii as f64) / (bytes.len() as f64) < 0.95 {
        return None;
    }
    Some(bytes.iter().map(|&b| cp1252_char(b)).collect())
}

/// Windows-1252 byte to char: ASCII and Latin-1 map directly; 0x80-0x9F
/// are the cp1252 punctuation specials (curly quotes, dashes, ellipsis).
fn cp1252_char(b: u8) -> char {
    match b {
        0x80 => '\u{20AC}', 0x82 => '\u{201A}', 0x83 => '\u{0192}', 0x84 => '\u{201E}',
        0x85 => '\u{2026}', 0x86 => '\u{2020}', 0x87 => '\u{2021}', 0x88 => '\u{02C6}',
        0x89 => '\u{2030}', 0x8A => '\u{0160}', 0x8B => '\u{2039}', 0x8C => '\u{0152}',
        0x8E => '\u{017D}', 0x91 => '\u{2018}', 0x92 => '\u{2019}', 0x93 => '\u{201C}',
        0x94 => '\u{201D}', 0x95 => '\u{2022}', 0x96 => '\u{2013}', 0x97 => '\u{2014}',
        0x98 => '\u{02DC}', 0x99 => '\u{2122}', 0x9A => '\u{0161}', 0x9B => '\u{203A}',
        0x9C => '\u{0153}', 0x9E => '\u{017E}', 0x9F => '\u{0178}',
        other => other as char,
    }
}
