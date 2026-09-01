use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use media_db::TechInfo;
use serde_json::Value;

/// A chapter marker from the container, times in ms. Title is empty when
/// the chapter is unnamed.
#[derive(Debug, Clone)]
pub struct Chapter {
    pub start_ms: i64,
    pub end_ms: i64,
    pub title: String,
}

/// Probe a video file with ffprobe. Returns the technical attributes plus
/// any genre embedded in the container tags and the chapter markers.
pub fn probe(ffprobe: &str, path: &Path) -> Result<(TechInfo, Option<String>, Vec<Chapter>)> {
    let output = Command::new(ffprobe)
        .args([
            "-v", "error", "-print_format", "json",
            "-show_format", "-show_streams", "-show_chapters",
        ])
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
                    // avg_frame_rate is the honest figure for VFR files;
                    // r_frame_rate (the base rate) fills in when the
                    // average is unknown ("0/0" on some streams).
                    tech.frame_rate = ["avg_frame_rate", "r_frame_rate"]
                        .iter()
                        .find_map(|key| {
                            parse_frame_rate(stream.get(*key).and_then(|v| v.as_str())?)
                        });
                }
                Some("audio") if tech.audio_codec.is_none() => {
                    let profile = stream.get("profile").and_then(|p| p.as_str());
                    tech.audio_profile = codec_name
                        .as_deref()
                        .map(|c| super::audio::ffprobe_label(c, profile));
                    tech.audio_codec = codec_name;
                    let num = |key: &str| {
                        stream.get(key).and_then(|v| {
                            v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
                        })
                    };
                    tech.audio_bitrate = num("bit_rate").map(|bps| bps / 1000).filter(|k| *k > 0);
                    tech.audio_sample_rate = num("sample_rate");
                    tech.audio_channels = num("channels");
                    tech.audio_bit_depth = num("bits_per_raw_sample")
                        .or_else(|| num("bits_per_sample"))
                        .filter(|b| *b > 0);
                }
                _ => {}
            }
        }
    }
    let chapters = parse_chapters(&json);
    Ok((tech, genre, chapters))
}

/// ffprobe frame rates are rationals ("24000/1001", "25/1"); "0/0" means
/// unknown. Rejects the nonsensical (zero, or beyond any real video).
fn parse_frame_rate(text: &str) -> Option<f64> {
    let (num, den) = text.split_once('/').unwrap_or((text, "1"));
    let (num, den) = (num.trim().parse::<f64>().ok()?, den.trim().parse::<f64>().ok()?);
    let fps = num / den;
    (fps.is_finite() && fps > 0.0 && fps <= 1000.0).then_some(fps)
}

fn parse_chapters(json: &Value) -> Vec<Chapter> {
    let mut chapters = Vec::new();
    if let Some(list) = json.get("chapters").and_then(|c| c.as_array()) {
        for chapter in list {
            let secs = |key: &str| {
                chapter
                    .get(key)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| (s * 1000.0) as i64)
            };
            let (Some(start_ms), Some(end_ms)) = (secs("start_time"), secs("end_time")) else {
                continue;
            };
            let title = chapter
                .pointer("/tags/title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            chapters.push(Chapter { start_ms, end_ms, title });
        }
    }
    chapters
}
