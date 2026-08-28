//! Leaf-count queries for the browse tree — one grouped or scalar query
//! per container view, mirroring each listing's grouping and rendition
//! merging so the number matches what the page shows. Callers cache the
//! results; nothing here is called per row.

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::models::MediaKind;
use crate::queries::music::DISPLAY_ARTIST;

/// A movie in several qualities counts once — the same (title, year) key
/// the listings' rendition merge uses. SQL lower() differs from Rust
/// lowercasing only for non-ASCII, where an off-by-one count is harmless.
const MERGED_MOVIES: &str = "COUNT(DISTINCT lower(m.title) || char(31) || COALESCE(m.year, ''))";
const READY_MOVIES: &str = "FROM movies m JOIN files f ON f.id = m.file_id WHERE f.status = 'ready'";

fn one(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<i64> {
    Ok(conn.query_row(sql, params, |r| r.get(0))?)
}

fn pairs<K: rusqlite::types::FromSql>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(K, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn movies_total(conn: &Connection) -> Result<i64> {
    one(conn, &format!("SELECT {MERGED_MOVIES} {READY_MOVIES}"), [])
}

pub fn movies_uhd(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!("SELECT {MERGED_MOVIES} {READY_MOVIES} AND (f.width > 1920 OR f.height > 1080)"),
        [],
    )
}

/// Movies that appear under By Year / By Decade (a year is known).
pub fn movies_dated(conn: &Connection) -> Result<i64> {
    one(conn, &format!("SELECT {MERGED_MOVIES} {READY_MOVIES} AND m.year IS NOT NULL"), [])
}

pub fn movies_with_genre(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!(
            "SELECT {MERGED_MOVIES} {READY_MOVIES}
             AND f.id IN (SELECT file_id FROM movie_genres)"
        ),
        [],
    )
}

pub fn movies_with_director(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!(
            "SELECT {MERGED_MOVIES} {READY_MOVIES}
             AND f.id IN (SELECT file_id FROM movie_directors)"
        ),
        [],
    )
}

/// Members of the franchises the By Franchise view lists (two or more
/// library entries, like `movies::franchises`).
pub fn movies_in_franchises(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!(
            "SELECT {MERGED_MOVIES} {READY_MOVIES}
             AND m.collection COLLATE NOCASE IN (
                 SELECT m2.collection FROM movies m2 JOIN files f2 ON f2.id = m2.file_id
                 WHERE f2.status = 'ready'
                   AND m2.collection IS NOT NULL AND m2.collection != ''
                 GROUP BY m2.collection COLLATE NOCASE HAVING count(*) >= 2)"
        ),
        [],
    )
}

pub fn movies_rated(conn: &Connection, lo: f64, hi: f64) -> Result<i64> {
    one(
        conn,
        &format!("SELECT {MERGED_MOVIES} {READY_MOVIES} AND m.rating >= ?1 AND m.rating < ?2"),
        params![lo, hi],
    )
}

pub fn movies_unrated(conn: &Connection) -> Result<i64> {
    one(conn, &format!("SELECT {MERGED_MOVIES} {READY_MOVIES} AND m.rating IS NULL"), [])
}

pub fn movies_by_year(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT m.year, {MERGED_MOVIES} {READY_MOVIES} AND m.year IS NOT NULL
             GROUP BY m.year"
        ),
        [],
    )
}

pub fn movies_by_decade(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT (m.year / 10) * 10 AS decade, {MERGED_MOVIES}
             {READY_MOVIES} AND m.year IS NOT NULL GROUP BY decade"
        ),
        [],
    )
}

pub fn movies_by_genre(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT mg.genre_id, {MERGED_MOVIES}
             FROM movie_genres mg
             JOIN movies m ON m.file_id = mg.file_id
             JOIN files f ON f.id = m.file_id
             WHERE f.status = 'ready' GROUP BY mg.genre_id"
        ),
        [],
    )
}

pub fn movies_by_director(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT md.director_id, {MERGED_MOVIES}
             FROM movie_directors md
             JOIN movies m ON m.file_id = md.file_id
             JOIN files f ON f.id = m.file_id
             WHERE f.status = 'ready' GROUP BY md.director_id"
        ),
        [],
    )
}

