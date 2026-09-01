pub mod audio;
pub mod nfo;
pub mod segments;
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
    // A crafted file that trips a panic inside a tag or container parser
    // must cost that file its row, not the daemon its life.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        try_extract(conn, ffprobe, root, rel_path, file_id)
    }));
    match outcome {
        Ok(Ok(())) => {
            tracing::info!("extracted {}/{}", root.path, rel_path);
            Ok(())
        }
        Ok(Err(err)) => {
            tracing::warn!("extraction failed for {}/{}: {err:#}", root.path, rel_path);
            files::mark_error(conn, file_id)?;
            Ok(())
        }
        Err(_) => {
            tracing::error!("extraction panicked on {}/{}; marked error", root.path, rel_path);
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
            let (tech, tag_genre, chapters) = probe_or_default(ffprobe, &abs);
            let (mut title, mut year) = nameparse::movie(&stem);
            let mut genres: Vec<String> =
                tag_genre.map(|g| audio::split_genres(&g)).unwrap_or_default();
            let mut directors: Vec<String> = Vec::new();
            let mut rating = None;
            let mut plot = None;
            let mut imdb_id = None;
            let mut collection = None;
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
                collection = nfo.set;
            }
            let sort = nameparse::sort_title(&title);
            movies::finalize_movie(
                conn, file_id, &tech, &title, &sort, year, rating, plot.as_deref(),
                imdb_id.as_deref(), collection.as_deref(), &genres, &directors,
            )?;
            store_segments(conn, file_id, &abs, &tech, &chapters)?;
        }
        MediaKind::Tv => {
            let (tech, _, chapters) = probe_or_default(ffprobe, &abs);
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
            store_segments(conn, file_id, &abs, &tech, &chapters)?;
        }
        MediaKind::Music => {
            let (tech, mut meta, embedded_art) = audio::extract(&abs, &stem, &parent_dirs)?;
            if let Some(over) = nearest_music_meta(&abs, Path::new(&root.path)) {
                if over.track_number_prefix == Some(false) {
                    // Undo the number-prefix strip, but only when the title
                    // actually came from the path fallback (tags win).
                    let (_, _, fb_track, fb_title) =
                        nameparse::music_from_path(&stem, &parent_dirs);
                    if meta.title == fb_title {
                        meta.title = nameparse::clean_name(&stem);
                        if meta.track_no == fb_track {
                            meta.track_no = None;
                        }
                    }
                }
                if over.artist.is_some() {
                    meta.artist = over.artist;
                }
                if over.album_artist.is_some() {
                    meta.album_artist = over.album_artist;
                }
                if over.album.is_some() {
                    meta.album = over.album;
                }
                if let Some(genre) = over.genre {
                    meta.genres = audio::split_genres(&genre);
                }
            }
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
    files::record_edl_mtime(conn, file_id, segments::edl_mtime(&abs))?;
    Ok(())
}

/// Discover and store the skippable segments for one video file.
fn store_segments(
    conn: &Connection,
    file_id: i64,
    abs: &Path,
    tech: &TechInfo,
    chapters: &[video::Chapter],
) -> Result<()> {
    let (source, segs) = segments::discover(abs, tech.duration_ms, chapters);
    media_db::queries::segments::replace_for_file(conn, file_id, source, &segs)?;
    if !segs.is_empty() {
        tracing::debug!("{} skippable segments ({source}) for {}", segs.len(), abs.display());
    }
    Ok(())
}

/// Directory-level music metadata overrides: a music.toml anywhere above a
/// track (within its root) applies to everything beneath, nearest file
/// wins. Fields present override tags and path fallback; absent fields
/// resolve as usual.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirMusicMeta {
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// Set false when leading digits are part of the title ("30 Minutes"
    /// meditation tracks), not track numbers — the fallback title keeps
    /// them and no track number is inferred. Tag-supplied titles are
    /// unaffected.
    pub track_number_prefix: Option<bool>,
}

pub const MUSIC_META_FILE: &str = "music.toml";

fn ancestor_meta_paths(abs: &Path, root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut dir = abs.parent();
    while let Some(d) = dir {
        if !d.starts_with(root) {
            break;
        }
        out.push(d.join(MUSIC_META_FILE));
        dir = d.parent();
    }
    out // nearest first
}

