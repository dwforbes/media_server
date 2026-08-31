mod analyze;
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

/// Resolve the enrich command: an explicit path is used as-is; a bare name
/// prefers a sibling of this executable (the binaries are built together),
/// falling back to $PATH lookup.
fn resolve_enrich_command(command: &str) -> PathBuf {
    if !command.contains(std::path::MAIN_SEPARATOR) {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(sibling) = exe.parent().map(|d| d.join(command)) {
                if sibling.is_file() {
                    return sibling;
                }
            }
        }
    }
    PathBuf::from(command)
}

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
                        // Failed run (network down, missing key, ...): the
                        // backlog still needs enriching — retry on the
                        // min-interval throttle until a run succeeds.
                        tracing::warn!("media-enrich exited with {status}; will retry");
                        self.pending = true;
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
        let command = resolve_enrich_command(&self.cfg.command);
        match std::process::Command::new(&command)
            .arg("--config")
            .arg(&self.config_path)
            .spawn()
        {
            Ok(child) => {
                tracing::info!("new media settled; running {}", command.display());
                self.child = Some(child);
            }
            Err(err) => {
                tracing::warn!("could not launch {}: {err}; will retry", command.display());
                self.pending = true;
            }
        }
    }
}

/// Debounced driver for the audio segment detector: one season per tick,
/// which keeps the watcher responsive, and only after new media has
/// settled and enrichment is idle — enrichment may rewrite media files
/// (subtitle embedding, remuxing), which would immediately re-stale the
/// fingerprints it just computed.
struct AnalyzeRunner {
    ffmpeg: String,
    pending: bool,
    last_add: Instant,
}

const ANALYZE_QUIET: Duration = Duration::from_secs(60);

impl AnalyzeRunner {
    fn note_media_added(&mut self) {
        self.pending = true;
        self.last_add = Instant::now();
    }

