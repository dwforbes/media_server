mod config;
mod extract;
mod reconcile;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use media_db::queries::files;
use media_db::Root;
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use rusqlite::Connection;
use walkdir::WalkDir;

use config::Config;

#[derive(Parser)]
#[command(about = "Watches media source folders and maintains the shared catalog database")]
struct Args {
    /// Path to the TOML config file.
    #[arg(long, default_value = "media-scanner.toml")]
    config: PathBuf,
    /// Run a single reconcile pass and exit (no watching).
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let db_path = cfg.db_path();
    tracing::info!("opening catalog database {}", db_path.display());
    let mut conn = media_db::open_rw(&db_path)?;

    let roots = files::sync_roots(&conn, &cfg.root_specs())?;
    reconcile_all(&mut conn, &cfg, &roots)?;

    if args.once {
        tracing::info!("--once: reconcile complete, exiting");
        return Ok(());
    }

    // Watch all roots; the debouncer waits for events to settle before
    // delivering, which absorbs most mid-copy churn.
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = new_debouncer(Duration::from_millis(cfg.settle_ms), None, tx)
        .context("starting filesystem watcher")?;
    for root in &roots {
        debouncer
            .watch(Path::new(&root.path), RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.path))?;
        tracing::info!("watching {}", root.path);
    }

    let reconcile_every = Duration::from_secs(cfg.reconcile_interval_hours * 3600);
    let mut last_reconcile = Instant::now();

    loop {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(events)) => {
                let mut paths: Vec<PathBuf> = Vec::new();
                for event in events {
                    for path in &event.paths {
                        if !paths.contains(path) {
                            paths.push(path.clone());
                        }
                    }
                }
                for path in paths {
                    if let Err(err) = handle_path(&mut conn, &cfg, &roots, &path) {
                        tracing::warn!("handling {}: {err:#}", path.display());
                    }
                }
            }
            Ok(Err(errors)) => {
                for err in errors {
                    tracing::warn!("watcher error: {err}");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("filesystem watcher channel closed unexpectedly");
            }
        }

        if cfg.reconcile_interval_hours > 0 && last_reconcile.elapsed() >= reconcile_every {
            reconcile_all(&mut conn, &cfg, &roots)?;
            last_reconcile = Instant::now();
        }
    }
}

fn reconcile_all(conn: &mut Connection, cfg: &Config, roots: &[Root]) -> Result<()> {
    for root in roots {
        let n = reconcile::reconcile_root(conn, &cfg.ffprobe_path, root)?;
        tracing::info!("reconciled {} ({n} files extracted)", root.path);
    }
    Ok(())
}

/// React to one filesystem event path.
fn handle_path(conn: &mut Connection, cfg: &Config, roots: &[Root], path: &Path) -> Result<()> {
    // Longest matching root wins, in case one root nests inside another.
    let Some(root) = roots
        .iter()
        .filter(|r| path.starts_with(&r.path))
        .max_by_key(|r| r.path.len())
    else {
        return Ok(());
    };
    let rel = path
        .strip_prefix(&root.path)
        .context("path not under root")?
        .to_string_lossy()
        .to_string();
    if rel.is_empty() || rel.split('/').any(|c| c.starts_with('.')) {
        return Ok(());
    }

    if !path.exists() {
        let n = files::delete_by_prefix(conn, root.id, &rel)?;
        if n > 0 {
            tracing::info!("removed {n} catalog entries under {}/{}", root.path, rel);
        }
        return Ok(());
    }

    if path.is_dir() {
        scan_subtree(conn, cfg, root, path)?;
        return Ok(());
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if ext == "nfo" {
        return refresh_nfo_sibling(conn, cfg, root, &rel);
    }
    if ext == "jpg" || ext == "png" {
        return refresh_art_siblings(conn, cfg, root, &rel);
    }

    let Some(mime) = reconcile::media_mime(root, path) else {
        return Ok(());
    };

    // Settle check: a file still being copied grows between two stats.
    let Some((size, _)) = reconcile::stat(path) else { return Ok(()) };
    std::thread::sleep(Duration::from_millis(500));
    let Some((size2, mtime)) = reconcile::stat(path) else { return Ok(()) };
    if size != size2 {
        tracing::debug!("{} still changing; leaving for next event", path.display());
        return Ok(());
    }

    // Overlapping events (directory + file) both land here; skip work the
    // catalog already reflects.
    if let Some((_, db_size, db_mtime, status)) = files::lookup(conn, root.id, &rel)? {
        if db_size == size2 && db_mtime == mtime && status == "ready" {
            return Ok(());
        }
    }

    let id = files::upsert_pending(conn, root.id, &rel, size2, mtime, root.kind, mime)?;
    extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, id)?;
    Ok(())
}

/// A directory appeared (new folder, or moved in): catalog its contents.
fn scan_subtree(conn: &mut Connection, cfg: &Config, root: &Root, dir: &Path) -> Result<()> {
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
    {
        if entry.file_type().is_file() {
            handle_path(conn, cfg, &[root.clone()], entry.path())?;
        }
    }
    Ok(())
}

/// An image changed: re-extract whatever media it decorates — the matching
/// movie for "<stem>-poster.*", every file in the directory for cover.jpg
/// and friends.
fn refresh_art_siblings(conn: &mut Connection, cfg: &Config, root: &Root, rel: &str) -> Result<()> {
    let (dir, name) = match rel.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), rel.to_string()),
    };
    let lower = name.to_lowercase();
    let dir_prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };

    let known = files::known_files(conn, root.id)?;
    let poster_stem = lower
        .strip_suffix("-poster.jpg")
        .or_else(|| lower.strip_suffix("-poster.png"))
        .map(|s| format!("{dir_prefix}{}.", &name[..s.len()]));

    for (rel2, (id, _, _, status, ..)) in known {
        if status != "ready" {
            continue;
        }
        let affected = match &poster_stem {
            Some(prefix) => rel2.starts_with(prefix.as_str()),
            None => {
                // Directory art covers files in the directory and (for
                // series posters over season subfolders) one level below.
                extract::DIR_ART_NAMES.contains(&lower.as_str())
                    && rel2
                        .strip_prefix(&dir_prefix)
                        .is_some_and(|rest| rest.matches('/').count() <= 1)
            }
        };
        if affected {
            tracing::info!("artwork changed; re-extracting {}/{rel2}", root.path);
            extract::extract_file(conn, &cfg.ffprobe_path, root, &rel2, id)?;
        }
    }
    Ok(())
}

/// An .nfo changed: re-extract the media file it sits beside.
fn refresh_nfo_sibling(conn: &mut Connection, cfg: &Config, root: &Root, nfo_rel: &str) -> Result<()> {
    let stem_prefix = format!("{}.", nfo_rel.trim_end_matches("nfo").trim_end_matches('.'));
    let known = files::known_files(conn, root.id)?;
    for (rel, (id, ..)) in known {
        if rel.starts_with(&stem_prefix) && rel != nfo_rel {
            tracing::info!("nfo changed; re-extracting {}/{rel}", root.path);
            extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, id)?;
        }
    }
    Ok(())
}
