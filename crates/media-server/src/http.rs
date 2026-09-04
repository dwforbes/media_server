use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::Router;
use media_db::mime::{AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};
use media_db::queries::{self, files, music, tv};
use media_db::sidecar;
use rusqlite::Connection;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::didl::{self, xml_escape, DLNA_FEATURES};
use crate::objectid::ObjectId;
use crate::{soap, tree, xml};

/// Everything before <body>: doctype, <html>/<head>, charset, viewport,
/// the title, the device icon as favicon / home-screen icon (the same
/// PNGs device.xml advertises) and any page-specific head markup. Every
/// page ends with PAGE_CLOSE.
fn page_head(title: &str, extra: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title>\
         <link rel=\"icon\" type=\"image/png\" sizes=\"48x48\" href=\"/icon/48.png\">\
         <link rel=\"icon\" type=\"image/png\" sizes=\"120x120\" href=\"/icon/120.png\">\
         <link rel=\"apple-touch-icon\" href=\"/icon/120.png\">{BASE_STYLE}{extra}</head>\n"
    )
}

/// Layout shared by every page. Widths and margins live here rather than
/// inline so that phones get side padding (auto margins collapse to zero
/// once the viewport is narrower than max-width, leaving text on the
/// screen edge) and the detail page's poster stops floating beside a
/// heading it would otherwise squeeze.
const BASE_STYLE: &str = "<style>\
html{-webkit-text-size-adjust:100%}\
body{font-family:sans-serif;max-width:44em;margin:2em auto;padding:0 1.25rem 2rem;box-sizing:border-box}\
body.detail{max-width:46em;line-height:1.5}\
body.player{max-width:60em;margin:1.5em auto;background:#111;color:#ddd;overflow-x:hidden}\
div.videowrap{position:relative;width:100vw;margin-left:calc(50% - 50vw)}\
div.videowrap video{display:block;width:100%;max-height:85vh;background:#000}\
img.art{float:right;max-width:220px;margin:0 0 1em 1.5em;border-radius:6px}\
div.hdr{display:grid;grid-template-columns:1fr auto;grid-template-areas:\"top art\" \"desc art\";column-gap:1.5em;row-gap:.4em;align-items:start}\
div.hdr-top{grid-area:top}div.hdr-desc{grid-area:desc}\
div.hdr img.art{grid-area:art;float:none;margin:0 0 1em}\
table{max-width:100%}td{overflow-wrap:anywhere}input{font-size:1rem}\
.card{display:none;position:absolute;left:0;top:100%;z-index:10;width:32em;max-width:90vw;padding:.9em 1em;background:#1c1c1c;color:#ddd;border:1px solid #444;border-radius:6px;box-shadow:0 8px 22px rgba(0,0,0,.55);font-size:.85rem;font-weight:normal;line-height:1.5;text-align:left;box-sizing:border-box}\
.card img{float:right;width:6em;margin:0 0 .6em .9em;border-radius:4px}\
.card .facts{display:block;color:#9a9a9a;margin-top:.5em}.card .plot{display:block;margin-top:.5em}\
.card .nav{display:flex;justify-content:space-between;gap:1.5em;clear:both;margin-top:.6em;padding-top:.5em;border-top:1px solid #3a3a3a}.card .nav span:last-child{text-align:right}\
.card a:link,.card a:visited{color:#9cf}.card a:hover,.card a:active{color:#cef}\
div.covers{display:flex;flex-wrap:wrap;gap:.6em;padding:.4em 0 1em}\
div.covers .cover{position:relative;flex:none;width:120px;height:180px}\
div.covers .cover>a{display:block;width:100%;height:100%;border-radius:6px;background:#eee;box-sizing:border-box}\
div.covers .cover>a img{display:block;width:100%;height:100%;object-fit:cover;border-radius:6px}\
div.covers .cover>a.noart{display:flex;align-items:center;justify-content:center;text-align:center;padding:.6em;border:2px solid #999;background:none;color:#333;font-size:.85em;line-height:1.3;text-decoration:none;overflow-wrap:anywhere}\
div.covers .cover>a:hover{outline:2px solid #0645ad;outline-offset:1px}\
div.covers .cover:hover .card.loaded,div.covers .cover:focus-within .card.loaded{display:block}\
div.covers .card{margin-top:.35em;cursor:default}div.covers .cover.flip .card{left:auto;right:0}\
p.controls{display:flex;flex-wrap:wrap;gap:.3em 1.5em;align-items:baseline}\
html a.home,html a.home:link,html a.home:visited,html a.home:hover,html a.home:active{color:inherit;text-decoration:none;font-size:1.15em;line-height:1}\
html a.home:hover{opacity:.65}\
p.controls input[type=checkbox]{vertical-align:middle;margin:0 .3em 0 0;position:relative;top:-.08em}\
@media (max-width:40em){\
body{margin:1em auto;padding:0 1rem 1.5rem}\
body.player{margin:.5em auto}\
img.art{float:none;display:block;max-width:55%;margin:0 auto 1em}\
div.hdr{grid-template-columns:1fr;grid-template-areas:\"top\" \"art\" \"desc\"}\
div.hdr img.art{justify-self:center;margin:0 0 .5em}\
h1{font-size:1.5em}}\
</style>";

const PAGE_CLOSE: &str = "\n</body></html>\n";

/// Truncate on a word boundary with an ellipsis. Long synopses are cut
/// down for the player's hover card and for link-preview descriptions.
fn truncate_words(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map(|(head, _)| head.to_string()).unwrap_or(cut);
    format!("{cut}…")
}

/// Open Graph tags (plus a Twitter card hint) for the page head, so a
/// link shared into a chat or feed unfurls into a title / description /
/// poster card. Scrapers resolve no relative URLs, so everything is
/// absolute against the origin the visitor actually used; the image is
/// the downscaled /art/{id}/og.jpg rendition because several platforms
/// silently drop preview images beyond a few hundred KB.
fn og_meta(
    base: &str,
    path: &str,
    kind: &str,
    title: &str,
    description: Option<&str>,
    art_id: Option<i64>,
    site: &str,
) -> String {
    let mut out = format!(
        "<meta property=\"og:type\" content=\"{kind}\">\
         <meta property=\"og:site_name\" content=\"{}\">\
         <meta property=\"og:url\" content=\"{}{}\">\
         <meta property=\"og:title\" content=\"{}\">\
         <meta name=\"twitter:card\" content=\"summary\">",
        xml_escape(site),
        xml_escape(base),
        xml_escape(path),
        xml_escape(title)
    );
    if let Some(desc) = description.map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(&format!(
            "<meta property=\"og:description\" content=\"{}\">",
            xml_escape(&truncate_words(desc, 300))
        ));
    }
    if let Some(id) = art_id {
        out.push_str(&format!(
            "<meta property=\"og:image\" content=\"{}/art/{id}/og.jpg\">",
            xml_escape(base)
        ));
    }
    out
}

/// The browser-tab <title> for one playable item. Episodes carry their
/// season and series — "Do Not Resuscitate (S02 - The Sopranos)" — so a
/// tab stays identifiable among others; everything else is the title
/// with its year.
fn tab_title(detail: &files::ItemDetail) -> String {
    match (&detail.series, detail.season) {
        (Some(series), Some(season)) => {
            format!("{} (S{season:02} - {series})", detail.title)
        }
        _ => match detail.year {
            Some(year) => format!("{} ({year})", detail.title),
            None => detail.title.clone(),
        },
    }
}

/// OG tags for one playable item, shared by the detail and player pages.
/// The plain-text title carries the context a bare episode or track title
/// lacks when it lands in a chat: series and SxxEyy for TV, the artist
/// for music, the year for movies.
fn item_og_meta(base: &str, path: &str, detail: &files::ItemDetail, site: &str) -> String {
    let title = match (&detail.series, detail.season, detail.episode) {
        (Some(series), Some(season), Some(episode)) => {
            format!("{series} S{season:02}E{episode:02} — {}", detail.title)
        }
        _ => match (detail.kind, detail.artist.as_deref()) {
            (media_db::MediaKind::Music, Some(artist)) => {
                format!("{artist} — {}", detail.title)
            }
            _ => match detail.year {
                Some(year) => format!("{} ({year})", detail.title),
                None => detail.title.clone(),
            },
        },
    };
    let description = detail
        .plot
        .clone()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            // Tracks have no synopsis; the album is the next best context.
            (detail.kind == media_db::MediaKind::Music)
                .then(|| detail.album.clone())
                .flatten()
        });
    let kind = match detail.kind {
        media_db::MediaKind::Movies => "video.movie",
        media_db::MediaKind::Tv => "video.episode",
        media_db::MediaKind::Music => "music.song",
    };
    og_meta(
        base,
        path,
        kind,
        &title,
        description.as_deref(),
        detail.has_art.then_some(detail.file_id),
        site,
    )
}

const CDS_SERVICE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CMS_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

pub struct AppState {
    pub db: tokio::sync::Mutex<Connection>,
    pub update_id: AtomicU32,
    /// Leaf counts per browse container, generation-keyed on update_id.
    pub counts: std::sync::Mutex<crate::counts::Cache>,
    pub uuid: String,
    pub friendly_name: String,
    pub base_url: String,
    /// (120px icon bytes, 48px icon bytes, whether user-supplied).
    pub icon: (Vec<u8>, Vec<u8>, bool),
    pub recent_count: usize,
    pub ffmpeg: String,
    pub ffprobe: String,
    pub vtt_cache: std::path::PathBuf,
    /// The HTTPS listener, when configured.
    pub tls: Option<TlsInfo>,
    /// Subtitle extractions in progress, by file id. The first request
    /// spawns one detached ffmpeg and parks a receiver here; every later
    /// request for the same file awaits that receiver instead of starting
    /// another ffmpeg. The entry is removed (after the cache file lands)
    /// just before the sender is dropped, which is what wakes the waiters.
    pub subs_inflight: std::sync::Mutex<std::collections::HashMap<i64, tokio::sync::watch::Receiver<()>>>,
    /// Permits for ffprobe/ffmpeg children (see text_sub_stream): the
    /// cap on how many a flood of requests can have running at once.
    pub probes: tokio::sync::Semaphore,
}

pub struct TlsInfo {
    pub hostname: String,
    pub port: u16,
    pub redirect_pages: bool,
}

impl TlsInfo {
    /// "https://host" or "https://host:port".
    pub fn origin(&self) -> String {
        if self.port == 443 {
            format!("https://{}", self.hostname)
        } else {
            format!("https://{}:{}", self.hostname, self.port)
        }
    }
}

/// Request extension marking requests that arrived over the HTTPS listener.
#[derive(Clone, Copy)]
pub struct Https;

/// The same routes as `router`, with every request marked as HTTPS.
pub fn router_tls(state: Arc<AppState>) -> Router {
    router(state).layer(middleware::from_fn(|mut req: Request, next: Next| async move {
        req.extensions_mut().insert(Https);
        next.run(req).await
    }))
}

/// Absolute URL prefix for links that leave the page (playlists): the
/// host the client actually used when over HTTPS, else the canonical
/// UPnP base URL.
fn request_base_url(state: &AppState, headers: &HeaderMap, https: bool) -> String {
    if !https {
        return state.base_url.clone();
    }
    match headers.get(header::HOST).and_then(|h| h.to_str().ok()) {
        Some(host) if !host.is_empty() => format!("https://{host}"),
        _ => state.tls.as_ref().map(TlsInfo::origin).unwrap_or_else(|| state.base_url.clone()),
    }
}

