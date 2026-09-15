//! The web side of viewer profiles (see `profiles`): the "who's
//! watching?" page, the cookie that remembers the choice, the two
//! reports the player sends (where it left off, what it finished), the
//! seen tick on listings, and the fragments the pages show — continue
//! watching, up next, resume points.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use media_db::queries::{files, tv};
use media_db::BrowseItem;
use rusqlite::Connection;

use crate::didl::xml_escape;
use crate::http::{page_head, AppState, PAGE_CLOSE};
use crate::profiles::{self, Profile, WatchRow};
use crate::tree;

pub type States = HashMap<String, WatchRow>;

/// The profile this request belongs to: the cookie names one that still
/// exists.
pub fn current(state: &AppState, headers: &HeaderMap) -> Option<Profile> {
    let id = profiles::cookie_profile_id(headers)?;
    let conn = state.profiles.lock().unwrap_or_else(|e| e.into_inner());
    profiles::get(&conn, id).ok().flatten()
}

fn store(state: &AppState) -> std::sync::MutexGuard<'_, Connection> {
    state.profiles.lock().unwrap_or_else(|e| e.into_inner())
}

/// The profile's state for the trackable items among `items`.
pub fn states_for<'a>(
    state: &AppState,
    profile: &Profile,
    items: impl Iterator<Item = &'a BrowseItem>,
) -> States {
    let keys: Vec<String> = items.filter_map(profiles::item_key).collect();
    if keys.is_empty() {
        return States::new();
    }
    profiles::states(&store(state), profile.id, &keys).unwrap_or_else(|err| {
        tracing::warn!("watch states: {err:#}");
        States::new()
    })
}

/// The corner chip on every page: who is watching, linking to the
/// picker to switch.
pub fn chip_html(profile: Option<&Profile>) -> String {
    match profile {
        Some(p) => format!(
            "<p class=\"who\"><a href=\"/profiles\" title=\"Switch profile\">👤 {}</a></p>",
            xml_escape(&p.name)
        ),
        None => "<p class=\"who\"><a href=\"/profiles\">👤 Who's watching?</a></p>".to_string(),
    }
}

