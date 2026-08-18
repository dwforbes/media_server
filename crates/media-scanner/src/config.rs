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