/// The merged music.toml chain above a track: each field resolves to the
/// nearest ancestor that sets it, so a top-level file can declare
/// collection-wide values (artist, track_number_prefix) while deeper files
/// override only what differs (album).
pub fn nearest_music_meta(abs: &Path, root: &Path) -> Option<DirMusicMeta> {
    let mut merged: Option<DirMusicMeta> = None;
    for candidate in ancestor_meta_paths(abs, root) {
        let Ok(text) = media_db::sidecar::read_text_capped(&candidate, media_db::sidecar::MAX_TEXT) else {
            continue;
        };
        let parsed: DirMusicMeta = match toml::from_str(&text) {
            Ok(meta) => meta,
            Err(err) => {
                tracing::warn!("ignoring malformed {}: {err}", candidate.display());
                continue;
            }
        };
        let m = merged.get_or_insert_with(DirMusicMeta::default);
        if m.artist.is_none() {
            m.artist = parsed.artist;
        }
        if m.album_artist.is_none() {
            m.album_artist = parsed.album_artist;
        }
        if m.album.is_none() {
            m.album = parsed.album;
        }
        if m.genre.is_none() {
            m.genre = parsed.genre;
        }
        if m.track_number_prefix.is_none() {
            m.track_number_prefix = parsed.track_number_prefix;
        }
    }
    merged
}

/// Newest mtime among ancestor music.toml files (reconcile staleness).
pub fn music_meta_mtime(abs: &Path, root: &Path) -> Option<i64> {
    ancestor_meta_paths(abs, root)
        .iter()
        .filter_map(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        })
        .max()
}

/// Directory-level TV sidecars: series metadata in the series folder,
/// season metadata in a season subfolder (Kodi names).
pub const SHOW_NFO: &str = "tvshow.nfo";
pub const SEASON_NFO: &str = "season.nfo";

/// Ingest a directory-level TV sidecar into the tv_series / tv_seasons
/// decoration tables. Cheap enough to re-run on every watcher event and
/// every reconcile pass; a deleted sidecar leaves the stored row behind
/// (harmless — rows only decorate series that exist in tv_episodes).
pub fn ingest_tv_dir_nfo(conn: &Connection, root: &Root, rel: &str) -> Result<()> {
    let abs = Path::new(&root.path).join(rel);
    let Some(data) = nfo::read_file(&abs) else { return Ok(()) };
    let dir_name = |p: Option<&Path>| {
        p.and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
    };
    let dir = abs.parent();
    if abs.file_name().and_then(|n| n.to_str()) == Some(SHOW_NFO) {
        // The series name: nfo <title>, else the folder name.
        let Some(series) = data.title.clone().or_else(|| dir_name(dir)) else { return Ok(()) };
        tv::upsert_series(
            conn, &series, data.plot.as_deref(), data.rating, data.imdb_id.as_deref(),
        )?;
        tracing::debug!("ingested series metadata for {series:?} from {}/{rel}", root.path);
    } else {
        // season.nfo: series from <showtitle> (else the folder above the
        // season folder), season from <seasonnumber>/<season> (else the
        // first number in the season folder's name).
        let series = data.show_title.clone().or_else(|| dir_name(dir.and_then(Path::parent)));
        let season = data.season.or_else(|| {
            let name = dir_name(dir)?;
            let digits: String =
                name.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        let (Some(series), Some(season)) = (series, season) else {
            tracing::warn!("{}/{rel}: cannot tell which series/season this is; ignored", root.path);
            return Ok(());
        };
        tv::upsert_season(conn, &series, season, data.plot.as_deref())?;
        tracing::debug!("ingested season metadata for {series:?} S{season} from {}/{rel}", root.path);
    }
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
fn probe_or_default(ffprobe: &str, abs: &Path) -> (TechInfo, Option<String>, Vec<video::Chapter>) {
    match video::probe(ffprobe, abs) {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("ffprobe unavailable/failed ({err:#}); cataloguing without tech info");
            (TechInfo::default(), None, Vec::new())
        }
    }
}
