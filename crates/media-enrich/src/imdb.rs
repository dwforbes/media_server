//! IMDb non-commercial datasets: title.ratings.tsv.gz has the real IMDb
//! rating for every title, keyed by IMDb id. ~7 MB, refreshed daily by
//! IMDb; we cache it locally and stream-scan it for just the ids we need.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

const DATASET_URL: &str = "https://datasets.imdbws.com/title.ratings.tsv.gz";

fn cache_path() -> PathBuf {
    media_db::open::default_db_path()
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("title.ratings.tsv.gz")
}

fn cache_is_fresh(path: &PathBuf, max_age_days: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < Duration::from_secs(max_age_days * 86400))
}

/// Ratings (0-10) for the requested IMDb ids. Downloads/refreshes the
/// dataset as needed, then makes one streaming pass over it.
pub fn ratings_for(
    needed: &HashSet<String>,
    max_age_days: u64,
) -> Result<HashMap<String, f64>> {
    let path = cache_path();
    if !cache_is_fresh(&path, max_age_days) {
        tracing::info!("downloading IMDb ratings dataset ({DATASET_URL})");
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        let bytes = client
            .get(DATASET_URL)
            .send()
            .context("downloading IMDb ratings dataset")?
            .error_for_status()?
            .bytes()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &bytes)
            .with_context(|| format!("writing {}", path.display()))?;
    }

    let file = std::fs::File::open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(file));
    let mut out = HashMap::new();
    for line in reader.lines() {
        let line = line?;
        // tconst \t averageRating \t numVotes
        let mut fields = line.split('\t');
        let (Some(tconst), Some(rating)) = (fields.next(), fields.next()) else { continue };
        if needed.contains(tconst) {
            if let Ok(value) = rating.parse::<f64>() {
                out.insert(tconst.to_string(), value);
            }
            if out.len() == needed.len() {
                break;
            }
        }
    }
    Ok(out)
}
