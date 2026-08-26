use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use media_db::mime::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};
use media_db::queries::{self, files, music, tv};
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
    /// Subtitle extractions in progress, by file id. The first request
    /// spawns one detached ffmpeg and parks a receiver here; every later
    /// request for the same file awaits that receiver instead of starting
    /// another ffmpeg. The entry is removed (after the cache file lands)
    /// just before the sender is dropped, which is what wakes the waiters.
    pub subs_inflight: std::sync::Mutex<std::collections::HashMap<i64, tokio::sync::watch::Receiver<()>>>,
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
        .route("/search", get(search_page))
        .route("/playlist/search", get(search_playlist))
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

/// Cap on search results, so a one-letter query can't build an unbounded
/// page or DIDL document.
const SEARCH_LIMIT: usize = 500;

#[derive(serde::Deserialize)]
struct SearchQuery {
    /// Named "mq", not "q": browsers key saved form history by field name,
    /// and "q" is the web's universal search-box name, so the dropdown
    /// filled with queries typed into other sites entirely.
    #[serde(rename = "mq", default)]
    q: String,
    /// ObjectID of the container to search within ("0" = everything).
    #[serde(rename = "in", default)]
    scope: String,
}

/// Which media kinds a UPnP upnp:class constraint admits.
#[derive(Clone, Copy, PartialEq)]
enum ClassFilter {
    Any,
    Video,
    Audio,
    /// A class we hold nothing of (images, container-only searches).
    Nothing,
}

fn class_allows(filter: ClassFilter, kind: media_db::MediaKind) -> bool {
    match filter {
        ClassFilter::Any => true,
        ClassFilter::Video => kind != media_db::MediaKind::Music,
        ClassFilter::Audio => kind == media_db::MediaKind::Music,
        ClassFilter::Nothing => false,
    }
}

/// Everything a search term is matched against.
fn search_haystack(item: &media_db::BrowseItem) -> String {
    let mut hay = item.title.to_lowercase();
    for extra in [
        &item.series,
        &item.artist,
        &item.album,
        &item.genre,
        &item.director,
    ] {
        if let Some(value) = extra {
            hay.push(' ');
            hay.push_str(&value.to_lowercase());
        }
    }
    if let Some(year) = item.year {
        hay.push(' ');
        hay.push_str(&year.to_string());
    }
    hay
}

/// Items anywhere under `scope` matching every term (AND, case-insensitive
/// substrings). Reuses the playlist flatten, so any node of the virtual
/// tree — a genre, a series, a decade, the root — can be a search scope.
fn search_scope(
    conn: &Connection,
    scope: &ObjectId,
    terms: &[String],
    class: ClassFilter,
    recent_count: usize,
) -> anyhow::Result<Vec<media_db::BrowseItem>> {
    let mut seen = std::collections::HashSet::new();
    let mut all = Vec::new();
    flatten_items(conn, scope, recent_count, 5, &mut seen, &mut all)?;
    let mut hits: Vec<media_db::BrowseItem> = all
        .into_iter()
        .filter(|i| class_allows(class, i.kind))
        .filter(|i| {
            let hay = search_haystack(i);
            terms.iter().all(|t| hay.contains(t.as_str()))
        })
        .map(|mut i| {
            i.title = playlist_title(&i);
            i
        })
        .collect();
    hits.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    hits.truncate(SEARCH_LIMIT);
    Ok(hits)
}

fn query_terms(q: &str) -> Vec<String> {
    q.split_whitespace().map(|t| t.to_lowercase()).collect()
}

