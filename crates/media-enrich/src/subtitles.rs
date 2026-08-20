//! Embed a sidecar .srt into MP4 files that have no subtitle stream.
//!
//! Equivalent to the manual recipe
//!   ffmpeg -i in.mp4 -i in.srt -c copy -c:s mov_text out.mp4
//! — existing streams are copied untouched; only a subtitle track is
//! added. Unlike every other enrichment step this replaces a whole media
//! file, so it is deliberately conservative: strict preconditions, a mux
//! into a temp file in the same directory, ffprobe verification of the
//! result, then a single atomic rename over the original.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

pub enum Outcome {
    /// Not a candidate (wrong format, no .srt, already subtitled, ...).
    Skipped(&'static str),
    Embedded,
}

/// Decode subtitle bytes to UTF-8 text, or None if the encoding can't be
/// determined safely. Handles: UTF-8 (BOM stripped), UTF-16 LE/BE with a
/// BOM, and Windows-1252/Latin-1 when the content is >= 95% ASCII (the
/// English-subtitle case; a mostly-non-ASCII single-byte file could be any
/// codepage, so those are refused rather than guessed).
fn decode_subtitle_text(bytes: &[u8]) -> Option<String> {
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

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| e == "mp4" || e == "m4v")
}

/// Stream census of a file.
#[derive(Debug, Default, Clone, Copy)]
struct Probe {
    video: usize,
    audio: usize,
    subtitle: usize,
    duration: f64,
}

fn probe(ffprobe: &str, path: &Path) -> Result<Probe> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_type:format=duration",
            "-of", "default=nw=1",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut p = Probe::default();
    for line in text.lines() {
        match line.strip_prefix("codec_type=") {
            Some("video") => p.video += 1,
            Some("audio") => p.audio += 1,
            Some("subtitle") => p.subtitle += 1,
            Some(_) => {} // data/attachment streams: irrelevant to the check
            None => {
                if let Some(d) = line.strip_prefix("duration=") {
                    p.duration = d.parse().unwrap_or(0.0);
                }
            }
        }
    }
    Ok(p)
}

/// Embed `<stem>.srt` into an MP4 with no subtitle stream.
pub fn embed_if_applicable(ffmpeg: &str, ffprobe: &str, media: &Path) -> Result<Outcome> {
    if !is_mp4(media) {
        return Ok(Outcome::Skipped("not mp4"));
    }
    let srt = media.with_extension("srt");
    if !srt.is_file() {
        return Ok(Outcome::Skipped("no .srt sidecar"));
    }
    // mov_text needs clean UTF-8 text. Decode the unambiguous encodings
    // (UTF-16 via BOM, mostly-ASCII Windows-1252/Latin-1) rather than
    // embedding mojibake; anything unclear is skipped.
    let srt_bytes = std::fs::read(&srt)?;
    if srt_bytes.is_empty() {
        return Ok(Outcome::Skipped("srt is empty"));
    }
    let Some(srt_text) = decode_subtitle_text(&srt_bytes) else {
        return Ok(Outcome::Skipped("srt encoding unrecognized"));
    };
    // Mux from a UTF-8 temp sidecar when conversion was needed; the
    // original .srt is never modified.
    let needs_conversion = srt_text.as_bytes() != srt_bytes.as_slice();

    let before = probe(ffprobe, media)?;
    if before.subtitle > 0 {
        return Ok(Outcome::Skipped("already has subtitles"));
    }
    if before.video == 0 {
        return Ok(Outcome::Skipped("no video stream"));
    }

    // Mux to a temp file beside the original (same filesystem => atomic
    // rename), with a name the scanner ignores (leading dot).
    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp: PathBuf = dir.join(format!(".{stem}.subtitles-tmp.mp4"));
    let _ = std::fs::remove_file(&temp);
    let srt_input: PathBuf = if needs_conversion {
        let converted = dir.join(format!(".{stem}.subtitles-tmp.srt"));
        std::fs::write(&converted, &srt_text)
            .with_context(|| format!("writing {}", converted.display()))?;
        converted
    } else {
        srt.clone()
    };
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .arg("-i")
        .arg(&srt_input)
        // A plain single-pass stream copy, exactly like the manual recipe.
        // No +faststart: it rewrites the whole file a second time to move
        // the moov atom, doubling I/O and leaving a window where the output
        // can lack its moov entirely; range-request streaming doesn't need it.
        .args([
            "-map", "0:v", "-map", "0:a?", "-map", "0:s?", "-map", "1",
            "-c", "copy", "-c:s", "mov_text",
            "-metadata:s:s:0", "language=eng",
            "-y",
        ])
        .arg(&temp)
        .status()
        .with_context(|| format!("running {ffmpeg}"))?;
    if needs_conversion {
        let _ = std::fs::remove_file(&srt_input);
    }
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg mux failed ({status})");
    }
    // Make sure the mux is fully on disk before probing or renaming.
    if let Ok(f) = std::fs::File::open(&temp) {
        let _ = f.sync_all();
    }

    // Verify before replacing anything: every video/audio stream preserved,
    // a subtitle present, duration unchanged. Data/attachment streams are
    // ignored on both sides — faithful copies of odd containers carry them.
    let after = probe(ffprobe, &temp)?;
    let sane = after.subtitle >= 1
        && after.video == before.video
        && after.audio == before.audio
        && (after.duration - before.duration).abs() <= 1.0 + before.duration * 0.01;
    if !sane {
        let _ = std::fs::remove_file(&temp);
        bail!(
            "mux verification failed (video {}->{}, audio {}->{}, subs {}, \
             duration {:.1}->{:.1}); original untouched",
            before.video, after.video, before.audio, after.audio, after.subtitle,
            before.duration, after.duration
        );
    }

    std::fs::rename(&temp, media)
        .with_context(|| format!("replacing {}", media.display()))?;
    Ok(Outcome::Embedded)
}
