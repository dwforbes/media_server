use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::models::{BrowseItem, FileRow, MediaKind, Root, ServableFile, TechInfo};

// ---------------------------------------------------------------- roots

/// Make the roots table match the configured list. Removed roots cascade,
/// deleting their files and attribute rows.
pub fn sync_roots(conn: &Connection, configured: &[(String, MediaKind)]) -> Result<Vec<Root>> {
    let existing = list_roots(conn)?;
    for root in &existing {
        if !configured.iter().any(|(p, _)| *p == root.path) {
            tracing::info!("removing root no longer in config: {}", root.path);
            conn.execute("DELETE FROM files WHERE root_id = ?1", [root.id])?;
            conn.execute("DELETE FROM roots WHERE id = ?1", [root.id])?;
        }
    }
    for (path, kind) in configured {
        conn.execute(
            "INSERT INTO roots(path, kind) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind",
            params![path, kind.as_str()],
        )?;
    }
    list_roots(conn)
}

pub fn list_roots(conn: &Connection) -> Result<Vec<Root>> {
    let mut stmt = conn.prepare("SELECT id, path, kind FROM roots ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, path, kind) = row?;
        let kind = MediaKind::parse(&kind).context("bad kind in roots table")?;
        out.push(Root { id, path, kind });
    }
    Ok(out)
}

pub fn roots_of_kind(conn: &Connection, kind: MediaKind) -> Result<Vec<Root>> {
    Ok(list_roots(conn)?
        .into_iter()
        .filter(|r| r.kind == kind)
        .collect())
}

pub fn get_root(conn: &Connection, id: i64) -> Result<Option<Root>> {
    Ok(list_roots(conn)?.into_iter().find(|r| r.id == id))
}

// ---------------------------------------------------------------- scanner side

/// Everything known for one root, keyed by rel_path.
#[derive(Debug, Clone)]
pub struct KnownFile {
    pub id: i64,
    pub size: i64,
    pub mtime: i64,
    pub status: String,
    pub nfo_mtime: Option<i64>,
    pub edl_mtime: Option<i64>,
    pub art: Option<String>,
    pub updated_at: i64,
}