/// "12:34" / "1:02:03".
fn clock(ms: i64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// What a row's state reads as beside a title: "12:34 left · 2 h ago",
/// "watched yesterday", or nothing.
pub fn note(row: &WatchRow) -> String {
    if row.in_progress() {
        let left = row
            .duration_ms
            .filter(|d| *d > row.position_ms)
            .map(|d| format!("{} left", clock(d - row.position_ms)))
            .unwrap_or_else(|| format!("at {}", clock(row.position_ms)));
        format!("{left} · {}", profiles::when(row.updated_at))
    } else if row.watched {
        format!("watched {}", profiles::when(row.updated_at))
    } else {
        String::new()
    }
}

/// The seen tick and the note for one listing row. Empty without a
/// profile, and for music.
pub fn row_marks(item: &BrowseItem, states: Option<&States>) -> (String, String) {
    let Some(states) = states else { return (String::new(), String::new()) };
    let Some(key) = profiles::item_key(item) else { return (String::new(), String::new()) };
    let row = states.get(&key);
    let checked = if row.is_some_and(|r| r.watched) { " checked" } else { "" };
    let tick = format!(
        "<input type=\"checkbox\" data-seen=\"{}\" title=\"Seen\" aria-label=\"Seen\"{checked}>",
        item.file_id
    );
    let note = format!(
        "<span class=\"wnote\">{}</span>",
        row.map(note).unwrap_or_default()
    );
    (tick, note)
}

/// The seen line on a detail page.
pub fn detail_marks(detail: &files::ItemDetail, state: &AppState, profile: Option<&Profile>) -> String {
    let (Some(profile), Some(key)) = (profile, profiles::detail_key(detail)) else {
        return String::new();
    };
    let row = profiles::one(&store(state), profile.id, &key).ok().flatten();
    let checked = if row.as_ref().is_some_and(|r| r.watched) { " checked" } else { "" };
    format!(
        "<p class=\"controls\" data-watch><label><input type=\"checkbox\" data-seen=\"{}\"{checked}> Seen</label>\
         <span class=\"wnote\">{}</span></p>",
        detail.file_id,
        row.as_ref().map(note).unwrap_or_default()
    )
}

/// Where the player should pick up, in whole seconds, when the profile
/// left this program part-way (the trivial first seconds don't count).
pub fn resume_secs(detail: &files::ItemDetail, state: &AppState, profile: Option<&Profile>) -> Option<i64> {
    let (profile, key) = (profile?, profiles::detail_key(detail)?);
    let row = profiles::one(&store(state), profile.id, &key).ok().flatten()?;
    (row.position_ms > 9000).then_some(row.position_ms / 1000)
}

/// Episodes in series order with the profile's rows: the one to pick up
/// (part-way through), else the first unseen after the last one seen.
fn up_next(episodes: &[BrowseItem], rows: &States) -> Option<(BrowseItem, Option<WatchRow>)> {
    let keyed: Vec<(Option<String>, &BrowseItem)> =
        episodes.iter().map(|e| (profiles::item_key(e), e)).collect();
    let (latest_idx, latest) = keyed
        .iter()
        .enumerate()
        .filter_map(|(i, (k, _))| rows.get(k.as_ref()?).map(|r| (i, r)))
        .max_by_key(|(_, r)| r.updated_at)?;
    if latest.in_progress() {
        return Some((keyed[latest_idx].1.clone(), Some(latest.clone())));
    }
    keyed[latest_idx + 1..]
        .iter()
        .find(|(k, _)| !k.as_ref().and_then(|k| rows.get(k)).is_some_and(|r| r.watched))
        .map(|(_, e)| ((*e).clone(), None))
}

fn play_link(item: &BrowseItem, label: &str) -> String {
    format!("<a href=\"/play/{}\">{}</a>", item.file_id, xml_escape(label))
}

/// A series page's "continue" line.
pub fn series_next_html(state: &AppState, catalog: &Connection, profile: &Profile, series: &str) -> String {
    let rows = match profiles::series_rows(&store(state), profile.id, series) {
        Ok(rows) if !rows.is_empty() => rows.into_iter().map(|r| (r.key.clone(), r)).collect::<States>(),
        _ => return String::new(),
    };
    let episodes = tv::series_episodes(catalog, series).unwrap_or_default();
    let Some((episode, row)) = up_next(&episodes, &rows) else {
        return "<p class=\"wnote\" style=\"margin-left:0\">Every episode seen.</p>".to_string();
    };
    let label = format!(
        "S{:02}E{:02} — {}",
        episode.season.unwrap_or(0),
        episode.episode.unwrap_or(0),
        episode.title
    );
    match row {
        Some(row) => format!(
            "<p>▶ Continue: {} <span class=\"wnote\">{}</span></p>",
            play_link(&episode, &label),
            note(&row)
        ),
        None => format!("<p>▶ Up next: {}</p>", play_link(&episode, &label)),
    }
}

/// The home page's "continue watching": programs left part-way, and
/// the episode after each series' latest finished one, latest activity
/// first.
pub fn continue_html(state: &AppState, catalog: &Connection, profile: &Profile) -> String {
    const SHOW: usize = 12;
    let recent = profiles::recent(&store(state), profile.id, 60).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut series_done: Vec<String> = Vec::new();
    for row in recent {
        if lines.len() >= SHOW {
            break;
        }
        // A vanished file drops out; a remuxed one still finds its row
        // by key, but the play link needs a current file.
        let Ok(Some(item)) = files::browse_item(catalog, row.file_id) else { continue };
        if let Some(series) = item.series.clone().filter(|_| item.kind == media_db::MediaKind::Tv) {
            let folded = series.to_lowercase();
            if series_done.contains(&folded) {
                continue;
            }
            series_done.push(folded);
            let rows = profiles::series_rows(&store(state), profile.id, &series)
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.key.clone(), r))
                .collect::<States>();
            let episodes = tv::series_episodes(catalog, &series).unwrap_or_default();
            match up_next(&episodes, &rows) {
                Some((episode, Some(row))) => lines.push(format!(
                    "<li>▶ {} <span class=\"wnote\">{}</span></li>",
                    play_link(&episode, &tree::recent_tv_title(&episode)),
                    note(&row)
                )),
                Some((episode, None)) => lines.push(format!(
                    "<li>▶ {} <span class=\"wnote\">up next</span></li>",
                    play_link(&episode, &tree::recent_tv_title(&episode))
                )),
                None => {}
            }
        } else if row.in_progress() {
            let label = match item.year {
                Some(year) => format!("{} ({year})", item.title),
                None => item.title.clone(),
            };
            lines.push(format!(
                "<li>▶ {} <span class=\"wnote\">{}</span></li>",
                play_link(&item, &label),
                note(&row)
            ));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "<h2 style=\"font-size:1.1em;margin:1em 0 .2em\">Continue watching</h2>\
         <ul style=\"list-style:none;padding:0;line-height:1.7;margin-top:0\">{}</ul>",
        lines.concat()
    )
}

