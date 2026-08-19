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

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| e == "mp4" || e == "m4v")
}

/// Counts of (subtitle streams, all streams) plus duration in seconds.
fn probe(ffprobe: &str, path: &Path) -> Result<(usize, usize, f64)> {
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
    let mut subs = 0;
    let mut streams = 0;
    let mut duration = 0.0;
    for line in text.lines() {
        if let Some(kind) = line.strip_prefix("codec_type=") {
            streams += 1;
            if kind == "subtitle" {
                subs += 1;
            }
        } else if let Some(d) = line.strip_prefix("duration=") {
            duration = d.parse().unwrap_or(0.0);
        }
    }
    Ok((subs, streams, duration))
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
    // mov_text needs clean text; refuse anything that isn't valid UTF-8
    // rather than embedding mojibake.
    let srt_bytes = std::fs::read(&srt)?;
    if srt_bytes.is_empty() || std::str::from_utf8(&srt_bytes).is_err() {
        return Ok(Outcome::Skipped("srt empty or not UTF-8"));
    }

    let (subs, streams, duration_before) = probe(ffprobe, media)?;
    if subs > 0 {
        return Ok(Outcome::Skipped("already has subtitles"));
    }
    if streams == 0 {
        return Ok(Outcome::Skipped("no streams"));
    }

    // Mux to a temp file beside the original (same filesystem => atomic
    // rename), with a name the scanner ignores (leading dot).
    let dir = media.parent().unwrap_or_else(|| Path::new("."));
    let stem = media.file_stem().unwrap_or_default().to_string_lossy();
    let temp: PathBuf = dir.join(format!(".{stem}.subtitles-tmp.mp4"));
    let _ = std::fs::remove_file(&temp);
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .arg("-i")
        .arg(&srt)
        .args([
            "-map", "0", "-map", "1:0",
            "-c", "copy", "-c:s", "mov_text",
            "-metadata:s:s:0", "language=eng",
            "-movflags", "+faststart",
            "-y",
        ])
        .arg(&temp)
        .status()
        .with_context(|| format!("running {ffmpeg}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        bail!("ffmpeg mux failed ({status})");
    }

    // Verify before replacing anything.
    let (subs_after, streams_after, duration_after) = probe(ffprobe, &temp)?;
    let sane = subs_after >= 1
        && streams_after == streams + 1
        && (duration_after - duration_before).abs() <= 1.0 + duration_before * 0.01;
    if !sane {
        let _ = std::fs::remove_file(&temp);
        bail!(
            "mux verification failed (streams {streams}->{streams_after}, subs {subs_after}, \
             duration {duration_before:.1}->{duration_after:.1}); original untouched"
        );
    }

    std::fs::rename(&temp, media)
        .with_context(|| format!("replacing {}", media.display()))?;
    Ok(Outcome::Embedded)
}