/// Extract terms and a class constraint from a ContentDirectory
/// SearchCriteria expression. The full grammar is a boolean expression
/// language; clients in practice send `dc:title contains "x"` clauses
/// optionally ANDed with an `upnp:class` constraint. Quoted operands
/// become search terms, upnp:class operands become the class filter, and
/// anything else is ignored — widening results rather than failing.
fn parse_search_criteria(criteria: &str) -> (Vec<String>, ClassFilter) {
    let text = criteria.trim();
    if text.is_empty() || text == "*" {
        return (Vec::new(), ClassFilter::Any);
    }
    let mut terms = Vec::new();
    let mut class = ClassFilter::Any;
    let mut cursor = 0usize;
    let mut last_end = 0usize;
    while let Some(rel) = text[cursor..].find('"') {
        let start = cursor + rel;
        let Some(end_rel) = text[start + 1..].find('"') else { break };
        let end = start + 1 + end_rel;
        let literal = &text[start + 1..end];
        let preceding = &text[last_end..start];
        if preceding.contains("upnp:class") {
            class = if literal.contains("audioItem") {
                ClassFilter::Audio
            } else if literal.contains("videoItem") {
                ClassFilter::Video
            } else if literal.contains("imageItem") || literal.contains("container") {
                ClassFilter::Nothing
            } else {
                ClassFilter::Any
            };
        } else if !literal.is_empty() && literal != "*" {
            terms.push(literal.to_lowercase());
        }
        last_end = end + 1;
        cursor = end + 1;
    }
    (terms, class)
}

/// The search box shown on browse pages, scoped to the current container.
fn search_form(scope_oid: &str, current: &str) -> String {
    format!(
        "<form action=\"/search\" method=\"get\" style=\"margin:.6em 0\">\
         <input type=\"hidden\" name=\"in\" value=\"{}\">\
         <input name=\"mq\" value=\"{}\" autocomplete=\"off\" \
          placeholder=\"Search here and below…\" \
          style=\"padding:.35em;width:16em\"> \
         <button type=\"submit\">Search</button></form>",
        xml_escape(scope_oid),
        xml_escape(current)
    )
}

/// Whether a child is worth walking when flattening its parent. A section
/// exposes the same items through many views — All Movies, Recently Added,
/// By Year, By Decade, By Genre, By Director, By Franchise, By Rating,
/// Folders — so walking all of them visits every movie six to nine times
/// and discards all but the first. One exhaustive view per section covers
/// the same items at a fraction of the cost. Every other node's children
/// are already disjoint, so they are walked as-is.
///
/// Note this also drops alternate renditions (a second copy of a movie is
/// merged away in All Movies but listed separately under Folders), which
/// is what you want here: one entry per work, not per file.
fn covering_child(parent: &ObjectId, child: &ObjectId) -> bool {
    use ObjectId::*;
    match (parent, child) {
        (Movies, MoviesAll) => true,
        (Movies, _) => false,
        (Music, MusicAlbums) => true,
        (Music, _) => false,
        (Tv, TvSeries(_)) => true,
        (Tv, _) => false,
        _ => true,
    }
}

/// Recursively collect the playable items under a tree node, deduplicated
/// by file id (an item reachable through several views appears once, at
/// its first-seen position).
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
                        if covering_child(oid, &child) {
                            flatten_items(conn, &child, recent_count, depth - 1, seen, out)?;
                        }
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
            // Items collected from Recently Added arrive already qualified
            // ("Series SxxEyy - Title"); qualifying again would repeat the
            // series. Detect that and leave them alone.
            if let Some(series) = &item.series {
                if item.title.starts_with(series.as_str()) {
                    return item.title.clone();
                }
            }
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
        media_db::MediaKind::Music => {
            if let Some(artist) = &item.artist {
                if item.title.starts_with(artist.as_str()) {
                    return item.title.clone();
                }
            }
            tree::recent_track_title(item)
        }
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