const PALETTE: [&str; 8] = [
    "#c0392b", "#2980b9", "#27ae60", "#8e44ad", "#d35400", "#16a085", "#7f8c8d", "#c2185b",
];

fn tile_colour(name: &str) -> &'static str {
    let h = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(h % PALETTE.len() as u32) as usize]
}

fn initials(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// The home page's picker, for a visitor without a profile.
pub fn home_picker_html(state: &AppState) -> String {
    let list = profiles::list(&store(state)).unwrap_or_default();
    format!(
        "<h2 style=\"font-size:1.1em;margin:1em 0 0\">Who's watching?</h2>{}",
        picker_html(&list, None, false)
    )
}

pub const PROFILES_STYLE: &str = "<style>\
div.tiles{display:flex;flex-wrap:wrap;gap:1.4em 1.8em;margin:1.2em 0}\
div.tiles form{display:flex;flex-direction:column;align-items:center;gap:.5em;width:7em;text-align:center;overflow-wrap:anywhere}\
button.tile{width:6em;height:6em;border-radius:50%;border:3px solid transparent;color:#fff;font-size:1.4em;font-weight:bold;cursor:pointer}\
button.tile:hover,button.tile:focus{border-color:#0645ad}\
button.tile.current{border-color:#333}\
button.tile.add{background:#eee;color:#555;font-size:2.2em;font-weight:normal;border-style:dashed;border-color:#aaa}\
div.tiles input{width:100%;box-sizing:border-box}\
button.x{font-size:.75em;color:#c00;background:none;border:1px solid #c00;border-radius:3px;cursor:pointer}\
p.err{color:#c00}\
</style>";

/// The tiles: one per profile (a click chooses it), plus "add". With
/// `manage`, each tile carries a remove button as well.
pub fn picker_html(profiles: &[Profile], current: Option<i64>, manage: bool) -> String {
    let mut html = String::from("<div class=\"tiles\">");
    for p in profiles {
        let cls = if current == Some(p.id) { "tile current" } else { "tile" };
        html.push_str(&format!(
            "<form method=\"post\" action=\"/profiles/select\"><input type=\"hidden\" name=\"id\" value=\"{id}\">\
             <button class=\"{cls}\" style=\"background:{colour}\" title=\"Watch as {name}\">{ini}</button>\
             <span>{name}</span>{remove}</form>",
            id = p.id,
            colour = tile_colour(&p.name),
            ini = xml_escape(&initials(&p.name)),
            name = xml_escape(&p.name),
            remove = if manage {
                format!(
                    "<button class=\"x\" formaction=\"/profiles/delete\">Remove {}</button>",
                    xml_escape(&p.name)
                )
            } else {
                String::new()
            }
        ));
    }
    html.push_str(
        "<form method=\"post\" action=\"/profiles/add\">\
         <button class=\"tile add\" title=\"Add a profile\">+</button>\
         <input name=\"name\" maxlength=\"40\" placeholder=\"New name\" required aria-label=\"New profile name\">\
         </form></div>",
    );
    html
}

#[derive(serde::Deserialize)]
pub struct PickerQuery {
    #[serde(default)]
    manage: Option<String>,
}

fn picker_page(state: &AppState, headers: &HeaderMap, manage: bool, error: Option<&str>) -> Response {
    let current = current(state, headers);
    let list = profiles::list(&store(state)).unwrap_or_default();
    let error = error
        .map(|e| format!("<p class=\"err\">{}</p>", xml_escape(e)))
        .unwrap_or_default();
    let intro = if list.is_empty() {
        "<p>Add a name to keep track of what you've watched and where you left off. \
         No password — this is a household server.</p>"
    } else {
        "<p>Pick yourself to keep track of what you've watched and where you left off.</p>"
    };
    let manage_link = if manage {
        "<p><a href=\"/profiles\">Done removing</a></p>".to_string()
    } else if list.is_empty() {
        String::new()
    } else {
        "<p><a href=\"/profiles?manage=1\" style=\"color:#666\">Remove a profile…</a></p>".to_string()
    };
    let signed_in = current
        .as_ref()
        .map(|p| {
            format!(
                "<form method=\"post\" action=\"/profiles/leave\" style=\"margin-top:1.5em;color:#666\">\
                 Watching as <strong>{}</strong>. <button>Forget on this browser</button></form>",
                xml_escape(&p.name)
            )
        })
        .unwrap_or_default();
    let head = page_head("Who's watching?", PROFILES_STYLE);
    let html = format!(
        "{head}<body><p><a href=\"/\" class=\"home\" title=\"Home\" aria-label=\"Home\">⌂</a></p>\
         <h1>Who's watching?</h1>{intro}{error}{}{manage_link}{signed_in}{PAGE_CLOSE}",
        picker_html(&list, current.as_ref().map(|p| p.id), manage)
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

pub async fn profiles_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<PickerQuery>,
) -> Response {
    picker_page(&state, &headers, q.manage.is_some(), None)
}

#[derive(serde::Deserialize)]
pub struct IdForm {
    id: i64,
}

#[derive(serde::Deserialize)]
pub struct NameForm {
    name: String,
}

fn with_cookie(id: Option<i64>, to: &str) -> Response {
    (
        AppendHeaders([(header::SET_COOKIE, profiles::set_cookie(id))]),
        Redirect::to(to),
    )
        .into_response()
}

pub async fn select(State(state): State<Arc<AppState>>, Form(form): Form<IdForm>) -> Response {
    match profiles::get(&store(&state), form.id) {
        Ok(Some(p)) => with_cookie(Some(p.id), "/"),
        _ => Redirect::to("/profiles").into_response(),
    }
}

pub async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NameForm>,
) -> Response {
    let added = profiles::add(&store(&state), &form.name);
    match added {
        Ok(id) => with_cookie(Some(id), "/"),
        Err(err) => picker_page(&state, &headers, false, Some(&err.to_string())),
    }
}

pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<IdForm>,
) -> Response {
    if let Err(err) = profiles::delete(&store(&state), form.id) {
        tracing::warn!("deleting profile {}: {err:#}", form.id);
    }
    // Removing oneself signs out as well.
    if profiles::cookie_profile_id(&headers) == Some(form.id) {
        return with_cookie(None, "/profiles");
    }
    Redirect::to("/profiles").into_response()
}

