use std::path::Path;

use anyhow::{Context, Result};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::mp4::{AudioObjectType, Mp4Codec, Mp4File};
use lofty::mpeg::{Layer, MpegFile};
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
    let (codec, profile) = codec_of(path, tagged.file_type());
    tech.audio_codec = Some(codec);
    tech.audio_profile = Some(profile);
    tech.audio_bitrate = props.audio_bitrate().map(i64::from);
    tech.audio_sample_rate = props.sample_rate().map(i64::from);
    tech.audio_bit_depth = props.bit_depth().map(i64::from);
    tech.audio_channels = props.channels().map(i64::from);

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

/// (short codec name, human label) for a file: lofty's generic properties
/// only know the container, so MP4 and MPEG files are re-read with their
/// concrete parsers to tell AAC LC from HE-AAC from ALAC, and MP3 from MP2.
fn codec_of(path: &Path, file_type: FileType) -> (String, String) {
    let s = |c: &str, p: &str| (c.to_string(), p.to_string());
    match file_type {
        FileType::Mp4 => {
            let props = std::fs::File::open(path)
                .ok()
                .and_then(|mut f| Mp4File::read_from(&mut f, ParseOptions::new()).ok())
                .map(|m| (*m.properties().codec(), m.properties().audio_object_type()));
            match props {
                Some((Mp4Codec::AAC, aot)) => {
                    let label = match aot {
                        Some(AudioObjectType::AacMain) => "AAC Main",
                        Some(AudioObjectType::SpectralBandReplication) => "HE-AAC",
                        Some(AudioObjectType::ParametricStereo) => "HE-AAC v2",
                        Some(AudioObjectType::AacLongTermPrediction) => "AAC LTP",
                        Some(AudioObjectType::ErrorResilientAacLowDelay) => "AAC LD",
                        Some(AudioObjectType::AacScalableSampleRate) => "AAC SSR",
                        _ => "AAC LC",
                    };
                    s("aac", label)
                }
                Some((Mp4Codec::ALAC, _)) => s("alac", "ALAC (Apple Lossless)"),
                Some((Mp4Codec::MP3, _)) => s("mp3", "MP3"),
                Some((Mp4Codec::FLAC, _)) => s("flac", "FLAC"),
                _ => s("mp4", "MP4 audio"),
            }
        }
        FileType::Mpeg => {
            let layer = std::fs::File::open(path)
                .ok()
                .and_then(|mut f| MpegFile::read_from(&mut f, ParseOptions::new()).ok())
                .map(|m| *m.properties().layer());
            match layer {
                Some(Layer::Layer1) => s("mp1", "MPEG Layer I"),
                Some(Layer::Layer2) => s("mp2", "MP2 (MPEG Layer II)"),
                _ => s("mp3", "MP3"),
            }
        }
        FileType::Aac => s("aac", "AAC (ADTS)"),
        FileType::Flac => s("flac", "FLAC"),
        FileType::Opus => s("opus", "Opus"),
        FileType::Vorbis => s("vorbis", "Vorbis"),
        FileType::Speex => s("speex", "Speex"),
        FileType::Wav => s("pcm", "WAV (PCM)"),
        FileType::Aiff => s("pcm", "AIFF (PCM)"),
        FileType::Ape => s("ape", "Monkey's Audio"),
        FileType::WavPack => s("wavpack", "WavPack"),
        FileType::Mpc => s("mpc", "Musepack"),
        other => {
            let name = format!("{other:?}").to_lowercase();
            (name.clone(), name)
        }
    }
}

/// Human label for an ffprobe audio codec name plus profile, used for the
/// audio track of video files: "AAC LC", "E-AC-3", "DTS-HD MA".
pub fn ffprobe_label(codec: &str, profile: Option<&str>) -> String {
    match (codec, profile) {
        ("aac", Some("LC")) | ("aac", None) => "AAC LC".into(),
        ("aac", Some("HE-AAC")) => "HE-AAC".into(),
        ("aac", Some("HE-AACv2")) => "HE-AAC v2".into(),
        ("aac", Some(p)) => format!("AAC {p}"),
        ("ac3", _) => "AC-3 (Dolby Digital)".into(),
        ("eac3", _) => "E-AC-3 (Dolby Digital Plus)".into(),
        ("truehd", _) => "Dolby TrueHD".into(),
        ("dts", Some(p)) => format!("DTS {p}").replace("DTS DTS", "DTS"),
        ("dts", None) => "DTS".into(),
        ("mp3", _) => "MP3".into(),
        ("mp2", _) => "MP2".into(),
        ("flac", _) => "FLAC".into(),
        ("alac", _) => "ALAC (Apple Lossless)".into(),
        ("opus", _) => "Opus".into(),
        ("vorbis", _) => "Vorbis".into(),
        (c, _) if c.starts_with("pcm_") => "PCM".into(),
        (c, _) => c.to_uppercase(),
    }
}

/// Tags often pack several genres into one string.
pub fn split_genres(raw: &str) -> Vec<String> {
    raw.split([';', '/', ','])
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect()
}