pub fn movies_by_franchise(conn: &Connection) -> Result<Vec<(String, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT m.collection, {MERGED_MOVIES}
             {READY_MOVIES} AND m.collection IS NOT NULL AND m.collection != ''
             GROUP BY m.collection COLLATE NOCASE HAVING count(*) >= 2"
        ),
        [],
    )
}

pub fn tracks_total(conn: &Connection) -> Result<i64> {
    one(
        conn,
        "SELECT COUNT(*) FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready'",
        [],
    )
}

pub fn tracks_with_genre(conn: &Connection) -> Result<i64> {
    one(
        conn,
        "SELECT COUNT(*) FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready' AND f.id IN (SELECT file_id FROM track_genres)",
        [],
    )
}

pub fn tracks_by_artist(conn: &Connection) -> Result<Vec<(String, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT {DISPLAY_ARTIST} AS a, COUNT(*)
             FROM music_tracks mu JOIN files f ON f.id = mu.file_id
             WHERE f.status = 'ready' GROUP BY a"
        ),
        [],
    )
}

pub fn tracks_by_album_for_artist(conn: &Connection, artist: &str) -> Result<Vec<(String, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT COALESCE(mu.album, 'Unknown Album') AS al, COUNT(*)
             FROM music_tracks mu JOIN files f ON f.id = mu.file_id
             WHERE f.status = 'ready' AND {DISPLAY_ARTIST} = ?1 GROUP BY al"
        ),
        [artist],
    )
}

/// (display artist, album, track count) — the All Albums view's grouping.
pub fn tracks_by_album(conn: &Connection) -> Result<Vec<(String, String, i64)>> {
    let sql = format!(
        "SELECT {DISPLAY_ARTIST} AS a, COALESCE(mu.album, 'Unknown Album') AS al, COUNT(*)
         FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready' GROUP BY a, al"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn tracks_by_genre(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        "SELECT tg.genre_id, COUNT(*) FROM track_genres tg JOIN files f ON f.id = tg.file_id
         WHERE f.status = 'ready' GROUP BY tg.genre_id",
        [],
    )
}

/// An episode in several qualities counts once — the (season, episode)
/// key the listings' rendition merge uses.
const MERGED_EPISODES: &str = "COUNT(DISTINCT t.season || char(31) || t.episode)";
const READY_EPISODES: &str =
    "FROM tv_episodes t JOIN files f ON f.id = t.file_id WHERE f.status = 'ready'";

pub fn episodes_total(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!(
            "SELECT COUNT(DISTINCT lower(t.series) || char(31) || t.season || char(31) || t.episode)
             {READY_EPISODES}"
        ),
        [],
    )
}

pub fn episodes_uhd(conn: &Connection) -> Result<i64> {
    one(
        conn,
        &format!(
            "SELECT COUNT(DISTINCT lower(t.series) || char(31) || t.season || char(31) || t.episode)
             {READY_EPISODES} AND (f.width > 1920 OR f.height > 1080)"
        ),
        [],
    )
}

pub fn episodes_by_series(conn: &Connection) -> Result<Vec<(String, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT t.series, {MERGED_EPISODES} {READY_EPISODES}
             GROUP BY t.series COLLATE NOCASE"
        ),
        [],
    )
}

pub fn episodes_by_season(conn: &Connection, series: &str) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        &format!(
            "SELECT t.season, COUNT(DISTINCT t.episode)
             {READY_EPISODES} AND t.series = ?1 COLLATE NOCASE GROUP BY t.season"
        ),
        [series],
    )
}

/// Catalogued files of one kind — the Folders views count files as they
/// are on disk, no rendition merging.
pub fn files_total(conn: &Connection, kind: MediaKind) -> Result<i64> {
    one(
        conn,
        "SELECT COUNT(*) FROM files WHERE status = 'ready' AND kind = ?1",
        [kind.as_str()],
    )
}

pub fn files_per_root(conn: &Connection, kind: MediaKind) -> Result<Vec<(i64, i64)>> {
    pairs(
        conn,
        "SELECT root_id, COUNT(*) FROM files WHERE status = 'ready' AND kind = ?1
         GROUP BY root_id",
        [kind.as_str()],
    )
}

/// Every ready rel_path under a root — the caller aggregates per-subtree
/// counts for the Folders view in one pass.
pub fn rel_paths(conn: &Connection, root_id: i64) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT rel_path FROM files WHERE root_id = ?1 AND status = 'ready'")?;
    let rows = stmt.query_map([root_id], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
