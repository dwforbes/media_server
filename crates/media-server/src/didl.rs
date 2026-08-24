use media_db::{BrowseItem, MediaKind};

use crate::tree::Entry;

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// DLNA duration format: H:MM:SS.
fn didl_duration(ms: i64) -> String {
    let total_secs = ms / 1000;
    format!(
        "{}:{:02}:{:02}",
        total_secs / 3600,
        (total_secs % 3600) / 60,
        total_secs % 60
    )
}

fn item_class(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Movies => "object.item.videoItem.movie",
        MediaKind::Tv => "object.item.videoItem",
        MediaKind::Music => "object.item.audioItem.musicTrack",
    }
}

/// protocolInfo fourth field: seek-by-byte-range supported, no conversion.
pub const DLNA_FEATURES: &str =
    "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

fn render_item(out: &mut String, id: &str, parent: &str, item: &BrowseItem, base_url: &str) {
    out.push_str(&format!(
        r#"<item id="{}" parentID="{}" restricted="1">"#,
        xml_escape(id),
        xml_escape(parent)
    ));
    out.push_str(&format!("<dc:title>{}</dc:title>", xml_escape(&item.title)));
    out.push_str(&format!(
        "<upnp:class>{}</upnp:class>",
        item_class(item.kind)
    ));
    if let Some(year) = item.year {
        out.push_str(&format!("<dc:date>{year}-01-01</dc:date>"));
    }
    if let Some(genre) = &item.genre {
        out.push_str(&format!("<upnp:genre>{}</upnp:genre>", xml_escape(genre)));
    }
    if let Some(director) = &item.director {
        out.push_str(&format!(
            "<upnp:director>{}</upnp:director>",
            xml_escape(director)
        ));
    }
    if let Some(artist) = &item.artist {
        out.push_str(&format!("<upnp:artist>{}</upnp:artist>", xml_escape(artist)));
        out.push_str(&format!("<dc:creator>{}</dc:creator>", xml_escape(artist)));
    }
    if let Some(album) = &item.album {
        out.push_str(&format!("<upnp:album>{}</upnp:album>", xml_escape(album)));
    }
    if let Some(track_no) = item.track_no {
        out.push_str(&format!(
            "<upnp:originalTrackNumber>{track_no}</upnp:originalTrackNumber>"
        ));
    }
    if let Some(series) = &item.series {
        out.push_str(&format!(
            "<upnp:seriesTitle>{}</upnp:seriesTitle>",
            xml_escape(series)
        ));
    }
    if let Some(episode) = item.episode {
        out.push_str(&format!(
            "<upnp:episodeNumber>{episode}</upnp:episodeNumber>"
        ));
    }
    if item.has_art {
        out.push_str(&format!(
            r#"<upnp:albumArtURI dlna:profileID="JPEG_TN">{base_url}/art/{}</upnp:albumArtURI>"#,
            item.art_file_id.unwrap_or(item.file_id)
        ));
    }

    // Primary rendition first; naive clients that only read the first
    // <res> get the best quality.
    render_res(out, &item.primary_rendition(), base_url);
    for rendition in &item.renditions {
        render_res(out, rendition, base_url);
    }
    out.push_str("</item>");
}

fn render_res(out: &mut String, r: &media_db::Rendition, base_url: &str) {
    let mut attrs = format!(
        r#"protocolInfo="http-get:*:{}:{}" size="{}""#,
        r.mime, DLNA_FEATURES, r.size
    );
    if let Some(ms) = r.duration_ms {
        attrs.push_str(&format!(r#" duration="{}""#, didl_duration(ms)));
    }
    if let (Some(w), Some(h)) = (r.width, r.height) {
        attrs.push_str(&format!(r#" resolution="{w}x{h}""#));
    }
    out.push_str(&format!("<res {attrs}>{base_url}/media/{}</res>", r.file_id));
}

fn render_container(
    out: &mut String,
    id: &str,
    parent: &str,
    title: &str,
    class: &str,
    art_item: Option<i64>,
    base_url: &str,
) {
    out.push_str(&format!(
        r#"<container id="{}" parentID="{}" restricted="1" searchable="1">"#,
        xml_escape(id),
        xml_escape(parent)
    ));
    out.push_str(&format!("<dc:title>{}</dc:title>", xml_escape(title)));
    out.push_str(&format!("<upnp:class>{class}</upnp:class>"));
    if let Some(art) = art_item {
        out.push_str(&format!(
            r#"<upnp:albumArtURI dlna:profileID="JPEG_TN">{base_url}/art/{art}</upnp:albumArtURI>"#
        ));
    }
    out.push_str("</container>");
}

/// Render entries as a DIDL-Lite document.
pub fn render(entries: &[Entry], base_url: &str) -> String {
    let mut out = String::from(
        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:dlna="urn:schemas-dlna-org:metadata-1-0/">"#,
    );
    for entry in entries {
        match entry {
            Entry::Container { id, parent, title, class, art_item } => {
                render_container(&mut out, id, parent, title, class, *art_item, base_url)
            }
            Entry::Item { id, parent, item } => {
                render_item(&mut out, id, parent, item, base_url)
            }
        }
    }
    out.push_str("</DIDL-Lite>");
    out
}
