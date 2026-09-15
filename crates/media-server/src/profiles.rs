//! Viewer profiles and what each has watched.
//!
//! A LAN media server for a household needs "who's watching?", not
//! accounts: a profile is a name, chosen from a tile page and remembered
//! in a cookie, with no password behind it. Against the profile the
//! player reports where a viewer left off and what they finished, so
//! the pages can show a "continue watching" list, a seen tick per
//! episode and the point to resume from.
//!
//! The state lives in its own SQLite file beside the catalog (the
//! scanner is the catalog's only writer; the server owns this one).
//! Watch rows key on the program, not the file — an episode by series,
//! season and number, a movie by title and year — so a remux or a
//! re-catalogue keeps a viewer's history.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use media_db::MediaKind;
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS profiles (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS profiles_name ON profiles(name COLLATE NOCASE);
CREATE TABLE IF NOT EXISTS watch (
    profile_id  INTEGER NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    file_id     INTEGER NOT NULL,
    position_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    watched     INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    dismissed   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (profile_id, key)
);
CREATE INDEX IF NOT EXISTS watch_recent ON watch(profile_id, updated_at DESC);
"#;

/// Longest profile name accepted.
pub const MAX_NAME: usize = 40;

/// Past this share of the running time, leaving counts as finished:
/// viewers rarely sit through the credits.
const FINISHED_FRACTION: f64 = 0.95;

/// Open (creating if needed) the profiles database beside the catalog.
pub fn open(catalog_path: &Path) -> Result<Connection> {
    let path = catalog_path.with_file_name("profiles.db");
    let conn = Connection::open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.execute_batch(SCHEMA)?;
    upgrade(&conn)?;
    tracing::info!("profiles database {}", path.display());
    Ok(conn)
}

/// Columns added since the first release, for a database created before
/// them (CREATE IF NOT EXISTS leaves an existing table alone).
fn upgrade(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(watch)")?;
    let columns: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<_, _>>()?;
    if !columns.iter().any(|c| c == "dismissed") {
        conn.execute_batch("ALTER TABLE watch ADD COLUMN dismissed INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: i64,
    pub name: String,
}

/// One program's state for one profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRow {
    pub key: String,
    /// The file the state was last recorded against (for a play link).
    pub file_id: i64,
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
    pub watched: bool,
    pub updated_at: i64,
    /// Taken off the continue-watching list by hand; any new report
    /// puts it back.
    pub dismissed: bool,
}

impl WatchRow {
    /// Part-way through: something to pick up again. Finishing and the
    /// seen tick both zero the position, so a seen program with a
    /// position is one being watched again.
    pub fn in_progress(&self) -> bool {
        self.position_ms > 0
    }
}

/// The identity a watch row keys on. None for music, which isn't tracked.
pub fn key(
    kind: MediaKind,
    file_id: i64,
    series: Option<&str>,
    season: Option<i64>,
    episode: Option<i64>,
    title: &str,
    year: Option<i64>,
) -> Option<String> {
    match kind {
        MediaKind::Tv => match (series, season, episode) {
            (Some(series), Some(season), Some(episode)) if episode > 0 => {
                Some(format!("tv|{}|{season}|{episode}", series.trim().to_lowercase()))
            }
            // An unnumbered episode has only its file to go by.
            _ => Some(format!("f|{file_id}")),
        },
        MediaKind::Movies => Some(format!(
            "mv|{}|{}",
            undecorated_movie_title(title, year).trim().to_lowercase(),
            year.map(|y| y.to_string()).unwrap_or_default()
        )),
        MediaKind::Music => None,
    }
}

/// The catalog title behind a movie's display title. Browse listings
/// decorate movie titles for clients that show nothing but a name —
/// "The Game (1997)" in most views, "8.5 · The Game" under By Rating
/// (see tree.rs) — and a key must not depend on which view an item was
/// seen in.
fn undecorated_movie_title(title: &str, year: Option<i64>) -> &str {
    let mut t = title.trim();
    if let Some(year) = year {
        if let Some(base) = t.strip_suffix(&format!(" ({year})")) {
            t = base;
        }
    }
    // A rating prefix: digits, a point, one digit, " · ".
    if let Some((head, tail)) = t.split_once(" · ") {
        let looks_like_rating = head.len() >= 3
            && head.ends_with(|c: char| c.is_ascii_digit())
            && head[..head.len() - 1].ends_with('.')
            && head[..head.len() - 2].bytes().all(|b| b.is_ascii_digit())
            && !head[..head.len() - 2].is_empty();
        if looks_like_rating {
            t = tail;
        }
    }
    t
}

pub fn item_key(item: &media_db::BrowseItem) -> Option<String> {
    key(item.kind, item.file_id, item.series.as_deref(), item.season, item.episode, &item.title, item.year)
}

pub fn detail_key(d: &media_db::queries::files::ItemDetail) -> Option<String> {
    key(d.kind, d.file_id, d.series.as_deref(), d.season, d.episode, &d.title, d.year)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn list(conn: &Connection) -> Result<Vec<Profile>> {
    let mut stmt = conn.prepare("SELECT id, name FROM profiles ORDER BY created_at, id")?;
    let rows = stmt.query_map([], |r| Ok(Profile { id: r.get(0)?, name: r.get(1)? }))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Profile>> {
    Ok(conn
        .query_row("SELECT id, name FROM profiles WHERE id = ?1", [id], |r| {
            Ok(Profile { id: r.get(0)?, name: r.get(1)? })
        })
        .optional()?)
}

/// A new profile. Names are trimmed, bounded, and unique regardless of
/// case; the error text is fit to show the viewer.
pub fn add(conn: &Connection, name: &str) -> Result<i64> {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if name.is_empty() {
        bail!("A profile needs a name.");
    }
    if name.chars().count() > MAX_NAME {
        bail!("That name is too long ({MAX_NAME} characters at most).");
    }
    let taken: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM profiles WHERE name = ?1 COLLATE NOCASE)",
        [&name],
        |r| r.get(0),
    )?;
    if taken {
        bail!("There is already a profile called {name}.");
    }
    conn.execute(
        "INSERT INTO profiles(name, created_at) VALUES (?1, ?2)",
        params![name, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
    Ok(())
}

/// The player reported a position (the viewer left, or moved on). Past
/// FINISHED_FRACTION of the running time it counts as finished, like a
/// natural end would; a program already seen stays seen.
pub fn set_position(
    conn: &Connection,
    profile_id: i64,
    key: &str,
    file_id: i64,
    position_ms: i64,
    duration_ms: Option<i64>,
) -> Result<()> {
    let finished = duration_ms
        .filter(|d| *d > 0)
        .is_some_and(|d| position_ms as f64 >= d as f64 * FINISHED_FRACTION);
    if finished {
        return set_finished(conn, profile_id, key, file_id, duration_ms);
    }
    conn.execute(
        "INSERT INTO watch(profile_id, key, file_id, position_ms, duration_ms, watched, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
         ON CONFLICT(profile_id, key) DO UPDATE SET
             file_id = excluded.file_id, position_ms = excluded.position_ms,
             duration_ms = COALESCE(excluded.duration_ms, duration_ms),
             updated_at = excluded.updated_at, dismissed = 0",
        params![profile_id, key, file_id, position_ms.max(0), duration_ms, now()],
    )?;
    Ok(())
}

/// The program ran to its end: seen, and nothing to resume.
pub fn set_finished(
    conn: &Connection,
    profile_id: i64,
    key: &str,
    file_id: i64,
    duration_ms: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO watch(profile_id, key, file_id, position_ms, duration_ms, watched, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, 1, ?5)
         ON CONFLICT(profile_id, key) DO UPDATE SET
             file_id = excluded.file_id, position_ms = 0, watched = 1,
             duration_ms = COALESCE(excluded.duration_ms, duration_ms),
             updated_at = excluded.updated_at, dismissed = 0",
        params![profile_id, key, file_id, duration_ms, now()],
    )?;
    Ok(())
}

/// The seen tick, set by hand. Unticking also forgets the position, so
/// the program starts over next time.
pub fn set_seen(conn: &Connection, profile_id: i64, key: &str, file_id: i64, seen: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO watch(profile_id, key, file_id, position_ms, watched, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?5)
         ON CONFLICT(profile_id, key) DO UPDATE SET
             file_id = excluded.file_id, position_ms = 0, watched = excluded.watched,
             updated_at = excluded.updated_at, dismissed = 0",
        params![profile_id, key, file_id, seen as i64, now()],
    )?;
    Ok(())
}

fn row_from(r: &rusqlite::Row) -> rusqlite::Result<WatchRow> {
    Ok(WatchRow {
        key: r.get(0)?,
        file_id: r.get(1)?,
        position_ms: r.get(2)?,
        duration_ms: r.get(3)?,
        watched: r.get::<_, i64>(4)? != 0,
        updated_at: r.get(5)?,
        dismissed: r.get::<_, i64>(6)? != 0,
    })
}

const ROW_SELECT: &str =
    "SELECT key, file_id, position_ms, duration_ms, watched, updated_at, dismissed FROM watch";

/// Take a program off the continue-watching list. The position and the
/// seen tick stay: this hides, it does not forget.
pub fn dismiss(conn: &Connection, profile_id: i64, key: &str) -> Result<()> {
    conn.execute(
        "UPDATE watch SET dismissed = 1 WHERE profile_id = ?1 AND key = ?2",
        params![profile_id, key],
    )?;
    Ok(())
}

/// The same for a whole series: every episode's row, so which of them
/// counts as latest no longer matters. Playing any episode again clears
/// that episode's flag and makes it the latest.
pub fn dismiss_series(conn: &Connection, profile_id: i64, series: &str) -> Result<()> {
    let prefix = format!("tv|{}|", series.trim().to_lowercase());
    conn.execute(
        "UPDATE watch SET dismissed = 1 WHERE profile_id = ?1 AND substr(key, 1, length(?2)) = ?2",
        params![profile_id, prefix],
    )?;
    Ok(())
}

pub fn one(conn: &Connection, profile_id: i64, key: &str) -> Result<Option<WatchRow>> {
    Ok(conn
        .query_row(
            &format!("{ROW_SELECT} WHERE profile_id = ?1 AND key = ?2"),
            params![profile_id, key],
            row_from,
        )
        .optional()?)
}

/// The state of each of `keys` (absent = never touched).
pub fn states(conn: &Connection, profile_id: i64, keys: &[String]) -> Result<HashMap<String, WatchRow>> {
    let mut out = HashMap::new();
    // Bound the variable count per statement; a search page can list
    // hundreds of programs.
    for chunk in keys.chunks(200) {
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!("{ROW_SELECT} WHERE profile_id = ? AND key IN ({marks})");
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&profile_id];
        params.extend(chunk.iter().map(|k| k as &dyn rusqlite::ToSql));
        let rows = stmt.query_map(params.as_slice(), row_from)?;
        for row in rows {
            let row = row?;
            out.insert(row.key.clone(), row);
        }
    }
    Ok(out)
}

/// Latest activity first. Within one second a live row outranks a
/// dismissed one: a report clears the flag, a dismissal leaves the time.
pub fn recent(conn: &Connection, profile_id: i64, limit: usize) -> Result<Vec<WatchRow>> {
    let mut stmt = conn.prepare(&format!(
        "{ROW_SELECT} WHERE profile_id = ?1 ORDER BY updated_at DESC, dismissed ASC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![profile_id, limit as i64], row_from)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Every row for one series (by key prefix), for "up next".
pub fn series_rows(conn: &Connection, profile_id: i64, series: &str) -> Result<Vec<WatchRow>> {
    let prefix = format!("tv|{}|", series.trim().to_lowercase());
    let mut stmt = conn.prepare(&format!(
        "{ROW_SELECT} WHERE profile_id = ?1 AND substr(key, 1, length(?2)) = ?2"
    ))?;
    let rows = stmt.query_map(params![profile_id, prefix], row_from)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// The profile named by the request's cookie, if any.
pub fn cookie_profile_id(headers: &axum::http::HeaderMap) -> Option<i64> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE).then(|| value.trim().parse::<i64>().ok()).flatten()
        })
        .next()
}

pub const COOKIE: &str = "profile";

/// A year-long cookie, plain HTTP and HTTPS alike (the pages are served
/// on both), never readable by script.
pub fn set_cookie(id: Option<i64>) -> String {
    match id {
        Some(id) => format!("{COOKIE}={id}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly"),
        None => format!("{COOKIE}=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly"),
    }
}

/// "just now", "3 h ago", "yesterday", "12 days ago", "2026-06-01".
pub fn when(updated_at: i64) -> String {
    let age = now() - updated_at;
    if age < 90 {
        "just now".into()
    } else if age < 3600 {
        format!("{} min ago", age / 60)
    } else if age < 86_400 {
        format!("{} h ago", age / 3600)
    } else if age < 2 * 86_400 {
        "yesterday".into()
    } else if age < 30 * 86_400 {
        format!("{} days ago", age / 86_400)
    } else {
        ymd(updated_at)
    }
}

/// Civil date of a unix timestamp (UTC — the day is what matters).
fn ymd(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    #[test]
    fn a_first_release_database_gains_the_dismissed_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&SCHEMA.replace("    dismissed   INTEGER NOT NULL DEFAULT 0,\n", "")).unwrap();
        upgrade(&conn).unwrap();
        upgrade(&conn).unwrap();   // idempotent
        let p = add(&conn, "x").unwrap();
        set_seen(&conn, p, "k", 1, true).unwrap();
        assert!(!one(&conn, p, "k").unwrap().unwrap().dismissed);
    }

    #[test]
    fn keys_identify_programs_not_files() {
        let a = key(MediaKind::Tv, 1, Some("Curb Your Enthusiasm"), Some(2), Some(8), "x", None);
        let b = key(MediaKind::Tv, 99, Some("curb your enthusiasm "), Some(2), Some(8), "y", None);
        assert_eq!(a, b);
        assert_eq!(key(MediaKind::Tv, 7, Some("Show"), Some(1), None, "Special", None).as_deref(), Some("f|7"));
        assert_eq!(key(MediaKind::Movies, 3, None, None, None, "Heat", Some(1995)).as_deref(), Some("mv|heat|1995"));
        // The same movie however a listing dressed its title.
        for shown in ["Heat (1995)", "8.3 · Heat", "8.3 · Heat (1995)", "  Heat "] {
            assert_eq!(key(MediaKind::Movies, 3, None, None, None, shown, Some(1995)).as_deref(), Some("mv|heat|1995"), "{shown}");
        }
        assert_eq!(key(MediaKind::Movies, 3, None, None, None, "2001 (1968)", None).as_deref(), Some("mv|2001 (1968)|"));
        assert_eq!(key(MediaKind::Movies, 3, None, None, None, "Se7en · Remastered", None).as_deref(), Some("mv|se7en · remastered|"));
        assert_eq!(key(MediaKind::Music, 3, None, None, None, "Song", None), None);
    }

    #[test]
    fn positions_finish_near_the_end_and_ticks_reset() {
        let conn = db();
        let p = add(&conn, " Dennis ").unwrap();
        assert!(add(&conn, "dennis").is_err(), "names are unique regardless of case");
        assert!(add(&conn, "  ").is_err());

        set_position(&conn, p, "tv|s|1|1", 10, 600_000, Some(1_500_000)).unwrap();
        let row = one(&conn, p, "tv|s|1|1").unwrap().unwrap();
        assert!(row.in_progress() && row.position_ms == 600_000 && row.file_id == 10);

        // Leaving during the credits is finishing.
        set_position(&conn, p, "tv|s|1|1", 11, 1_450_000, Some(1_500_000)).unwrap();
        let row = one(&conn, p, "tv|s|1|1").unwrap().unwrap();
        assert!(row.watched && row.position_ms == 0 && row.file_id == 11);

        // Rewatching part-way keeps the tick but records the spot.
        set_position(&conn, p, "tv|s|1|1", 11, 300_000, None).unwrap();
        let row = one(&conn, p, "tv|s|1|1").unwrap().unwrap();
        assert!(row.watched && row.position_ms == 300_000 && row.duration_ms == Some(1_500_000));

        set_seen(&conn, p, "tv|s|1|1", 11, false).unwrap();
        let row = one(&conn, p, "tv|s|1|1").unwrap().unwrap();
        assert!(!row.watched && row.position_ms == 0);

        set_seen(&conn, p, "tv|s|1|2", 12, true).unwrap();
        let states = states(&conn, p, &["tv|s|1|1".into(), "tv|s|1|2".into(), "tv|s|1|3".into()]).unwrap();
        assert_eq!(states.len(), 2);
        assert!(states["tv|s|1|2"].watched);
        assert_eq!(series_rows(&conn, p, "S").unwrap().len(), 2);
        // Both rows landed within the same second: age one to order them.
        conn.execute("UPDATE watch SET updated_at = updated_at - 60 WHERE key = 'tv|s|1|1'", []).unwrap();
        assert_eq!(recent(&conn, p, 1).unwrap()[0].key, "tv|s|1|2");

        // Dismissing a series flags every episode; the next report on one
        // clears it, and within the same second that live row ranks first.
        dismiss_series(&conn, p, "S").unwrap();
        assert!(one(&conn, p, "tv|s|1|1").unwrap().unwrap().dismissed);
        assert!(one(&conn, p, "tv|s|1|2").unwrap().unwrap().dismissed);
        conn.execute("UPDATE watch SET updated_at = ?1", [now()]).unwrap();
        set_position(&conn, p, "tv|s|1|1", 11, 1000, None).unwrap();
        assert!(!one(&conn, p, "tv|s|1|1").unwrap().unwrap().dismissed);
        assert_eq!(recent(&conn, p, 1).unwrap()[0].key, "tv|s|1|1", "same second: live row first");

        delete(&conn, p).unwrap();
        assert!(recent(&conn, p, 10).unwrap().is_empty(), "history goes with the profile");
    }

    #[test]
    fn cookie_and_dates() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::COOKIE, "autonext=1; profile=42; cc=0".parse().unwrap());
        assert_eq!(cookie_profile_id(&h), Some(42));
        h.insert(axum::http::header::COOKIE, "profile=abc".parse().unwrap());
        assert_eq!(cookie_profile_id(&h), None);
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_789_430_400), "2026-09-15");
        assert_eq!(when(now() - 10), "just now");
        assert_eq!(when(now() - 100_000), "yesterday");
    }
}
