pub mod counts;
pub mod files;
pub mod movies;
pub mod music;
pub mod segments;
pub mod tv;

use std::collections::HashMap;
use std::hash::Hash;

use anyhow::Result;
use rusqlite::Connection;

use crate::models::BrowseItem;

/// Collapse items sharing a key ("the same movie in several qualities")
/// into one item whose primary is the best rendition and whose extras are
/// kept best-first. First-seen list order is preserved.
pub fn merge_renditions<K: Eq + Hash>(
    items: Vec<BrowseItem>,
    key: impl Fn(&BrowseItem) -> K,
) -> Vec<BrowseItem> {
    let mut out: Vec<BrowseItem> = Vec::new();
    let mut position: HashMap<K, usize> = HashMap::new();
    for item in items {
        let Some(&index) = position.get(&key(&item)) else {
            position.insert(key(&item), out.len());
            out.push(item);
            continue;
        };
        let kept = &mut out[index];
        let incoming = item.primary_rendition();
        if !kept.has_art && item.has_art {
            kept.has_art = true;
            kept.art_file_id = Some(item.art_file_id.unwrap_or(item.file_id));
        }
        if incoming.quality() > kept.primary_rendition().quality() {
            let old_primary = kept.primary_rendition();
            kept.set_primary(incoming);
            kept.renditions.push(old_primary);
        } else {
            kept.renditions.push(incoming);
        }
        kept.renditions.sort_by_key(|r| std::cmp::Reverse(r.quality()));
    }
    out
}

/// Genre (id, name) pairs for one file — movie or music (one of the join
/// tables will match; the other contributes nothing).
pub fn genres_for_file(conn: &Connection, file_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, g.name FROM movie_genres mg JOIN genres g ON g.id = mg.genre_id
          WHERE mg.file_id = ?1
         UNION
         SELECT g.id, g.name FROM track_genres tg JOIN genres g ON g.id = tg.genre_id
          WHERE tg.file_id = ?1
         ORDER BY 2",
    )?;
    let rows = stmt.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Director (id, name) pairs for one file.
pub fn directors_for_file(conn: &Connection, file_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT d.id, d.name FROM movie_directors md JOIN directors d ON d.id = md.director_id
          WHERE md.file_id = ?1 ORDER BY d.name",
    )?;
    let rows = stmt.query_map([file_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Find-or-create a genre by name (case-insensitive), returning its id.
pub fn ensure_genre(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    conn.execute("INSERT OR IGNORE INTO genres(name) VALUES (?1)", [name])?;
    let id = conn.query_row(
        "SELECT id FROM genres WHERE name = ?1 COLLATE NOCASE",
        [name],
        |r| r.get(0),
    )?;
    Ok(id)
}
