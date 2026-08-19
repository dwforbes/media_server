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
    // mov_text needs clean text; refuse anything that isn't valid UTF-8
    // rather than embedding mojibake.
    let srt_bytes = std::fs::read(&srt)?;
    if srt_bytes.is_empty() || std::str::from_utf8(&srt_bytes).is_err() {
        return Ok(Outcome::Skipped("srt empty or not UTF-8"));
    }

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
    let status = Command::new(ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(media)
        .arg("-i")
        .arg(&srt)
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
