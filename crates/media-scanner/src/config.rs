use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use media_db::MediaKind;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Defaults to ~/Library/Application Support/mediaserver/media.db
    pub db_path: Option<PathBuf>,
    pub roots: Vec<RootConfig>,
    /// How long a file's size must stay unchanged before extraction (ms).
    #[serde(default = "default_settle_ms")]
    pub settle_ms: u64,
    /// Full reconcile interval, hours. 0 disables the periodic pass.
    #[serde(default = "default_reconcile_hours")]
    pub reconcile_interval_hours: u64,
    #[serde(default = "default_ffprobe")]
    pub ffprobe_path: String,
    /// Optional: run media-enrich automatically when new media appears.
    pub enrich: Option<EnrichConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrichConfig {
    /// Master switch; the section can stay in the file with auto = false.
    #[serde(default = "default_true")]
    pub auto: bool,
    /// The media-enrich binary (name on PATH or absolute path). It inherits
    /// the scanner's environment, so TMDB_API_KEY flows through.
    #[serde(default = "default_enrich_command")]
    pub command: String,
    /// Quiet period after the last new-media event before running, so a
    /// burst of files (a season drop) produces one run.
    #[serde(default = "default_quiet_secs")]
    pub quiet_secs: u64,
    /// Floor between consecutive runs.
    #[serde(default = "default_min_interval_secs")]
    pub min_interval_secs: u64,
    /// Read by media-enrich (which shares this file): neutralize embedded
    /// container titles in video files. Accepted here so the strict parser
    /// allows it; the scanner itself does not act on it.
    #[serde(default)]
    #[allow(dead_code)]
    pub strip_titles: bool,
    /// Read by media-enrich: embed same-name .srt sidecars into MP4s that
    /// have no subtitle stream. Accepted here; the scanner does not act on it.
    #[serde(default)]
    #[allow(dead_code)]
    pub embed_subtitles: bool,
    /// Read by media-enrich: remux MKV files to MP4. Accepted here; the
    /// scanner does not act on it.
    #[serde(default)]
    #[allow(dead_code)]
    pub remux_mkv: bool,
    /// Read by media-enrich: extract embedded text subtitles to .srt
    /// sidecars (default on). Accepted here; the scanner does not act on it.
    #[serde(default = "default_true")]
    #[allow(dead_code)]
    pub extract_subtitles: bool,
    /// Read by media-enrich: ffmpeg binary for subtitle embedding and remuxing.
    #[serde(default)]
    #[allow(dead_code)]
    pub ffmpeg_path: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_enrich_command() -> String {
    "media-enrich".into()
}
fn default_quiet_secs() -> u64 {
    60
}
fn default_min_interval_secs() -> u64 {
    600
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub path: PathBuf,
    pub kind: String,
}

fn default_settle_ms() -> u64 {
    2000
}
fn default_reconcile_hours() -> u64 {
    6
}
fn default_ffprobe() -> String {
    "ffprobe".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        if cfg.roots.is_empty() {
            bail!("config has no [[roots]] entries");
        }
        for root in &cfg.roots {
            if MediaKind::parse(&root.kind).is_none() {
                bail!(
                    "root {}: kind must be movies, music, or tv (got {:?})",
                    root.path.display(),
                    root.kind
                );
            }
        }
        Ok(cfg)
    }

    pub fn db_path(&self) -> PathBuf {
        self.db_path.clone().unwrap_or_else(media_db::open::default_db_path)
    }

    /// Roots as (absolute path string, kind), for media_db::sync_roots.
    pub fn root_specs(&self) -> Vec<(String, MediaKind)> {
        self.roots
            .iter()
            .map(|r| {
                (
                    r.path.to_string_lossy().trim_end_matches('/').to_string(),
                    MediaKind::parse(&r.kind).unwrap(),
                )
            })
            .collect()
    }
}
