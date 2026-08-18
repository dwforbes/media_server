use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use media_db::TechInfo;
use serde_json::Value;

/// Probe a video file with ffprobe. Returns the technical attributes plus
/// any genre embedded in the container tags.
pub fn probe(ffprobe: &str, path: &Path) -> Result<(TechInfo, Option<String>)> {
    let output = Command::new(ffprobe)
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    if !output.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: Value = serde_json::from_slice(&output.stdout).context("parsing ffprobe JSON")?;

    let mut tech = TechInfo::default();
    if let Some(format) = json.get("format") {
        tech.duration_ms = format
            .get("duration")
            .and_then(|d| d.as_str())
            .and_then(|d| d.parse::<f64>().ok())
            .map(|secs| (secs * 1000.0) as i64);
        tech.container = format
            .get("format_name")
            .and_then(|f| f.as_str())
            .map(|f| f.split(',').next().unwrap_or(f).to_string());
    }
    let mut genre = None;
    if let Some(tags) = json.pointer("/format/tags") {
        for key in ["genre", "GENRE", "Genre"] {
            if let Some(g) = tags.get(key).and_then(|v| v.as_str()) {
                genre = Some(g.to_string());
                break;
            }
        }
    }
    if let Some(streams) = json.get("streams").and_then(|s| s.as_array()) {
        for stream in streams {
            let codec_type = stream.get("codec_type").and_then(|t| t.as_str());
            let codec_name = stream
                .get("codec_name")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            match codec_type {
                Some("video") if tech.video_codec.is_none() => {
                    tech.video_codec = codec_name;
                    tech.width = stream.get("width").and_then(|w| w.as_i64());
                    tech.height = stream.get("height").and_then(|h| h.as_i64());
                }
                Some("audio") if tech.audio_codec.is_none() => {
                    tech.audio_codec = codec_name;
                }
                _ => {}
            }
        }
    }
    Ok((tech, genre))
}
