use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use media_db::mime::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};
use media_db::queries::files;
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
    let title = tree::browse_metadata(&conn, &node)
        .ok()
        .map(|entry| match entry {
            tree::Entry::Container { title, .. } => title,
            tree::Entry::Item { item, .. } => item.title,
        })
        .unwrap_or_else(|| "Browse".to_string());
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
            tree::Entry::Item { item, .. } => rows.push_str(&format!(
                "<li>▶ <a href=\"{}/media/{}\">{}</a></li>",
                state.base_url,
                item.file_id,
                xml_escape(&item.title)
            )),
        }
    }
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{}</title>\
         <body style=\"font-family:sans-serif;max-width:44em;margin:2em auto\">\
         <p><a href=\"/browse\">⌂ top</a></p><h1>{}</h1>\
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
        Ok(Err(err)) => {
            tracing::debug!("art {id} unavailable: {err:#}");
            StatusCode::NOT_FOUND.into_response()
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
