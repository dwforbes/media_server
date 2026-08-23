use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use media_db::mime::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};
use media_db::queries::{self, files};
use rusqlite::Connection;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::didl::{self, xml_escape, DLNA_FEATURES};
use crate::objectid::ObjectId;
use crate::{soap, tree, xml};

const CDS_SERVICE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CMS_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

pub struct AppState {
    pub db: tokio::sync::Mutex<Connection>,
    pub update_id: AtomicU32,
    pub uuid: String,
    pub friendly_name: String,
    pub base_url: String,
    /// (120px icon bytes, 48px icon bytes, whether user-supplied).
    pub icon: (Vec<u8>, Vec<u8>, bool),
    pub recent_count: usize,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub vtt_cache: std::path::PathBuf,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/device.xml", get(device_xml))
        .route("/scpd/cds.xml", get(|| async { xml_response(xml::CDS_SCPD.to_string()) }))
        .route("/scpd/cms.xml", get(|| async { xml_response(xml::CMS_SCPD.to_string()) }))
        .route("/control/cds", post(cds_control))
        .route("/control/cms", post(cms_control))
        .route("/event/cds", any(event_stub))
        .route("/event/cms", any(event_stub))
        .route("/media/{id}", get(serve_media))
        .route("/art/{id}", get(serve_art))
        .route("/icon/120.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.0.clone()) }))
        .route("/icon/48.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.1.clone()) }))
        .route("/", get(index_page))
        .route("/playlist.m3u", get(|s: State<Arc<AppState>>| playlist(s, Path("all".into()))))
        .route("/playlist/{section}", get(playlist))
        .route("/playlist/id/{oid}", get(playlist_by_id))
        .route("/browse", get(|s: State<Arc<AppState>>| browse_page(s, Path("0".into()))))
        .route("/browse/{oid}", get(browse_page))
        .route("/item/{id}", get(item_page))
        .route("/play/{id}", get(play_page))
        .route("/subs/{id}", get(serve_subs))
        .with_state(state)
}

fn xml_response(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

async fn device_xml(State(state): State<Arc<AppState>>) -> Response {
    xml_response(xml::device_description(
        &state.uuid,
        &state.friendly_name,
        &state.base_url,
        state.icon.2,
    ))
}

/// Recursively collect the playable items under a tree node, deduplicated
/// by file id (a movie reachable via All, By Year, and By Genre appears
/// once, at its first-seen position).
fn flatten_items(
    conn: &Connection,
    oid: &ObjectId,
    recent_count: usize,
    depth: usize,
    seen: &mut std::collections::HashSet<i64>,
    out: &mut Vec<media_db::BrowseItem>,
) -> anyhow::Result<()> {
    for entry in tree::browse_children(conn, oid, recent_count)? {
        match entry {
            tree::Entry::Item { item, .. } => {
                if seen.insert(item.file_id) {
                    out.push(item);
                }
            }
            tree::Entry::Container { id, .. } => {
                if depth > 0 {
                    if let Some(child) = ObjectId::parse(&id) {
                        flatten_items(conn, &child, recent_count, depth - 1, seen, out)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Fully-qualified playlist titles: flattened lists lose their container
/// context, so TV and music entries carry it themselves.
fn playlist_title(item: &media_db::BrowseItem) -> String {
    match item.kind {
        media_db::MediaKind::Tv => {
            // Season-view items arrive pre-decorated as "NN - Title";
            // drop that so the qualified form doesn't number twice.
            let mut episode = item.clone();
            if let Some(n) = episode.episode {
                if let Some(rest) = episode.title.strip_prefix(&format!("{n:02} - ")) {
                    episode.title = rest.to_string();
                }
            }
            tree::recent_tv_title(&episode)
        }
        media_db::MediaKind::Music => tree::recent_track_title(item),
        media_db::MediaKind::Movies => match item.year {
            Some(year) if !item.title.ends_with(&format!("({year})")) => {
                format!("{} ({year})", item.title)
            }
            _ => item.title.clone(),
        },
    }
}

fn render_m3u(state: &AppState, entries: &[media_db::BrowseItem]) -> Response {
    let mut out = String::from("#EXTM3U\n");
    for item in entries {
        let secs = item.duration_ms.map(|ms| ms / 1000).unwrap_or(-1);
        let title = playlist_title(item).replace(['\n', '\r'], " ");
        out.push_str(&format!(
            "#EXTINF:{secs},{title}\n{}/media/{}\n",
            state.base_url, item.file_id
        ));
    }
    (
        [(header::CONTENT_TYPE, "audio/x-mpegurl; charset=utf-8")],
        out,
    )
        .into_response()
}

/// M3U for any node of the virtual tree, by its object id.
async fn playlist_by_id(State(state): State<Arc<AppState>>, Path(oid): Path<String>) -> Response {
    let oid = oid.trim_end_matches(".m3u");
    let Some(node) = ObjectId::parse(oid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let conn = state.db.lock().await;
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    let result = flatten_items(&conn, &node, state.recent_count, 5, &mut seen, &mut entries);
    drop(conn);
    match result {
        Ok(()) => render_m3u(&state, &entries),
        Err(err) => {
            tracing::warn!("playlist {oid}: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Discovery-free fallback: the catalog as M3U, playable by anything that
/// can open a URL — no SSDP involved. Friendly aliases over the tree.
async fn playlist(State(state): State<Arc<AppState>>, Path(section): Path<String>) -> Response {
    let node = match section.trim_end_matches(".m3u") {
        "all" => ObjectId::Root,
        "movies" => ObjectId::Movies,
        "tv" => ObjectId::Tv,
        "music" => ObjectId::Music,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let conn = state.db.lock().await;
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    let result = flatten_items(&conn, &node, state.recent_count, 5, &mut seen, &mut entries);
    drop(conn);
    match result {
        Ok(()) => render_m3u(&state, &entries),
        Err(err) => {
            tracing::warn!("playlist {section}: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// HTML mirror of the virtual tree: every container is browsable and
/// offers its playlist; items link to their streams.
async fn browse_page(State(state): State<Arc<AppState>>, Path(oid): Path<String>) -> Response {
    let Some(node) = ObjectId::parse(&oid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let conn = state.db.lock().await;
    let (title, parent_id) = tree::browse_metadata(&conn, &node)
        .ok()
        .map(|entry| match entry {
            tree::Entry::Container { title, parent, .. } => (title, Some(parent)),
            tree::Entry::Item { item, .. } => (item.title, None),
        })
        .unwrap_or_else(|| ("Browse".to_string(), None));
    // "Back to <parent>" one level up; the root points at itself, so skip.
    let back_link = parent_id
        .filter(|p| *p != oid)
        .and_then(|p| {
            let parent_node = ObjectId::parse(&p)?;
            let parent_title = match tree::browse_metadata(&conn, &parent_node).ok()? {
                tree::Entry::Container { title, .. } => title,
                tree::Entry::Item { item, .. } => item.title,
            };
            let label = if parent_node == ObjectId::Root {
                "Back to top".to_string()
            } else {
                format!("Back to {}", xml_escape(&parent_title))
            };
            Some(format!("<p><a href=\"/browse/{p}\">← {label}</a></p>"))
        })
        .unwrap_or_default();
    let children = tree::browse_children(&conn, &node, state.recent_count);
    drop(conn);
    let children = match children {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("browse {oid}: {err:#}");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let mut rows = String::new();
    for entry in children {
        match entry {
            tree::Entry::Container { id, title, .. } => rows.push_str(&format!(
                "<li>📁 <a href=\"/browse/{id}\">{}</a> \
                 <small><a href=\"/playlist/id/{id}.m3u\">[playlist]</a></small></li>",
                xml_escape(&title)
            )),
            tree::Entry::Item { item, .. } => {
                let chip = if item.kind == media_db::MediaKind::Music {
                    String::new()
                } else {
                    uhd_chip(is_uhd(item.width, item.height))
                };
                rows.push_str(&format!(
                    "<li>{chip}<a href=\"/item/{0}\">{1}</a> \
                     <small><a href=\"{2}/media/{0}\">[▶ play]</a></small></li>",
                    item.file_id,
                    xml_escape(&item.title),
                    state.base_url
                ))
            }
        }
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{}</title>\
         <body style=\"font-family:sans-serif;max-width:44em;margin:2em auto\">\
         <p><a href=\"/browse\">⌂ top</a></p><h1>{}</h1>{back_link}\
         <p><a href=\"/playlist/id/{oid}.m3u\">Playlist of everything below this point</a></p>\
         <ul style=\"list-style:none;padding:0;line-height:1.7\">{rows}</ul>",
        xml_escape(&title),
        xml_escape(&title)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// Minimal index so the base URL is self-explanatory in a browser.
async fn index_page(State(state): State<Arc<AppState>>) -> Response {
    let conn = state.db.lock().await;
    let counts: [(String, i64); 3] = ["movies", "tv", "music"].map(|kind| {
        let n = conn
            .query_row(
                "SELECT count(*) FROM files WHERE kind = ?1 AND status = 'ready'",
                [kind],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (kind.to_string(), n)
    });
    drop(conn);
    let name = xml_escape(&state.friendly_name);
    let rows: String = counts
        .iter()
        .map(|(kind, n)| {
            format!(
                "<li><a href=\"/playlist/{kind}.m3u\">{kind}.m3u</a> — {n} items</li>"
            )
        })
        .collect();
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{name}</title>\
         <body style=\"font-family:sans-serif;max-width:40em;margin:2em auto\">\
         <h1>{name}</h1>\
         <p>UPnP/DLNA media server. Clients normally discover it automatically; \
         anything that can open a URL can also use these playlists directly:</p>\
         <ul><li><a href=\"/playlist.m3u\">playlist.m3u</a> — everything</li>{rows}</ul>\
         <p><a href=\"/browse\">Browse the virtual library</a> — every folder \
         (by genre, decade, rating, series/season, artist/album) offers its own playlist.</p>\
         <p>Device description: <a href=\"/device.xml\">device.xml</a></p>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

fn srt_to_vtt(srt: &str) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for line in srt.lines() {
        let line = line.trim_end_matches('\r');
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

fn vtt_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], body).into_response()
}

fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

const TEXT_SUB_CODECS: &[&str] =
    &["subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "subviewer"];

/// The ordinal (among subtitle streams) of the best text subtitle track:
/// English text track preferred, else the first text track. None when the
/// file has no text subtitles (bitmap PGS/VobSub can't become WebVTT).
async fn text_sub_stream(ffprobe: &str, path: &std::path::Path) -> Option<usize> {
    let out = tokio::process::Command::new(ffprobe)
        .args([
            "-v", "error", "-select_streams", "s",
            "-show_entries", "stream=codec_name:stream_tags=language",
            "-of", "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut first_text = None;
    for (ordinal, line) in String::from_utf8_lossy(&out.stdout).lines().enumerate() {
        let mut fields = line.split(',');
        let codec = fields.next().unwrap_or("").trim();
        let lang = fields.next().unwrap_or("").trim().to_lowercase();
        if !TEXT_SUB_CODECS.contains(&codec) {
            continue;
        }
        if lang == "eng" || lang == "en" {
            return Some(ordinal);
        }
        first_text.get_or_insert(ordinal);
    }
    first_text
}

/// Subtitles as WebVTT: the .srt sidecar when present, else the embedded
/// track extracted with ffmpeg (demux-only) and cached beside the catalog.
async fn serve_subs(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Ok(id) = id.trim_end_matches(".vtt").parse::<i64>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let servable = {
        let conn = state.db.lock().await;
        files::servable(&conn, id)
    };
    let Ok(Some(servable)) = servable else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // 1. Sidecar .srt, converted on the fly.
    let srt_path = servable.abs_path.with_extension("srt");
    if let Ok(bytes) = std::fs::read(&srt_path) {
        if let Some(text) = media_db::textenc::decode_subtitle_text(&bytes) {
            return vtt_response(srt_to_vtt(&text));
        }
    }

    // 2. Cached extraction, unless the media file changed since.
    let cache = state.vtt_cache.join(format!("{id}.vtt"));
    let media_mtime = file_mtime(&servable.abs_path);
    if let (Some(cache_time), Some(media_time)) = (file_mtime(&cache), media_mtime) {
        if cache_time >= media_time {
            if let Ok(body) = std::fs::read_to_string(&cache) {
                return vtt_response(body);
            }
        }
    }

    // 3. Extract the best text track (demux only, no decoding). Files with
    // only bitmap tracks (PGS/VobSub) have nothing extractable.
    let Some(ordinal) = text_sub_stream(&state.ffprobe, &servable.abs_path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let output = tokio::process::Command::new(&state.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&servable.abs_path)
        .args(["-map", &format!("0:s:{ordinal}"), "-f", "webvtt", "-"])
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            let body = String::from_utf8_lossy(&out.stdout).to_string();
            let _ = std::fs::write(&cache, &body);
            vtt_response(body)
        }
        Ok(out) => {
            tracing::debug!(
                "no extractable subtitles for {id}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            StatusCode::NOT_FOUND.into_response()
        }
        Err(err) => {
            tracing::debug!("ffmpeg unavailable for subtitle extraction: {err}");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// In-browser player: native <video> controls, poster art, and the .srt
/// sidecar as a selectable subtitle track.
async fn play_page(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    let conn = state.db.lock().await;
    let detail = files::detail(&conn, id);
    let servable = files::servable(&conn, id);
    drop(conn);
    let (Ok(Some(detail)), Ok(Some(servable))) = (detail, servable) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut heading = xml_escape(&detail.title);
    if let Some(year) = detail.year {
        heading.push_str(&format!(" ({year})"));
    }
    let poster = if detail.has_art {
        format!(" poster=\"/art/{id}\"")
    } else {
        String::new()
    };
    // Sidecar and cached subtitles are instant: a plain server-rendered
    // <track>. Extraction-needed subtitles (embedded track, no sidecar, no
    // cache yet) can take a minute of ffmpeg demuxing on a big file — so
    // the video starts immediately and a few lines of script fetch the
    // track asynchronously with a visible status, attaching it when ready.
    let instant_subs = servable.abs_path.with_extension("srt").is_file()
        || state.vtt_cache.join(format!("{id}.vtt")).is_file();
    let (track, subs_async) = if instant_subs {
        (
            format!(
                "<track kind=\"subtitles\" src=\"/subs/{id}.vtt\" \
                 srclang=\"en\" label=\"Subtitles\" default>"
            ),
            String::new(),
        )
    } else if text_sub_stream(&state.ffprobe, &servable.abs_path).await.is_some() {
        (
            String::new(),
            format!(
                "<p id=\"subs\" style=\"color:#888;font-size:.85em\">⏳ Extracting \
                 subtitles from the file — the video can play meanwhile; captions \
                 appear when ready (may take a minute for large files)…</p>\
                 <script>\
                 fetch('/subs/{id}.vtt').then(function(r) {{\
                   if (!r.ok) throw 0; return r.blob();\
                 }}).then(function(b) {{\
                   var t = document.createElement('track');\
                   t.kind = 'subtitles'; t.label = 'Subtitles'; t.srclang = 'en';\
                   t.src = URL.createObjectURL(b); t.default = true;\
                   var v = document.querySelector('video');\
                   v.appendChild(t); t.track.mode = 'showing';\
                   document.getElementById('subs').textContent = 'Subtitles ready.';\
                 }}).catch(function() {{\
                   document.getElementById('subs').textContent = \
                     'Subtitles could not be extracted.';\
                 }});\
                 </script>"
            ),
        )
    } else {
        (String::new(), String::new())
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{heading}</title>\
         <body style=\"font-family:sans-serif;max-width:60em;margin:1.5em auto;\
         background:#111;color:#ddd\">\
         <p><a href=\"/item/{id}\" style=\"color:#9cf\">← details</a></p>\
         <h2>{heading}</h2>\
         <video controls autoplay playsinline{poster} \
          style=\"width:100%;max-height:80vh;background:#000\">\
         <source src=\"{}/media/{id}\" type=\"{}\">{track}\
         Your browser cannot play this format.</video>{subs_async}",
        state.base_url,
        xml_escape(&servable.mime)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

fn is_uhd(width: Option<i64>, height: Option<i64>) -> bool {
    width.unwrap_or(0) > 1920 || height.unwrap_or(0) > 1080
}

/// A small "4K" chip; hidden-but-space-reserving for non-4K rows so
/// listing titles align.
fn uhd_chip(uhd: bool) -> String {
    let visibility = if uhd { "" } else { "visibility:hidden" };
    format!(
        "<span style=\"display:inline-block;font-size:.7em;border:1px solid #999;\
         border-radius:3px;padding:0 .3em;margin-right:.5em;color:#666;\
         vertical-align:middle;{visibility}\">4K</span>"
    )
}

fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn human_duration(ms: i64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Detail page: everything the catalog knows about one item.
async fn item_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    let conn = state.db.lock().await;
    let detail = files::detail(&conn, id);
    let genre_pairs = queries::genres_for_file(&conn, id).unwrap_or_default();
    let director_pairs = queries::directors_for_file(&conn, id).unwrap_or_default();
    drop(conn);
    let detail = match detail {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!("item {id}: {err:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut heading = xml_escape(&detail.title);
    if let Some(year) = detail.year {
        heading.push_str(&format!(" ({year})"));
    }
    if is_uhd(detail.width, detail.height) {
        heading.push_str(" <span style=\"font-size:.45em;border:1.5px solid #666;\
            border-radius:4px;padding:.05em .35em;color:#555;vertical-align:middle\">4K</span>");
    }
    let mut subtitle = String::new();
    if let (Some(series), Some(season), Some(episode)) =
        (&detail.series, detail.season, detail.episode)
    {
        subtitle = format!("{} — Season {season}, Episode {episode}", xml_escape(series));
    } else if let Some(artist) = &detail.artist {
        subtitle = xml_escape(artist);
        if let Some(album) = &detail.album {
            subtitle.push_str(&format!(" — {}", xml_escape(album)));
            if let Some(n) = detail.track_no {
                subtitle.push_str(&format!(", track {n}"));
            }
        }
    }

    let mut facts: Vec<(&str, String)> = Vec::new();
    let imdb = detail
        .imdb_id
        .as_deref()
        .filter(|id| id.starts_with("tt"));
    match (detail.rating, imdb) {
        (Some(rating), Some(id)) => facts.push((
            "IMDb",
            format!(
                "{rating:.1} / 10 — <a href=\"https://www.imdb.com/title/{id}/\">{id}</a>"
            ),
        )),
        (Some(rating), None) => facts.push(("IMDb rating", format!("{rating:.1} / 10"))),
        (None, Some(id)) => facts.push((
            "IMDb",
            format!("<a href=\"https://www.imdb.com/title/{id}/\">{id}</a>"),
        )),
        (None, None) => {}
    }
    // Genre and director labels link into their browse categories.
    if !genre_pairs.is_empty() {
        let prefix = match detail.kind {
            media_db::MediaKind::Music => "mu:genre",
            _ => "mv:genre",
        };
        let links: Vec<String> = genre_pairs
            .iter()
            .map(|(gid, name)| {
                format!("<a href=\"/browse/{prefix}:{gid}\">{}</a>", xml_escape(name))
            })
            .collect();
        facts.push(("Genre", links.join(", ")));
    } else if let Some(genre) = &detail.genre {
        facts.push(("Genre", xml_escape(genre)));
    }
    if let Some(collection) = &detail.collection {
        facts.push((
            "Franchise",
            format!(
                "<a href=\"/browse/{}\">{}</a>",
                ObjectId::MoviesFranchise(collection.clone()).to_id(),
                xml_escape(collection)
            ),
        ));
    }
    if !director_pairs.is_empty() {
        let links: Vec<String> = director_pairs
            .iter()
            .map(|(did, name)| {
                format!("<a href=\"/browse/mv:director:{did}\">{}</a>", xml_escape(name))
            })
            .collect();
        facts.push(("Director", links.join(", ")));
    } else if let Some(director) = &detail.director {
        facts.push(("Director", xml_escape(director)));
    }
    if let Some(ms) = detail.duration_ms {
        facts.push(("Duration", human_duration(ms)));
    }
    if let (Some(w), Some(h)) = (detail.width, detail.height) {
        facts.push(("Resolution", format!("{w} × {h}")));
    }
    let codecs = match (&detail.video_codec, &detail.audio_codec) {
        (Some(v), Some(a)) => Some(format!("{v} video, {a} audio")),
        (Some(v), None) => Some(format!("{v} video")),
        (None, Some(a)) => Some(a.to_string()),
        (None, None) => None,
    };
    if let Some(codecs) = codecs {
        facts.push(("Codecs", xml_escape(&codecs)));
    }
    if let Some(container) = &detail.container {
        facts.push(("Container", xml_escape(container)));
    }
    facts.push(("File size", human_size(detail.size)));
    facts.push(("MIME type", xml_escape(&detail.mime)));
    facts.push(("Added", xml_escape(&detail.added_at_text)));
    facts.push(("File", xml_escape(&detail.rel_path)));

    let art = if detail.has_art {
        format!(
            "<img src=\"/art/{id}\" alt=\"\" style=\"float:right;max-width:220px;\
             margin:0 0 1em 1.5em;border-radius:6px\">"
        )
    } else {
        String::new()
    };
    let play_links = if detail.kind == media_db::MediaKind::Music {
        format!("<p><a href=\"{}/media/{id}\" style=\"font-size:1.1em\">▶ Play</a></p>", state.base_url)
    } else {
        // Known browser blind spots, verified the hard way: Firefox neither
        // range-streams non-WebM Matroska (it downloads the whole file
        // linearly) nor, on most platforms, decodes HEVC; Safari does HEVC
        // fine (VideoToolbox) but cannot open the Matroska container at
        // all. Warn rather than let anyone pull gigabytes for nothing.
        let user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let is_firefox = user_agent.contains("Firefox/") && !user_agent.contains("Seamonkey");
        let is_safari = user_agent.contains("Safari/")
            && !user_agent.contains("Chrome/")
            && !user_agent.contains("Chromium/")
            && !user_agent.contains("CriOS/")
            && !user_agent.contains("Edg/");
        let is_mkv = detail.mime == "video/x-matroska";
        let is_hevc = matches!(
            detail.video_codec.as_deref(),
            Some("hevc") | Some("h265") | Some("x265")
        );
        let problem = if is_firefox && (is_mkv || is_hevc) {
            let what = match (is_mkv, is_hevc) {
                (true, true) => "an MKV with HEVC video",
                (true, false) => "an MKV file",
                _ => "HEVC video",
            };
            Some((what, "Firefox"))
        } else if is_safari && is_mkv {
            Some(("an MKV file", "Safari"))
        } else {
            None
        };
        let warning = problem
            .map(|(what, browser)| {
                format!(
                    "<p style=\"color:#c00;font-size:.85em;max-width:38em\">⚠ This is {what}, \
                     which {browser} likely cannot play — it may download the entire file \
                     without ever starting. Use the direct stream link in VLC instead.</p>"
                )
            })
            .unwrap_or_default();
        format!(
            "<p><a href=\"/play/{id}\" style=\"font-size:1.1em\">▶ Play in browser</a> \
             &nbsp; <small><a href=\"{}/media/{id}\">direct stream</a></small></p>{warning}",
            state.base_url
        )
    };
    let plot = detail
        .plot
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("<p style=\"max-width:38em\">{}</p>", xml_escape(p)))
        .unwrap_or_default();
    let subtitle_html = if subtitle.is_empty() {
        String::new()
    } else {
        format!("<p><strong>{subtitle}</strong></p>")
    };
    let rows: String = facts
        .iter()
        .map(|(k, v)| {
            format!(
                "<tr><td style=\"color:#666;padding-right:1em;white-space:nowrap\">{k}</td>\
                 <td>{v}</td></tr>"
            )
        })
        .collect();
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{heading}</title>\
         <body style=\"font-family:sans-serif;max-width:46em;margin:2em auto;line-height:1.5\">\
         <p><a href=\"/browse\">⌂ browse</a></p>{art}<h1 style=\"margin-bottom:.2em\">{heading}</h1>\
         {subtitle_html}{plot}{play_links}\
         <table style=\"border-collapse:collapse\">{rows}</table>"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

fn icon_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CACHE_CONTROL, "max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// GENA eventing is not implemented; polling clients work fine without it.
async fn event_stub() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

fn soap_fault(code: u32, description: &str) -> Response {
    let mut resp = xml_response(soap::fault(code, description));
    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    resp
}

async fn cds_control(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .and_then(soap::action_from_header)
        .unwrap_or_default();
    let update_id = state.update_id.load(Ordering::Relaxed);

    match action.as_str() {
        "Browse" => browse(&state, &body, update_id).await,
        "GetSystemUpdateID" => xml_response(soap::envelope(
            CDS_SERVICE,
            "GetSystemUpdateID",
            &[("Id", update_id.to_string())],
        )),
        "GetSearchCapabilities" => xml_response(soap::envelope(
            CDS_SERVICE,
            "GetSearchCapabilities",
            &[("SearchCaps", String::new())],
        )),
        "GetSortCapabilities" => xml_response(soap::envelope(
            CDS_SERVICE,
            "GetSortCapabilities",
            &[("SortCaps", String::new())],
        )),
        "Search" => soap_fault(602, "Search is not implemented"),
        other => {
            tracing::debug!("unsupported CDS action {other:?}");
            soap_fault(401, "Invalid Action")
        }
    }
}

async fn browse(state: &AppState, body: &str, update_id: u32) -> Response {
    let object_id = soap::param(body, "ObjectID").unwrap_or_else(|| "0".into());
    let browse_flag =
        soap::param(body, "BrowseFlag").unwrap_or_else(|| "BrowseDirectChildren".into());
    let starting_index: usize = soap::param(body, "StartingIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let requested_count: usize = soap::param(body, "RequestedCount")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let Some(oid) = ObjectId::parse(&object_id) else {
        return soap_fault(701, "No such object");
    };

    let conn = state.db.lock().await;
    let (didl_xml, number_returned, total_matches) = if browse_flag == "BrowseMetadata" {
        match tree::browse_metadata(&conn, &oid) {
            Ok(entry) => (didl::render(&[entry], &state.base_url), 1usize, 1usize),
            Err(err) => {
                tracing::debug!("BrowseMetadata {object_id:?} failed: {err:#}");
                return soap_fault(701, "No such object");
            }
        }
    } else {
        match tree::browse_children(&conn, &oid, state.recent_count) {
            Ok(entries) => {
                let total = entries.len();
                let end = if requested_count == 0 {
                    total
                } else {
                    (starting_index + requested_count).min(total)
                };
                let start = starting_index.min(total);
                let page = &entries[start..end.max(start)];
                (didl::render(page, &state.base_url), page.len(), total)
            }
            Err(err) => {
                tracing::warn!("Browse {object_id:?} failed: {err:#}");
                return soap_fault(701, "No such object");
            }
        }
    };
    drop(conn);

    xml_response(soap::envelope(
        CDS_SERVICE,
        "Browse",
        &[
            ("Result", xml_escape(&didl_xml)),
            ("NumberReturned", number_returned.to_string()),
            ("TotalMatches", total_matches.to_string()),
            ("UpdateID", update_id.to_string()),
        ],
    ))
}

async fn cms_control(headers: HeaderMap, _body: String) -> Response {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .and_then(soap::action_from_header)
        .unwrap_or_default();

    match action.as_str() {
        "GetProtocolInfo" => {
            let source = VIDEO_EXTENSIONS
                .iter()
                .chain(AUDIO_EXTENSIONS)
                .map(|(_, mime)| format!("http-get:*:{mime}:*"))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",");
            xml_response(soap::envelope(
                CMS_SERVICE,
                "GetProtocolInfo",
                &[("Source", xml_escape(&source)), ("Sink", String::new())],
            ))
        }
        "GetCurrentConnectionIDs" => xml_response(soap::envelope(
            CMS_SERVICE,
            "GetCurrentConnectionIDs",
            &[("ConnectionIDs", "0".to_string())],
        )),
        "GetCurrentConnectionInfo" => xml_response(soap::envelope(
            CMS_SERVICE,
            "GetCurrentConnectionInfo",
            &[
                ("RcsID", "-1".to_string()),
                ("AVTransportID", "-1".to_string()),
                ("ProtocolInfo", String::new()),
                ("PeerConnectionManager", String::new()),
                ("PeerConnectionID", "-1".to_string()),
                ("Direction", "Output".to_string()),
                ("Status", "OK".to_string()),
            ],
        )),
        other => {
            tracing::debug!("unsupported CMS action {other:?}");
            soap_fault(401, "Invalid Action")
        }
    }
}

async fn serve_art(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    let source = {
        let conn = state.db.lock().await;
        files::art_source(&conn, id)
    };
    let source = match source {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!("looking up art {id}: {err:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<u8>, String)> {
        match source {
            files::ArtSource::File(path) => {
                let mime = if path.extension().and_then(|e| e.to_str()) == Some("png") {
                    "image/png"
                } else {
                    "image/jpeg"
                };
                Ok((std::fs::read(path)?, mime.to_string()))
            }
            files::ArtSource::Embedded(media_path) => {
                use lofty::file::TaggedFileExt;
                let tagged = lofty::read_from_path(&media_path)?;
                let tag = tagged
                    .primary_tag()
                    .or_else(|| tagged.first_tag())
                    .ok_or_else(|| anyhow::anyhow!("no tag"))?;
                let picture = tag
                    .pictures()
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("no embedded picture"))?;
                let mime = picture
                    .mime_type()
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "image/jpeg".to_string());
                Ok((picture.data().to_vec(), mime))
            }
        }
    })
    .await;
    match result {
        Ok(Ok((bytes, mime))) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "max-age=86400".to_string()),
            ],
            bytes,
        )
            .into_response(),
        // The catalog says this item HAS art, so a read failure now is
        // transient (spun-down drive, EIO, a file mid-replacement) — not
        // "no such art". 404 is authoritative and clients cache it, which
        // turns a momentary blip into a permanently missing poster.
        Ok(Err(err)) => {
            tracing::warn!("art {id} temporarily unavailable: {err:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
            )
                .into_response()
        }
        Err(err) => {
            tracing::warn!("art task failed: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn serve_media(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    request: Request,
) -> Response {
    let servable = {
        let conn = state.db.lock().await;
        files::servable(&conn, id)
    };
    let servable = match servable {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!("looking up file {id}: {err:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // ServeFile handles Range/If-* headers and conditional responses.
    let result = ServeFile::new(&servable.abs_path).oneshot(request).await;
    let mut response = match result {
        Ok(resp) => resp.map(Body::new),
        Err(err) => {
            tracing::warn!("serving {}: {err}", servable.abs_path.display());
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let headers = response.headers_mut();
    if let Ok(value) = servable.mime.parse() {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        "contentFeatures.dlna.org",
        format!("http-get:*:{}:{}", servable.mime, DLNA_FEATURES)
            .parse()
            .unwrap(),
    );
    headers.insert("transferMode.dlna.org", "Streaming".parse().unwrap());
    response
}
