use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use media_db::queries::files;
use media_db::{mime, Root};
use rusqlite::Connection;
use walkdir::WalkDir;

use crate::extract;

/// Recognized media file under `root`? Returns its MIME type.
pub fn media_mime(root: &Root, path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    mime::mime_for_extension(&ext, root.kind.is_video())
}

/// A file modified within the last `grace_ms` is still being written
/// (network copies stall longer than any size-stability window; mtime age
/// is the reliable signal). Skipped files are picked up by the watcher's
/// post-quiet event or the next reconcile.
pub fn too_fresh(mtime: i64, grace_ms: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now - mtime < (grace_ms / 1000).max(1) as i64
}

pub fn stat(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((meta.len() as i64, mtime))
}

/// Full pass over one root: upsert new/changed files, extract anything not
/// ready, delete rows whose file is gone. Returns (new or changed media
/// files, total files extracted) — the distinction matters to the
/// auto-enrich trigger, which must ignore sidecar-driven re-extraction or
/// enrichment's own .nfo writes would re-trigger it.
pub fn reconcile_root(conn: &mut Connection, ffprobe: &str, root: &Root) -> Result<(usize, usize)> {
    let root_path = Path::new(&root.path);
    if !root_path.is_dir() {
        tracing::warn!(
            "root {} is not accessible; skipping (files kept in catalog)",
            root.path
        );
        return Ok((0, 0));
    }

    let mut known: HashMap<String, files::KnownFile> = files::known_files(conn, root.id)?;
    let mut to_extract: Vec<(i64, String)> = Vec::new();
    let mut new_media = 0usize;

    for entry in WalkDir::new(root_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("walk error under {}: {err}", root.path);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        // Directory-level TV sidecars aren't media files; ingest them
        // directly (idempotent, and cheap enough to redo every pass).
        if root.kind == media_db::MediaKind::Tv {
            let name = entry.file_name().to_str().unwrap_or("");
            if name == extract::SHOW_NFO || name == extract::SEASON_NFO {
                if let Ok(rel) = entry.path().strip_prefix(root_path) {
                    let rel = rel.to_string_lossy();
                    if let Err(err) = extract::ingest_tv_dir_nfo(conn, root, &rel) {
                        tracing::warn!("ingesting {}/{rel}: {err:#}", root.path);
                    }
                }
                continue;
            }
        }
        let Some(mime) = media_mime(root, entry.path()) else { continue };
        let Ok(rel) = entry.path().strip_prefix(root_path) else { continue };
        let rel = rel.to_string_lossy().to_string();
        let Some((size, mtime)) = stat(entry.path()) else { continue };
        if too_fresh(mtime, 2000) {
            tracing::debug!("skipping {} (still being written)", entry.path().display());
            known.remove(&rel);
            continue;
        }

        match known.remove(&rel) {
            Some(kf) => {
                let art_stale = || {
                    let found = extract::discover_sidecar_art(entry.path(), &rel, root.kind);
                    match kf.art.as_deref() {
                        // Embedded art isn't discoverable by stat; only a
                        // new sidecar (which takes precedence) matters.
                        Some("embedded") => found.is_some(),
                        stored => stored != found.as_deref(),
                    }
                };
                if kf.size != size || kf.mtime != mtime {
                    files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
                    new_media += 1;
                    to_extract.push((kf.id, rel));
                } else if kf.status != "ready" {
                    // Retry files that were pending/errored last time and
                    // have been stable since. Not counted as new media: a
                    // permanently-failing file must not re-trigger
                    // auto-enrichment every reconcile.
                    to_extract.push((kf.id, rel));
                } else if extract::nfo_mtime(entry.path()) != kf.nfo_mtime || art_stale() {
                    // A sidecar (.nfo or artwork) appeared, vanished, or
                    // changed since extraction.
                    to_extract.push((kf.id, rel));
                } else if root.kind == media_db::MediaKind::Music
                    && extract::music_meta_mtime(entry.path(), root_path)
                        .is_some_and(|t| t > kf.updated_at)
                {
                    // A directory music.toml changed after this track was
                    // extracted.
                    to_extract.push((kf.id, rel));
                }
            }
            None => {
                let id =
                    files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
                new_media += 1;
                to_extract.push((id, rel));
            }
        }
    }

    // Anything left in `known` no longer exists on disk.
    for (rel, kf) in &known {
        tracing::info!("removing vanished file {}/{}", root.path, rel);
        files::delete_file(conn, kf.id)?;
    }

    let count = to_extract.len();
    for (id, rel) in to_extract {
        extract::extract_file(conn, ffprobe, root, &rel, id)?;
    }
    Ok((new_media, count))
}