pub fn known_files(conn: &Connection, root_id: i64) -> Result<HashMap<String, KnownFile>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, id, size, mtime, status, nfo_mtime, edl_mtime, art, updated_at
         FROM files WHERE root_id = ?1",
    )?;
    let rows = stmt.query_map([root_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            KnownFile {
                id: r.get(1)?,
                size: r.get(2)?,
                mtime: r.get(3)?,
                status: r.get(4)?,
                nfo_mtime: r.get(5)?,
                edl_mtime: r.get(6)?,
                art: r.get(7)?,
                updated_at: r.get(8)?,
            },
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

/// One file's (id, size, mtime, status) by path, if catalogued.
pub fn lookup(
    conn: &Connection,
    root_id: i64,
    rel_path: &str,
) -> Result<Option<(i64, i64, i64, String)>> {
    conn.query_row(
        "SELECT id, size, mtime, status FROM files WHERE root_id = ?1 AND rel_path = ?2",
        params![root_id, rel_path],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
    .map_err(Into::into)
}

/// Insert a new file, or reset an existing one to pending after a change.
/// Returns the file id.
pub fn upsert_pending(
    conn: &Connection,
    root_id: i64,
    rel_path: &str,
    size: i64,
    mtime: i64,
    kind: MediaKind,
    mime: &str,
) -> Result<i64> {
    let existed = lookup(conn, root_id, rel_path)?.is_some();
    // added_at is set on insert only; the conflict branch leaves it alone.
    conn.execute(
        "INSERT INTO files(root_id, rel_path, size, mtime, kind, mime, status, updated_at, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', unixepoch(), unixepoch())
         ON CONFLICT(root_id, rel_path) DO UPDATE SET
             size = excluded.size, mtime = excluded.mtime, kind = excluded.kind,
             mime = excluded.mime, status = 'pending', updated_at = excluded.updated_at",
        params![root_id, rel_path, size, mtime, kind.as_str(), mime],
    )?;
    let id = conn.query_row(
        "SELECT id FROM files WHERE root_id = ?1 AND rel_path = ?2",
        params![root_id, rel_path],
        |r| r.get(0),
    )?;
    if !existed {
        inherit_added_at(conn, root_id, rel_path, id)?;
    }
    Ok(id)
}

/// `path/stem.ext` -> `path/stem`, if there is an extension.
fn strip_extension(rel_path: &str) -> Option<&str> {
    let name_start = rel_path.rfind('/').map_or(0, |i| i + 1);
    let dot = rel_path[name_start..].rfind('.')?;
    (dot > 0).then(|| &rel_path[..name_start + dot])
}

/// Catalogued siblings of `rel_path` with the same stem and a different
/// extension: `Heat (1995).mkv` beside `Heat (1995).mp4`. That is what a
/// container change (a remux) looks like from the catalog's side.
fn same_stem_siblings(conn: &Connection, root_id: i64, rel_path: &str) -> Result<Vec<(i64, i64)>> {
    let Some(stem) = strip_extension(rel_path) else { return Ok(Vec::new()) };
    let like = format!(
        "{}.%",
        stem.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    );
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, added_at FROM files
         WHERE root_id = ?1 AND rel_path LIKE ?2 ESCAPE '\\' AND rel_path != ?3",
    )?;
    let rows = stmt.query_map(params![root_id, like, rel_path], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, sibling, added_at) = row?;
        if strip_extension(&sibling) == Some(stem) {
            out.push((id, added_at));
        }
    }
    Ok(out)
}

/// A file that replaces a same-stem sibling (remuxed .mkv -> .mp4) is not
/// new to the library: it takes the sibling's added_at so Recently Added
/// does not fill with conversions.
fn inherit_added_at(conn: &Connection, root_id: i64, rel_path: &str, id: i64) -> Result<()> {
    if let Some(earliest) = same_stem_siblings(conn, root_id, rel_path)?
        .into_iter()
        .map(|(_, added_at)| added_at)
        .min()
    {
        conn.execute(
            "UPDATE files SET added_at = min(added_at, ?2) WHERE id = ?1",
            params![id, earliest],
        )?;
    }
    Ok(())
}

/// The vanished file at `rel_path` is being removed from the catalog: if
/// a same-stem sibling is catalogued, hand it the vanished file's added_at
/// first (covers the create-before-delete event order of a remux).
pub fn bequeath_added_at(conn: &Connection, root_id: i64, rel_path: &str) -> Result<()> {
    let Some((_, _, _, _)) = lookup(conn, root_id, rel_path)? else { return Ok(()) };
    let added_at: i64 = conn.query_row(
        "SELECT added_at FROM files WHERE root_id = ?1 AND rel_path = ?2",
        params![root_id, rel_path],
        |r| r.get(0),
    )?;
    for (sibling_id, _) in same_stem_siblings(conn, root_id, rel_path)? {
        conn.execute(
            "UPDATE files SET added_at = min(added_at, ?2) WHERE id = ?1",
            params![sibling_id, added_at],
        )?;
    }
    Ok(())
}

pub fn delete_file(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
    Ok(())
}

pub fn delete_by_path(conn: &Connection, root_id: i64, rel_path: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM files WHERE root_id = ?1 AND rel_path = ?2",
        params![root_id, rel_path],
    )?;
    Ok(())
}

/// Delete a path and anything under it (covers removed directories).
pub fn delete_by_prefix(conn: &Connection, root_id: i64, rel_path: &str) -> Result<usize> {
    let like = format!(
        "{}/%",
        rel_path.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    );
    let n = conn.execute(
        "DELETE FROM files WHERE root_id = ?1
         AND (rel_path = ?2 OR rel_path LIKE ?3 ESCAPE '\\')",
        params![root_id, rel_path, like],
    )?;
    Ok(n)
}

