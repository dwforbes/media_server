//! Leaf counts for browse containers — how many playable items live under
//! each folder link. Computed with one grouped query per page (never per
//! row) and cached per container until the scanner commits: the
//! data_version poll in main.rs bumps `update_id` on any catalog change,
//! and that counter doubles as the cache generation.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use media_db::queries::counts as q;
use media_db::MediaKind;
use rusqlite::Connection;

use crate::http::AppState;
use crate::objectid::ObjectId;
use crate::tree::RATING_BUCKETS;

#[derive(Default)]
pub struct Cache {
    generation: u32,
    per_parent: HashMap<String, Arc<HashMap<String, i64>>>,
}

/// Canonical lookup key for a child container. Views grouped
/// case-insensitively (series, franchises) fold to lowercase so the count
/// rows and the tree entries agree no matter which casing each query
/// happened to surface; everything else keys on the object id as-is.
pub fn canon(oid: &ObjectId) -> String {
    use ObjectId::*;
    match oid {
        TvSeries(s) => TvSeries(s.to_lowercase()).to_id(),
        TvSeason { series, season } => {
            TvSeason { series: series.to_lowercase(), season: *season }.to_id()
        }
        MoviesFranchise(n) => MoviesFranchise(n.to_lowercase()).to_id(),
        other => other.to_id(),
    }
}

/// Leaf counts for the children of `parent`, keyed by `canon` of the
/// child id. Missing entries simply render without a count.
pub fn for_children(
    state: &AppState,
    conn: &Connection,
    parent: &ObjectId,
) -> Arc<HashMap<String, i64>> {
    let generation = state.update_id.load(Ordering::Relaxed);
    let parent_key = parent.to_id();
    let mut cache = state.counts.lock().unwrap();
    if cache.generation != generation {
        cache.per_parent.clear();
        cache.generation = generation;
    }
    if let Some(map) = cache.per_parent.get(&parent_key) {
        return map.clone();
    }
    let map = Arc::new(compute(conn, parent, state.recent_count).unwrap_or_else(|err| {
        tracing::warn!("counting children of {parent_key}: {err:#}");
        HashMap::new()
    }));
    cache.per_parent.insert(parent_key, map.clone());
    map
}

fn compute(
    conn: &Connection,
    parent: &ObjectId,
    recent_count: usize,
) -> Result<HashMap<String, i64>> {
    use ObjectId::*;
    let recent_cap = recent_count as i64;
    let mut out: HashMap<String, i64> = HashMap::new();
    match parent {
        Root => {
            out.insert(canon(&Movies), q::movies_total(conn)?);
            out.insert(canon(&Music), q::tracks_total(conn)?);
            out.insert(canon(&Tv), q::episodes_total(conn)?);
        }
        Movies => {
            let total = q::movies_total(conn)?;
            let dated = q::movies_dated(conn)?;
            out.insert(canon(&MoviesAll), total);
            out.insert(canon(&MoviesRecent), total.min(recent_cap));
            out.insert(canon(&MoviesUhd), q::movies_uhd(conn)?);
            out.insert(canon(&MoviesByYear), dated);
            out.insert(canon(&MoviesByDecade), dated);
            out.insert(canon(&MoviesByGenre), q::movies_with_genre(conn)?);
            out.insert(canon(&MoviesByDirector), q::movies_with_director(conn)?);
            out.insert(canon(&MoviesByFranchise), q::movies_in_franchises(conn)?);
            out.insert(canon(&MoviesByRating), total);
            out.insert(canon(&MoviesFolders), q::files_total(conn, MediaKind::Movies)?);
        }
        MoviesByYear => {
            for (year, n) in q::movies_by_year(conn)? {
                out.insert(canon(&MoviesYear(year)), n);
            }
        }
        MoviesByDecade => {
            for (decade, n) in q::movies_by_decade(conn)? {
                out.insert(canon(&MoviesDecade(decade)), n);
            }
        }
        MoviesByGenre => {
            for (id, n) in q::movies_by_genre(conn)? {
                out.insert(canon(&MoviesGenre(id)), n);
            }
        }
        MoviesByDirector => {
            for (id, n) in q::movies_by_director(conn)? {
                out.insert(canon(&MoviesDirector(id)), n);
            }
        }
        MoviesByFranchise => {
            for (name, n) in q::movies_by_franchise(conn)? {
                out.insert(canon(&MoviesFranchise(name)), n);
            }
        }
        MoviesByRating => {
            for (bucket, (_, lo, hi)) in RATING_BUCKETS.iter().enumerate() {
                out.insert(canon(&MoviesRating(bucket)), q::movies_rated(conn, *lo, *hi)?);
            }
            out.insert(canon(&MoviesRating(RATING_BUCKETS.len())), q::movies_unrated(conn)?);
        }
        Music => {
            let total = q::tracks_total(conn)?;
            out.insert(canon(&MusicRecent), total.min(recent_cap));
            out.insert(canon(&MusicArtists), total);
            out.insert(canon(&MusicAlbums), total);
            out.insert(canon(&MusicByGenre), q::tracks_with_genre(conn)?);
            out.insert(canon(&MusicFolders), q::files_total(conn, MediaKind::Music)?);
        }
        MusicArtists => {
            for (artist, n) in q::tracks_by_artist(conn)? {
                out.insert(canon(&MusicArtist(artist)), n);
            }
        }
        MusicArtist(artist) => {
            for (album, n) in q::tracks_by_album_for_artist(conn, artist)? {
                out.insert(canon(&MusicAlbum { artist: artist.clone(), album }), n);
            }
        }
        MusicAlbums => {
            for (artist, album, n) in q::tracks_by_album(conn)? {
                out.insert(canon(&MusicAlbum { artist, album }), n);
            }
        }
        MusicByGenre => {
            for (id, n) in q::tracks_by_genre(conn)? {
                out.insert(canon(&MusicGenre(id)), n);
            }
        }
        Tv => {
            out.insert(canon(&TvRecent), q::episodes_total(conn)?.min(recent_cap));
            out.insert(canon(&TvUhd), q::episodes_uhd(conn)?);
            out.insert(canon(&TvFolders), q::files_total(conn, MediaKind::Tv)?);
            for (series, n) in q::episodes_by_series(conn)? {
                out.insert(canon(&TvSeries(series)), n);
            }
        }
        TvSeries(series) => {
            for (season, n) in q::episodes_by_season(conn, series)? {
                out.insert(canon(&TvSeason { series: series.clone(), season }), n);
            }
        }
        MoviesFolders | MusicFolders | TvFolders => {
            let kind = match parent {
                MoviesFolders => MediaKind::Movies,
                MusicFolders => MediaKind::Music,
                _ => MediaKind::Tv,
            };
            for (root_id, n) in q::files_per_root(conn, kind)? {
                out.insert(canon(&Dir { root_id, rel_dir: String::new() }), n);
            }
        }
        Dir { root_id, rel_dir } => {
            let prefix =
                if rel_dir.is_empty() { String::new() } else { format!("{rel_dir}/") };
            for rel in q::rel_paths(conn, *root_id)? {
                let Some(remainder) = rel.strip_prefix(&prefix) else { continue };
                // Files directly in this directory are leaf rows, not
                // containers; only paths in a subdirectory count.
                let Some((first, _)) = remainder.split_once('/') else { continue };
                let child = Dir { root_id: *root_id, rel_dir: format!("{prefix}{first}") };
                *out.entry(canon(&child)).or_insert(0) += 1;
            }
        }
        // Leaf-only or empty parents: nothing to count.
        _ => {}
    }
    Ok(out)
}