/// Search results page, scoped to a container and everything below it.
async fn search_page(State(state): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> Response {
    let scope_id = if q.scope.is_empty() { "0".to_string() } else { q.scope.clone() };
    let Some(scope) = ObjectId::parse(&scope_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let terms = query_terms(&q.q);
    let conn = state.db.lock().await;
    let scope_title = tree::browse_metadata(&conn, &scope)
        .ok()
        .map(|e| match e {
            tree::Entry::Container { title, .. } => title,
            tree::Entry::Item { item, .. } => item.title,
        })
        .unwrap_or_else(|| "Media".to_string());
    let hits = if terms.is_empty() {
        Ok(Vec::new())
    } else {
        search_scope(&conn, &scope, &terms, ClassFilter::Any, state.recent_count)
    };
    drop(conn);
    let hits = match hits {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!("search {:?} in {scope_id}: {err:#}", q.q);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut rows = String::new();
    for item in &hits {
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
        ));
    }
    let summary = if terms.is_empty() {
        "Enter a search term.".to_string()
    } else if hits.is_empty() {
        format!("No matches in {}.", xml_escape(&scope_title))
    } else {
        let capped = if hits.len() == SEARCH_LIMIT {
            " (showing the first 500)"
        } else {
            ""
        };
        format!(
            "{} match{} in {}{capped} — \
             <a href=\"/playlist/search?mq={}&in={}\">playlist of these results</a>",
            hits.len(),
            if hits.len() == 1 { "" } else { "es" },
            xml_escape(&scope_title),
            urlencode(&q.q),
            urlencode(&scope_id)
        )
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>Search</title>\
         <body style=\"font-family:sans-serif;max-width:44em;margin:2em auto\">\
         <p><a href=\"/browse\">⌂ top</a></p><h1>Search</h1>\
         <p><a href=\"/browse/{}\">← Back to {}</a></p>{}\
         <p>{summary}</p>\
         <ul style=\"list-style:none;padding:0;line-height:1.7\">{rows}</ul>",
        xml_escape(&scope_id),
        xml_escape(&scope_title),
        search_form(&scope_id, &q.q)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// The same search results as an M3U.
async fn search_playlist(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let scope_id = if q.scope.is_empty() { "0".to_string() } else { q.scope.clone() };
    let Some(scope) = ObjectId::parse(&scope_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let terms = query_terms(&q.q);
    if terms.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let conn = state.db.lock().await;
    let hits = search_scope(&conn, &scope, &terms, ClassFilter::Any, state.recent_count);
    drop(conn);
    match hits {
        Ok(entries) => render_m3u(&state, &entries),
        Err(err) => {
            tracing::warn!("search playlist: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Minimal percent-encoding for values we put into our own links.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
    let search_box = search_form(&oid, "");
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
         <p><a href=\"/browse\">⌂ top</a></p><h1>{}</h1>{back_link}{search_box}\
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
    let search_box = search_form("0", "");
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
         {search_box}\
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

    // 3. Extract (or join an extraction already running for this file),
    // then serve whatever it left in the cache.
    let mut rx = {
        let mut inflight = state.subs_inflight.lock().unwrap_or_else(|e| e.into_inner());
        match inflight.get(&id) {
            Some(rx) => rx.clone(),
            None => {
                let (tx, rx) = tokio::sync::watch::channel(());
                inflight.insert(id, rx.clone());
                let state = state.clone();
                let path = servable.abs_path.clone();
                let cache = cache.clone();
                // Detached from the request: a client that navigates away
                // mid-extraction (closing the connection) must not kill the
                // ffmpeg that everyone else is waiting on.
                tokio::spawn(async move {
                    extract_subs(&state, id, &path, &cache).await;
                    state
                        .subs_inflight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    drop(tx);
                });
                rx
            }
        }
    };
    // Err once the sender is dropped, i.e. the extraction finished.
    let _ = rx.changed().await;
    match std::fs::read_to_string(&cache) {
        Ok(body) => vtt_response(body),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Extract the best text track (demux only, no decoding) to `cache`,
/// written via a temp file and rename so a concurrent reader never sees a
/// partial file. Files with only bitmap tracks (PGS/VobSub) have nothing
/// extractable and leave no cache file.
async fn extract_subs(state: &AppState, id: i64, path: &std::path::Path, cache: &std::path::Path) {
    let Some(ordinal) = text_sub_stream(&state.ffprobe, path).await else {
        tracing::debug!("no text subtitle track in {id}");
        return;
    };
    let output = tokio::process::Command::new(&state.ffmpeg)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", &format!("0:s:{ordinal}"), "-f", "webvtt", "-"])
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => {
            let tmp = cache.with_extension("vtt.part");
            let written = std::fs::write(&tmp, &out.stdout)
                .and_then(|_| std::fs::rename(&tmp, cache));
            if let Err(err) = written {
                tracing::warn!("could not write subtitle cache for {id}: {err}");
                let _ = std::fs::remove_file(&tmp);
            }
        }
        Ok(out) => {
            tracing::debug!(
                "no extractable subtitles for {id}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(err) => {
            tracing::debug!("ffmpeg unavailable for subtitle extraction: {err}");
        }
    }
}

/// Hover/focus card on the player page. Pure CSS: no script, and
/// :focus-within means keyboard users get it too. Touch devices can't
/// hover, which is why the season/episode context is also rendered
/// inline — the card is enrichment, not the only route to the facts.
const PLAYER_STYLE: &str = r#"<style>
.infowrap { position: relative; display: inline-block; }
.infowrap .card { display: none; position: absolute; left: 0; top: 1.7em; z-index: 10;
  width: 32em; max-width: 90vw; padding: .9em 1em; background: #1c1c1c; color: #ddd;
  border: 1px solid #444; border-radius: 6px; box-shadow: 0 8px 22px rgba(0,0,0,.55);
  font-size: .85em; line-height: 1.5; text-align: left; }
.infowrap:hover .card, .infowrap:focus-within .card { display: block; }
.infowrap .card img { float: right; width: 6em; margin: 0 0 .6em .9em; border-radius: 4px; }
.infowrap .card .facts { color: #9a9a9a; margin-top: .5em; }
.infowrap .card .plot { margin-top: .5em; }
/* Dark surfaces (the player page, the card on either page): browser
   default link colours — navy visited links especially — vanish on
   near-black, so pin every link state to a light blue. */
body.player a:link, body.player a:visited,
.infowrap .card a:link, .infowrap .card a:visited { color: #9cf; }
body.player a:hover, body.player a:active,
.infowrap .card a:hover, .infowrap .card a:active { color: #cef; }
</style>"#;

/// Resume support: the playback position rides in the URL fragment, so a
/// paused (or scrubbed) player yields a shareable/bookmarkable link that
/// picks up where it left off. Fragments never reach the server, so this
/// has to be client side. Also the keyboard: ← / → skip 10 seconds.
const RESUME_SCRIPT: &str = r#"<script>
(function () {
  var v = document.querySelector('video');
  if (!v) return;
  // timeupdate fires ~4x/second; flooring to whole seconds throttles the
  // stamping to once per second, which also keeps us far below browsers'
  // history-API rate limits. replaceState mutates the current session
  // entry rather than adding one, so history never grows.
  var last = -1;
  var honoured = true;   // false while an incoming #position is pending
  function stamp() {
    if (!honoured) return;   // never overwrite a URL we have not acted on yet
    var t = Math.floor(v.currentTime || 0);
    if (t === last) return;
    last = t;
    history.replaceState(null, '', location.pathname + (t > 0 ? '#' + t + 's' : ''));
  }
  v.addEventListener('timeupdate', stamp);
  v.addEventListener('pause', stamp);
  v.addEventListener('seeked', stamp);
  // Arrow keys skip 10 s. Registered on the capture phase and stopping
  // propagation so the native controls (which seek by their own step
  // when the <video> is focused) never see the key — otherwise the two
  // seeks would stack. Modified keys and text fields are left alone.
  document.addEventListener('keydown', function (e) {
    if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return;
    var step = e.key === 'ArrowLeft' ? -10 : e.key === 'ArrowRight' ? 10 : 0;
    if (!step) return;
    var t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    e.preventDefault();
    e.stopPropagation();
    var end = isFinite(v.duration) ? v.duration : Infinity;
    try { v.currentTime = Math.max(0, Math.min(end, (v.currentTime || 0) + step)); } catch (err) {}
  }, true);
  var start = parseFloat((location.hash || '').replace(/[^0-9.]/g, ''));
  if (!(start > 0)) return;
  honoured = false;
  function seek() {
    try { v.currentTime = start; } catch (e) {}
    honoured = true;
  }
  if (v.readyState >= 1) seek();
  else v.addEventListener('loadedmetadata', seek, { once: true });
  var note = document.getElementById('resume');
  if (note) {
    var m = Math.floor(start / 60), sec = Math.floor(start % 60);
    note.innerHTML = 'Resuming at ' + m + ':' + (sec < 10 ? '0' : '') + sec +
      ' — <a href="' + location.pathname + '">start from the beginning</a>';
  }
})();
</script>"#;

/// "Series — Season N, Episode M" with the series and season linked into
/// their browse pages, so an episode reached via search (or a bookmark)
/// is one click from its siblings rather than a trip back to the root.
fn tv_context_html(series: &str, season: i64, episode: i64) -> String {
    format!(
        "<a href=\"/browse/{}\">{}</a> — <a href=\"/browse/{}\">Season {season}</a>, Episode {episode}",
        ObjectId::TvSeries(series.to_string()).to_id(),
        xml_escape(series),
        ObjectId::TvSeason { series: series.to_string(), season }.to_id(),
    )
}

/// "Artist — Album" (optionally ", track N") with the artist and album
/// linked into their browse pages. The browse tree keys on the album
/// artist when one is set, so the links use that even though the text
/// shows the track's own artist (the two differ on compilations).
fn music_context_html(detail: &media_db::queries::files::ItemDetail, with_track: bool) -> String {
    let Some(artist) = detail.artist.as_deref() else {
        return String::new();
    };
    let key_artist = detail.album_artist.as_deref().unwrap_or(artist).to_string();
    let mut out = format!(
        "<a href=\"/browse/{}\">{}</a>",
        ObjectId::MusicArtist(key_artist.clone()).to_id(),
        xml_escape(artist)
    );
    if let Some(album) = detail.album.as_deref() {
        out.push_str(&format!(
            " — <a href=\"/browse/{}\">{}</a>",
            ObjectId::MusicAlbum { artist: key_artist, album: album.to_string() }.to_id(),
            xml_escape(album)
        ));
        if let (true, Some(n)) = (with_track, detail.track_no) {
            out.push_str(&format!(", track {n}"));
        }
    }
    out
}

/// The episode (or track) before and after this one, in the order the
/// season (album) browse page lists them. Renditions of one episode are
/// merged there, so a lower-quality copy still finds its place. Movies
/// have no natural neighbours and get (None, None).
fn neighbours(
    conn: &rusqlite::Connection,
    detail: &files::ItemDetail,
) -> (Option<media_db::BrowseItem>, Option<media_db::BrowseItem>) {
    let siblings = match (detail.kind, &detail.series, detail.season) {
        (media_db::MediaKind::Tv, Some(series), Some(season)) => {
            tv::episodes(conn, series, season)
        }
        (media_db::MediaKind::Music, _, _) => {
            // Same keys the browse tree uses: album artist first, and the
            // "Unknown" placeholders for untagged files.
            let artist = detail
                .album_artist
                .as_deref()
                .or(detail.artist.as_deref())
                .unwrap_or("Unknown Artist");
            let album = detail.album.as_deref().unwrap_or("Unknown Album");
            music::tracks_for_album(conn, artist, album)
        }
        _ => return (None, None),
    };
    let Ok(mut siblings) = siblings else {
        return (None, None);
    };
    let Some(index) = siblings.iter().position(|item| {
        item.file_id == detail.file_id
            || item.renditions.iter().any(|r| r.file_id == detail.file_id)
    }) else {
        return (None, None);
    };
    let next = if index + 1 < siblings.len() {
        Some(siblings.remove(index + 1))
    } else {
        None
    };
    let prev = if index > 0 {
        Some(siblings.swap_remove(index - 1))
    } else {
        None
    };
    (prev, next)
}

/// "« Prior episode: Title" on the left, "Next episode: Title »" on the
/// right, spanning the same width as the details above.
fn neighbour_nav_html(
    kind: media_db::MediaKind,
    prev: Option<&media_db::BrowseItem>,
    next: Option<&media_db::BrowseItem>,
) -> String {
    if prev.is_none() && next.is_none() {
        return String::new();
    }
    let noun = match kind {
        media_db::MediaKind::Music => "song",
        _ => "episode",
    };
    let label = |item: &media_db::BrowseItem| match (kind, item.episode, item.track_no) {
        (media_db::MediaKind::Tv, Some(n), _) => format!("{n:02} - {}", xml_escape(&item.title)),
        (media_db::MediaKind::Music, _, Some(n)) => format!("{n}. {}", xml_escape(&item.title)),
        _ => xml_escape(&item.title),
    };
    let prev_html = prev
        .map(|item| {
            format!(
                "<a href=\"/item/{}\">« Prior {noun}: {}</a>",
                item.file_id,
                label(item)
            )
        })
        .unwrap_or_default();
    let next_html = next
        .map(|item| {
            format!(
                "<a href=\"/item/{}\">Next {noun}: {} »</a>",
                item.file_id,
                label(item)
            )
        })
        .unwrap_or_default();
    // Empty spans keep a lone "next" on the right and a lone "prior" on
    // the left; clear:both drops the row below the floated poster.
    format!(
        "<p style=\"clear:both;display:flex;justify-content:space-between;gap:2em;\
         margin-top:1.5em;padding-top:.8em;border-top:1px solid #ddd\">\
         <span>{prev_html}</span><span style=\"text-align:right\">{next_html}</span></p>"
    )
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
    // Which season/episode (or artist/album) this is — the thing you
    // otherwise had to click back to the detail page to remember.
    let context = match (&detail.series, detail.season, detail.episode) {
        (Some(series), Some(season), Some(episode)) => {
            tv_context_html(series, season, episode)
        }
        _ => music_context_html(&detail, false),
    };
    let context_line = if context.is_empty() {
        String::new()
    } else {
        format!("<p style=\"color:#aaa;margin:.2em 0 .8em\">{context}</p>")
    };

    // Everything the detail page knows, in a hover card on the back link.
    let mut card = String::new();
    if detail.has_art {
        card.push_str(&format!("<img src=\"/art/{id}\" alt=\"\">"));
    }
    card.push_str(&format!("<strong>{heading}</strong>"));
    if !context.is_empty() {
        card.push_str(&format!("<br>{context}"));
    }
    if let Some(plot) = detail.plot.as_deref().filter(|p| !p.trim().is_empty()) {
        // Keep the card a card: truncate long synopses on a word boundary.
        let trimmed = if plot.chars().count() > 400 {
            let cut: String = plot.chars().take(400).collect();
            let cut = cut.rsplit_once(' ').map(|(head, _)| head.to_string()).unwrap_or(cut);
            format!("{cut}…")
        } else {
            plot.to_string()
        };
        card.push_str(&format!("<div class=\"plot\">{}</div>", xml_escape(&trimmed)));
    }
    let mut facts: Vec<String> = Vec::new();
    // The rating links to the IMDb entry when we know it — the episode's
    // own tconst for TV, the film's for movies.
    let imdb = detail.imdb_id.as_deref().filter(|id| id.starts_with("tt"));
    match (detail.rating, imdb) {
        (Some(rating), Some(id)) => facts.push(format!(
            "<a href=\"https://www.imdb.com/title/{id}/\">IMDb {rating:.1}</a>"
        )),
        (Some(rating), None) => facts.push(format!("IMDb {rating:.1}")),
        (None, Some(id)) => facts.push(format!("<a href=\"https://www.imdb.com/title/{id}/\">IMDb</a>")),
        (None, None) => {}
    }
    if let Some(genre) = &detail.genre {
        facts.push(xml_escape(genre));
    }
    if let Some(director) = &detail.director {
        facts.push(format!("dir. {}", xml_escape(director)));
    }
    if let Some(ms) = detail.duration_ms {
        facts.push(human_duration(ms));
    }
    if let (Some(w), Some(h)) = (detail.width, detail.height) {
        facts.push(format!("{w}×{h}{}", if is_uhd(detail.width, detail.height) { " (4K)" } else { "" }));
    }
    if let Some(codec) = &detail.video_codec {
        facts.push(xml_escape(codec));
    }
    facts.push(human_size(detail.size));
    card.push_str(&format!("<div class=\"facts\">{}</div>", facts.join(" · ")));

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
    } else if state
        .subs_inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&id)
        || text_sub_stream(&state.ffprobe, &servable.abs_path).await.is_some()
    {
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
        "<!doctype html><meta charset=utf-8><title>{heading}</title>{PLAYER_STYLE}\
         <body class=\"player\" style=\"font-family:sans-serif;max-width:60em;margin:1.5em auto;\
         background:#111;color:#ddd\">\
         <p><span class=\"infowrap\"><a href=\"/item/{id}\">← details</a>\
         <span class=\"card\">{card}</span></span></p>\
         <h2 style=\"margin-bottom:.1em\">{heading}</h2>{context_line}\
         <video controls autoplay playsinline{poster} \
          style=\"width:100%;max-height:80vh;background:#000\">\
         <source src=\"{}/media/{id}\" type=\"{}\">{track}\
         Your browser cannot play this format.</video>\
         <p id=\"resume\" style=\"color:#9c9;font-size:.9em\"></p>\
         <p style=\"color:#666;font-size:.8em\">← / → skip 10 seconds</p>\
         {subs_async}{RESUME_SCRIPT}",
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
    let detail = match detail {
        Ok(Some(d)) => d,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!("item {id}: {err:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let (prev, next) = neighbours(&conn, &detail);
    drop(conn);
    let nav = neighbour_nav_html(detail.kind, prev.as_ref(), next.as_ref());

    let mut heading = xml_escape(&detail.title);
    if let Some(year) = detail.year {
        heading.push_str(&format!(" ({year})"));
    }
    // The 4K chip is markup for the <h1> only; <title> takes the plain text.
    let mut heading_html = heading.clone();
    if is_uhd(detail.width, detail.height) {
        heading_html.push_str(" <span style=\"font-size:.45em;border:1.5px solid #666;\
            border-radius:4px;padding:.05em .35em;color:#555;vertical-align:middle\">4K</span>");
    }
    let subtitle = match (&detail.series, detail.season, detail.episode) {
        (Some(series), Some(season), Some(episode)) => tv_context_html(series, season, episode),
        _ => music_context_html(&detail, true),
    };

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
        "<!doctype html><meta charset=utf-8><title>{heading}</title>{PLAYER_STYLE}\
         <body style=\"font-family:sans-serif;max-width:46em;margin:2em auto;line-height:1.5\">\
         <p><a href=\"/browse\">⌂ browse</a></p>{art}<h1 style=\"margin-bottom:.2em\">{heading_html}</h1>\
         {subtitle_html}{plot}{play_links}\
         <table style=\"border-collapse:collapse\">{rows}</table>{nav}"
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
            &[(
                "SearchCaps",
                "dc:title,upnp:class,upnp:artist,upnp:album,upnp:genre".to_string(),
            )],
        )),
        "GetSortCapabilities" => xml_response(soap::envelope(
            CDS_SERVICE,
            "GetSortCapabilities",
            &[("SortCaps", String::new())],
        )),
        "Search" => search_action(&state, &body, update_id).await,
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

/// ContentDirectory Search: the same scoped search the web UI uses,
/// rendered as DIDL. Clients search within a container id, so the scope
/// is any node of the virtual tree.
async fn search_action(state: &AppState, body: &str, update_id: u32) -> Response {
    let container_id = soap::param(body, "ContainerID").unwrap_or_else(|| "0".into());
    let criteria = soap::param(body, "SearchCriteria").unwrap_or_default();
    let starting_index: usize = soap::param(body, "StartingIndex")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let requested_count: usize = soap::param(body, "RequestedCount")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let Some(scope) = ObjectId::parse(&container_id) else {
        return soap_fault(701, "No such object");
    };
    let (terms, class) = parse_search_criteria(&criteria);

    let conn = state.db.lock().await;
    let found = search_scope(&conn, &scope, &terms, class, state.recent_count);
    drop(conn);
    let found = match found {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!("Search {criteria:?} in {container_id}: {err:#}");
            return soap_fault(720, "Search failed");
        }
    };

    let total = found.len();
    let end = if requested_count == 0 {
        total
    } else {
        (starting_index + requested_count).min(total)
    };
    let start = starting_index.min(total);
    let page: Vec<media_db::BrowseItem> = found[start..end.max(start)].to_vec();
    let returned = page.len();
    let didl_xml = didl::render(&tree::entries_for(&scope, page), &state.base_url);

    xml_response(soap::envelope(
        CDS_SERVICE,
        "Search",
        &[
            ("Result", xml_escape(&didl_xml)),
            ("NumberReturned", returned.to_string()),
            ("TotalMatches", total.to_string()),
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