/// With `redirect_pages`, HTML page requests on the plain listener go to
/// the HTTPS origin. Everything UPnP clients fetch (device.xml, control,
/// /media, /art, playlists, icons) is left alone.
async fn redirect_pages(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(tls) = state.tls.as_ref().filter(|t| t.redirect_pages) {
        let path = req.uri().path();
        let is_page = path == "/"
            || path == "/browse"
            || path.starts_with("/browse/")
            || path.starts_with("/item/")
            || path.starts_with("/play/")
            || path.starts_with("/captions/")
            || path == "/search";
        if is_page && req.method() == Method::GET && req.extensions().get::<Https>().is_none() {
            let rest = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
            return Redirect::temporary(&format!("{}{rest}", tls.origin())).into_response();
        }
    }
    next.run(req).await
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
        .route("/art/{id}/og.jpg", get(serve_art_og))
        .route("/icon/120.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.0.clone()) }))
        .route("/icon/48.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.1.clone()) }))
        // Browsers ask for these by convention; same icon as the UPnP device.
        .route("/favicon.ico", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.1.clone()) }))
        .route("/apple-touch-icon.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.0.clone()) }))
        .route("/apple-touch-icon-precomposed.png", get(|State(s): State<Arc<AppState>>| async move { icon_response(s.icon.0.clone()) }))
        // The root is the library itself; /browse stays as an alias.
        .route(
            "/",
            get(|s: State<Arc<AppState>>, h: HeaderMap, https: Option<Extension<Https>>| {
                browse_page(s, Path("0".into()), h, https)
            }),
        )
        .route(
            "/playlist.m3u",
            get(|s: State<Arc<AppState>>, h: HeaderMap, https: Option<Extension<Https>>| {
                playlist(s, Path("all".into()), h, https)
            }),
        )
        .route("/playlist/{section}", get(playlist))
        .route("/playlist/id/{oid}", get(playlist_by_id))
        .route(
            "/browse",
            get(|s: State<Arc<AppState>>, h: HeaderMap, https: Option<Extension<Https>>| {
                browse_page(s, Path("0".into()), h, https)
            }),
        )
        .route("/browse/{oid}", get(browse_page))
        .route("/item/{id}", get(item_page))
        .route("/search", get(search_page))
        .route("/playlist/search", get(search_playlist))
        .route("/play/{id}", get(play_page))
        .route("/captions/{id}", get(captions_page))
        .route("/card/{id}", get(card_fragment))
        .route("/subs/{id}", get(serve_subs))
        .layer(middleware::from_fn_with_state(state.clone(), redirect_pages))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Defence in depth on every response: no MIME sniffing (a share file
/// served as an image can never be reinterpreted as a document), no
/// framing, no referrer leakage, and a Content-Security-Policy that
/// permits exactly the inline scripts the pages carry — by hash — so an
/// escaping slip anywhere in the format!-built pages cannot run script.
/// On XML and media responses the headers are inert.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if let Ok(value) = HeaderValue::from_str(csp()) {
        h.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    res
}

/// The policy, built once: the SHA-256 of the player script's body is
/// what lets it run; inline styles stay allowed (the pages lean on
/// style attributes, which cannot execute anything); blob: covers the
/// subtitle track attached from an extraction fetch.
fn csp() -> &'static str {
    static CSP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CSP.get_or_init(|| {
        use base64::Engine;
        let hashes: Vec<String> = [PLAYER_SCRIPT, CC_PANEL_SCRIPT, CAPTIONS_SCRIPT, COVERS_SCRIPT]
            .iter()
            .map(|script| {
                let body = script
                    .strip_prefix("<script>")
                    .and_then(|s| s.strip_suffix("</script>"))
                    .unwrap_or(script);
                let digest = ring::digest::digest(&ring::digest::SHA256, body.as_bytes());
                format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest.as_ref()))
            })
            .collect();
        format!(
            "default-src 'self'; script-src {}; style-src 'self' 'unsafe-inline'; \
             img-src 'self'; media-src 'self' blob:; connect-src 'self'; object-src 'none'; \
             base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
            hashes.join(" ")
        )
    })
}

