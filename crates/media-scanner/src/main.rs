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

use config::{Config, EnrichConfig};

/// Debounced, serialized runner for the media-enrich subprocess. Triggered
/// only by new/changed media files — never by sidecar events, which is what
/// keeps enrichment's own .nfo writes from re-triggering it.
struct EnrichRunner {
    cfg: EnrichConfig,
    config_path: PathBuf,
    pending: bool,
    last_add: Instant,
    last_run: Option<Instant>,
    child: Option<std::process::Child>,
}

impl EnrichRunner {
    fn new(cfg: EnrichConfig, config_path: PathBuf) -> Self {
        EnrichRunner {
            cfg,
            config_path,
            pending: false,
            last_add: Instant::now(),
            last_run: None,
            child: None,
        }
    }

    fn note_media_added(&mut self) {
        self.pending = true;
        self.last_add = Instant::now();
    }

    /// Reap a finished run; launch a new one when due.
    fn tick(&mut self) {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        tracing::info!("media-enrich run finished");
                    } else {
                        tracing::warn!("media-enrich exited with {status}");
                    }
                    self.child = None;
                }
                Ok(None) => return, // still running
                Err(err) => {
                    tracing::warn!("waiting on media-enrich: {err}");
                    self.child = None;
                }
            }
        }
        if !self.pending
            || self.last_add.elapsed() < Duration::from_secs(self.cfg.quiet_secs)
            || self.last_run.is_some_and(|t| {
                t.elapsed() < Duration::from_secs(self.cfg.min_interval_secs)
            })
        {
            return;
        }
        self.pending = false;
        self.last_run = Some(Instant::now());
        match std::process::Command::new(&self.cfg.command)
            .arg("--config")
            .arg(&self.config_path)
            .spawn()
        {
            Ok(child) => {
                tracing::info!("new media settled; running {}", self.cfg.command);
                self.child = Some(child);
            }
            Err(err) => tracing::warn!("could not launch {}: {err}", self.cfg.command),
        }
    }
}

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
    let added = reconcile_all(&mut conn, &cfg, &roots)?;
    let enrich_enabled = cfg.enrich.as_ref().is_some_and(|e| e.auto);

    if args.once {
        if enrich_enabled && added {
            // Synchronous: enrich, then a second pass to ingest the
            // sidecars it wrote.
            let enrich = cfg.enrich.as_ref().unwrap();
            tracing::info!("new media found; running {}", enrich.command);
            let status = std::process::Command::new(&enrich.command)
                .arg("--config")
                .arg(&args.config)
                .status();
            match status {
                Ok(s) if s.success() => {
                    reconcile_all(&mut conn, &cfg, &roots)?;
                }
                Ok(s) => tracing::warn!("media-enrich exited with {s}"),
                Err(err) => tracing::warn!("could not launch {}: {err}", enrich.command),
            }
        }
        tracing::info!("--once: reconcile complete, exiting");
        return Ok(());
    }

    let mut enricher = if enrich_enabled {
        let mut runner = EnrichRunner::new(cfg.enrich.clone().unwrap(), args.config.clone());
        if added {
            runner.note_media_added();
        }
        Some(runner)
    } else {
        None
    };

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
        // Short timeout so the enrich debounce gets regular ticks.
        match rx.recv_timeout(Duration::from_secs(10)) {
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
                    match handle_path(&mut conn, &cfg, &roots, &path) {
                        Ok(true) => {
                            if let Some(runner) = &mut enricher {
                                runner.note_media_added();
                            }
                        }
                        Ok(false) => {}
                        Err(err) => tracing::warn!("handling {}: {err:#}", path.display()),
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
            if reconcile_all(&mut conn, &cfg, &roots)? {
                if let Some(runner) = &mut enricher {
                    runner.note_media_added();
                }
            }
            last_reconcile = Instant::now();
        }
        if let Some(runner) = &mut enricher {
            runner.tick();
        }
    }
}

/// Returns whether any new/changed media files were catalogued (sidecar-
/// driven re-extraction doesn't count — see EnrichRunner).
fn reconcile_all(conn: &mut Connection, cfg: &Config, roots: &[Root]) -> Result<bool> {
    let mut any_new = false;
    for root in roots {
        let (new_media, extracted) = reconcile::reconcile_root(conn, &cfg.ffprobe_path, root)?;
        tracing::info!("reconciled {} ({extracted} files extracted)", root.path);
        any_new |= new_media > 0;
    }
    Ok(any_new)
}

/// React to one filesystem event path. Returns true when a new/changed
/// media file was catalogued (the auto-enrich trigger).
fn handle_path(conn: &mut Connection, cfg: &Config, roots: &[Root], path: &Path) -> Result<bool> {
    // Longest matching root wins, in case one root nests inside another.
    let Some(root) = roots
        .iter()
        .filter(|r| path.starts_with(&r.path))
        .max_by_key(|r| r.path.len())
    else {
        return Ok(false);
    };
    let rel = path
        .strip_prefix(&root.path)
        .context("path not under root")?
        .to_string_lossy()
        .to_string();
    if rel.is_empty() || rel.split('/').any(|c| c.starts_with('.')) {
        return Ok(false);
    }

    if !path.exists() {
        let n = files::delete_by_prefix(conn, root.id, &rel)?;
        if n > 0 {
            tracing::info!("removed {n} catalog entries under {}/{}", root.path, rel);
        }
        return Ok(false);
    }

    if path.is_dir() {
        return scan_subtree(conn, cfg, root, path);
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if ext == "nfo" {
        refresh_nfo_sibling(conn, cfg, root, &rel)?;
        return Ok(false);
    }
    if ext == "jpg" || ext == "png" {
        refresh_art_siblings(conn, cfg, root, &rel)?;
        return Ok(false);
    }

    let Some(mime) = reconcile::media_mime(root, path) else {
        return Ok(false);
    };

    // Settle check: a file still being copied grows between two stats.
    let Some((size, _)) = reconcile::stat(path) else { return Ok(false) };
    std::thread::sleep(Duration::from_millis(500));
    let Some((size2, mtime)) = reconcile::stat(path) else { return Ok(false) };
    if size != size2 {
        tracing::debug!("{} still changing; leaving for next event", path.display());
        return Ok(false);
    }

    // Overlapping events (directory + file) both land here; skip work the
    // catalog already reflects.
    if let Some((_, db_size, db_mtime, status)) = files::lookup(conn, root.id, &rel)? {
        if db_size == size2 && db_mtime == mtime && status == "ready" {
            return Ok(false);
        }
    }

    let id = files::upsert_pending(conn, root.id, &rel, size2, mtime, root.kind, mime)?;
    extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, id)?;
    Ok(true)
}

/// A directory appeared (new folder, or moved in): catalog its contents.
/// Returns whether any media files were catalogued.
fn scan_subtree(conn: &mut Connection, cfg: &Config, root: &Root, dir: &Path) -> Result<bool> {
    let mut any = false;
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
    {
        if entry.file_type().is_file() {
            any |= handle_path(conn, cfg, &[root.clone()], entry.path())?;
        }
    }
    Ok(any)
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
