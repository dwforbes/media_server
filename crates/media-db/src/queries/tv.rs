use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::models::{BrowseItem, MediaKind, TechInfo};
use crate::queries::{files, merge_renditions};

/// Write a fully-extracted episode in one transaction and mark it ready.
pub fn finalize_episode(
    conn: &mut Connection,
    file_id: i64,
    tech: &TechInfo,
    series: &str,
    season: i64,
    episode: i64,
    title: &str,
    plot: Option<&str>,
    rating: Option<f64>,
    imdb_id: Option<&str>,
) -> Result<()> {
    let tx = conn.transaction()?;
    files::update_tech(&tx, file_id, tech)?;
    tx.execute(
        "INSERT INTO tv_episodes(file_id, series, season, episode, title, plot, rating, imdb_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(file_id) DO UPDATE SET
             series = excluded.series, season = excluded.season,
             episode = excluded.episode, title = excluded.title, plot = excluded.plot,
             rating = excluded.rating, imdb_id = excluded.imdb_id",
        params![file_id, series, season, episode, title, plot, rating, imdb_id],
    )?;
    files::mark_ready(&tx, file_id)?;
    tx.commit()?;
    Ok(())
}

const EPISODE_SELECT: &str = "
    SELECT f.id, f.mime, f.size, f.duration_ms, f.width, f.height,
           t.series, t.season, t.episode, t.title, f.art IS NOT NULL, t.rating
    FROM tv_episodes t JOIN files f ON f.id = t.file_id
    WHERE f.status = 'ready'";

fn episode_from_row(row: &Row) -> rusqlite::Result<BrowseItem> {
    let mut item = BrowseItem::new(
        row.get(0)?,
        MediaKind::Tv,
        row.get(9)?,
        row.get(1)?,
        row.get(2)?,
    );
    item.duration_ms = row.get(3)?;
    item.width = row.get(4)?;
    item.height = row.get(5)?;
    item.series = row.get(6)?;
    item.season = row.get(7)?;
    item.episode = row.get(8)?;
    item.has_art = row.get(10)?;
    item.rating = row.get(11)?;
    Ok(item)
}

/// Every ready episode, ordered series/season/episode (playlist export).
pub fn all_episodes(conn: &Connection) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{EPISODE_SELECT} ORDER BY t.series COLLATE NOCASE, t.season, t.episode"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], episode_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Episodes above 1080p, ordered series/season/episode.
pub fn uhd(conn: &Connection) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{EPISODE_SELECT} AND (f.width > 1920 OR f.height > 1080)
         ORDER BY t.series COLLATE NOCASE, t.season, t.episode"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], episode_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// The most recently catalogued episodes, newest first (no rendition
/// merging: a REPACK arriving is itself a recent addition).
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<BrowseItem>> {
    let sql = format!("{EPISODE_SELECT} ORDER BY f.added_at DESC, f.id DESC LIMIT ?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([limit as i64], episode_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Series grouped case-insensitively (release names vary in casing):
/// (display name, art file id).
pub fn series_list(conn: &Connection) -> Result<Vec<(String, Option<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT t.series,
                (SELECT f2.id FROM tv_episodes t2 JOIN files f2 ON f2.id = t2.file_id
                  WHERE f2.status = 'ready' AND f2.art IS NOT NULL
                    AND t2.series = t.series COLLATE NOCASE LIMIT 1)
         FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready'
         GROUP BY t.series COLLATE NOCASE
         ORDER BY t.series COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Seasons of a series as (season, art file id): each season is
/// represented by the artwork of its first episode that has any (usually
/// the season or series poster every episode in the folder inherits).
pub fn seasons(conn: &Connection, series: &str) -> Result<Vec<(i64, Option<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT t.season,
                (SELECT f2.id FROM tv_episodes t2 JOIN files f2 ON f2.id = t2.file_id
                  WHERE f2.status = 'ready' AND f2.art IS NOT NULL
                    AND t2.series = ?1 COLLATE NOCASE AND t2.season = t.season
                  ORDER BY t2.episode LIMIT 1)
         FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND t.series = ?1 COLLATE NOCASE
         GROUP BY t.season ORDER BY t.season",
    )?;
    let rows = stmt.query_map([series], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// One episode's art to stand in for the whole series, first season first
/// (the same promotion `series_list` does, for a single series).
pub fn series_art(conn: &Connection, series: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT f.id FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND f.art IS NOT NULL
           AND t.series = ?1 COLLATE NOCASE
         ORDER BY t.season, t.episode LIMIT 1",
    )?;
    Ok(stmt.query_row([series], |r| r.get(0)).optional()?)
}

/// One episode's art to stand in for a season.
pub fn season_art(conn: &Connection, series: &str, season: i64) -> Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT f.id FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND f.art IS NOT NULL
           AND t.series = ?1 COLLATE NOCASE AND t.season = ?2
         ORDER BY t.episode LIMIT 1",
    )?;
    Ok(stmt.query_row(params![series, season], |r| r.get(0)).optional()?)
}

/// Every episode of a series across all seasons, ordered season/episode,
/// renditions merged per (season, episode) — the series overview grid.
pub fn series_episodes(conn: &Connection, series: &str) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{EPISODE_SELECT} AND t.series = ?1 COLLATE NOCASE ORDER BY t.season, t.episode"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![series], episode_from_row)?;
    Ok(merge_renditions(
        rows.collect::<Result<Vec<_>, _>>()?,
        |e| (e.season, e.episode.unwrap_or(-e.file_id)),
    ))
}

pub fn episodes(conn: &Connection, series: &str, season: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{EPISODE_SELECT} AND t.series = ?1 COLLATE NOCASE AND t.season = ?2 ORDER BY t.episode"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![series, season], episode_from_row)?;
    // Same episode number twice (a REPACK next to the original) = one
    // episode in several qualities; unnumbered episodes never merge.
    Ok(merge_renditions(
        rows.collect::<Result<Vec<_>, _>>()?,
        |e| e.episode.unwrap_or(-e.file_id),
    ))
}
