use std::path::Path;

use anyhow::{Context, Result};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{Accessor, ItemKey};
use media_db::queries::music::TrackMeta;
use media_db::TechInfo;

use super::nameparse;

/// Read tags and audio properties with lofty; fall back to
/// Artist/Album/NN Title path conventions for anything missing.
/// The final bool reports whether the tags embed a picture.
pub fn extract(
    path: &Path,
    stem: &str,
    parent_dirs: &[&str],
) -> Result<(TechInfo, TrackMeta, bool)> {
    let (path_artist, path_album, path_track, path_title) =
        nameparse::music_from_path(stem, parent_dirs);

    let mut tech = TechInfo::default();
    let mut meta = TrackMeta {
        title: path_title,
        artist: path_artist,
        album: path_album,
        track_no: path_track,
        ..Default::default()
    };

    let tagged = lofty::read_from_path(path)
        .with_context(|| format!("reading tags from {}", path.display()))?;
    let props = tagged.properties();
    tech.duration_ms = Some(props.duration().as_millis() as i64);
    tech.audio_codec = Some(format!("{:?}", tagged.file_type()).to_lowercase());

    let mut embedded_art = false;
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        embedded_art = !tag.pictures().is_empty();
        if let Some(title) = tag.title() {
            if !title.trim().is_empty() {
                meta.title = title.trim().to_string();
            }
        }
        if let Some(artist) = tag.artist() {
            if !artist.trim().is_empty() {
                meta.artist = Some(artist.trim().to_string());
            }
        }
        if let Some(album) = tag.album() {
            if !album.trim().is_empty() {
                meta.album = Some(album.trim().to_string());
            }
        }
        if let Some(album_artist) = tag.get_string(&ItemKey::AlbumArtist) {
            if !album_artist.trim().is_empty() {
                meta.album_artist = Some(album_artist.trim().to_string());
            }
        }
        if let Some(track) = tag.track() {
            meta.track_no = Some(track as i64);
        }
        if let Some(disc) = tag.disk() {
            meta.disc_no = Some(disc as i64);
        }
        if let Some(year) = tag.year() {
            meta.year = Some(year as i64);
        }
        if let Some(genre) = tag.genre() {
            meta.genres = split_genres(&genre);
        }
    }
    Ok((tech, meta, embedded_art))
}

/// Tags often pack several genres into one string.
pub fn split_genres(raw: &str) -> Vec<String> {
    raw.split([';', '/', ','])
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect()
}
