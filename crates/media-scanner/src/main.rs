mod analyze;
mod config;
mod extract;
mod reconcile;

use std::collections::HashMap;
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
use extract::Extracted;

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
/// by new/changed media files, and by a written .srt (so a corrected
/// sidecar gets re-embedded) — never by other sidecar events, which is
/// what keeps enrichment's own .nfo writes from re-triggering it. The
/// .srt files a run writes itself (extracted tracks) are told apart by
/// time: sidecar events while a run is in flight, or within a grace
/// period after it ends, are ignored.
struct EnrichRunner {
    cfg: EnrichConfig,
    config_path: PathBuf,
    pending: bool,
    last_add: Instant,
    last_run: Option<Instant>,
    /// When the last child exited, for the sidecar grace period.
    finished_at: Option<Instant>,
    child: Option<std::process::Child>,
}

/// How long after a run ends its own .srt writes may still surface as
/// events (the debouncer holds them for settle_ms, and a rename lands
/// last).
const SIDECAR_GRACE: Duration = Duration::from_secs(60);

impl EnrichRunner {
    fn new(cfg: EnrichConfig, config_path: PathBuf) -> Self {
        EnrichRunner {
            cfg,
            config_path,
            pending: false,
            last_add: Instant::now(),
            last_run: None,
            finished_at: None,
            child: None,
        }
    }

    fn note_media_added(&mut self) {
        self.pending = true;
        self.last_add = Instant::now();
    }

