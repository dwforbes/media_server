use anyhow::Result;
use rusqlite::{params, Connection, Row};

use crate::models::{BrowseItem, MediaKind, TechInfo};
use crate::queries::{ensure_genre, files, merge_renditions};

/// Same title + year = the same movie in multiple qualities.
fn merge_movies(items: Vec<BrowseItem>) -> Vec<BrowseItem> {
    merge_renditions(items, |m| (m.title.to_lowercase(), m.year))
}

/// Write a fully-extracted movie in one transaction and mark it ready.
#[allow(clippy::too_many_arguments)]
pub fn finalize_movie(
    conn: &mut Connection,
    file_id: i64,
    tech: &TechInfo,
    title: &str,
    sort_title: &str,
    year: Option<i64>,
    rating: Option<f64>,
    genres: &[String],
    directors: &[String],
) -> Result<()> {
    let tx = conn.transaction()?;
    files::update_tech(&tx, file_id, tech)?;
    tx.execute(
        "INSERT INTO movies(file_id, title, sort_title, year, rating) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(file_id) DO UPDATE SET
             title = excluded.title, sort_title = excluded.sort_title,
             year = excluded.year, rating = excluded.rating",
        params![file_id, title, sort_title, year, rating],
    )?;
    tx.execute("DELETE FROM movie_genres WHERE file_id = ?1", [file_id])?;
    for genre in genres {
        let gid = ensure_genre(&tx, genre)?;
        tx.execute(
            "INSERT OR IGNORE INTO movie_genres(file_id, genre_id) VALUES (?1, ?2)",
            params![file_id, gid],
        )?;
    }
    tx.execute("DELETE FROM movie_directors WHERE file_id = ?1", [file_id])?;
    for director in directors {
        let name = director.trim();
        tx.execute("INSERT OR IGNORE INTO directors(name) VALUES (?1)", [name])?;
        let did: i64 = tx.query_row(
            "SELECT id FROM directors WHERE name = ?1 COLLATE NOCASE",
            [name],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO movie_directors(file_id, director_id) VALUES (?1, ?2)",
            params![file_id, did],
        )?;
    }
    files::mark_ready(&tx, file_id)?;
    tx.commit()?;
    Ok(())
}

const MOVIE_SELECT: &str = "
    SELECT f.id, f.mime, f.size, f.duration_ms, f.width, f.height,
           m.title, m.year,
           (SELECT group_concat(g.name, ', ') FROM movie_genres mg
             JOIN genres g ON g.id = mg.genre_id WHERE mg.file_id = f.id),
           (SELECT group_concat(d.name, ', ') FROM movie_directors md
             JOIN directors d ON d.id = md.director_id WHERE md.file_id = f.id),
           f.art IS NOT NULL, m.rating
    FROM movies m JOIN files f ON f.id = m.file_id
    WHERE f.status = 'ready'";

fn movie_from_row(row: &Row) -> rusqlite::Result<BrowseItem> {
    let mut item = BrowseItem::new(
        row.get(0)?,
        MediaKind::Movies,
        row.get(6)?,
        row.get(1)?,
        row.get(2)?,
    );
    item.duration_ms = row.get(3)?;
    item.width = row.get(4)?;
    item.height = row.get(5)?;
    item.year = row.get(7)?;
    item.genre = row.get(8)?;
    item.director = row.get(9)?;
    item.has_art = row.get(10)?;
    item.rating = row.get(11)?;
    Ok(item)
}

pub fn all_movies(conn: &Connection) -> Result<Vec<BrowseItem>> {
    let sql = format!("{MOVIE_SELECT} ORDER BY m.sort_title COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

pub fn years(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT m.year FROM movies m JOIN files f ON f.id = m.file_id
         WHERE f.status = 'ready' AND m.year IS NOT NULL ORDER BY m.year DESC",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Decades (as their first year, e.g. 1980) with at least one ready movie,
/// newest first.
pub fn decades(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT (m.year / 10) * 10 AS decade
         FROM movies m JOIN files f ON f.id = m.file_id
         WHERE f.status = 'ready' AND m.year IS NOT NULL ORDER BY decade DESC",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn by_decade(conn: &Connection, decade: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{MOVIE_SELECT} AND m.year >= ?1 AND m.year < ?1 + 10
         ORDER BY m.sort_title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([decade], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

pub fn by_year(conn: &Connection, year: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!("{MOVIE_SELECT} AND m.year = ?1 ORDER BY m.sort_title COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([year], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

/// Genres that have at least one ready movie: (genre id, name).
pub fn genres(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT g.id, g.name FROM genres g
         JOIN movie_genres mg ON mg.genre_id = g.id
         JOIN files f ON f.id = mg.file_id
         WHERE f.status = 'ready' ORDER BY g.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Directors with at least one ready movie: (director id, name).
pub fn directors(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.id, d.name FROM directors d
         JOIN movie_directors md ON md.director_id = d.id
         JOIN files f ON f.id = md.file_id
         WHERE f.status = 'ready' ORDER BY d.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn by_director(conn: &Connection, director_id: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{MOVIE_SELECT} AND f.id IN (SELECT file_id FROM movie_directors WHERE director_id = ?1)
         ORDER BY m.year, m.sort_title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([director_id], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

/// Movies with lo <= rating < hi, best first.
pub fn by_rating(conn: &Connection, lo: f64, hi: f64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{MOVIE_SELECT} AND m.rating >= ?1 AND m.rating < ?2
         ORDER BY m.rating DESC, m.sort_title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![lo, hi], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

pub fn unrated(conn: &Connection) -> Result<Vec<BrowseItem>> {
    let sql = format!("{MOVIE_SELECT} AND m.rating IS NULL ORDER BY m.sort_title COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}

pub fn by_genre(conn: &Connection, genre_id: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{MOVIE_SELECT} AND f.id IN (SELECT file_id FROM movie_genres WHERE genre_id = ?1)
         ORDER BY m.sort_title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([genre_id], movie_from_row)?;
    Ok(merge_movies(rows.collect::<Result<Vec<_>, _>>()?))
}
