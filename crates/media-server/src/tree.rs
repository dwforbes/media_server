use anyhow::{anyhow, Result};
use media_db::queries::{files, movies, music, tv};
use media_db::{BrowseItem, MediaKind};
use rusqlite::Connection;

use crate::objectid::ObjectId;

pub const CLASS_FOLDER: &str = "object.container.storageFolder";
const CLASS_MOVIE_GENRE: &str = "object.container.genre.movieGenre";
const CLASS_MUSIC_GENRE: &str = "object.container.genre.musicGenre";
const CLASS_ARTIST: &str = "object.container.person.musicArtist";
const CLASS_PERSON: &str = "object.container.person";

/// By Rating buckets: (label, lo, hi) with lo <= rating < hi. The final
/// "Unrated" bucket is index RATING_BUCKETS.len().
const RATING_BUCKETS: &[(&str, f64, f64)] = &[
    ("9 and above", 9.0, 10.1),
    ("8 – 9", 8.0, 9.0),
    ("7 – 8", 7.0, 8.0),
    ("6 – 7", 6.0, 7.0),
    ("Below 6", 0.0, 6.0),
];

fn rating_bucket_title(bucket: usize) -> String {
    RATING_BUCKETS
        .get(bucket)
        .map(|(label, ..)| label.to_string())
        .unwrap_or_else(|| "Unrated".to_string())
}

/// Movies in a rating bucket, titles prefixed with the score (DIDL has no
/// widely-supported rating field, so it rides in the display name).
fn rating_bucket_items(conn: &Connection, bucket: usize) -> Result<Vec<media_db::BrowseItem>> {
    let mut movies_in_bucket = match RATING_BUCKETS.get(bucket) {
        Some((_, lo, hi)) => movies::by_rating(conn, *lo, *hi)?,
        None => movies::unrated(conn)?,
    };
    for movie in &mut movies_in_bucket {
        if let Some(rating) = movie.rating {
            movie.title = format!("{rating:.1} · {}", movie.title);
        }
    }
    Ok(movies_in_bucket)
}
const CLASS_ALBUM: &str = "object.container.album.musicAlbum";

/// One DIDL-Lite entry, fully resolved (ids as strings, parent included).
pub enum Entry {
    Container {
        id: String,
        parent: String,
        title: String,
        class: &'static str,
        /// A file id whose /art/{id} image decorates this container.
        art_item: Option<i64>,
    },
    Item {
        id: String,
        parent: String,
        item: BrowseItem,
    },
}

fn container(id: &ObjectId, parent: &ObjectId, title: impl Into<String>) -> Entry {
    container_class(id, parent, title, CLASS_FOLDER)
}

fn container_class(
    id: &ObjectId,
    parent: &ObjectId,
    title: impl Into<String>,
    class: &'static str,
) -> Entry {
    Entry::Container {
        id: id.to_id(),
        parent: parent.to_id(),
        title: title.into(),
        class,
        art_item: None,
    }
}

fn with_art(mut entry: Entry, art: Option<i64>) -> Entry {
    if let Entry::Container { art_item, .. } = &mut entry {
        *art_item = art;
    }
    entry
}

fn item(parent: &ObjectId, item: BrowseItem) -> Entry {
    Entry::Item {
        id: ObjectId::Item(item.file_id).to_id(),
        parent: parent.to_id(),
        item,
    }
}

fn items(parent: &ObjectId, list: Vec<BrowseItem>) -> Vec<Entry> {
    list.into_iter().map(|i| item(parent, i)).collect()
}

