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
    aired: Option<&str>,
) -> Result<()> {
    let tx = conn.transaction()?;
    files::update_tech(&tx, file_id, tech)?;
    tx.execute(
        "INSERT INTO tv_episodes(file_id, series, season, episode, title, plot, rating, imdb_id, aired)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(file_id) DO UPDATE SET
             series = excluded.series, season = excluded.season,
             episode = excluded.episode, title = excluded.title, plot = excluded.plot,
             rating = excluded.rating, imdb_id = excluded.imdb_id, aired = excluded.aired",
        params![file_id, series, season, episode, title, plot, rating, imdb_id, aired],
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
/// A series in the listing: (name, art file id, year).
pub type SeriesEntry = (String, Option<i64>, Option<i64>);
/// A season in a series' listing: (season, art file id, year).
pub type SeasonEntry = (i64, Option<i64>, Option<i64>);

/// Every series as (name, art file id, year): the art is promoted from
/// an episode, the year is the tvshow.nfo premiere, else the earliest
/// episode air date on hand.
pub fn series_list(conn: &Connection) -> Result<Vec<SeriesEntry>> {
    let mut stmt = conn.prepare(
        "SELECT t.series,
                (SELECT f2.id FROM tv_episodes t2 JOIN files f2 ON f2.id = t2.file_id
                  WHERE f2.status = 'ready' AND f2.art IS NOT NULL
                    AND t2.series = t.series COLLATE NOCASE LIMIT 1),
                COALESCE((SELECT substr(s.premiered, 1, 4) FROM tv_series s
                           WHERE s.name = t.series COLLATE NOCASE AND s.premiered IS NOT NULL),
                         MIN(substr(t.aired, 1, 4)))
         FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready'
         GROUP BY t.series COLLATE NOCASE
         ORDER BY t.series COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, year_from(r.get::<_, Option<String>>(2)?))))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// The year in a stored date's first four characters, if they are one.
fn year_from(date: Option<String>) -> Option<i64> {
    date.as_deref()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse::<i64>().ok())
        .filter(|y| (1801..2200).contains(y))
}

/// A series' year on its own (see series_list).
pub fn series_year(conn: &Connection, series: &str) -> Result<Option<i64>> {
    let date: Option<String> = conn.query_row(
        "SELECT COALESCE((SELECT substr(s.premiered, 1, 4) FROM tv_series s
                           WHERE s.name = ?1 COLLATE NOCASE AND s.premiered IS NOT NULL),
                         (SELECT MIN(substr(t.aired, 1, 4)) FROM tv_episodes t
                            JOIN files f ON f.id = t.file_id
                           WHERE f.status = 'ready' AND t.series = ?1 COLLATE NOCASE))",
        [series],
        |r| r.get(0),
    )?;
    Ok(year_from(date))
}

/// A season's year: its earliest episode air date on hand.
pub fn season_year(conn: &Connection, series: &str, season: i64) -> Result<Option<i64>> {
    let date: Option<String> = conn.query_row(
        "SELECT MIN(substr(t.aired, 1, 4)) FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND t.series = ?1 COLLATE NOCASE AND t.season = ?2",
        params![series, season],
        |r| r.get(0),
    )?;
    Ok(year_from(date))
}

/// Seasons of a series as (season, art file id, year): each season is
/// represented by the artwork of its first episode that has any (usually
/// the season or series poster every episode in the folder inherits),
/// and dated by its earliest episode air date.
pub fn seasons(conn: &Connection, series: &str) -> Result<Vec<SeasonEntry>> {
    let mut stmt = conn.prepare(
        "SELECT t.season,
                (SELECT f2.id FROM tv_episodes t2 JOIN files f2 ON f2.id = t2.file_id
                  WHERE f2.status = 'ready' AND f2.art IS NOT NULL
                    AND t2.series = ?1 COLLATE NOCASE AND t2.season = t.season
                  ORDER BY t2.episode LIMIT 1),
                MIN(substr(t.aired, 1, 4))
         FROM tv_episodes t JOIN files f ON f.id = t.file_id
         WHERE f.status = 'ready' AND t.series = ?1 COLLATE NOCASE
         GROUP BY t.season ORDER BY t.season",
    )?;
    let rows = stmt.query_map([series], |r| Ok((r.get(0)?, r.get(1)?, year_from(r.get::<_, Option<String>>(2)?))))?;
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

/// Series-level metadata ingested from a tvshow.nfo sidecar.
pub struct SeriesMeta {
    pub plot: Option<String>,
    pub rating: Option<f64>,
    pub imdb_id: Option<String>,
    /// First air date (ISO), from tvshow.nfo <premiered>.
    pub premiered: Option<String>,
}

pub fn upsert_series(
    conn: &Connection,
    name: &str,
    plot: Option<&str>,
    rating: Option<f64>,
    imdb_id: Option<&str>,
    premiered: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tv_series(name, plot, rating, imdb_id, premiered) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             plot = excluded.plot, rating = excluded.rating, imdb_id = excluded.imdb_id,
             premiered = excluded.premiered",
        params![name, plot, rating, imdb_id, premiered],
    )?;
    Ok(())
}

pub fn upsert_season(conn: &Connection, series: &str, season: i64, plot: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT INTO tv_seasons(series, season, plot) VALUES (?1, ?2, ?3)
         ON CONFLICT(series, season) DO UPDATE SET plot = excluded.plot",
        params![series, season, plot],
    )?;
    Ok(())
}

pub fn series_info(conn: &Connection, series: &str) -> Result<Option<SeriesMeta>> {
    let mut stmt =
        conn.prepare("SELECT plot, rating, imdb_id, premiered FROM tv_series WHERE name = ?1")?;
    Ok(stmt
        .query_row([series], |r| {
            Ok(SeriesMeta { plot: r.get(0)?, rating: r.get(1)?, imdb_id: r.get(2)?, premiered: r.get(3)? })
        })
        .optional()?)
}

pub fn season_info(conn: &Connection, series: &str, season: i64) -> Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT plot FROM tv_seasons WHERE series = ?1 AND season = ?2")?;
    let plot: Option<Option<String>> =
        stmt.query_row(params![series, season], |r| r.get(0)).optional()?;
    Ok(plot.flatten())
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