    /// An .srt was written. Scheduled like new media, unless a run is in
    /// flight or just ended — then it is most likely the run's own doing.
    fn note_caption_sidecar_changed(&mut self, path: &Path) {
        let own_write = self.child.is_some()
            || self.finished_at.is_some_and(|t| t.elapsed() < SIDECAR_GRACE);
        if own_write {
            tracing::debug!("{}: sidecar written during/after an enrichment run; ignoring", path.display());
            return;
        }
        tracing::info!("{}: caption sidecar changed; enrichment scheduled", path.display());
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
                    self.finished_at = Some(Instant::now());
                }
                Ok(None) => return, // still running
                Err(err) => {
                    tracing::warn!("waiting on media-enrich: {err}");
                    self.child = None;
                    self.finished_at = Some(Instant::now());
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

/// Media files the watcher reported, held until they stop changing.
///
/// The debouncer is no help here: it does not wait for quiet, it emits a
/// Modify event every settle_ms for as long as a copy keeps writing, and
/// a stat-to-stat size comparison is no defence against a network copy
/// that stalls for seconds at a time. So a file is probed only once its
/// size and mtime have held still for the whole settle window. If
/// ffprobe still can't read it then (an MP4's moov atom lands last), the
/// copying client may have preallocated the size and pinned the mtime up
/// front, so the file is tried again a few times with a growing delay
/// before it is catalogued as-is.
struct SettleQueue {
    window: Duration,
    entries: HashMap<PathBuf, Settling>,
}

struct Settling {
    size: i64,
    mtime: i64,
    /// When the current (size, mtime) was first observed.
    stable_since: Instant,
    /// Attempts that ended with ffprobe unable to read the file.
    probe_failures: u32,
    /// Earliest next attempt, for the post-failure back-off.
    not_before: Instant,
}

/// Unreadable-file retries: 30 s window → 1, 2, 4, 5, 5 min → ~17 min in
/// all before giving up, which outlasts any realistic copy stall.
const MAX_PROBE_RETRIES: u32 = 5;
const MAX_PROBE_BACKOFF: Duration = Duration::from_secs(300);

impl SettleQueue {
    fn new(window: Duration) -> Self {
        SettleQueue { window, entries: HashMap::new() }
    }

    /// An event reported this file at this size and mtime.
    fn note(&mut self, path: &Path, size: i64, mtime: i64) {
        let now = Instant::now();
        match self.entries.get_mut(path) {
            Some(e) if e.size == size && e.mtime == mtime => {}
            Some(e) => {
                e.size = size;
                e.mtime = mtime;
                e.stable_since = now;
            }
            None => {
                tracing::info!("{}: new media; waiting for it to settle", path.display());
                self.entries.insert(
                    path.to_path_buf(),
                    Settling {
                        size,
                        mtime,
                        stable_since: now,
                        probe_failures: 0,
                        not_before: now,
                    },
                );
            }
        }
    }

    /// Re-stat every entry: files whose size and mtime have held for the
    /// window (and whose back-off has passed) are due. Vanished files
    /// are dropped — their remove event takes care of the catalog.
    fn due(&mut self) -> Vec<PathBuf> {
        let now = Instant::now();
        let window = self.window;
        let mut due = Vec::new();
        self.entries.retain(|path, e| {
            let Some((size, mtime)) = reconcile::stat(path) else { return false };
            if size != e.size || mtime != e.mtime {
                e.size = size;
                e.mtime = mtime;
                e.stable_since = now;
            } else if now.duration_since(e.stable_since) >= window && now >= e.not_before {
                due.push(path.clone());
            }
            true
        });
        due
    }

    fn will_retry(&self, path: &Path) -> bool {
        self.entries
            .get(path)
            .is_some_and(|e| e.probe_failures < MAX_PROBE_RETRIES)
    }

    /// ffprobe couldn't read the file: schedule another attempt. Returns
    /// the delay, or None once the retries are used up.
    fn retry_later(&mut self, path: &Path) -> Option<Duration> {
        let e = self.entries.get_mut(path)?;
        if e.probe_failures >= MAX_PROBE_RETRIES {
            return None;
        }
        e.probe_failures += 1;
        let delay = (self.window * 2u32.pow(e.probe_failures)).min(MAX_PROBE_BACKOFF);
        e.not_before = Instant::now() + delay;
        Some(delay)
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
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

    // Watch all roots. The debouncer only coalesces events; new media
    // waits in the settle queue until it stops changing.
    let mut settle = SettleQueue::new(cfg.settle());
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
        // Short timeout so the runners get regular ticks; shorter still
        // while files are settling, so one is catalogued soon after its
        // window passes.
        let tick = if settle.is_empty() { Duration::from_secs(10) } else { Duration::from_secs(2) };
        match rx.recv_timeout(tick) {
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
                    match handle_path(&mut conn, &cfg, &roots, &mut settle, &path) {
                        Ok(Handled::CaptionSidecar) => {
                            if let Some(runner) = &mut enricher {
                                runner.note_caption_sidecar_changed(&path);
                            }
                        }
                        Ok(Handled::Nothing) => {}
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

        for path in settle.due() {
            let final_attempt = !settle.will_retry(&path);
            let catalogued = match catalog_settled(&mut conn, &cfg, &roots, &path, final_attempt) {
                Ok(Some(Extracted::NoTechInfo)) => match settle.retry_later(&path) {
                    Some(delay) => {
                        tracing::info!(
                            "{}: unreadable so far (copy still in flight?); retrying in {}s",
                            path.display(),
                            delay.as_secs()
                        );
                        continue;
                    }
                    None => {
                        tracing::warn!(
                            "{}: still unreadable after {} attempts; catalogued without tech info",
                            path.display(),
                            MAX_PROBE_RETRIES + 1
                        );
                        true
                    }
                },
                Ok(Some(Extracted::Ready)) => true,
                Ok(Some(Extracted::Failed)) | Ok(None) => false,
                Err(err) => {
                    tracing::warn!("cataloguing {}: {err:#}", path.display());
                    false
                }
            };
            settle.remove(&path);
            if catalogued {
                if let Some(runner) = &mut enricher {
                    runner.note_media_added();
                }
                if let Some(runner) = &mut analyzer {
                    runner.note_media_added();
                }
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
        let (new_media, extracted) =
            reconcile::reconcile_root(conn, &cfg.ffprobe_path, root, cfg.settle())?;
        tracing::info!("reconciled {} ({extracted} files extracted)", root.path);
        any_new |= new_media > 0;
    }
    Ok(any_new)
}

/// What an event under a root amounted to, for the runners: a caption
/// sidecar (enrichment wants to know) or nothing. New media is not an
/// outcome here — it goes to the settle queue, and counts once it is
/// catalogued from there. Ordered so a subtree scan can keep the
/// strongest of its files' outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Handled {
    Nothing,
    CaptionSidecar,
}

/// The root a path belongs to and the path relative to it. Longest
/// matching root wins, in case one root nests inside another. None for
/// paths outside every root, the root itself, and hidden components.
fn locate<'a>(roots: &'a [Root], path: &Path) -> Result<Option<(&'a Root, String)>> {
    let Some(root) = roots
        .iter()
        .filter(|r| path.starts_with(&r.path))
        .max_by_key(|r| r.path.len())
    else {
        return Ok(None);
    };
    let rel = path
        .strip_prefix(&root.path)
        .context("path not under root")?
        .to_string_lossy()
        .to_string();
    if rel.is_empty() || rel.split('/').any(|c| c.starts_with('.')) {
        return Ok(None);
    }
    Ok(Some((root, rel)))
}

/// React to one filesystem event path.
fn handle_path(
    conn: &mut Connection,
    cfg: &Config,
    roots: &[Root],
    settle: &mut SettleQueue,
    path: &Path,
) -> Result<Handled> {
    let Some((root, rel)) = locate(roots, path)? else {
        return Ok(Handled::Nothing);
    };

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
        return Ok(Handled::Nothing);
    }

    // Symlinks are never catalogued (the reconcile walker skips them as
    // well): a link planted on the share could point at anything on this
    // host, and the server would stream it.
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(Handled::Nothing);
    }

    if path.is_dir() {
        return scan_subtree(conn, cfg, root, settle, path);
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
        return Ok(Handled::Nothing);
    }
    if ext == "edl" {
        refresh_sidecar_sibling(conn, cfg, root, &rel, "edl", |abs, kf| {
            extract::segments::edl_mtime(abs) == kf.edl_mtime
        })?;
        return Ok(Handled::Nothing);
    }
    if ext == "srt" {
        // Nothing to catalog — the server reads the sidecar at request
        // time — but a corrected sidecar is a reason to run enrichment,
        // which re-embeds it into the video it was embedded in before.
        return Ok(Handled::CaptionSidecar);
    }
    if ext == "rttm" {
        // A speaker diarization beside the video (see media_db::rttm):
        // nothing to catalog, the server reads it at request time.
        return Ok(Handled::Nothing);
    }
    if ext == "jpg" || ext == "png" {
        refresh_art_siblings(conn, cfg, root, &rel)?;
        return Ok(Handled::Nothing);
    }
    if root.kind == media_db::MediaKind::Music
        && path.file_name().and_then(|n| n.to_str()) == Some(extract::MUSIC_META_FILE)
    {
        refresh_music_meta(conn, cfg, root, &rel)?;
        return Ok(Handled::Nothing);
    }

    if reconcile::media_mime(root, path).is_none() {
        return Ok(Handled::Nothing);
    }
    let Some((size, mtime)) = reconcile::stat(path) else { return Ok(Handled::Nothing) };

    // Spurious events (CIFS surfaces them for files nobody modified) and
    // overlapping ones (directory + file) both land here; skip files the
    // catalog already reflects.
    if catalog_reflects(conn, root, &rel, size, mtime)? {
        return Ok(Handled::Nothing);
    }
    settle.note(path, size, mtime);
    Ok(Handled::Nothing)
}

fn catalog_reflects(conn: &Connection, root: &Root, rel: &str, size: i64, mtime: i64) -> Result<bool> {
    Ok(files::lookup(conn, root.id, rel)?
        .is_some_and(|(_, db_size, db_mtime, status)| {
            db_size == size && db_mtime == mtime && status == "ready"
        }))
}

/// Catalog a file the settle queue found stable. None when there was
/// nothing to do (gone, or catalogued meanwhile by a reconcile pass).
/// Unless this is the final attempt, a file ffprobe can't read is put
/// back to pending — hidden from the server — so the retry can finalize
/// it properly.
fn catalog_settled(
    conn: &mut Connection,
    cfg: &Config,
    roots: &[Root],
    path: &Path,
    final_attempt: bool,
) -> Result<Option<Extracted>> {
    let Some((root, rel)) = locate(roots, path)? else { return Ok(None) };
    let Some(mime) = reconcile::media_mime(root, path) else { return Ok(None) };
    let Some((size, mtime)) = reconcile::stat(path) else { return Ok(None) };
    if catalog_reflects(conn, root, &rel, size, mtime)? {
        return Ok(None);
    }
    let id = files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
    let outcome = extract::extract_file(conn, &cfg.ffprobe_path, root, &rel, id)?;
    if outcome == Extracted::NoTechInfo && !final_attempt {
        files::upsert_pending(conn, root.id, &rel, size, mtime, root.kind, mime)?;
    }
    Ok(Some(outcome))
}

/// A directory appeared (new folder, or moved in): its media files join
/// the settle queue, its sidecars are handled as usual.
fn scan_subtree(
    conn: &mut Connection,
    cfg: &Config,
    root: &Root,
    settle: &mut SettleQueue,
    dir: &Path,
) -> Result<Handled> {
    let mut any = Handled::Nothing;
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
    {
        if entry.file_type().is_file() {
            any = any.max(handle_path(conn, cfg, std::slice::from_ref(root), settle, entry.path())?);
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
    // Indexed on `name` itself: lowercasing can change byte length (İ
    // becomes two chars), so a stem length taken from the lowercase copy
    // would slice `name` wrong — or out of bounds, which panics.
    let poster_stem = ["-poster.jpg", "-poster.png"].iter().find_map(|suffix| {
        let n = name.len().checked_sub(suffix.len())?;
        name.get(n..)
            .filter(|tail| tail.eq_ignore_ascii_case(suffix))
            .map(|_| format!("{dir_prefix}{}.", &name[..n]))
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("media-scanner-settle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn settle_queue_waits_for_stability_then_backs_off() {
        let path = scratch_file("episode.mp4");
        std::fs::write(&path, b"partial").unwrap();
        let (size, mtime) = reconcile::stat(&path).unwrap();

        let mut queue = SettleQueue::new(Duration::from_millis(300));
        queue.note(&path, size, mtime);
        assert!(queue.due().is_empty(), "not due before the window passes");

        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(queue.due(), vec![path.clone()]);

        // The copy resumed: the window starts over.
        std::fs::write(&path, b"partial, then some more").unwrap();
        assert!(queue.due().is_empty());
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(queue.due(), vec![path.clone()]);

        // Unreadable: retried with a growing delay, then given up on.
        assert!(queue.will_retry(&path));
        let mut delays = Vec::new();
        while let Some(delay) = queue.retry_later(&path) {
            delays.push(delay);
        }
        assert_eq!(delays.len(), MAX_PROBE_RETRIES as usize);
        assert!(delays.windows(2).all(|w| w[0] <= w[1]));
        assert!(!queue.will_retry(&path));
        assert!(queue.due().is_empty(), "back-off holds the file");

        // A vanished file leaves the queue.
        queue.remove(&path);
        queue.note(&path, size, mtime);
        std::fs::remove_file(&path).unwrap();
        assert!(queue.due().is_empty());
        assert!(queue.entries.is_empty());
    }
}
