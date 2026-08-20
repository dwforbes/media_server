pub mod audio;
pub mod nfo;
pub mod video;

pub use media_db::nameparse;

use std::path::Path;

use anyhow::Result;
use media_db::queries::{files, movies, music, tv};
use media_db::{MediaKind, Root, TechInfo};
use rusqlite::Connection;

/// Extract attributes for one pending file and finalize it (status ready).
/// On failure the row is marked error and the daemon carries on.
pub fn extract_file(
    conn: &mut Connection,
    ffprobe: &str,
    root: &Root,
    rel_path: &str,
    file_id: i64,
) -> Result<()> {
    match try_extract(conn, ffprobe, root, rel_path, file_id) {
        Ok(()) => {
            tracing::info!("extracted {}/{}", root.path, rel_path);
            Ok(())
        }
        Err(err) => {
            tracing::warn!("extraction failed for {}/{}: {err:#}", root.path, rel_path);
            files::mark_error(conn, file_id)?;
            Ok(())
        }
    }
}

fn try_extract(
    conn: &mut Connection,
    ffprobe: &str,
    root: &Root,
    rel_path: &str,
    file_id: i64,
) -> Result<()> {
    let abs = Path::new(&root.path).join(rel_path);
    let stem = Path::new(rel_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| rel_path.to_string());
    let parent_dirs: Vec<&str> = Path::new(rel_path)
        .parent()
        .map(|p| {
            p.iter()
                .map(|c| c.to_str().unwrap_or(""))
                .filter(|c| !c.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut embedded = false;
    match root.kind {
        MediaKind::Movies => {
            let (tech, tag_genre) = probe_or_default(ffprobe, &abs);
            let (mut title, mut year) = nameparse::movie(&stem);
            let mut genres: Vec<String> =
                tag_genre.map(|g| audio::split_genres(&g)).unwrap_or_default();
            let mut directors: Vec<String> = Vec::new();
            let mut rating = None;
            let mut plot = None;
            let mut imdb_id = None;
            if let Some(nfo) = nfo::read_sidecar(&abs) {
                if let Some(t) = nfo.title {
                    title = t;
                }
                if nfo.year.is_some() {
                    year = nfo.year;
                }
                if !nfo.genres.is_empty() {
                    genres = nfo.genres;
                }
                directors = nfo.directors;
                rating = nfo.rating;
                plot = nfo.plot;
                imdb_id = nfo.imdb_id;
            }
            let sort = nameparse::sort_title(&title);
            movies::finalize_movie(
                conn, file_id, &tech, &title, &sort, year, rating, plot.as_deref(),
                imdb_id.as_deref(), &genres, &directors,
            )?;
        }
        MediaKind::Tv => {
            let (tech, _) = probe_or_default(ffprobe, &abs);
            let parsed = nameparse::episode(&stem, &parent_dirs);
            let nfo = nfo::read_sidecar(&abs);
            // nfo values win; name-parse fills the gaps.
            let series = nfo
                .as_ref()
                .and_then(|n| n.show_title.clone())
                .or_else(|| parsed.as_ref().map(|p| p.series.clone()))
                .unwrap_or_else(|| "Unknown Series".to_string());
            let season = nfo
                .as_ref()
                .and_then(|n| n.season)
                .or_else(|| parsed.as_ref().map(|p| p.season))
                .unwrap_or(0);
            let episode = nfo
                .as_ref()
                .and_then(|n| n.episode)
                .or_else(|| parsed.as_ref().map(|p| p.episode))
                .unwrap_or(0);
            let plot = nfo.as_ref().and_then(|n| n.plot.clone());
            let rating = nfo.as_ref().and_then(|n| n.rating);
            let ep_imdb = nfo.as_ref().and_then(|n| n.imdb_id.clone());
            let title = nfo
                .and_then(|n| n.title)
                .or_else(|| parsed.map(|p| p.title))
                .unwrap_or_else(|| nameparse::clean_name(&stem));
            tv::finalize_episode(
                conn, file_id, &tech, &series, season, episode, &title, plot.as_deref(),
                rating, ep_imdb.as_deref(),
            )?;
        }
        MediaKind::Music => {
            let (tech, meta, embedded_art) = audio::extract(&abs, &stem, &parent_dirs)?;
            music::finalize_track(conn, file_id, &tech, &meta)?;
            if embedded_art {
                embedded = true;
            }
        }
    }
    let art = discover_sidecar_art(&abs, rel_path, root.kind)
        .or_else(|| embedded.then(|| "embedded".to_string()));
    files::record_art(conn, file_id, art.as_deref())?;
    files::record_nfo_mtime(conn, file_id, nfo_mtime(&abs))?;
    Ok(())
}

/// Sidecar image names recognized as art for a whole directory.
pub const DIR_ART_NAMES: &[&str] =
    &["cover.jpg", "cover.png", "folder.jpg", "front.jpg", "poster.jpg", "poster.png"];

/// Find sidecar artwork for a media file, returned as a root-relative path.
/// - Movies: "<stem>-poster.jpg/png" only — directory-level images would
///   wrongly apply to every movie sharing a folder.
/// - Music: directory-level cover images.
/// - TV: "<stem>-poster.*" first, then a directory-level poster (the series
///   folder), then the parent directory's poster (series folder when the
///   file sits in a season subfolder). Never the root itself, where a
///   poster would wrongly claim unrelated loose files.
pub fn discover_sidecar_art(abs: &Path, rel_path: &str, kind: MediaKind) -> Option<String> {
    let dir_abs = abs.parent()?;
    let rel_dir = rel_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let joined = |dir: &str, name: &str| {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };

    match kind {
        MediaKind::Music => {
            for name in DIR_ART_NAMES {
                if dir_abs.join(name).is_file() {
                    return Some(joined(rel_dir, name));
                }
            }
        }
        MediaKind::Movies | MediaKind::Tv => {
            let stem = Path::new(rel_path).file_stem()?.to_str()?;
            for ext in ["jpg", "png"] {
                let name = format!("{stem}-poster.{ext}");
                if dir_abs.join(&name).is_file() {
                    return Some(joined(rel_dir, &name));
                }
            }
            if kind == MediaKind::Tv {
                for name in ["poster.jpg", "poster.png", "folder.jpg"] {
                    if !rel_dir.is_empty() && dir_abs.join(name).is_file() {
                        return Some(joined(rel_dir, name));
                    }
                }
                if let Some((parent_rel, _)) = rel_dir.rsplit_once('/') {
                    let parent_abs = dir_abs.parent()?;
                    for name in ["poster.jpg", "poster.png", "folder.jpg"] {
                        if parent_abs.join(name).is_file() {
                            return Some(joined(parent_rel, name));
                        }
                    }
                }
            }
        }
    }
    None
}

/// mtime of the .nfo sidecar next to a media file, if one exists.
pub fn nfo_mtime(media_path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(media_path.with_extension("nfo")).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(mtime)
}

/// ffprobe failure degrades to an empty TechInfo: the file is still
/// catalogued and playable, just without duration/resolution attributes.
fn probe_or_default(ffprobe: &str, abs: &Path) -> (TechInfo, Option<String>) {
    match video::probe(ffprobe, abs) {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("ffprobe unavailable/failed ({err:#}); cataloguing without tech info");
            (TechInfo::default(), None)
        }
    }
}