    fn tick(&mut self, conn: &mut Connection, enrich_idle: bool) {
        if !self.pending || !enrich_idle || self.last_add.elapsed() < ANALYZE_QUIET {
            return;
        }
        match analyze::analyze_next(conn, &self.ffmpeg) {
            Ok(true) => {} // a season was analyzed; more may remain — keep pending
            Ok(false) => self.pending = false,
            Err(err) => {
                tracing::warn!("segment analysis: {err:#}");
                self.pending = false;
            }
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
            let command = resolve_enrich_command(&enrich.command);
            tracing::info!("new media found; running {}", command.display());
            let status = std::process::Command::new(&command)
                .arg("--config")
                .arg(&args.config)
                .status();
            match status {
                Ok(s) if s.success() => {
                    reconcile_all(&mut conn, &cfg, &roots)?;
                }
                Ok(s) => tracing::warn!("media-enrich exited with {s}"),
                Err(err) => tracing::warn!("could not launch {}: {err}", command.display()),
            }
        }
        if cfg.segments.auto {
            let ffmpeg = cfg.segments_ffmpeg();
            while analyze::analyze_next(&mut conn, &ffmpeg)? {}
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
    // pending from the start: seasons left stale by an earlier run (or a
    // schema upgrade) get picked up once the startup quiet period passes.
    let mut analyzer = cfg.segments.auto.then(|| AnalyzeRunner {
        ffmpeg: cfg.segments_ffmpeg(),
        pending: true,
        last_add: Instant::now(),
    });

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
                    // Reads/opens are not changes; CIFS and atime updates
                    // produce these for files nobody modified.
                    if matches!(
                        event.kind,
                        notify_debouncer_full::notify::EventKind::Access(_)
                    ) {
                        continue;
                    }
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
                            if let Some(runner) = &mut analyzer {
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
                if let Some(runner) = &mut analyzer {
                    runner.note_media_added();
                }
            }
            last_reconcile = Instant::now();
        }
        if let Some(runner) = &mut enricher {
            runner.tick();
        }
        if let Some(runner) = &mut analyzer {
            let enrich_idle = enricher
                .as_ref()
                .map_or(true, |e| e.child.is_none() && !e.pending);
            runner.tick(&mut conn, enrich_idle);
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
        // A remux replaces x.mkv with x.mp4; the .mp4's row (if the create
        // event landed first) should keep the original's added_at.
        if reconcile::media_mime(root, path).is_some() {
            files::bequeath_added_at(conn, root.id, &rel)?;
        }
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
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if root.kind == media_db::MediaKind::Tv
            && (name == extract::SHOW_NFO || name == extract::SEASON_NFO)
        {
            extract::ingest_tv_dir_nfo(conn, root, &rel)?;
        } else {
            refresh_sidecar_sibling(conn, cfg, root, &rel, "nfo", |abs, kf| {
                extract::nfo_mtime(abs) == kf.nfo_mtime
            })?;
        }
        return Ok(false);
    }
    if ext == "edl" {
        refresh_sidecar_sibling(conn, cfg, root, &rel, "edl", |abs, kf| {
            extract::segments::edl_mtime(abs) == kf.edl_mtime
        })?;
        return Ok(false);
    }
    if ext == "jpg" || ext == "png" {
        refresh_art_siblings(conn, cfg, root, &rel)?;
        return Ok(false);
    }
    if root.kind == media_db::MediaKind::Music
        && path.file_name().and_then(|n| n.to_str()) == Some(extract::MUSIC_META_FILE)
    {
        refresh_music_meta(conn, cfg, root, &rel)?;
        return Ok(false);
    }

    let Some(mime) = reconcile::media_mime(root, path) else {
        return Ok(false);
    };

    // Settle checks: a file still being copied grows between two stats, and
    // — the stronger signal, since network copies stall longer than any
    // stat window — carries a fresh mtime. The debouncer fires the final
    // event only after settle_ms of quiet, so a completed copy passes.
    let Some((size, _)) = reconcile::stat(path) else { return Ok(false) };
    std::thread::sleep(Duration::from_millis(500));
    let Some((size2, mtime)) = reconcile::stat(path) else { return Ok(false) };
    if size != size2 || reconcile::too_fresh(mtime, cfg.settle_ms.min(2000)) {
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

    for (rel2, kf) in known {
        if kf.status != "ready" {
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
        if !affected {
            continue;
        }
        // Verify actual change before re-extracting: network filesystems
        // (CIFS especially) surface events for files nobody modified.
        let abs = Path::new(&root.path).join(&rel2);
        let discovered = extract::discover_sidecar_art(&abs, &rel2, root.kind);
        let same_art = match (kf.art.as_deref(), discovered.as_deref()) {
            (Some("embedded"), None) => true,
            (stored, found) => stored == found,
        };
        if same_art {
            // Same art file as before: only re-extract if its content is
            // newer than our extraction.
            let art_fresh = discovered
                .as_deref()
                .and_then(|art_rel| reconcile::stat(&Path::new(&root.path).join(art_rel)))
                .is_some_and(|(_, art_mtime)| art_mtime > kf.updated_at);
            if !art_fresh {
                tracing::debug!("ignoring no-op artwork event for {}/{rel2}", root.path);
                continue;
            }
        }
        tracing::info!("artwork changed; re-extracting {}/{rel2}", root.path);
        extract::extract_file(conn, &cfg.ffprobe_path, root, &rel2, kf.id)?;
    }
    Ok(())
}

/// A directory music.toml changed: re-extract every catalogued track at
/// or below its directory (overrides apply recursively).
fn refresh_music_meta(conn: &mut Connection, cfg: &Config, root: &Root, toml_rel: &str) -> Result<()> {
    let dir_prefix = match toml_rel.rsplit_once('/') {
        Some((d, _)) => format!("{d}/"),
        None => String::new(),
    };
    let known = files::known_files(conn, root.id)?;
    let mut n = 0usize;
    for (rel, kf) in known {
        if kf.status == "ready" && rel.starts_with(&dir_prefix) {
            extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, kf.id)?;
            n += 1;
        }
    }
    tracing::info!("music.toml changed; re-extracted {n} tracks under {}/{dir_prefix}", root.path);
    Ok(())
}

/// A sidecar (.nfo, .edl) changed: re-extract the media file it sits
/// beside. `unchanged` reports whether the catalog already reflects the
/// sidecar's state — spurious events are common on CIFS.
fn refresh_sidecar_sibling(
    conn: &mut Connection,
    cfg: &Config,
    root: &Root,
    sidecar_rel: &str,
    ext: &str,
    unchanged: impl Fn(&Path, &files::KnownFile) -> bool,
) -> Result<()> {
    let stem_prefix = format!("{}.", sidecar_rel.trim_end_matches(ext).trim_end_matches('.'));
    let known = files::known_files(conn, root.id)?;
    for (rel, kf) in known {
        if !rel.starts_with(&stem_prefix) || rel == sidecar_rel {
            continue;
        }
        let abs = Path::new(&root.path).join(&rel);
        if unchanged(&abs, &kf) {
            tracing::debug!("ignoring no-op {ext} event for {}/{rel}", root.path);
            continue;
        }
        tracing::info!("{ext} changed; re-extracting {}/{rel}", root.path);
        extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, kf.id)?;
    }
    Ok(())
}