/// An IMDb title id fit to print and link: "tt" plus 7–9 digits and
/// nothing else. Sidecar .nfo files are writable by anything that can
/// reach the share, so the id is validated where it is rendered.
fn imdb_title_id(id: &str) -> Option<&str> {
    let digits = id.strip_prefix("tt")?;
    ((7..=9).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// The image types the art routes serve, told apart by magic bytes: the
/// MIME comes from the bytes, never from a tag or a file extension that
/// something on the share chose (an APIC frame declaring text/html would
/// otherwise have a browser render its payload on this origin).
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
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

/// How many distinct terms a search honours. Every item in scope is
/// checked against every term, so a request with thousands of quoted
/// literals must not multiply into thousands of passes over the catalog.
const MAX_TERMS: usize = 12;

fn limit_terms(mut terms: Vec<String>) -> Vec<String> {
    terms.sort();
    terms.dedup();
    terms.truncate(MAX_TERMS);
    terms
}

fn query_terms(q: &str) -> Vec<String> {
    limit_terms(q.split_whitespace().map(|t| t.to_lowercase()).collect())
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
    (limit_terms(terms), class)
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

fn render_m3u(base_url: &str, entries: &[media_db::BrowseItem]) -> Response {
    let mut out = String::from("#EXTM3U\n");
    for item in entries {
        let secs = item.duration_ms.map(|ms| ms / 1000).unwrap_or(-1);
        let title = playlist_title(item).replace(['\n', '\r'], " ");
        out.push_str(&format!("#EXTINF:{secs},{title}\n{base_url}/media/{}\n", item.file_id));
    }
    (
        [(header::CONTENT_TYPE, "audio/x-mpegurl; charset=utf-8")],
        out,
    )
        .into_response()
}

/// M3U for any node of the virtual tree, by its object id.
async fn playlist_by_id(
    State(state): State<Arc<AppState>>,
    Path(oid): Path<String>,
    headers: HeaderMap,
    https: Option<Extension<Https>>,
) -> Response {
    let base = request_base_url(&state, &headers, https.is_some());
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
        Ok(()) => render_m3u(&base, &entries),
        Err(err) => {
            tracing::warn!("playlist {oid}: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Discovery-free fallback: the catalog as M3U, playable by anything that
/// can open a URL — no SSDP involved. Friendly aliases over the tree.
async fn playlist(
    State(state): State<Arc<AppState>>,
    Path(section): Path<String>,
    headers: HeaderMap,
    https: Option<Extension<Https>>,
) -> Response {
    let base = request_base_url(&state, &headers, https.is_some());
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
        Ok(()) => render_m3u(&base, &entries),
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
        rows.push_str(&listing_row(item));
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
            "{} match{} in {}{capped}",
            hits.len(),
            if hits.len() == 1 { "" } else { "es" },
            xml_escape(&scope_title)
        )
    };
    let head = page_head("Search", "");
    let html = format!(
        "{head}<body>\
         <h1>Search</h1>\
         <p><a href=\"/browse/{}\">← Back to {}</a></p>{}\
         <p>{summary}</p>\
         <ul style=\"list-style:none;padding:0;line-height:1.7\">{rows}</ul>{PAGE_CLOSE}",
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
    headers: HeaderMap,
    https: Option<Extension<Https>>,
) -> Response {
    let base = request_base_url(&state, &headers, https.is_some());
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
        Ok(entries) => render_m3u(&base, &entries),
        Err(err) => {
            tracing::warn!("search playlist: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}


/// HTML mirror of the virtual tree: every container is browsable and
/// offers its playlist; items link to their streams.
async fn browse_page(
    State(state): State<Arc<AppState>>,
    Path(oid): Path<String>,
    headers: HeaderMap,
    https: Option<Extension<Https>>,
) -> Response {
    let Some(node) = ObjectId::parse(&oid) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let conn = state.db.lock().await;
    let (title, parent_id, art_item) = tree::browse_metadata(&conn, &node)
        .ok()
        .map(|entry| match entry {
            tree::Entry::Container { title, parent, art_item, .. } => {
                (title, Some(parent), art_item)
            }
            tree::Entry::Item { item, .. } => (item.title, None, None),
        })
        .unwrap_or_else(|| ("Browse".to_string(), None, None));
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
    // A series page also gets the season × episode ratings grid.
    let grid = match &node {
        ObjectId::TvSeries(series) => tv::series_episodes(&conn, series)
            .map(|episodes| episode_grid_html(&episodes))
            .unwrap_or_default(),
        _ => String::new(),
    };
    // Series and season pages carry the description (and, for a series,
    // the IMDb rating/link) ingested from tvshow.nfo / season.nfo. The
    // raw plot is kept alongside the rendered HTML for the OG tags.
    let (description, og_plot) = match &node {
        ObjectId::TvSeries(series) => tv::series_info(&conn, series)
            .ok()
            .flatten()
            .map(|meta| (series_meta_html(&meta), meta.plot))
            .unwrap_or_default(),
        ObjectId::TvSeason { series, season } => tv::season_info(&conn, series, *season)
            .ok()
            .flatten()
            .map(|plot| {
                (
                    format!("<p style=\"max-width:38em\">{}</p>", xml_escape(&plot)),
                    Some(plot),
                )
            })
            .unwrap_or_default(),
        _ => (String::new(), None),
    };
    let leaf_counts = crate::counts::for_children(&state, &conn, &node);
    drop(conn);
    let children = match children {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!("browse {oid}: {err:#}");
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    let search_box = search_form(&oid, "");
    // A leaf grouping of movies (a franchise, a genre, a year, All Movies)
    // also gets its covers under the listing, wrapping into as many rows
    // as it takes: the same order and the same links; a movie without a
    // poster holds its place as an outlined card with its name.
    let covers = {
        let movies: Vec<&media_db::BrowseItem> = children
            .iter()
            .filter_map(|e| match e {
                tree::Entry::Item { item, .. } if item.kind == media_db::MediaKind::Movies => Some(item),
                _ => None,
            })
            .collect();
        if movies.is_empty() {
            String::new()
        } else {
            let mut strip = String::from("<div class=\"covers\">");
            for item in movies {
                let name = xml_escape(&item.title);   // as the row shows it (year included)
                // The details card sits beside the link, not inside it (a
                // card holds links of its own, and anchors do not nest);
                // COVERS_SCRIPT fills it from /card/{id} on hover.
                let link = if item.has_art {
                    format!(
                        "<a href=\"/item/{}\" title=\"{name}\"><img src=\"{}\" alt=\"{name}\" loading=\"lazy\"></a>",
                        item.file_id,
                        art_url(item)
                    )
                } else {
                    format!(
                        "<a class=\"noart\" href=\"/item/{}\" title=\"{name}\"><span>{name}</span></a>",
                        item.file_id
                    )
                };
                strip.push_str(&format!(
                    "<span class=\"cover\" data-card=\"{}\">{link}<span class=\"card\"></span></span>",
                    item.file_id
                ));
            }
            strip.push_str("</div>");
            strip
        }
    };
    let mut rows = String::new();
    for entry in children {
        match entry {
            tree::Entry::Container { id, title, .. } => {
                let count = ObjectId::parse(&id)
                    .and_then(|child| leaf_counts.get(&crate::counts::canon(&child)).copied())
                    .map(|n| format!(" <span style=\"color:#666\">({n})</span>"))
                    .unwrap_or_default();
                rows.push_str(&format!(
                    "<li>📁 <a href=\"/browse/{id}\">{}</a>{count}</li>",
                    xml_escape(&title)
                ));
            }
            tree::Entry::Item { item, .. } => rows.push_str(&listing_row(&item)),
        }
    }
    // A container with representative art (a series or season, borrowing
    // an episode's poster) shows it to the right of the header block —
    // title, way up, search, then the description — the same shape every
    // container page has, art or not; on narrow screens the art slots in
    // between search and description (div.hdr in BASE_STYLE).
    let art = art_item
        .map(|id| format!("<img src=\"/art/{id}\" alt=\"\" class=\"art\">"))
        .unwrap_or_default();
    let description = if description.is_empty() {
        String::new()
    } else {
        format!("<div class=\"hdr-desc\">{description}</div>")
    };
    // Shared links unfurl into a poster/description card. Series and
    // season pages have both; other containers at least name themselves.
    let og = og_meta(
        &request_base_url(&state, &headers, https.is_some()),
        &if oid == "0" { "/".to_string() } else { format!("/browse/{oid}") },
        if matches!(node, ObjectId::TvSeries(_)) { "video.tv_show" } else { "website" },
        &title,
        og_plot.as_deref(),
        art_item,
        &state.friendly_name,
    );
    let head = page_head(&xml_escape(&title), &og);
    let html = format!(
        "{head}<body>\
         <div class=\"hdr\"><div class=\"hdr-top\"><h1>{}</h1>{back_link}{search_box}</div>\
         {art}{description}</div>\
         <ul style=\"list-style:none;padding:0;line-height:1.7\">{rows}</ul>{covers}{grid}{covers_script}{PAGE_CLOSE}",
        xml_escape(&title),
        covers_script = if covers.is_empty() { "" } else { COVERS_SCRIPT }
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// An item's art URL, versioned by its extraction time when known so the
/// response can be cached for a year: artwork changes re-extract the
/// file, which changes the version and so the URL.
fn art_url(item: &media_db::BrowseItem) -> String {
    let id = item.art_file_id.unwrap_or(item.file_id);
    match item.art_version {
        Some(v) => format!("/art/{id}?v={v}"),
        None => format!("/art/{id}"),
    }
}


/// Series description plus an IMDb rating/link, same idiom as the detail
/// page's facts table.
fn series_meta_html(meta: &media_db::queries::tv::SeriesMeta) -> String {
    let mut out = String::new();
    if let Some(plot) = &meta.plot {
        out.push_str(&format!("<p style=\"max-width:38em\">{}</p>", xml_escape(plot)));
    }
    let imdb = meta.imdb_id.as_deref().and_then(imdb_title_id);
    let line = match (meta.rating, imdb) {
        (Some(rating), Some(id)) => Some(format!(
            "IMDb {rating:.1} / 10 — <a href=\"https://www.imdb.com/title/{id}/\">{id}</a>"
        )),
        (Some(rating), None) => Some(format!("IMDb {rating:.1} / 10")),
        (None, Some(id)) => Some(format!(
            "IMDb — <a href=\"https://www.imdb.com/title/{id}/\">{id}</a>"
        )),
        (None, None) => None,
    };
    if let Some(line) = line {
        out.push_str(&format!("<p style=\"color:#666\">{line}</p>"));
    }
    out
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

/// The ordinal (among subtitle streams) of the text subtitle track worth
/// showing — full English captions, SDH preferred, forced tracks last (see
/// media_db::subtitles). None when the file has no text subtitles (bitmap
/// PGS/VobSub can't become WebVTT).
///
/// "None" is remembered as `{id}.nosubs` beside the VTT cache (good while
/// newer than the media file), so a program without captions costs one
/// probe rather than one per page view; and probes take a permit from
/// `state.probes`, the small semaphore every ffmpeg/ffprobe spawn shares,
/// so a burst of requests queues instead of forking a process apiece.
async fn text_sub_stream(state: &AppState, id: i64, path: &std::path::Path) -> Option<usize> {
    let marker = state.vtt_cache.join(format!("{id}.nosubs"));
    if let (Some(marker_time), Some(media_time)) = (file_mtime(&marker), file_mtime(path)) {
        if marker_time >= media_time {
            return None;
        }
    }
    let _permit = state.probes.acquire().await.ok()?;
    let out = tokio::process::Command::new(&state.ffprobe)
        .args(media_db::subtitles::ffprobe_args())
        .arg(path)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tracks = media_db::subtitles::parse_ffprobe(&String::from_utf8_lossy(&out.stdout));
    let found = media_db::subtitles::best_text_track(&tracks).map(|t| t.ordinal);
    if found.is_none() {
        let _ = std::fs::write(&marker, b"");
    }
    found
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
    if let Ok(bytes) = sidecar::read_capped(&srt_path, sidecar::MAX_TEXT) {
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
    let Some(ordinal) = text_sub_stream(state, id, path).await else {
        tracing::debug!("no text subtitle track in {id}");
        return;
    };
    let _permit = state.probes.acquire().await;
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
///
/// The "details" link rides at the right of the heading; the card
/// anchors to the heading itself (not the link) so it always opens
/// along the column's left edge instead of wherever the link landed.
/// The link's padding-bottom is the hover bridge: vertical padding on
/// an inline box is hit-tested but never moves the layout, so the
/// pointer can travel from the link down into the card without
/// crossing a dead zone that would dismiss it.
const PLAYER_STYLE: &str = r#"<style>
#heading { position: relative; }
/* The way home, to the left of the title: normal weight, a little
   quieter than the title, in the text colour rather than a link's. */
#heading a.home { font-weight: normal; color: #aaa; margin-right: .7em; vertical-align: .05em; }
.infowrap { display: inline; font-size: 1rem; font-weight: normal;
  margin-left: 1em; padding-bottom: 1em; }
.infowrap:hover .card, .infowrap:focus-within .card { display: block; }
/* Dark surfaces (the player page, the card on either page): browser
   default link colours — navy visited links especially — vanish on
   near-black, so pin every link state to a light blue. */
body.player a:link, body.player a:visited { color: #9cf; }
body.player a:hover, body.player a:active { color: #cef; }
/* Captions panel ("CC"): a fixed column down the right edge, the page
   re-centred in what remains. The body is centred by auto margins, so
   widening its max-width by the panel's width and handing that width
   back as right padding keeps the content box centred in the free
   region; the full-bleed video wrap does the same sum for its breakout. */
:root { --ccw: min(24rem, 40vw); }   /* rem: the panel's own font is smaller */
/* The hint line under the video: skip hint left, resume notes centred,
   CC at the right border — three equal flex sections so the centre stays
   centred whatever the other two hold (the note collapses when hidden,
   leaving the halves still aligned outward). */
p.hint { display: flex; align-items: center; gap: 1em; color: #666; font-size: .8em; }
p.hint > * { flex: 1 1 0; }
p.hint > .right { text-align: right; }
#rejoin { text-align: center; font-size: 1.15em; }
#cc { font: inherit; font-size: .75rem; font-weight: bold; letter-spacing: .06em;
  padding: .1em .5em; background: none; color: #aaa; border: 1px solid #666; border-radius: 3px;
  cursor: pointer; }
#cc[aria-pressed="true"] { background: #9cf; color: #111; border-color: #9cf; }
/* Lit, but the captions are in their own window: outlined, not filled. */
#cc.detached { background: none; color: #9cf; border-color: #9cf; border-style: dashed; }
body.cc { max-width: calc(60em + var(--ccw)); padding-right: calc(1.25rem + var(--ccw)); }
body.cc div.videowrap { width: calc(100vw - var(--ccw)); margin-left: calc(50% - 50vw + var(--ccw) / 2); }
/* Phones: no room beside the video, so the panel sits below the controls. */
@media (max-width: 40em) {
  body.cc { max-width: none; padding-right: 1rem; }
  body.cc div.videowrap { width: 100vw; margin-left: calc(50% - 50vw); }
  #cc-panel { position: static; width: auto; height: 45vh; margin-top: 1em;
    border: 1px solid #333; border-radius: 6px; }
  /* Equal thirds are too tight here: the button takes only its width,
     the hint its own, and the note gets the rest (centred within it). */
  p.hint > * { flex: 0 1 auto; }
  p.hint > #rejoin { flex: 1 1 0; }
}
</style>"#;

/// The captions panel's own styles, shared by the player page (where it
/// is a column down the right edge) and the pop-out window (where it is
/// the whole page — see CAPTIONS_STYLE).
const CC_STYLE: &str = concat!("<style>\n", r#"#cc-panel { position: fixed; top: 0; right: 0; bottom: 0; width: var(--ccw); box-sizing: border-box;
  display: flex; flex-direction: column; background: #161616; border-left: 1px solid #333;
  font-size: .85rem; }
#cc-panel[hidden] { display: none; }
#cc-panel .head { flex: none; display: flex; justify-content: space-between; align-items: center;
  gap: .5em; padding: .45em .8em; border-bottom: 1px solid #333; color: #aaa; }
#cc-panel .head .tools { display: flex; gap: .15em; flex: none; }
#cc-panel .head button { font: inherit; font-size: 1.1em; line-height: 1; padding: .1em .4em;
  background: none; color: #aaa; border: 0; border-radius: 3px; cursor: pointer; }
#cc-panel .head button:hover { background: #2a2a2a; color: #fff; }
/* Touch devices have no floating windows to pop out into. */
@media (hover: none) { #cc-pop { display: none; } }
#cc-list { position: relative; flex: 1; overflow-y: auto; overscroll-behavior: contain; }
/* flow-root: the first line's top margin (its lead-in gap) must not
   collapse through the track, which anchors the playhead and the rule. */
#cc-track { position: relative; display: flow-root; margin: .4em 0; }
/* The timeline: a rule that runs on through the gaps, so a stretch with
   nothing said still reads as time passing. */
#cc-track::before { content: ""; position: absolute; top: 0; bottom: 0; left: 4em;
  border-left: 1px solid #2c2c2c; }
#cc-track a.cue { position: relative; display: flex; gap: .7em; padding: .12em .8em .12em .6em;
  line-height: 1.35; text-decoration: none; }
#cc-track a.cue, #cc-track a.cue:link, #cc-track a.cue:visited { color: #c8c8c8; }
#cc-track a.cue:hover { background: #222; color: #fff; }
#cc-track a.cue.on { background: #26313f; color: #fff; }
#cc-track a.cue time { flex: none; width: 3.4em; text-align: right; color: #777;
  font-size: .9em; font-variant-numeric: tabular-nums; }
#cc-track a.cue.on time { color: #9cf; }
#cc-now { position: absolute; left: 0; right: 0; height: 2px; background: #9cf; opacity: .6;
  pointer-events: none; }
#cc-panel .empty { padding: 1em .9em; color: #888; }
"#, "</style>");

/// The pop-out captions window: the panel is the page.
const CAPTIONS_STYLE: &str = r#"<style>
body.captions { max-width: none; margin: 0; padding: 0; background: #161616; color: #ddd; overflow: hidden; }
body.captions #cc-panel { position: fixed; top: 0; left: 0; right: 0; bottom: 0; width: auto; border: 0;
  font-size: .95rem; }
body.captions #cc-panel .head { color: #ddd; }
body.captions #cc-panel .head #cc-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#cc-dock { font-size: .8em !important; border: 1px solid #555 !important; }
</style>"#;

/// The captions panel, shared by the player page and the pop-out window:
/// a program's cues down a column, placed along the running time (a line
/// sits at its start time on a pixels-per-second scale, pushed down only
/// as far as the line above needs, so silence is blank space and quick
/// exchanges pack solid), with a playhead bar, the current line
/// highlighted, the list following playback unless the viewer just
/// scrolled it, and a click seeking. The host supplies the clock
/// (opts.time, opts.duration), the seek (opts.seek) and a line's link
/// target (opts.href), and calls follow() as the clock moves.
const CC_PANEL_SCRIPT: &str = r#"<script>
window.ccPanel = function (list, track, opts) {
  var cues = [], cueEls = [], tops = [], bottoms = [], now = null, active = -1;
  var userUntil = 0;   // the viewer scrolled the list: no following until then
  function mmss(t) {
    var m = Math.floor(t / 60), s = Math.floor(t % 60);
    return m + ':' + (s < 10 ? '0' : '') + s;
  }
  function visible() { return list.clientHeight > 0; }
  function empty(text) {
    cues = []; cueEls = []; tops = []; bottoms = []; now = null; active = -1;
    track.innerHTML = '';
    var d = document.createElement('div');
    d.className = 'empty'; d.textContent = text;
    track.appendChild(d);
  }
  function render(newCues) {
    cues = newCues; cueEls = []; active = -1;
    var frag = document.createDocumentFragment();
    for (var i = 0; i < cues.length; i++) {
      var a = document.createElement('a');
      a.className = 'cue';
      a.href = opts.href ? opts.href(cues[i].start) : '#';
      a.dataset.i = i;
      var t = document.createElement('time');
      t.textContent = mmss(cues[i].start);
      var s = document.createElement('span');
      s.textContent = cues[i].text;
      a.appendChild(t); a.appendChild(s);
      frag.appendChild(a);
      cueEls.push(a);
    }
    now = document.createElement('div');
    now.id = 'cc-now';
    frag.appendChild(now);
    track.innerHTML = '';
    track.appendChild(frag);
    layout();
    follow(true);
  }
  // Each line goes at its start time × scale, or straight under the line
  // above when that would overlap. The scale is the program's own: the
  // median of (line height ÷ seconds to the next line), so a typical pair
  // of lines just touches. All writes, then all reads, then all writes:
  // interleaving them would force a reflow per line.
  function layout() {
    var n = cueEls.length;
    if (!n) return;
    var i, heights = [];
    for (i = 0; i < n; i++) cueEls[i].style.marginTop = '0';
    for (i = 0; i < n; i++) heights.push(cueEls[i].offsetHeight);
    var ratios = [];
    for (i = 1; i < n; i++) {
      var gap = cues[i].start - cues[i - 1].start;
      if (gap > 0.2) ratios.push(heights[i - 1] / gap);
    }
    ratios.sort(function (a, b) { return a - b; });
    var scale = ratios.length ? ratios[ratios.length >> 1] : 6;
    scale = Math.max(1, Math.min(12, scale));
    var bottom = 0;
    for (i = 0; i < n; i++) {
      var top = Math.max(cues[i].start * scale, bottom);
      cueEls[i].style.marginTop = Math.round((top - bottom) * 10) / 10 + 'px';
      bottom = top + heights[i];
    }
    var d = opts.duration();
    var end = isFinite(d) && d > 0 ? d : cues[n - 1].end;
    track.style.minHeight = (Math.max(bottom, end * scale) + 24) + 'px';
    tops = []; bottoms = [];
    for (i = 0; i < n; i++) {
      tops.push(cueEls[i].offsetTop);
      bottoms.push(cueEls[i].offsetTop + heights[i]);
    }
  }
  function index(t) {
    var lo = 0, hi = cues.length - 1, i = -1;
    while (lo <= hi) {
      var mid = (lo + hi) >> 1;
      if (cues[mid].start <= t) { i = mid; lo = mid + 1; } else hi = mid - 1;
    }
    return i;
  }
  // The playhead's place in the column: part-way down the current line,
  // or proportionally through the gap to the next one.
  function y(t, i) {
    if (i < 0) return cues[0].start > 0 ? Math.min(1, t / cues[0].start) * tops[0] : 0;
    var c = cues[i];
    if (t < c.end) return tops[i] + Math.min(1, (t - c.start) / Math.max(0.1, c.end - c.start)) * (bottoms[i] - tops[i]);
    var from = c.end, y0 = bottoms[i], to, y1;
    if (i + 1 < cues.length) { to = cues[i + 1].start; y1 = tops[i + 1]; }
    else { var d = opts.duration(); to = isFinite(d) ? d : from; y1 = track.offsetHeight; }
    if (to <= from) return y0;
    return y0 + Math.min(1, (t - from) / (to - from)) * (y1 - y0);
  }
  function follow(force) {
    if (!cues.length || !now || !visible()) return;
    var t = opts.time() || 0;
    var i = index(t);
    var yy = y(t, i);
    now.style.top = yy + 'px';
    var cur = i >= 0 && t < cues[i].end ? i : -1;
    if (cur !== active) {
      if (active >= 0) cueEls[active].classList.remove('on');
      if (cur >= 0) cueEls[cur].classList.add('on');
      active = cur;
    }
    if (!force && Date.now() < userUntil) return;
    var top = list.scrollTop, h = list.clientHeight;
    var pos = yy + track.offsetTop;
    if (!force && pos >= top + h * 0.15 && pos <= top + h * 0.7) return;
    var target = Math.max(0, pos - h * 0.3);
    var smooth = !force && Math.abs(target - top) < 3 * h;
    try { list.scrollTo({ top: target, behavior: smooth ? 'smooth' : 'auto' }); }
    catch (e) { list.scrollTop = target; }
  }
  ['wheel', 'touchmove', 'mousedown', 'keydown'].forEach(function (ev) {
    list.addEventListener(ev, function () { userUntil = Date.now() + 6000; }, { passive: true });
  });
  // Click a line: seek there (modifier clicks keep the link's own meaning).
  track.addEventListener('click', function (e) {
    var a = e.target && e.target.closest ? e.target.closest('a.cue') : null;
    if (!a || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    var c = cues[+a.dataset.i];
    if (!c) return;
    userUntil = 0;
    opts.seek(c.start);
    follow(true);
  });
  var resizeTimer = null;
  addEventListener('resize', function () {
    if (!cues.length || !visible()) return;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(function () { if (visible()) { layout(); follow(true); } }, 150);
  });
  return { setCues: render, empty: empty, follow: follow, layout: layout,
           hasCues: function () { return cues.length > 0; } };
};
</script>"#;

/// The pop-out window's script: it shows one program's captions (fetched
/// as the same WebVTT the player uses) and follows the player tab that
/// opened it over a BroadcastChannel, by session id — position and
/// program in, seeks out. When the player moves to the next episode, the
/// window loads that one's captions. "Dock" asks the player to show its
/// own panel again and closes the window.
const CAPTIONS_SCRIPT: &str = r#"<script>
(function () {
  var list = document.getElementById('cc-list'), track = document.getElementById('cc-track');
  var titleEl = document.getElementById('cc-title');
  var session = new URLSearchParams(location.search).get('s') || '';
  var id = location.pathname.replace(/.*\//, '');
  var state = { t: 0, duration: NaN };
  var bc = null;
  try { bc = new BroadcastChannel('mediaserver-player'); } catch (e) {}
  function post(m) { if (bc) { m.s = session; bc.postMessage(m); } }
  var panel = ccPanel(list, track, {
    time: function () { return state.t; },
    duration: function () { return state.duration; },
    href: function (t) { return '/play/' + id + '#' + Math.floor(t) + 's'; },
    seek: function (t) { state.t = t; post({ type: 'seek', t: t }); }
  });
  // WebVTT to cues: blocks with a timing line; tags and ASS-style
  // positioning stripped, entities decoded, whitespace folded.
  function decode(s) { var ta = document.createElement('textarea'); ta.innerHTML = s; return ta.value; }
  function ts(s) {
    var p = s.trim().split(' ')[0].split(':');
    var sec = parseFloat(p.pop()), m = parseInt(p.pop() || '0', 10), h = parseInt(p.pop() || '0', 10);
    return h * 3600 + m * 60 + sec;
  }
  function parseVtt(text) {
    var out = [], blocks = text.replace(/\r/g, '').split(/\n\n+/);
    for (var i = 0; i < blocks.length; i++) {
      var lines = blocks[i].split('\n'), ti = -1;
      for (var j = 0; j < lines.length; j++) if (lines[j].indexOf('-->') >= 0) { ti = j; break; }
      if (ti < 0) continue;
      var times = lines[ti].split('-->');
      var start = ts(times[0]), end = ts(times[1] || '');
      if (isNaN(start) || isNaN(end)) continue;
      var body = lines.slice(ti + 1).join(' ').replace(/<[^>]*>/g, '').replace(/\{\\[^}]*\}/g, '');
      body = decode(body).replace(/\s+/g, ' ').trim();
      if (body) out.push({ start: start, end: end, text: body });
    }
    out.sort(function (a, b) { return a.start - b.start; });
    return out;
  }
  function load(newId) {
    id = newId;
    panel.empty('Loading captions…');
    fetch('/subs/' + newId + '.vtt').then(function (r) {
      if (!r.ok) throw 0;
      return r.text();
    }).then(function (text) {
      if (id !== newId) return;
      var cues = parseVtt(text);
      if (cues.length) panel.setCues(cues); else panel.empty('No captions for this program.');
    }).catch(function () {
      if (id === newId) panel.empty('No captions for this program.');
    });
  }
  if (bc) {
    var goneTimer = null;
    bc.onmessage = function (e) {
      var m = e.data;
      if (!m || m.s !== session) return;
      if (m.type === 'gone') {
        // The player page is going away. A reload or a move to another
        // program brings a player back within the grace period (its
        // ping or state cancels this); otherwise this window is orphaned.
        clearTimeout(goneTimer);
        goneTimer = setTimeout(function () { window.close(); }, 4000);
        return;
      }
      clearTimeout(goneTimer);
      if (m.type === 'ping') { post({ type: 'pong' }); return; }   // the player asks if we are still here
      if (m.type === 'close') { window.close(); return; }
      if (m.type !== 'state') return;
      if (m.title && titleEl.textContent !== m.title) {
        titleEl.textContent = m.title;
        document.title = 'Captions — ' + m.title;
      }
      state.t = m.t; state.duration = m.duration;
      if (m.id && m.id !== id) load(m.id);
      panel.follow(false);
    };
  } else {
    titleEl.textContent = 'This browser cannot follow the player from a second window.';
  }
  document.getElementById('cc-dock').addEventListener('click', function () {
    post({ type: 'dock' });
    window.close();
  });
  // Closed by any route (dock, the player's request, the window's own
  // close box): the player unlights its button.
  addEventListener('pagehide', function () { post({ type: 'bye' }); });
  load(id);
  post({ type: 'hello' });   // a paused player answers with its state
})();
</script>"#;

/// The cover grid's hover cards: a cover's details card is fetched from
/// /card/{id} the first time the pointer rests on it (a short delay, so
/// sweeping across the grid fetches nothing) or it takes focus, kept
/// thereafter, and opened leftward when it would run off the right edge.
const COVERS_SCRIPT: &str = r#"<script>
(function () {
  var grid = document.querySelector('div.covers');
  if (!grid) return;
  var timer = null, current = null;
  function show(cover) {
    var card = cover.querySelector('.card');
    if (!card) return;
    var width = Math.min(parseFloat(getComputedStyle(card).width) || 440, innerWidth * 0.9);
    cover.classList.toggle('flip', cover.getBoundingClientRect().left + width > innerWidth - 8);
    if (card.dataset.state) return;
    card.dataset.state = 'loading';
    fetch('/card/' + cover.dataset.card).then(function (r) {
      if (!r.ok) throw 0;
      return r.text();
    }).then(function (html) {
      card.innerHTML = html;
      card.dataset.state = 'loaded';
      card.classList.add('loaded');
    }).catch(function () { delete card.dataset.state; });
  }
  grid.addEventListener('mouseover', function (e) {
    var cover = e.target.closest ? e.target.closest('.cover') : null;
    if (!cover || cover === current) return;
    current = cover;
    clearTimeout(timer);
    timer = setTimeout(function () { show(cover); }, 120);
  });
  grid.addEventListener('mouseout', function (e) {
    var cover = e.target.closest ? e.target.closest('.cover') : null;
    if (cover && !cover.contains(e.relatedTarget)) { current = null; clearTimeout(timer); }
  });
  grid.addEventListener('focusin', function (e) {
    var cover = e.target.closest ? e.target.closest('.cover') : null;
    if (cover) show(cover);
  });
})();
</script>"#;

/// The player's script: resume (the playback position rides in the URL
/// fragment, so a paused or scrubbed player yields a bookmarkable link
/// that picks up where it left off — fragments never reach the server, so
/// this is client side; the position is also stashed in sessionStorage
/// per file, backing a 30-second "resume at m:ss" offer for viewers who
/// come back via links instead of Back), ← / → skipping 10 seconds and space toggling
/// play/pause wherever focus is, asynchronous subtitle
/// extraction, the captions panel (every subtitle line down a column at the
/// right, laid out along the running time, click to seek — see the CC block
/// inside), and auto-play of the next episode. That last one swaps the
/// next episode's page pieces into this one instead of navigating: the
/// <video> element survives, so a fullscreen player stays fullscreen, and
/// pushState keeps the URL truthful for reload/bookmark/resume. The same
/// script serves the music detail page, whose <audio> is the player: it
/// fetches the page at the same path prefix (/play/ or /item/) and
/// replaces every element marked data-swap, and links carrying
/// data-swap-id (prior/next/next-up) swap instead of navigating.
const PLAYER_SCRIPT: &str = r#"<script>
(function () {
  var v = document.getElementById('player');
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
    if (!rejoinHold) savePos(v.dataset.id, t);
    history.replaceState(null, '', location.pathname + (t > 0 ? '#' + t + 's' : ''));
  }
  v.addEventListener('timeupdate', stamp);
  v.addEventListener('pause', stamp);
  v.addEventListener('seeked', stamp);
  // Arrow keys skip 10 s and space toggles play/pause, wherever focus
  // is. Registered on the capture phase and stopping propagation so the
  // native controls (which seek by their own step and toggle on space
  // when the <video> is focused) never see the key — otherwise the two
  // actions would stack. Modified keys and text fields are left alone,
  // and so is space on a focused button: that is how a button is pressed.
  function ownsKey(e) {
    if (e.altKey || e.ctrlKey || e.metaKey || e.shiftKey) return false;
    var t = e.target, tag = t && t.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (t && t.isContentEditable)) return false;
    var space = e.key === ' ' || e.key === 'Spacebar';
    if (space && tag === 'BUTTON') return false;
    return space || e.key === 'ArrowLeft' || e.key === 'ArrowRight';
  }
  document.addEventListener('keydown', function (e) {
    if (!ownsKey(e)) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === ' ' || e.key === 'Spacebar') {
      if (e.repeat) return;   // a held key is one press, not a flicker
      if (v.paused) {
        var p = v.play();
        if (p && p.catch) p.catch(function () {});
      } else {
        v.pause();
      }
      return;
    }
    var step = e.key === 'ArrowLeft' ? -10 : 10;
    var end = isFinite(v.duration) ? v.duration : Infinity;
    try { v.currentTime = Math.max(0, Math.min(end, (v.currentTime || 0) + step)); } catch (err) {}
  }, true);
  // The keyup as well: a button presses on space's keyup, and the native
  // controls' play button is one.
  document.addEventListener('keyup', function (e) {
    if (ownsKey(e)) { e.preventDefault(); e.stopPropagation(); }
  }, true);

  // Subtitles that need extracting (#subs carries the file id) are fetched
  // while the video already plays and attached when ready.
  function attachSubs() {
    var note = document.getElementById('subs');
    if (!note || !note.dataset.extract) return;
    var id = note.dataset.extract;
    fetch('/subs/' + id + '.vtt').then(function (r) {
      if (!r.ok) throw 0;
      return r.blob();
    }).then(function (b) {
      if (v.dataset.id !== id) return;   // moved on to another episode meanwhile
      var t = document.createElement('track');
      t.kind = 'subtitles'; t.label = 'Subtitles'; t.srclang = 'en';
      t.src = URL.createObjectURL(b); t.default = true;
      v.appendChild(t); t.track.mode = 'showing';
      note.textContent = 'Subtitles ready.';
      ccRefresh();   // the captions panel can fill in now
    }).catch(function () {
      if (v.dataset.id === id) note.textContent = 'Subtitles could not be extracted.';
    });
  }
  attachSubs();

  // Second, ephemeral resume path: the URL fragment is lost when the
  // viewer wanders off through links and returns some other way than
  // Back, so the position is also stashed in sessionStorage per file id.
  // A stored position surfaces as a "resume at m:ss" link beside the
  // skip hint for 30 seconds; while the offer stands, stamping leaves
  // the stored position alone so playback from 0:00 cannot clobber it,
  // and clicking seeks there. sessionStorage can be walled off (private
  // windows): every access is guarded and the feature sits out.
  function loadPos(id) {
    try { return parseFloat(sessionStorage.getItem('playpos:' + id)) || 0; } catch (e) { return 0; }
  }
  function savePos(id, t) {
    try {
      if (t > 0) sessionStorage.setItem('playpos:' + id, String(t));
      else sessionStorage.removeItem('playpos:' + id);
    } catch (e) {}
  }
  function mmss(t) {
    var m = Math.floor(t / 60), s = Math.floor(t % 60);
    return m + ':' + (s < 10 ? '0' : '') + s;
  }
  // The video page hosts both resume notes in the #rejoin slot beside the
  // skip hint; the music page has only its #resume line below the player.
  var rejoin = document.getElementById('rejoin') || document.getElementById('resume');
  var rejoinTimer = null, rejoinHold = false;
  function dropRejoin() {
    if (rejoinTimer) { clearTimeout(rejoinTimer); rejoinTimer = null; }
    rejoinHold = false;
    if (rejoin) { rejoin.hidden = true; rejoin.textContent = ''; }
  }
  function offerRejoin() {
    dropRejoin();
    if (!rejoin) return;
    var t = loadPos(v.dataset.id);
    if (!(t > 9)) return;   // trivial positions are not worth an offer
    var id = v.dataset.id;
    var a = document.createElement('a');
    a.href = location.pathname + '#' + Math.floor(t) + 's';
    a.textContent = 'resume at ' + mmss(t);
    a.addEventListener('click', function (e) {
      e.preventDefault();
      dropRejoin();
      // Metadata may still be loading; seeking before it throws away the
      // position, so defer like the fragment path (id-checked: the page
      // may have swapped to another episode by then).
      var seekBack = function () {
        if (v.dataset.id === id) { try { v.currentTime = t; } catch (err) {} }
      };
      if (v.readyState >= 1) seekBack();
      else v.addEventListener('loadedmetadata', seekBack, { once: true });
      var p = v.play();
      if (p && p.catch) p.catch(function () {});
    });
    rejoin.appendChild(a);
    rejoin.hidden = false;
    rejoinHold = true;
    rejoinTimer = setTimeout(dropRejoin, 30000);
  }
  // "Resuming at m:ss — start from the beginning", 30 seconds in the
  // rejoin slot. No hold: this resume is already in motion.
  function showResuming(t) {
    if (!rejoin) return;
    dropRejoin();
    rejoin.innerHTML = 'Resuming at ' + mmss(t) +
      ' — <a href="' + location.pathname + '">start from the beginning</a>';
    rejoin.hidden = false;
    rejoinTimer = setTimeout(dropRejoin, 30000);
  }
  // Catch positions the 1-second stamping missed on the way out, and
  // remember whether playback was running for the pageshow handler.
  var wasPlaying = false;
  addEventListener('pagehide', function () {
    wasPlaying = !v.paused && !v.ended;
    if (!rejoinHold && honoured) savePos(v.dataset.id, Math.floor(v.currentTime || 0));
  });
  // Back to a page the browser kept in its back/forward cache: nothing
  // re-runs and media is paused on entry, so the video sits silent at the
  // right position with no note. Restore what a fresh fragment load would
  // have done: the resuming note, and playback if it was running.
  addEventListener('pageshow', function (e) {
    if (!e.persisted) return;
    var t = Math.floor(v.currentTime || 0);
    if (t > 0) showResuming(t);
    if (wasPlaying) {
      var p = v.play();
      if (p && p.catch) p.catch(function () {});
    }
  });

  // Skip intro / credits: segments the catalog knows (named chapter
  // markers or .edl sidecars) surface as a button over the player while
  // playback is inside one — a nudge, never an auto-skip. A credits
  // segment usually reaches the end of the file, so skipping it fires
  // 'ended' and the auto-play-next machinery takes over from there.
  var skipBtn = document.getElementById('skipseg');
  var segs = [];
  function loadSegs() {
    try { segs = JSON.parse(v.dataset.segments || '[]'); } catch (e) { segs = []; }
  }
  loadSegs();
  var SEG_LABELS = { intro: 'Skip intro', credits: 'Skip credits', recap: 'Skip recap' };
  function activeSeg() {
    var t = v.currentTime || 0;
    for (var i = 0; i < segs.length; i++) {
      if (t >= segs[i].start && t < segs[i].end - 0.5) return segs[i];
    }
    return null;
  }
  if (skipBtn) {
    v.addEventListener('timeupdate', function () {
      var s = activeSeg();
      if (!s) { skipBtn.hidden = true; return; }
      skipBtn.textContent = SEG_LABELS[s.kind] || 'Skip';
      skipBtn.hidden = false;
    });
    skipBtn.addEventListener('click', function () {
      var s = activeSeg();
      if (!s) return;
      var end = isFinite(v.duration) ? v.duration : s.end;
      try { v.currentTime = Math.min(s.end, end); } catch (e) {}
      skipBtn.hidden = true;
    });
  }

  // Auto-play next: remembered per browser, on unless switched off.
  var box = document.getElementById('autonext');
  if (box) {
    try { box.checked = localStorage.getItem('autonext') !== '0'; } catch (e) {}
    box.addEventListener('change', function () {
      try { localStorage.setItem('autonext', box.checked ? '1' : '0'); } catch (e) {}
    });
  }
  // Captions panel ("CC" at the right end of the hint line): every line
  // of the subtitle track down a column at the right edge, laid out along
  // the running time by the shared panel code (CC_PANEL_SCRIPT — the
  // pop-out window uses it too). This side reads the cues from the
  // <track> the browser already parsed — the .srt sidecar as WebVTT, or
  // the extracted embedded track — so nothing is fetched twice; keeps
  // the page layout and the button in step; and talks to a popped-out
  // captions window over a BroadcastChannel keyed by a per-tab session
  // (position and program out, seeks in). Remembered per browser like
  // auto-play; shown only while the program has captions.
  // Where the captions are: 'off', 'panel' (the column in this page) or
  // 'window' (popped out). The button is lit for either of the last two
  // and a click on it closes whichever is open; the two never coexist.
  var panelEl = document.getElementById('cc-panel');
  var ccMode = 'off', ccGen = 0, ccKey = '', popWin = null;
  try {
    var stored = localStorage.getItem('cc');
    ccMode = stored === '1' ? 'panel' : stored === 'window' ? 'window' : 'off';
  } catch (e) {}
  // A remembered window has to prove it is still there: it is asked
  // below and answers 'pong'; until then the captions count as off.
  var windowExpected = ccMode === 'window';
  if (windowExpected) ccMode = 'off';
  function seekTo(t) {
    dropRejoin();
    try { v.currentTime = t; } catch (err) {}
    var p = v.play();
    if (p && p.catch) p.catch(function () {});
  }
  var panel = panelEl && window.ccPanel ? ccPanel(
    document.getElementById('cc-list'), document.getElementById('cc-track'), {
      time: function () { return v.currentTime || 0; },
      duration: function () { return v.duration; },
      href: function (t) { return location.pathname + '#' + Math.floor(t) + 's'; },
      seek: seekTo
    }) : null;
  // The session id a pop-out window follows; sessionStorage is per tab
  // and survives reloads, so the window keeps following.
  var session = '';
  try {
    session = sessionStorage.getItem('ccsession') || '';
    if (!session) {
      session = Math.random().toString(36).slice(2, 12);
      sessionStorage.setItem('ccsession', session);
    }
  } catch (e) { session = String(Date.now()); }
  var bc = null;
  try { bc = new BroadcastChannel('mediaserver-player'); } catch (e) {}
  function ccPost() {
    if (!bc) return;
    bc.postMessage({ s: session, type: 'state', id: v.dataset.id, title: document.title,
      t: v.currentTime || 0, duration: v.duration, paused: v.paused });
  }
  if (bc) {
    ['timeupdate', 'seeked', 'play', 'pause', 'loadedmetadata'].forEach(function (ev) {
      v.addEventListener(ev, ccPost);
    });
    bc.onmessage = function (e) {
      var m = e.data;
      if (!m || m.s !== session) return;
      if (m.type === 'seek') seekTo(m.t);
      else if (m.type === 'hello' || m.type === 'pong') {
        // A window for this tab is alive (just opened, or found again
        // after this page reloaded): the captions are there.
        if (ccMode !== 'window') setMode('window');
        if (m.type === 'hello') ccPost();
      }
      else if (m.type === 'dock') setMode('panel');
      else if (m.type === 'bye' && ccMode === 'window') setMode('off');
    };
    if (windowExpected) bc.postMessage({ s: session, type: 'ping' });
    // Leaving (closing the tab, navigating away, reloading): tell the
    // window, which closes unless a player page in this tab comes back
    // within a few seconds — a reload, or a move to another program.
    addEventListener('pagehide', function () { bc.postMessage({ s: session, type: 'gone' }); });
  }
  function setMode(mode) {
    ccMode = mode;
    try { localStorage.setItem('cc', mode === 'panel' ? '1' : mode === 'window' ? 'window' : '0'); } catch (e) {}
    ccRefresh();
  }
  // The window closes on request — by message (this page may have
  // reloaded since it opened the window and lost its handle) and by the
  // handle when there is one.
  function closeWindow() {
    if (bc) bc.postMessage({ s: session, type: 'close' });
    if (popWin && !popWin.closed) { try { popWin.close(); } catch (e) {} }
    popWin = null;
    setMode('off');
  }
  // Sync the button, the page layout and the panel with the state and
  // whatever track the player carries right now. Called on toggle, after
  // an episode swap, and when an extracted track arrives.
  function ccRefresh() {
    var btn = document.getElementById('cc');
    if (!panelEl || !panel) return;
    var show = ccMode === 'panel' && !!btn && !btn.hidden;
    if (btn) {
      btn.setAttribute('aria-pressed', ccMode === 'off' ? 'false' : 'true');
      btn.classList.toggle('detached', ccMode === 'window');
      btn.title = ccMode === 'window'
        ? 'The captions are in their own window — click to close it'
        : 'Captions panel: every line along the running time — click one to jump there';
    }
    document.body.classList.toggle('cc', show);
    panelEl.hidden = !show;
    if (!show) return;
    var gen = ++ccGen, id = v.dataset.id;
    var trackEl = v.querySelector('track');
    if (!trackEl) {
      var note = document.getElementById('subs');
      ccKey = '';
      panel.empty(note && note.dataset.extract ? 'Extracting captions from the file…' : 'No captions for this program.');
      return;
    }
    var key = id + '|' + trackEl.src;
    if (key === ccKey && panel.hasCues()) { panel.layout(); panel.follow(true); return; }
    ccKey = '';
    panel.empty('Loading captions…');
    readCues(trackEl, function (list) {
      if (gen !== ccGen || v.dataset.id !== id) return;
      if (!list.length) { panel.empty('No captions for this program.'); return; }
      ccKey = key;
      panel.setCues(list);
    });
  }
  // Plain text of a cue: the browser's own rendering of the WebVTT markup
  // (<i>, <c>, entities), then any ASS-style {\an8} positioning the sidecar
  // carried through, newlines folded.
  function cueText(c) {
    var s;
    try { s = c.getCueAsHTML().textContent; } catch (e) { s = String(c.text || '').replace(/<[^>]*>/g, ''); }
    return s.replace(/\{\\[^}]*\}/g, '').replace(/\s+/g, ' ').trim();
  }
  // A track exposes its cues only once loaded and while not disabled (a
  // viewer may have switched captions off in the native controls);
  // 'hidden' loads them without drawing them.
  function readCues(trackEl, cb) {
    var tt = trackEl.track;
    function snap() {
      if (tt.mode === 'disabled') tt.mode = 'hidden';
      var out = [], list = tt.cues || [];
      for (var i = 0; i < list.length; i++) {
        var text = cueText(list[i]);
        if (text) out.push({ start: list[i].startTime, end: list[i].endTime, text: text });
      }
      cb(out);
    }
    if (trackEl.readyState === 2) return snap();
    if (trackEl.readyState === 3) return cb([]);
    trackEl.addEventListener('load', snap, { once: true });
    trackEl.addEventListener('error', function () { cb([]); }, { once: true });
    if (tt.mode === 'disabled') tt.mode = 'hidden';
  }
  // Pop out: the captions in a window of their own (draggable to another
  // monitor), and the page gets its width back. Reusing the window name
  // brings an existing one forward instead of opening a second.
  function popOut() {
    var w = window.open('/captions/' + v.dataset.id + '?s=' + encodeURIComponent(session),
      'mediaserver-captions', 'popup=yes,width=440,height=820');
    if (w) { popWin = w; setMode('window'); }
  }
  if (panelEl && panel) {
    v.addEventListener('timeupdate', function () { panel.follow(false); });
    v.addEventListener('seeked', function () { panel.follow(false); });
    // Delegated: the button is swapped along with the episode.
    document.addEventListener('click', function (e) {
      var t = e.target;
      if (!t || !t.closest) return;
      if (t.closest('#cc')) {
        if (ccMode === 'window') closeWindow();
        else setMode(ccMode === 'panel' ? 'off' : 'panel');
      }
      else if (t.closest('#cc-close')) setMode('off');
      else if (t.closest('#cc-pop')) popOut();
    });
    ccRefresh();
  }
  function replaceById(id, doc) {
    var mine = document.getElementById(id), theirs = doc.getElementById(id);
    if (mine && theirs) mine.replaceWith(document.importNode(theirs, true));
  }
  function each(list, f) { Array.prototype.slice.call(list).forEach(f); }
  var prefix = location.pathname.replace(/[^\/]*$/, '');   // "/play/" or "/item/"
  function swapTo(id) {
    fetch(prefix + id).then(function (r) {
      if (!r.ok) throw 0;
      return r.text();
    }).then(function (html) {
      var doc = new DOMParser().parseFromString(html, 'text/html');
      var nv = doc.getElementById('player');
      if (!nv) throw 0;
      v.pause();
      each(v.querySelectorAll('source, track'), function (n) { n.remove(); });
      if (nv.hasAttribute('poster')) v.setAttribute('poster', nv.getAttribute('poster'));
      else v.removeAttribute('poster');
      each(nv.querySelectorAll('source, track'), function (n) { v.appendChild(document.importNode(n, true)); });
      v.dataset.id = nv.dataset.id;
      v.dataset.next = nv.dataset.next || '';
      v.dataset.segments = nv.dataset.segments || '[]';
      loadSegs();
      if (skipBtn) skipBtn.hidden = true;
      document.title = doc.title;
      each(document.querySelectorAll('[data-swap]'), function (el) { replaceById(el.id, doc); });
      last = -1;
      history.pushState({ id: id }, '', prefix + id);
      v.load();
      var p = v.play();
      if (p && p.catch) p.catch(function () {});
      attachSubs();
      ccRefresh();     // the captions panel follows the new track
      ccPost();        // and so does a popped-out window
      offerRejoin();   // the incoming episode may have its own stored position
    }).catch(function () {
      location.href = prefix + id;   // plain navigation as the fallback
    });
  }
  v.addEventListener('ended', function () {
    savePos(v.dataset.id, 0);   // finished: no resume offer next time
    if (box && box.checked && v.dataset.next) swapTo(v.dataset.next);
  });
  // Prior / next / next-up links: swap in place (plain clicks only —
  // modifier clicks keep their open-in-new-tab meaning).
  document.addEventListener('click', function (e) {
    var a = e.target && e.target.closest ? e.target.closest('a[data-swap-id]') : null;
    if (!a || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    e.preventDefault();
    swapTo(a.dataset.swapId);
  });
  // Back/forward across swapped episodes: just render whatever the URL says.
  window.addEventListener('popstate', function () { location.reload(); });

  var start = parseFloat((location.hash || '').replace(/[^0-9.]/g, ''));
  if (!(start > 0)) {
    // No fragment resume in motion: offer the stored position instead.
    offerRejoin();
    return;
  }
  honoured = false;
  function seek() {
    try { v.currentTime = start; } catch (e) {}
    honoured = true;
  }
  if (v.readyState >= 1) seek();
  else v.addEventListener('loadedmetadata', seek, { once: true });
  showResuming(start);
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

/// The episode (or track) before and after this one, in series (album)
/// order. Renditions of one episode are
/// merged there, so a lower-quality copy still finds its place. Movies
/// have no natural neighbours and get (None, None).
fn neighbours(
    conn: &rusqlite::Connection,
    detail: &files::ItemDetail,
) -> (Option<media_db::BrowseItem>, Option<media_db::BrowseItem>) {
    let siblings = match (detail.kind, &detail.series, detail.season) {
        // The whole series in season/episode order, so the last episode
        // of a season continues into the next season.
        (media_db::MediaKind::Tv, Some(series), _) => tv::series_episodes(conn, series),
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

fn neighbour_noun(kind: media_db::MediaKind) -> &'static str {
    match kind {
        media_db::MediaKind::Music => "song",
        _ => "episode",
    }
}

/// "03 - Title" for an episode (prefixed "S2 " when it is in a different
/// season than the one being viewed), "3. Title" for a track.
fn neighbour_label(kind: media_db::MediaKind, season: Option<i64>, item: &media_db::BrowseItem) -> String {
    match (kind, item.episode, item.track_no) {
        (media_db::MediaKind::Tv, Some(n), _) => {
            let prefix = match item.season {
                Some(s) if Some(s) != season => format!("S{s} "),
                _ => String::new(),
            };
            format!("{prefix}{n:02} - {}", xml_escape(&item.title))
        }
        (media_db::MediaKind::Music, _, Some(n)) => format!("{n}. {}", xml_escape(&item.title)),
        _ => xml_escape(&item.title),
    }
}

/// "« Prior episode: Title" on the left, "Next episode: Title »" on the
/// right, spanning the same width as the details above.
fn neighbour_nav_html(
    kind: media_db::MediaKind,
    season: Option<i64>,
    prev: Option<&media_db::BrowseItem>,
    next: Option<&media_db::BrowseItem>,
) -> String {
    if prev.is_none() && next.is_none() {
        return String::new();
    }
    let noun = neighbour_noun(kind);
    let label = |item: &media_db::BrowseItem| neighbour_label(kind, season, item);
    // data-swap-id: with a player on the page the script swaps the
    // neighbour in without a navigation; otherwise a plain link.
    let prev_html = prev
        .map(|item| {
            format!(
                "<a href=\"/item/{0}\" data-swap-id=\"{0}\">« Prior {noun}: {1}</a>",
                item.file_id,
                label(item)
            )
        })
        .unwrap_or_default();
    let next_html = next
        .map(|item| {
            format!(
                "<a href=\"/item/{0}\" data-swap-id=\"{0}\">Next {noun}: {1} »</a>",
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

/// In-browser player: native <video> controls, poster art, the .srt
/// sidecar as a selectable subtitle track, and the "CC" captions panel
/// that lists the track's lines along the running time.
async fn play_page(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    https: Option<Extension<Https>>,
) -> Response {
    let conn = state.db.lock().await;
    let detail = files::detail(&conn, id);
    let servable = files::servable(&conn, id);
    let (Ok(Some(detail)), Ok(Some(servable))) = (detail, servable) else {
        drop(conn);
        return StatusCode::NOT_FOUND.into_response();
    };
    let (prev, next) = neighbours(&conn, &detail);
    let segments = queries::segments::for_file(&conn, id).unwrap_or_default();
    drop(conn);
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
    // Always present (hidden when empty) so an in-place episode swap has
    // an element to replace.
    let context_line = format!(
        "<p id=\"context\" data-swap style=\"color:#aaa;margin:.2em 0 .8em{}\">{context}</p>",
        if context.is_empty() { ";display:none" } else { "" }
    );

    // Everything the detail page knows, in a hover card on the "details"
    // link beside the heading — the same card the cover grid fetches.
    let mut card = info_card(id, &detail, &context);
    // Prior/next for serial programs. neighbours() orders the whole
    // series, so the last episode of a season leads into the next season
    // and vice versa; the links stay in the player (and swap in place).
    if prev.is_some() || next.is_some() {
        let noun = neighbour_noun(detail.kind);
        let link = |item: &media_db::BrowseItem, text: String| {
            format!("<a href=\"/play/{0}\" data-swap-id=\"{0}\">{text}</a>", item.file_id)
        };
        let prev_html = prev
            .as_ref()
            .map(|item| {
                link(item, format!("« Prior {noun}: {}", neighbour_label(detail.kind, detail.season, item)))
            })
            .unwrap_or_default();
        let next_html = next
            .as_ref()
            .map(|item| {
                link(item, format!("Next {noun}: {} »", neighbour_label(detail.kind, detail.season, item)))
            })
            .unwrap_or_default();
        card.push_str(&format!(
            "<span class=\"nav\"><span>{prev_html}</span><span>{next_html}</span></span>"
        ));
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
    let instant_subs = sidecar::is_regular_within(&servable.abs_path.with_extension("srt"), sidecar::MAX_TEXT)
        || state.vtt_cache.join(format!("{id}.vtt")).is_file();
    let subs_note = |extract: bool, text: &str| {
        format!(
            "<p id=\"subs\" data-swap data-extract=\"{}\" style=\"color:#888;font-size:.85em\">{text}</p>",
            if extract { id.to_string() } else { String::new() }
        )
    };
    let (track, subs_async, has_subs) = if instant_subs {
        (
            format!(
                "<track kind=\"subtitles\" src=\"/subs/{id}.vtt\" \
                 srclang=\"en\" label=\"Subtitles\" default>"
            ),
            subs_note(false, ""),
            true,
        )
    } else if state
        .subs_inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&id)
        || text_sub_stream(&state, id, &servable.abs_path).await.is_some()
    {
        (
            String::new(),
            subs_note(
                true,
                "⏳ Extracting subtitles from the file — the video can play meanwhile; \
                 captions appear when ready (may take a minute for large files)…",
            ),
            true,
        )
    } else {
        (String::new(), subs_note(false, ""), false)
    };
    // Episodes and album tracks continue into the next one when it ends;
    // the script swaps the player's contents in place (see PLAYER_SCRIPT).
    let next_id = next.as_ref().map(|n| n.file_id.to_string()).unwrap_or_default();
    let autonext = if detail.kind == media_db::MediaKind::Movies {
        String::new()
    } else {
        let noun = neighbour_noun(detail.kind);
        let next_note = match &next {
            Some(n) => format!(
                "Next up: <a href=\"/play/{0}\" data-swap-id=\"{0}\">{1}</a>",
                n.file_id,
                neighbour_label(detail.kind, detail.season, n)
            ),
            None => format!("This is the last {noun} available."),
        };
        format!(
            "<p class=\"controls\" style=\"color:#aaa;font-size:.9em\"><label><input type=\"checkbox\" \
             id=\"autonext\" checked> Auto-play next {noun}</label>\
             <span id=\"next-note\" data-swap>{next_note}</span></p>"
        )
    };
    // The "CC" toggle for the captions panel rides at the right border of
    // the hint line (skip hint left, resume notes centred); PLAYER_SCRIPT
    // fills the panel from the subtitle track. A program without captions
    // keeps the button hidden rather than absent, so an in-place episode
    // swap has an element to replace either way. The panel is fixed to
    // the right edge on desktop; on phones it flows in below the controls.
    let cc_hidden = if has_subs { "" } else { " hidden" };
    let panel = "<aside id=\"cc-panel\" hidden aria-label=\"Captions\">\
         <div class=\"head\"><span id=\"cc-title\">Captions</span><span class=\"tools\">\
         <button type=\"button\" id=\"cc-pop\" title=\"Open the captions in a window of their own\" \
          aria-label=\"Pop the captions out into a window\">⧉</button>\
         <button type=\"button\" id=\"cc-close\" aria-label=\"Close the captions panel\">×</button>\
         </span></div>\
         <div id=\"cc-list\"><div id=\"cc-track\"></div></div></aside>";
    let og = item_og_meta(
        &request_base_url(&state, &headers, https.is_some()),
        &format!("/play/{id}"),
        &detail,
        &state.friendly_name,
    );
    // Skippable segments (intro/credits) for the script, seconds to match
    // currentTime. Kinds come from the schema's CHECK set, JSON-safe as-is.
    let segments_json = format!(
        "[{}]",
        segments
            .iter()
            .map(|s| {
                format!(
                    "{{\"kind\":\"{}\",\"start\":{},\"end\":{}}}",
                    s.kind.as_str(),
                    s.start_ms as f64 / 1000.0,
                    s.end_ms as f64 / 1000.0
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    );
    let head = page_head(&xml_escape(&tab_title(&detail)), &format!("{CC_STYLE}{PLAYER_STYLE}{og}"));
    let html = format!(
        "{head}<body class=\"player\">\
         <h2 id=\"heading\" data-swap style=\"margin-bottom:.1em\">\
         <a href=\"/\" class=\"home\" title=\"Home\" aria-label=\"Home\">⌂</a>{heading}\
         <span class=\"infowrap\"><a href=\"/item/{id}\">details</a>\
         <span class=\"card\">{card}</span></span></h2>{context_line}\
         <div class=\"videowrap\">\
         <video id=\"player\" controls autoplay playsinline{poster} \
          data-id=\"{id}\" data-next=\"{next_id}\" data-segments=\"{segments_attr}\">\
         <source src=\"/media/{id}\" type=\"{}\">{track}\
         Your browser cannot play this format.</video>\
         <button id=\"skipseg\" hidden style=\"position:absolute;right:1.2em;bottom:3.4em;\
          font-size:1em;padding:.55em 1.1em;background:rgba(15,15,15,.85);color:#fff;\
          border:1px solid #999;border-radius:4px;cursor:pointer\">Skip</button></div>\
         <p class=\"hint\"><span>← / → skip 10 seconds · space play/pause</span>\
         <span id=\"rejoin\" hidden></span>\
         <span class=\"right\"><button id=\"cc\" type=\"button\" data-swap aria-pressed=\"false\" \
          aria-controls=\"cc-panel\" title=\"Captions panel: every line along the \
          running time — click one to jump there\"{cc_hidden}>CC</button></span></p>\
         {autonext}{panel}{subs_async}{CC_PANEL_SCRIPT}{PLAYER_SCRIPT}{PAGE_CLOSE}",
        xml_escape(&servable.mime),
        segments_attr = xml_escape(&segments_json)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// The pop-out captions window for a program: the panel as a page of its
/// own, following the player tab that opened it (see CAPTIONS_SCRIPT).
async fn captions_page(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    let detail = {
        let conn = state.db.lock().await;
        files::detail(&conn, id)
    };
    let Ok(Some(detail)) = detail else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let title = xml_escape(&tab_title(&detail));
    let head = page_head(&format!("Captions — {title}"), &format!("{CC_STYLE}{CAPTIONS_STYLE}"));
    let html = format!(
        "{head}<body class=\"captions\">\
         <aside id=\"cc-panel\" aria-label=\"Captions\">\
         <div class=\"head\"><span id=\"cc-title\">{title}</span><span class=\"tools\">\
         <button type=\"button\" id=\"cc-dock\" title=\"Back into the player page\">dock</button>\
         </span></div>\
         <div id=\"cc-list\"><div id=\"cc-track\"></div></div></aside>\
         {CC_PANEL_SCRIPT}{CAPTIONS_SCRIPT}{PAGE_CLOSE}"
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// The details card for an item: poster, title and year, where it sits
/// (series/season or artist/album), the plot cut to a card's worth, and
/// a line of facts — IMDb rating linked to the entry, genre, director,
/// duration, resolution, codec, size. The player renders it inline on
/// its "details" link; the browse pages' cover grid fetches it from
/// /card/{id} on hover.
fn info_card(id: i64, detail: &files::ItemDetail, context: &str) -> String {
    let mut heading = xml_escape(&detail.title);
    if let Some(year) = detail.year {
        heading.push_str(&format!(" ({year})"));
    }
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
        card.push_str(&format!(
            "<span class=\"plot\">{}</span>",
            xml_escape(&truncate_words(plot, 400))
        ));
    }
    let mut facts: Vec<String> = Vec::new();
    // The rating links to the IMDb entry when we know it — the episode's
    // own tconst for TV, the film's for movies.
    let imdb = detail.imdb_id.as_deref().and_then(imdb_title_id);
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
    card.push_str(&format!("<span class=\"facts\">{}</span>", facts.join(" · ")));
    card
}

/// The details card as an HTML fragment, for hover cards that load on
/// demand (the cover grid). Escaped server-side like every page; the
/// page drops it into the card element as-is.
async fn card_fragment(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    let detail = {
        let conn = state.db.lock().await;
        files::detail(&conn, id)
    };
    let Ok(Some(detail)) = detail else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let context = match (&detail.series, detail.season, detail.episode) {
        (Some(series), Some(season), Some(episode)) => tv_context_html(series, season, episode),
        _ => music_context_html(&detail, false),
    };
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        info_card(id, &detail, &context),
    )
        .into_response()
}

/// "24" for whole rates, "23.976" for the NTSC-style fractions.
fn fps_label(fps: f64) -> String {
    if (fps - fps.round()).abs() < 0.005 {
        format!("{:.0}", fps.round())
    } else {
        format!("{fps:.3}").trim_end_matches('0').to_string()
    }
}

fn is_uhd(width: Option<i64>, height: Option<i64>) -> bool {
    width.unwrap_or(0) > 1920 || height.unwrap_or(0) > 1080
}

/// One listing row: 4K chip, title, and the IMDb rating (movies and
/// episodes that have one) boxed at the right edge.
fn listing_row(item: &media_db::BrowseItem) -> String {
    let chip = if item.kind == media_db::MediaKind::Music {
        String::new()
    } else {
        uhd_chip(is_uhd(item.width, item.height))
    };
    format!(
        "<li style=\"display:flex;align-items:center\">{chip}<a href=\"/item/{0}\">{1}</a>{2}</li>",
        item.file_id,
        xml_escape(&item.title),
        rating_chip(item.rating)
    )
}

/// (background, text) for a rating: green from 7.5, yellow from 5.5, red
/// below — white text on green/red, dark on yellow.
fn rating_colours(rating: f64) -> (&'static str, &'static str) {
    if rating >= 7.5 {
        ("#2e7d32", "#fff")
    } else if rating >= 5.5 {
        ("#fbc02d", "#222")
    } else {
        ("#c62828", "#fff")
    }
}

/// The IMDb rating as a small coloured box at the right edge of a listing
/// row. Empty when the item has no rating.
fn rating_chip(rating: Option<f64>) -> String {
    let Some(rating) = rating else {
        return String::new();
    };
    let (background, colour) = rating_colours(rating);
    format!(
        "<span title=\"IMDb rating\" style=\"margin-left:auto;flex-shrink:0;\
         font-size:.75em;font-weight:bold;color:{colour};background:{background};\
         border-radius:3px;padding:.1em .45em;min-width:2.4em;\
         text-align:center\">{rating:.1}</span>"
    )
}

/// Season × episode grid of rating boxes for a series page: seasons down,
/// episode numbers across, each box linking to the episode's page. Grey
/// "–" for episodes without a rating; blank where a number is missing.
/// Empty when nothing is numbered.
fn episode_grid_html(episodes: &[media_db::BrowseItem]) -> String {
    let numbered: Vec<&media_db::BrowseItem> = episodes
        .iter()
        .filter(|e| e.season.is_some() && e.episode.is_some_and(|n| n > 0))
        .collect();
    let Some(max_episode) = numbered.iter().filter_map(|e| e.episode).max() else {
        return String::new();
    };
    let mut seasons: Vec<i64> = numbered.iter().filter_map(|e| e.season).collect();
    seasons.sort_unstable();
    seasons.dedup();

    let label = "color:#666;font-size:.75em;font-weight:normal;text-align:center;padding:0 .2em";
    let mut html = String::from(
        "<h2 style=\"font-size:1em;margin-top:1.5em\">Episode ratings</h2>\
         <div style=\"overflow-x:auto\"><table style=\"border-collapse:separate;\
         border-spacing:3px;font-size:.85em\"><tr><th></th>",
    );
    for n in 1..=max_episode {
        html.push_str(&format!("<th style=\"{label}\">{n}</th>"));
    }
    html.push_str("</tr>");
    for season in seasons {
        html.push_str(&format!("<tr><th style=\"{label}\">S{season}</th>"));
        for n in 1..=max_episode {
            let cell = numbered
                .iter()
                .find(|e| e.season == Some(season) && e.episode == Some(n));
            match cell {
                Some(e) => {
                    let (background, colour, text) = match e.rating {
                        Some(r) => {
                            let (bg, fg) = rating_colours(r);
                            (bg, fg, format!("{r:.1}"))
                        }
                        None => ("#ddd", "#555", "–".to_string()),
                    };
                    html.push_str(&format!(
                        "<td><a href=\"/item/{}\" title=\"S{season:02}E{n:02} · {}\" \
                         style=\"display:block;min-width:2.6em;padding:.15em .2em;\
                         text-align:center;font-weight:bold;text-decoration:none;\
                         border-radius:3px;color:{colour};background:{background}\">{text}</a></td>",
                        e.file_id,
                        xml_escape(&e.title)
                    ));
                }
                None => html.push_str("<td></td>"),
            }
        }
        html.push_str("</tr>");
    }
    html.push_str("</table></div>");
    html
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
    https: Option<Extension<Https>>,
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
    let nav = neighbour_nav_html(detail.kind, detail.season, prev.as_ref(), next.as_ref());

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
    let imdb = detail.imdb_id.as_deref().and_then(imdb_title_id);
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
        let fps = detail
            .frame_rate
            .map(|fps| format!(" @ {} fps", fps_label(fps)))
            .unwrap_or_default();
        facts.push(("Resolution", format!("{w} × {h}{fps}")));
    }
    let audio = detail.audio_profile.as_deref().or(detail.audio_codec.as_deref());
    match (&detail.video_codec, audio) {
        (Some(v), Some(a)) => facts.push(("Codecs", xml_escape(&format!("{v} video, {a} audio")))),
        (Some(v), None) => facts.push(("Codecs", xml_escape(&format!("{v} video")))),
        (None, Some(a)) => facts.push(("Codec", xml_escape(a))),
        (None, None) => {}
    }
    if let Some(kbps) = detail.audio_bitrate {
        facts.push(("Bitrate", format!("{kbps} kbps")));
    }
    if let Some(hz) = detail.audio_sample_rate {
        let khz = hz as f64 / 1000.0;
        facts.push(("Sample rate", format!("{} kHz", if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") })));
    }
    if let Some(bits) = detail.audio_bit_depth {
        facts.push(("Bit depth", format!("{bits}-bit")));
    }
    if let Some(n) = detail.audio_channels {
        let label = match n {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            6 => "5.1".to_string(),
            8 => "7.1".to_string(),
            n => format!("{n} channels"),
        };
        facts.push(("Channels", label));
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
            "<img src=\"/art/{id}\" alt=\"\" class=\"art\">"
        )
    } else {
        String::new()
    };
    let play_links = if detail.kind == media_db::MediaKind::Music {
        String::new() // the player itself is on the page, below
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
             &nbsp; <small><a href=\"/media/{id}\">direct stream</a></small></p>{warning}"
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
    let og = item_og_meta(
        &request_base_url(&state, &headers, https.is_some()),
        &format!("/item/{id}"),
        &detail,
        &state.friendly_name,
    );
    let head = page_head(&xml_escape(&tab_title(&detail)), &format!("{PLAYER_STYLE}{og}"));
    // Same home icon as the browse pages, plus a jump to the item's section.
    let top_nav = {
        let (section_id, section_name) = match detail.kind {
            media_db::MediaKind::Movies => ("mv", "Movies"),
            media_db::MediaKind::Tv => ("tv", "TV Shows"),
            media_db::MediaKind::Music => ("mu", "Music"),
        };
        format!("<a href=\"/\" class=\"home\" title=\"Home\" aria-label=\"Home\">⌂</a> · <a href=\"/browse/{section_id}\">{section_name}</a>")
    };
    let html = if detail.kind == media_db::MediaKind::Music {
        // The player lives on the detail page itself, and the next track
        // is swapped in place when one ends (see PLAYER_SCRIPT): the
        // regions marked data-swap are replaced, the <audio> stays.
        let next_note = match &next {
            Some(n) => format!(
                "Next up: <a href=\"/item/{0}\" data-swap-id=\"{0}\">{1}</a>",
                n.file_id,
                neighbour_label(detail.kind, detail.season, n)
            ),
            None => "This is the last song available.".to_string(),
        };
        let next_id = next.as_ref().map(|n| n.file_id.to_string()).unwrap_or_default();
        format!(
            "{head}<body class=\"detail\">\
             <p id=\"top\" data-swap>{top_nav}</p>\
             <div id=\"above\" data-swap>{art}<h1 style=\"margin-bottom:.2em\">{heading_html}</h1>\
             {subtitle_html}{plot}</div>\
             <div style=\"overflow:hidden\">\
             <audio id=\"player\" controls preload=\"metadata\" data-id=\"{id}\" data-next=\"{next_id}\" \
              style=\"display:block;width:100%\">\
             <source src=\"/media/{id}\" type=\"{mime}\"></audio>\
             <p class=\"controls\" style=\"color:#666;font-size:.9em;margin:.4em 0\">\
             <label><input type=\"checkbox\" id=\"autonext\" checked> Auto-play next song</label>\
             <span id=\"next-note\" data-swap>{next_note}</span>\
             <small id=\"direct\" data-swap><a href=\"/media/{id}\">direct stream</a></small></p>\
             <p id=\"resume\" style=\"color:#393;font-size:.9em\"></p></div>\
             <div id=\"below\" data-swap><table style=\"border-collapse:collapse\">{rows}</table>{nav}</div>\
             {PLAYER_SCRIPT}{PAGE_CLOSE}",
            mime = xml_escape(&detail.mime)
        )
    } else {
        format!(
            "{head}<body class=\"detail\">\
             <p>{top_nav}</p>{art}<h1 style=\"margin-bottom:.2em\">{heading_html}</h1>\
             {subtitle_html}{plot}{play_links}\
             <table style=\"border-collapse:collapse\">{rows}</table>{nav}{PAGE_CLOSE}"
        )
    };
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
                    starting_index.saturating_add(requested_count).min(total)
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
        starting_index.saturating_add(requested_count).min(total)
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

/// The raw art bytes and their MIME type for an art source. Blocking
/// (file reads, tag parsing) — call from spawn_blocking.
fn read_art(source: files::ArtSource) -> anyhow::Result<(Vec<u8>, String)> {
    let bytes = match source {
        files::ArtSource::File(path) => sidecar::read_capped(&path, sidecar::MAX_IMAGE)?,
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
            let data = picture.data();
            anyhow::ensure!(data.len() as u64 <= sidecar::MAX_IMAGE, "embedded picture too large");
            data.to_vec()
        }
    };
    let mime = image_mime(&bytes).ok_or_else(|| anyhow::anyhow!("art is not an image"))?;
    Ok((bytes, mime.to_string()))
}

/// Art downscaled for link-preview cards: a JPEG capped at 900px on the
/// long edge (a 2:3 poster becomes 600×900). Preview scrapers silently
/// drop images beyond a few hundred KB, which full-size posters routinely
/// exceed. Art that fails to decode — or is already small — passes
/// through unchanged.
fn og_art(bytes: Vec<u8>, mime: String) -> (Vec<u8>, String) {
    const MAX_EDGE: u32 = 900;
    // Decode under limits: a crafted poster (a 40000×40000 PNG is a few
    // KB on disk) must not turn a preview fetch into a multi-GB decode.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let decoded = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|mut reader| {
            reader.limits(limits);
            reader.decode().ok()
        });
    let Some(img) = decoded else {
        return (bytes, mime);
    };
    let img = if img.width().max(img.height()) > MAX_EDGE {
        img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3)
    } else if mime == "image/jpeg" {
        return (bytes, mime);
    } else {
        img
    };
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
    match encoder.encode_image(&img.to_rgb8()) {
        Ok(()) => (out, "image/jpeg".to_string()),
        Err(_) => (bytes, mime),
    }
}

/// `?v=` on an art URL: the page vouches for the version (see art_url),
/// so the response may live in caches for a year.
#[derive(serde::Deserialize)]
struct ArtQuery {
    #[serde(default)]
    v: Option<String>,
}

async fn serve_art_variant(
    state: Arc<AppState>,
    id: i64,
    social: bool,
    request_headers: &HeaderMap,
    versioned: bool,
) -> Response {
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
    // Validators from the file the art comes from (the media file itself
    // for an embedded picture): a client holding a copy revalidates for a
    // 304 instead of downloading again, and a versioned URL never asks.
    let path = match &source {
        files::ArtSource::File(p) | files::ArtSource::Embedded(p) => p.clone(),
    };
    let stamp = std::fs::metadata(&path)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    let etag = stamp.map(|(mtime, len)| {
        let secs = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("\"{secs:x}-{len:x}{}\"", if social { "-og" } else { "" })
    });
    let cache_control = if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=86400, stale-while-revalidate=604800"
    };
    if let Some(etag) = &etag {
        let matches = request_headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
        if matches {
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag.clone()),
                    (header::CACHE_CONTROL, cache_control.to_string()),
                ],
            )
                .into_response();
        }
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<u8>, String)> {
        let (bytes, mime) = read_art(source)?;
        Ok(if social { og_art(bytes, mime) } else { (bytes, mime) })
    })
    .await;
    match result {
        Ok(Ok((bytes, mime))) => {
            let mut response = (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, cache_control.to_string()),
                ],
                bytes,
            )
                .into_response();
            if let (Some(etag), Some((mtime, _))) = (etag, stamp) {
                let h = response.headers_mut();
                if let Ok(v) = HeaderValue::from_str(&etag) {
                    h.insert(header::ETAG, v);
                }
                if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(mtime)) {
                    h.insert(header::LAST_MODIFIED, v);
                }
            }
            response
        }
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

async fn serve_art(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ArtQuery>,
    headers: HeaderMap,
) -> Response {
    serve_art_variant(state, id, false, &headers, q.v.is_some()).await
}

/// The og:image target: the same art, sized for preview cards.
async fn serve_art_og(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Query(q): Query<ArtQuery>,
    headers: HeaderMap,
) -> Response {
    serve_art_variant(state, id, true, &headers, q.v.is_some()).await
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
    if let Ok(value) = format!("http-get:*:{}:{}", servable.mime, DLNA_FEATURES).parse() {
        headers.insert("contentFeatures.dlna.org", value);
    }
    headers.insert("transferMode.dlna.org", "Streaming".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(width, height);
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn og_art_downscales_to_capped_jpeg() {
        let poster = encoded(1200, 1800, image::ImageFormat::Png);
        let (bytes, mime) = og_art(poster, "image/png".to_string());
        assert_eq!(mime, "image/jpeg");
        let img = image::load_from_memory(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (600, 900));
    }

    #[test]
    fn og_art_passes_small_jpeg_through() {
        let small = encoded(400, 600, image::ImageFormat::Jpeg);
        let (bytes, mime) = og_art(small.clone(), "image/jpeg".to_string());
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, small);
    }

    #[test]
    fn og_art_leaves_undecodable_bytes_alone() {
        let junk = b"not an image".to_vec();
        let (bytes, mime) = og_art(junk.clone(), "image/jpeg".to_string());
        assert_eq!((bytes, mime), (junk, "image/jpeg".to_string()));
    }

    #[test]
    fn truncate_words_cuts_on_a_word_boundary() {
        assert_eq!(truncate_words("short enough", 300), "short enough");
        let long = "word ".repeat(100);
        let cut = truncate_words(&long, 30);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= 31);
        assert!(!cut.trim_end_matches('…').ends_with(' '));
    }
}