pub fn mark_error(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET status = 'error', updated_at = unixepoch() WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

pub fn get_file(conn: &Connection, id: i64) -> Result<Option<FileRow>> {
    conn.query_row(
        "SELECT id, root_id, rel_path, size, mtime, kind, mime, status FROM files WHERE id = ?1",
        [id],
        |r| {
            Ok(FileRow {
                id: r.get(0)?,
                root_id: r.get(1)?,
                rel_path: r.get(2)?,
                size: r.get(3)?,
                mtime: r.get(4)?,
                kind: MediaKind::parse(&r.get::<_, String>(5)?).unwrap_or(MediaKind::Movies),
                mime: r.get(6)?,
                status: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Files still pending or errored, for the reconcile pass to retry.
pub fn unfinished_files(conn: &Connection) -> Result<Vec<FileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, root_id, rel_path, size, mtime, kind, mime, status
         FROM files WHERE status != 'ready'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(FileRow {
            id: r.get(0)?,
            root_id: r.get(1)?,
            rel_path: r.get(2)?,
            size: r.get(3)?,
            mtime: r.get(4)?,
            kind: MediaKind::parse(&r.get::<_, String>(5)?).unwrap_or(MediaKind::Movies),
            mime: r.get(6)?,
            status: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Write technical attributes onto a files row (still leaves status alone;
/// callers finalize via the kind-specific functions in one transaction).
pub fn update_tech(conn: &Connection, id: i64, tech: &TechInfo) -> Result<()> {
    conn.execute(
        "UPDATE files SET container = ?2, duration_ms = ?3, width = ?4, height = ?5,
                          video_codec = ?6, audio_codec = ?7, audio_profile = ?8,
                          audio_bitrate = ?9, audio_sample_rate = ?10, audio_bit_depth = ?11,
                          audio_channels = ?12, frame_rate = ?13, updated_at = unixepoch()
         WHERE id = ?1",
        params![
            id,
            tech.container,
            tech.duration_ms,
            tech.width,
            tech.height,
            tech.video_codec,
            tech.audio_codec,
            tech.audio_profile,
            tech.audio_bitrate,
            tech.audio_sample_rate,
            tech.audio_bit_depth,
            tech.audio_channels,
            tech.frame_rate
        ],
    )?;
    Ok(())
}

/// Record the sidecar mtime observed during extraction (None = no sidecar),
/// so reconcile can spot sidecars that changed while the daemon was down.
pub fn record_nfo_mtime(conn: &Connection, id: i64, nfo_mtime: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE files SET nfo_mtime = ?2 WHERE id = ?1",
        params![id, nfo_mtime],
    )?;
    Ok(())
}

/// Record the .edl sidecar mtime observed during extraction (None = no
/// sidecar); the same staleness contract as record_nfo_mtime.
pub fn record_edl_mtime(conn: &Connection, id: i64, edl_mtime: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE files SET edl_mtime = ?2 WHERE id = ?1",
        params![id, edl_mtime],
    )?;
    Ok(())
}

/// Record artwork: a root-relative sidecar image path, the literal
/// "embedded" for pictures inside audio tags, or None.
pub fn record_art(conn: &Connection, id: i64, art: Option<&str>) -> Result<()> {
    conn.execute("UPDATE files SET art = ?2 WHERE id = ?1", params![id, art])?;
    Ok(())
}

pub fn mark_ready(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE files SET status = 'ready', updated_at = unixepoch() WHERE id = ?1",
        [id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- server side

pub fn servable(conn: &Connection, file_id: i64) -> Result<Option<ServableFile>> {
    conn.query_row(
        "SELECT r.path, f.rel_path, f.mime FROM files f
         JOIN roots r ON r.id = f.root_id
         WHERE f.id = ?1 AND f.status = 'ready'",
        [file_id],
        |row| {
            let root: String = row.get(0)?;
            let rel: String = row.get(1)?;
            Ok(ServableFile {
                abs_path: PathBuf::from(root).join(rel),
                mime: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Everything known about one item, for a detail view.
#[derive(Debug, Clone)]
pub struct ItemDetail {
    pub file_id: i64,
    pub kind: MediaKind,
    pub title: String,
    pub rel_path: String,
    pub size: i64,
    pub mime: String,
    pub container: Option<String>,
    pub duration_ms: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub frame_rate: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_profile: Option<String>,
    pub audio_bitrate: Option<i64>,
    pub audio_sample_rate: Option<i64>,
    pub audio_bit_depth: Option<i64>,
    pub audio_channels: Option<i64>,
    pub added_at_text: String,
    pub has_art: bool,
    pub year: Option<i64>,
    pub rating: Option<f64>,
    pub plot: Option<String>,
    pub genre: Option<String>,
    pub director: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<i64>,
    pub series: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub imdb_id: Option<String>,
    pub collection: Option<String>,
    /// The tag-level album artist, when set. The browse tree files tracks
    /// under COALESCE(album_artist, artist), so links back into it must
    /// key on this rather than the per-track artist.
    pub album_artist: Option<String>,
}

pub fn detail(conn: &Connection, file_id: i64) -> Result<Option<ItemDetail>> {
    conn.query_row(
        "SELECT f.id, f.kind, f.rel_path, f.size, f.mime, f.container, f.duration_ms,
                f.width, f.height, f.video_codec, f.audio_codec,
                datetime(f.added_at, 'unixepoch', 'localtime'), f.art IS NOT NULL,
                m.title, m.year, m.rating, m.plot, m.imdb_id,
                (SELECT group_concat(g.name, ', ') FROM movie_genres mg
                  JOIN genres g ON g.id = mg.genre_id WHERE mg.file_id = f.id),
                (SELECT group_concat(d.name, ', ') FROM movie_directors md
                  JOIN directors d ON d.id = md.director_id WHERE md.file_id = f.id),
                t.series, t.season, t.episode, t.title, t.plot,
                mu.title, mu.artist, mu.album, mu.track_no, mu.year,
                (SELECT group_concat(g.name, ', ') FROM track_genres tg
                  JOIN genres g ON g.id = tg.genre_id WHERE tg.file_id = f.id),
                t.rating, t.imdb_id, m.collection, mu.album_artist,
                f.audio_profile, f.audio_bitrate, f.audio_sample_rate, f.audio_bit_depth,
                f.audio_channels, f.frame_rate
         FROM files f
         LEFT JOIN movies m        ON m.file_id  = f.id
         LEFT JOIN tv_episodes t   ON t.file_id  = f.id
         LEFT JOIN music_tracks mu ON mu.file_id = f.id
         WHERE f.id = ?1 AND f.status = 'ready'",
        [file_id],
        |r| {
            let kind = MediaKind::parse(&r.get::<_, String>(1)?).unwrap_or(MediaKind::Movies);
            let rel_path: String = r.get(2)?;
            let movie_title: Option<String> = r.get(13)?;
            let ep_title: Option<String> = r.get(23)?;
            let track_title: Option<String> = r.get(25)?;
            let stem = rel_path
                .rsplit('/')
                .next()
                .unwrap_or(&rel_path)
                .rsplit_once('.')
                .map(|(s, _)| s.to_string())
                .unwrap_or_else(|| rel_path.clone());
            Ok(ItemDetail {
                file_id: r.get(0)?,
                kind,
                title: movie_title.or(track_title).or(ep_title).unwrap_or(stem),
                rel_path,
                size: r.get(3)?,
                mime: r.get(4)?,
                container: r.get(5)?,
                duration_ms: r.get(6)?,
                width: r.get(7)?,
                height: r.get(8)?,
                frame_rate: r.get(40)?,
                video_codec: r.get(9)?,
                audio_codec: r.get(10)?,
                audio_profile: r.get(35)?,
                audio_bitrate: r.get(36)?,
                audio_sample_rate: r.get(37)?,
                audio_bit_depth: r.get(38)?,
                audio_channels: r.get(39)?,
                added_at_text: r.get(11)?,
                has_art: r.get(12)?,
                year: r.get::<_, Option<i64>>(14)?.or(r.get(29)?),
                rating: r.get::<_, Option<f64>>(15)?.or(r.get(31)?),
                plot: r.get::<_, Option<String>>(16)?.or(r.get(24)?),
                imdb_id: r.get::<_, Option<String>>(17)?.or(r.get(32)?),
                collection: r.get::<_, Option<String>>(33)?.filter(|c| !c.is_empty()),
                genre: r.get::<_, Option<String>>(18)?.or(r.get(30)?),
                director: r.get(19)?,
                series: r.get(20)?,
                season: r.get(21)?,
                episode: r.get(22)?,
                artist: r.get(26)?,
                album: r.get(27)?,
                track_no: r.get(28)?,
                album_artist: r.get(34)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Where the artwork for one file lives.
pub enum ArtSource {
    /// A sidecar image file on disk.
    File(PathBuf),
    /// A picture embedded in this media file's tags.
    Embedded(PathBuf),
}

pub fn art_source(conn: &Connection, file_id: i64) -> Result<Option<ArtSource>> {
    conn.query_row(
        "SELECT r.path, f.rel_path, f.art FROM files f
         JOIN roots r ON r.id = f.root_id
         WHERE f.id = ?1 AND f.status = 'ready' AND f.art IS NOT NULL",
        [file_id],
        |row| {
            let root: String = row.get(0)?;
            let rel: String = row.get(1)?;
            let art: String = row.get(2)?;
            Ok(if art == "embedded" {
                ArtSource::Embedded(PathBuf::from(root).join(rel))
            } else {
                ArtSource::File(PathBuf::from(root).join(art))
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Shared SELECT + mapper used by the folder view: any ready file with its
/// kind-specific attributes left-joined in.
const ITEM_SELECT: &str = "
    SELECT f.id, f.kind, f.mime, f.size, f.duration_ms, f.width, f.height, f.rel_path,
           f.art IS NOT NULL,
           m.title, m.year,
           t.series, t.season, t.episode, t.title,
           mu.title, mu.artist, mu.album, mu.track_no, mu.year,
           m.rating, t.rating
    FROM files f
    LEFT JOIN movies m        ON m.file_id  = f.id
    LEFT JOIN tv_episodes t   ON t.file_id  = f.id
    LEFT JOIN music_tracks mu ON mu.file_id = f.id
    WHERE f.status = 'ready'";

fn item_from_row(row: &Row) -> rusqlite::Result<BrowseItem> {
    let kind = MediaKind::parse(&row.get::<_, String>(1)?).unwrap_or(MediaKind::Movies);
    let rel_path: String = row.get(7)?;
    let movie_title: Option<String> = row.get(9)?;
    let ep_title: Option<String> = row.get(14)?;
    let track_title: Option<String> = row.get(15)?;
    let stem = rel_path
        .rsplit('/')
        .next()
        .unwrap_or(&rel_path)
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| rel_path.clone());
    let title = movie_title
        .or(track_title)
        .or(ep_title)
        .unwrap_or(stem);
    let mut item = BrowseItem::new(row.get(0)?, kind, title, row.get(2)?, row.get(3)?);
    item.duration_ms = row.get(4)?;
    item.width = row.get(5)?;
    item.height = row.get(6)?;
    item.has_art = row.get(8)?;
    item.year = row.get::<_, Option<i64>>(10)?.or(row.get(19)?);
    item.series = row.get(11)?;
    item.season = row.get(12)?;
    item.episode = row.get(13)?;
    item.artist = row.get(16)?;
    item.album = row.get(17)?;
    item.track_no = row.get(18)?;
    item.rating = row.get::<_, Option<f64>>(20)?.or(row.get(21)?);
    Ok(item)
}

/// Immediate children of a directory in the folder view.
/// `dir` is "" for the root of a source root, otherwise "Sub/Path" with no
/// trailing slash. Returns (subdirectory names, files directly in the dir).
pub fn dir_children(
    conn: &Connection,
    root_id: i64,
    dir: &str,
) -> Result<(Vec<String>, Vec<BrowseItem>)> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    let sql = format!("{ITEM_SELECT} AND f.root_id = ?1 AND f.rel_path LIKE ?2 ESCAPE '\\'");
    let escaped = format!(
        "{}%",
        prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![root_id, escaped], |row| {
        let rel: String = row.get(7)?;
        Ok((rel, item_from_row(row)?))
    })?;

    let mut subdirs: Vec<String> = Vec::new();
    let mut items = Vec::new();
    for row in rows {
        let (rel, item) = row?;
        let remainder = &rel[prefix.len()..];
        match remainder.split_once('/') {
            Some((first, _)) => {
                if !subdirs.iter().any(|d| d == first) {
                    subdirs.push(first.to_string());
                }
            }
            None => items.push(item),
        }
    }
    subdirs.sort_unstable_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    items.sort_unstable_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    Ok((subdirs, items))
}

pub fn browse_item(conn: &Connection, file_id: i64) -> Result<Option<BrowseItem>> {
    let sql = format!("{ITEM_SELECT} AND f.id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([file_id], item_from_row)?;
    match rows.next() {
        Some(item) => Ok(Some(item?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_extension_handles_dotted_names_and_dirs() {
        assert_eq!(strip_extension("Movies/The.Mummy.2026.mkv"), Some("Movies/The.Mummy.2026"));
        assert_eq!(strip_extension("Heat (1995).mp4"), Some("Heat (1995)"));
        assert_eq!(strip_extension("dir.with.dots/noext"), None);
        assert_eq!(strip_extension(".hidden"), None);
    }

    #[test]
    fn a_remuxed_file_keeps_the_original_added_at() {
        let dir = std::env::temp_dir().join(format!("media-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::open::open_rw(&dir.join("t.db")).unwrap();
        let roots = sync_roots(&conn, &[("/m".to_string(), MediaKind::Movies)]).unwrap();
        let root = roots[0].id;
        let old = upsert_pending(&conn, root, "Heat (1995).mkv", 1, 1, MediaKind::Movies, "video/x-matroska").unwrap();
        conn.execute("UPDATE files SET added_at = 1000 WHERE id = ?1", [old]).unwrap();
        // Unrelated file with a longer name that also starts with the stem.
        upsert_pending(&conn, root, "Heat (1995) Extras.mkv", 1, 1, MediaKind::Movies, "video/x-matroska").unwrap();

        // Create-before-delete (event order): the new row inherits on insert.
        let new = upsert_pending(&conn, root, "Heat (1995).mp4", 2, 2, MediaKind::Movies, "video/mp4").unwrap();
        let added: i64 = conn.query_row("SELECT added_at FROM files WHERE id = ?1", [new], |r| r.get(0)).unwrap();
        assert_eq!(added, 1000);
        let extras: i64 = conn
            .query_row("SELECT added_at FROM files WHERE rel_path = 'Heat (1995) Extras.mkv'", [], |r| r.get(0))
            .unwrap();
        assert!(extras > 1000, "unrelated sibling untouched");

        // Delete-before-create: bequeath then delete, the later insert
        // finds nothing but keeps what it was given.
        conn.execute("UPDATE files SET added_at = 500 WHERE id = ?1", [old]).unwrap();
        bequeath_added_at(&conn, root, "Heat (1995).mkv").unwrap();
        let added: i64 = conn.query_row("SELECT added_at FROM files WHERE id = ?1", [new], |r| r.get(0)).unwrap();
        assert_eq!(added, 500);
        delete_file(&conn, old).unwrap();
        // Re-upserting the existing row never resets added_at.
        upsert_pending(&conn, root, "Heat (1995).mp4", 3, 3, MediaKind::Movies, "video/mp4").unwrap();
        let added: i64 = conn.query_row("SELECT added_at FROM files WHERE id = ?1", [new], |r| r.get(0)).unwrap();
        assert_eq!(added, 500);
    }
}