fn root_title(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn season_title(season: i64) -> String {
    if season == 0 {
        "Specials".to_string()
    } else {
        format!("Season {season}")
    }
}

/// Children of a container, for BrowseDirectChildren.
pub fn browse_children(conn: &Connection, oid: &ObjectId) -> Result<Vec<Entry>> {
    use ObjectId::*;
    Ok(match oid {
        Root => {
            let roots = files::list_roots(conn)?;
            let mut out = Vec::new();
            for (kind, node, title) in [
                (MediaKind::Movies, Movies, "Movies"),
                (MediaKind::Music, Music, "Music"),
                (MediaKind::Tv, Tv, "TV Shows"),
            ] {
                if roots.iter().any(|r| r.kind == kind) {
                    out.push(container(&node, &Root, title));
                }
            }
            out
        }

        Movies => vec![
            container(&MoviesAll, oid, "All Movies"),
            container(&MoviesByYear, oid, "By Year"),
            container(&MoviesByGenre, oid, "By Genre"),
            container(&MoviesByDirector, oid, "By Director"),
            container(&MoviesByRating, oid, "By Rating"),
            container(&MoviesFolders, oid, "Folders"),
        ],
        MoviesAll => items(oid, movies::all_movies(conn)?),
        MoviesByYear => movies::years(conn)?
            .into_iter()
            .map(|y| container(&MoviesYear(y), oid, y.to_string()))
            .collect(),
        MoviesYear(y) => items(oid, movies::by_year(conn, *y)?),
        MoviesByGenre => movies::genres(conn)?
            .into_iter()
            .map(|(id, name)| container_class(&MoviesGenre(id), oid, name, CLASS_MOVIE_GENRE))
            .collect(),
        MoviesGenre(g) => items(oid, movies::by_genre(conn, *g)?),
        MoviesByDirector => movies::directors(conn)?
            .into_iter()
            .map(|(id, name)| container_class(&MoviesDirector(id), oid, name, CLASS_PERSON))
            .collect(),
        MoviesDirector(d) => items(oid, movies::by_director(conn, *d)?),
        MoviesByRating => (0..=RATING_BUCKETS.len())
            .map(|bucket| container(&MoviesRating(bucket), oid, rating_bucket_title(bucket)))
            .collect(),
        MoviesRating(bucket) => items(oid, rating_bucket_items(conn, *bucket)?),
        MoviesFolders => folder_roots(conn, MediaKind::Movies, oid)?,

        Music => vec![
            container(&MusicArtists, oid, "Artists"),
            container(&MusicAlbums, oid, "Albums"),
            container(&MusicByGenre, oid, "Genres"),
            container(&MusicFolders, oid, "Folders"),
        ],
        MusicArtists => music::artists(conn)?
            .into_iter()
            .map(|a| container_class(&MusicArtist(a.clone()), oid, a, CLASS_ARTIST))
            .collect(),
        MusicArtist(artist) => music::albums_for_artist(conn, artist)?
            .into_iter()
            .map(|(album, art)| {
                with_art(
                    container_class(
                        &MusicAlbum { artist: artist.clone(), album: album.clone() },
                        oid,
                        album,
                        CLASS_ALBUM,
                    ),
                    art,
                )
            })
            .collect(),
        MusicAlbums => music::albums(conn)?
            .into_iter()
            .map(|(artist, album, art)| {
                with_art(
                    container_class(
                        &MusicAlbum { artist: artist.clone(), album: album.clone() },
                        &MusicArtist(artist),
                        album,
                        CLASS_ALBUM,
                    ),
                    art,
                )
            })
            .collect(),
        MusicAlbum { artist, album } => items(oid, music::tracks_for_album(conn, artist, album)?),
        MusicByGenre => music::genres(conn)?
            .into_iter()
            .map(|(id, name)| container_class(&MusicGenre(id), oid, name, CLASS_MUSIC_GENRE))
            .collect(),
        MusicGenre(g) => items(oid, music::by_genre(conn, *g)?),
        MusicFolders => folder_roots(conn, MediaKind::Music, oid)?,

        Tv => {
            let mut out = vec![container(&TvFolders, oid, "Folders")];
            for (series, art) in tv::series_list(conn)? {
                out.push(with_art(
                    container(&TvSeries(series.clone()), oid, series),
                    art,
                ));
            }
            out
        }
        TvSeries(series) => tv::seasons(conn, series)?
            .into_iter()
            .map(|season| {
                container(
                    &TvSeason { series: series.clone(), season },
                    oid,
                    season_title(season),
                )
            })
            .collect(),
        TvSeason { series, season } => {
            let eps = tv::episodes(conn, series, *season)?
                .into_iter()
                .map(|mut ep| {
                    if let Some(n) = ep.episode {
                        ep.title = format!("{n:02} - {}", ep.title);
                    }
                    ep
                })
                .collect();
            items(oid, eps)
        }
        TvFolders => folder_roots(conn, MediaKind::Tv, oid)?,

        Dir { root_id, rel_dir } => {
            let (subdirs, files_in_dir) = files::dir_children(conn, *root_id, rel_dir)?;
            let mut out = Vec::new();
            for name in subdirs {
                let child_rel = if rel_dir.is_empty() {
                    name.clone()
                } else {
                    format!("{rel_dir}/{name}")
                };
                out.push(container(
                    &Dir { root_id: *root_id, rel_dir: child_rel },
                    oid,
                    name,
                ));
            }
            out.extend(items(oid, files_in_dir));
            out
        }

        Item(_) => Vec::new(),
    })
}

fn folder_roots(conn: &Connection, kind: MediaKind, parent: &ObjectId) -> Result<Vec<Entry>> {
    Ok(files::roots_of_kind(conn, kind)?
        .into_iter()
        .map(|r| {
            container(
                &ObjectId::Dir { root_id: r.id, rel_dir: String::new() },
                parent,
                root_title(&r.path),
            )
        })
        .collect())
}

/// The object itself, for BrowseMetadata.
pub fn browse_metadata(conn: &Connection, oid: &ObjectId) -> Result<Entry> {
    use ObjectId::*;
    let entry = match oid {
        Root => container(&Root, &Root, "Media"),
        Movies => container(oid, &Root, "Movies"),
        MoviesAll => container(oid, &Movies, "All Movies"),
        MoviesByYear => container(oid, &Movies, "By Year"),
        MoviesYear(y) => container(oid, &MoviesByYear, y.to_string()),
        MoviesByGenre => container(oid, &Movies, "By Genre"),
        MoviesGenre(g) => {
            container_class(oid, &MoviesByGenre, genre_name(conn, *g)?, CLASS_MOVIE_GENRE)
        }
        MoviesByDirector => container(oid, &Movies, "By Director"),
        MoviesDirector(d) => {
            let name: String =
                conn.query_row("SELECT name FROM directors WHERE id = ?1", [d], |r| r.get(0))?;
            container_class(oid, &MoviesByDirector, name, CLASS_PERSON)
        }
        MoviesByRating => container(oid, &Movies, "By Rating"),
        MoviesRating(bucket) => {
            container(oid, &MoviesByRating, rating_bucket_title(*bucket))
        }
        MoviesFolders => container(oid, &Movies, "Folders"),
        Music => container(oid, &Root, "Music"),
        MusicArtists => container(oid, &Music, "Artists"),
        MusicArtist(a) => container_class(oid, &MusicArtists, a.clone(), CLASS_ARTIST),
        MusicAlbums => container(oid, &Music, "Albums"),
        MusicAlbum { artist, album } => container_class(
            oid,
            &MusicArtist(artist.clone()),
            album.clone(),
            CLASS_ALBUM,
        ),
        MusicByGenre => container(oid, &Music, "Genres"),
        MusicGenre(g) => {
            container_class(oid, &MusicByGenre, genre_name(conn, *g)?, CLASS_MUSIC_GENRE)
        }
        MusicFolders => container(oid, &Music, "Folders"),
        Tv => container(oid, &Root, "TV Shows"),
        TvSeries(s) => container(oid, &Tv, s.clone()),
        TvSeason { series, season } => {
            container(oid, &TvSeries(series.clone()), season_title(*season))
        }
        TvFolders => container(oid, &Tv, "Folders"),
        Dir { root_id, rel_dir } => {
            let root = files::get_root(conn, *root_id)?
                .ok_or_else(|| anyhow!("unknown root {root_id}"))?;
            let folders_node = match root.kind {
                MediaKind::Movies => MoviesFolders,
                MediaKind::Music => MusicFolders,
                MediaKind::Tv => TvFolders,
            };
            if rel_dir.is_empty() {
                container(oid, &folders_node, root_title(&root.path))
            } else {
                let (parent_rel, name) = match rel_dir.rsplit_once('/') {
                    Some((p, n)) => (p.to_string(), n.to_string()),
                    None => (String::new(), rel_dir.clone()),
                };
                container(
                    oid,
                    &Dir { root_id: *root_id, rel_dir: parent_rel },
                    name,
                )
            }
        }
        Item(file_id) => {
            let browse = files::browse_item(conn, *file_id)?
                .ok_or_else(|| anyhow!("no such item {file_id}"))?;
            let row = files::get_file(conn, *file_id)?
                .ok_or_else(|| anyhow!("no such file {file_id}"))?;
            let rel_dir = row
                .rel_path
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            item(&Dir { root_id: row.root_id, rel_dir }, browse)
        }
    };
    Ok(entry)
}

fn genre_name(conn: &Connection, id: i64) -> Result<String> {
    Ok(conn.query_row("SELECT name FROM genres WHERE id = ?1", [id], |r| r.get(0))?)
}
