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
use media_db::textenc::decode_subtitle_text;

pub enum Outcome {
    /// Not a candidate (wrong format, no .srt, already subtitled, ...).
    Skipped(&'static str),
    Embedded,
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

const TEXT_SUB_CODECS: &[&str] =
    &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];

fn probe(ffprobe: &str, path: &Path) -> Result<Probe> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_name,codec_type:format=duration",
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
    // Per stream, codec_name precedes codec_type in the output.
    let mut last_codec = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("codec_name=") {
            last_codec = name.to_string();
        } else if let Some(kind) = line.strip_prefix("codec_type=") {
            match kind {
                "video" => p.video += 1,
                "audio" => p.audio += 1,
                // Only text subtitles count: a bitmap-only (PGS/VobSub)
                // file still deserves the .srt embedded.
                "subtitle" if TEXT_SUB_CODECS.contains(&last_codec.as_str()) => {
                    p.subtitle += 1
                }
                _ => {}
            }
        } else if let Some(d) = line.strip_prefix("duration=") {
            p.duration = d.parse().unwrap_or(0.0);
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
        return Ok(Outcome::Skipped("already has text subtitles"));
    }
    if before.video == 0 {
        return Ok(Outcome::Skipped("no video stream"));
    }

    // Mux to a temp file beside the original (same filesystem => atomic
    // rename), with a name the scanner ignores (leading dot).
    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp = crate::remux::temp_beside(dir, &stem, "subtitles-tmp");
    let srt_input: PathBuf = if needs_conversion {
        let converted = temp.with_extension("srt");
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
    let after = match probe(ffprobe, &temp) {
        Ok(after) => after,
        Err(err) => {
            let _ = std::fs::remove_file(&temp);
            return Err(err.context("verifying the muxed file; original untouched"));
        }
    };
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
