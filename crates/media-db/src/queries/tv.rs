use anyhow::Result;
use rusqlite::{params, Connection, Row};

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
) -> Result<()> {
    let tx = conn.transaction()?;
    files::update_tech(&tx, file_id, tech)?;
    tx.execute(
        "INSERT INTO tv_episodes(file_id, series, season, episode, title)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(file_id) DO UPDATE SET
             series = excluded.series, season = excluded.season,
             episode = excluded.episode, title = excluded.title",
        params![file_id, series, season, episode, title],
    )?;
    files::mark_ready(&tx, file_id)?;
    tx.commit()?;
    Ok(())
}

const EPISODE_SELECT: &str = "
    SELECT f.id, f.mime, f.size, f.duration_ms, f.width, f.height,
           t.series, t.season, t.episode, t.title, f.art IS NOT NULL
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
    Ok(item)
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

pub fn seasons(conn: &Connection, series: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.season FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND t.series = ?1 COLLATE NOCASE ORDER BY t.season",
    )?;
    let rows = stmt.query_map([series], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
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