pub async fn leave() -> Response {
    with_cookie(None, "/profiles")
}

/// What the player sends: the position on the way out of a program
/// ("leave") and once a minute while playing ("tick"); "ended" at its
/// end. Seconds, as the video element counts them.
#[derive(serde::Deserialize)]
struct WatchReport {
    id: i64,
    #[serde(default)]
    event: String,
    #[serde(default)]
    position: f64,
    #[serde(default)]
    duration: Option<f64>,
}

/// POST /api/watch — a beacon, so the body is read as text and answered
/// with nothing. Without a profile there is nothing to record.
pub async fn api_watch(State(state): State<Arc<AppState>>, headers: HeaderMap, body: String) -> StatusCode {
    let Some(profile) = current(&state, &headers) else { return StatusCode::NO_CONTENT };
    let Ok(report) = serde_json::from_str::<WatchReport>(&body) else { return StatusCode::BAD_REQUEST };
    if !report.position.is_finite() || report.position < 0.0 {
        return StatusCode::BAD_REQUEST;
    }
    let detail = {
        let conn = state.db.lock().await;
        files::detail(&conn, report.id)
    };
    let Ok(Some(detail)) = detail else { return StatusCode::NOT_FOUND };
    let Some(key) = profiles::detail_key(&detail) else { return StatusCode::NO_CONTENT };
    let duration_ms = report
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0) as i64)
        .or(detail.duration_ms);
    let position_ms = (report.position * 1000.0) as i64;
    let conn = store(&state);
    let result = if report.event == "ended" {
        profiles::set_finished(&conn, profile.id, &key, detail.file_id, duration_ms)
    } else {
        profiles::set_position(&conn, profile.id, &key, detail.file_id, position_ms, duration_ms)
    };
    if let Err(err) = result {
        tracing::warn!("recording watch state for {}: {err:#}", detail.file_id);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::NO_CONTENT
}

#[derive(serde::Deserialize)]
struct SeenReport {
    id: i64,
    seen: bool,
}

