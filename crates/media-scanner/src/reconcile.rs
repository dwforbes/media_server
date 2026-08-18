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
/// ready, delete rows whose file is gone. Returns number extracted.
pub fn reconcile_root(conn: &mut Connection, ffprobe: &str, root: &Root) -> Result<usize> {
    let root_path = Path::new(&root.path);
    if !root_path.is_dir() {
        tracing::warn!(
            "root {} is not accessible; skipping (files kept in catalog)",
            root.path
        );
        return Ok(0);
    }

    let mut known: HashMap<String, files::KnownFile> = files::known_files(conn, root.id)?;
    let mut to_extract: Vec<(i64, String)> = Vec::new();

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
        let Some(mime) = media_mime(root, entry.path()) else { continue };
        let Ok(rel) = entry.path().strip_prefix(root_path) else { continue };
        let rel = rel.to_string_lossy().to_string();
        let Some((size, mtime)) = stat(entry.path()) else { continue };

        match known.remove(&rel) {
            Some((id, db_size, db_mtime, status, db_nfo_mtime, db_art)) => {
                let art_stale = || {
                    let found = extract::discover_sidecar_art(entry.path(), &rel, root.kind);
                    match db_art.as_deref() {
                        // Embedded art isn't discoverable by stat; only a
                        // new sidecar (which takes precedence) matters.
                        Some("embedded") => found.is_some(),
                        stored => stored != found.as_deref(),
                    }
                };
                if db_size != size || db_mtime != mtime {
                    files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
                    to_extract.push((id, rel));
                } else if status != "ready" {
                    // Retry files that were pending/errored last time and
                    // have been stable since.
                    to_extract.push((id, rel));
                } else if extract::nfo_mtime(entry.path()) != db_nfo_mtime || art_stale() {
                    // A sidecar (.nfo or artwork) appeared, vanished, or
                    // changed since extraction.
                    to_extract.push((id, rel));
                }
            }
            None => {
                let id =
                    files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
                to_extract.push((id, rel));
            }
        }
    }

    // Anything left in `known` no longer exists on disk.
    for (rel, (id, ..)) in &known {
        tracing::info!("removing vanished file {}/{}", root.path, rel);
        files::delete_file(conn, *id)?;
    }

    let count = to_extract.len();
    for (id, rel) in to_extract {
        extract::extract_file(conn, ffprobe, root, &rel, id)?;
    }
    Ok(count)
}
