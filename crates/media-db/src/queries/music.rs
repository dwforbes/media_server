use anyhow::Result;
use rusqlite::{params, Connection, Row};

use crate::models::{BrowseItem, MediaKind, TechInfo};
use crate::queries::{ensure_genre, files};

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: String,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub year: Option<i64>,
    pub genres: Vec<String>,
}

/// Write a fully-extracted track in one transaction and mark it ready.
pub fn finalize_track(
    conn: &mut Connection,
    file_id: i64,
    tech: &TechInfo,
    meta: &TrackMeta,
) -> Result<()> {
    let tx = conn.transaction()?;
    files::update_tech(&tx, file_id, tech)?;
    tx.execute(
        "INSERT INTO music_tracks(file_id, title, artist, album_artist, album,
                                  track_no, disc_no, year)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(file_id) DO UPDATE SET
             title = excluded.title, artist = excluded.artist,
             album_artist = excluded.album_artist, album = excluded.album,
             track_no = excluded.track_no, disc_no = excluded.disc_no,
             year = excluded.year",
        params![
            file_id,
            meta.title,
            meta.artist,
            meta.album_artist,
            meta.album,
            meta.track_no,
            meta.disc_no,
            meta.year
        ],
    )?;
    tx.execute("DELETE FROM track_genres WHERE file_id = ?1", [file_id])?;
    for genre in &meta.genres {
        let gid = ensure_genre(&tx, genre)?;
        tx.execute(
            "INSERT OR IGNORE INTO track_genres(file_id, genre_id) VALUES (?1, ?2)",
            params![file_id, gid],
        )?;
    }
    files::mark_ready(&tx, file_id)?;
    tx.commit()?;
    Ok(())
}

const TRACK_SELECT: &str = "
    SELECT f.id, f.mime, f.size, f.duration_ms,
           mu.title, mu.artist, mu.album, mu.track_no, mu.year,
           (SELECT group_concat(g.name, ', ') FROM track_genres tg
             JOIN genres g ON g.id = tg.genre_id WHERE tg.file_id = f.id),
           f.art IS NOT NULL
    FROM music_tracks mu JOIN files f ON f.id = mu.file_id
    WHERE f.status = 'ready'";

fn track_from_row(row: &Row) -> rusqlite::Result<BrowseItem> {
    let mut item = BrowseItem::new(
        row.get(0)?,
        MediaKind::Music,
        row.get(4)?,
        row.get(1)?,
        row.get(2)?,
    );
    item.duration_ms = row.get(3)?;
    item.artist = row.get(5)?;
    item.album = row.get(6)?;
    item.track_no = row.get(7)?;
    item.year = row.get(8)?;
    item.genre = row.get(9)?;
    item.has_art = row.get(10)?;
    Ok(item)
}

/// Every ready track, ordered artist/album/track (playlist export).
pub fn all_tracks(conn: &Connection) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{TRACK_SELECT} ORDER BY {DISPLAY_ARTIST} COLLATE NOCASE,
         COALESCE(mu.album, 'Unknown Album') COLLATE NOCASE, mu.disc_no, mu.track_no"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], track_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// The most recently catalogued tracks, newest first.
pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<BrowseItem>> {
    let sql = format!("{TRACK_SELECT} ORDER BY f.added_at DESC, f.id DESC LIMIT ?1");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([limit as i64], track_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Display artist = album_artist when set, else track artist.
const DISPLAY_ARTIST: &str = "COALESCE(mu.album_artist, mu.artist, 'Unknown Artist')";

pub fn artists(conn: &Connection) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT {DISPLAY_ARTIST} AS a
         FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready' ORDER BY a COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// A file id in this album that has artwork, to decorate the container.
const ALBUM_ART: &str = "
    (SELECT f2.id FROM music_tracks mu2 JOIN files f2 ON f2.id = mu2.file_id
      WHERE f2.status = 'ready' AND f2.art IS NOT NULL
        AND COALESCE(mu2.album_artist, mu2.artist, 'Unknown Artist')
            = COALESCE(mu.album_artist, mu.artist, 'Unknown Artist')
        AND COALESCE(mu2.album, 'Unknown Album') = COALESCE(mu.album, 'Unknown Album')
      LIMIT 1)";

/// Albums of one artist: (album name, art file id).
pub fn albums_for_artist(
    conn: &Connection,
    artist: &str,
) -> Result<Vec<(String, Option<i64>)>> {
    let sql = format!(
        "SELECT COALESCE(mu.album, 'Unknown Album') AS al, {ALBUM_ART}
         FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready' AND {DISPLAY_ARTIST} = ?1
         GROUP BY al ORDER BY al COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([artist], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn tracks_for_album(conn: &Connection, artist: &str, album: &str) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{TRACK_SELECT} AND {DISPLAY_ARTIST} = ?1 AND COALESCE(mu.album, 'Unknown Album') = ?2
         ORDER BY mu.disc_no, mu.track_no, mu.title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artist, album], track_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// All albums: (display artist, album name, art file id).
pub fn albums(conn: &Connection) -> Result<Vec<(String, String, Option<i64>)>> {
    let sql = format!(
        "SELECT {DISPLAY_ARTIST} AS a, COALESCE(mu.album, 'Unknown Album') AS al, {ALBUM_ART}
         FROM music_tracks mu JOIN files f ON f.id = mu.file_id
         WHERE f.status = 'ready'
         GROUP BY a, al ORDER BY al COLLATE NOCASE, a COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn genres(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT g.id, g.name FROM genres g
         JOIN track_genres tg ON tg.genre_id = g.id
         JOIN files f ON f.id = tg.file_id
         WHERE f.status = 'ready' ORDER BY g.name",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn by_genre(conn: &Connection, genre_id: i64) -> Result<Vec<BrowseItem>> {
    let sql = format!(
        "{TRACK_SELECT} AND f.id IN (SELECT file_id FROM track_genres WHERE genre_id = ?1)
         ORDER BY mu.title COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([genre_id], track_from_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