/// POST /api/seen — the tick on a listing. Answers with the note to show.
pub async fn api_seen(State(state): State<Arc<AppState>>, headers: HeaderMap, body: String) -> Response {
    let Some(profile) = current(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "choose a profile first").into_response();
    };
    let Ok(report) = serde_json::from_str::<SeenReport>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let detail = {
        let conn = state.db.lock().await;
        files::detail(&conn, report.id)
    };
    let Ok(Some(detail)) = detail else { return StatusCode::NOT_FOUND.into_response() };
    let Some(key) = profiles::detail_key(&detail) else { return StatusCode::BAD_REQUEST.into_response() };
    let conn = store(&state);
    if let Err(err) = profiles::set_seen(&conn, profile.id, &key, detail.file_id, report.seen) {
        tracing::warn!("recording seen for {}: {err:#}", detail.file_id);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let note = profiles::one(&conn, profile.id, &key)
        .ok()
        .flatten()
        .map(|r| note(&r))
        .unwrap_or_default();
    let json = serde_json::json!({ "seen": report.seen, "note": note });
    ([(header::CONTENT_TYPE, "application/json")], json.to_string()).into_response()
}

/// The seen ticks on listings and detail pages: a change posts to
/// /api/seen and the note beside the title follows the answer.
pub const WATCH_SCRIPT: &str = r#"<script>
(function () {
  document.addEventListener('change', function (e) {
    var box = e.target;
    if (!box || !box.matches || !box.matches('input[data-seen]')) return;
    var holder = box.closest('[data-watch]');
    var note = holder ? holder.querySelector('.wnote') : null;
    var seen = box.checked;
    box.disabled = true;
    fetch('/api/seen', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: parseInt(box.dataset.seen, 10), seen: seen })
    }).then(function (r) {
      if (!r.ok) throw 0;
      return r.json();
    }).then(function (j) {
      if (note) note.textContent = j.note || '';
    }).catch(function () {
      box.checked = !seen;
    }).then(function () {
      box.disabled = false;
    });
  });
})();
</script>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use media_db::MediaKind;

    fn ep(id: i64, season: i64, episode: i64) -> BrowseItem {
        let mut e = BrowseItem::new(id, MediaKind::Tv, format!("Ep {episode}"), "video/mp4".into(), 1);
        e.series = Some("Show".into());
        e.season = Some(season);
        e.episode = Some(episode);
        e
    }

    fn row(key: &str, watched: bool, position_ms: i64, updated_at: i64) -> (String, WatchRow) {
        (
            key.to_string(),
            WatchRow { key: key.to_string(), file_id: 0, position_ms, duration_ms: Some(940_000), watched, updated_at },
        )
    }

    #[test]
    fn up_next_resumes_or_advances_across_seasons() {
        let eps = vec![ep(1, 1, 1), ep(2, 1, 2), ep(3, 2, 1)];
        assert!(up_next(&eps, &States::new()).is_none());

        // Part-way through S01E02: pick it up.
        let rows: States = [row("tv|show|1|1", true, 0, 10), row("tv|show|1|2", false, 500, 20)].into();
        let (next, r) = up_next(&eps, &rows).unwrap();
        assert_eq!((next.file_id, r.is_some()), (2, true));

        // Finished S01E02 last: the first unseen after it is S02E01.
        let rows: States = [row("tv|show|1|1", true, 0, 10), row("tv|show|1|2", true, 0, 20)].into();
        let (next, r) = up_next(&eps, &rows).unwrap();
        assert_eq!((next.file_id, r.is_none()), (3, true));

        // Everything after the latest is seen: nothing to offer.
        let rows: States = [row("tv|show|1|2", true, 0, 20), row("tv|show|2|1", true, 0, 5)].into();
        assert!(up_next(&eps, &rows).is_none());
    }

    #[test]
    fn notes_and_tiles() {
        let (_, r) = row("k", false, 90_000, profiles_now() - 30);
        assert_eq!(note(&r), "14:10 left · just now");
        let (_, r) = row("k", true, 0, profiles_now() - 30);
        assert_eq!(note(&r), "watched just now");
        assert_eq!(initials("dennis forbes"), "DF");
        assert_eq!(initials("Émile"), "É");
        assert_eq!(tile_colour("a"), tile_colour("a"));
    }

    fn profiles_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}
