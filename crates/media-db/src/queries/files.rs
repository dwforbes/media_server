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
    pub art: Option<String>,
    pub updated_at: i64,
}

pub fn known_files(conn: &Connection, root_id: i64) -> Result<HashMap<String, KnownFile>> {
    let mut stmt = conn.prepare(
        "SELECT rel_path, id, size, mtime, status, nfo_mtime, art, updated_at
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
                art: r.get(6)?,
                updated_at: r.get(7)?,
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
    conn.execute(
        "INSERT INTO files(root_id, rel_path, size, mtime, kind, mime, status, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', unixepoch())
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
    Ok(id)
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
                          video_codec = ?6, audio_codec = ?7, updated_at = unixepoch()
         WHERE id = ?1",
        params![
            id,
            tech.container,
            tech.duration_ms,
            tech.width,
            tech.height,
            tech.video_codec,
            tech.audio_codec
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
           mu.title, mu.artist, mu.album, mu.track_no, mu.year
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
