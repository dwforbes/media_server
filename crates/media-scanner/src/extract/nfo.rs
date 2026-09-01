use std::path::Path;

use anyhow::Result;
use quick_xml::events::{BytesRef, Event};
use quick_xml::Reader;

/// Fields a Kodi-style .nfo sidecar can override. Movie nfos use
/// <movie><title><year><genre>; episode nfos use <episodedetails> with
/// <showtitle><title><season><episode>.
#[derive(Debug, Default)]
pub struct NfoData {
    pub title: Option<String>,
    pub year: Option<i64>,
    pub genres: Vec<String>,
    pub directors: Vec<String>,
    pub rating: Option<f64>,
    pub plot: Option<String>,
    pub imdb_id: Option<String>,
    /// Movie set/collection (Kodi flat <set> form).
    pub set: Option<String>,
    pub show_title: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
}

/// Read the sidecar if it exists. Unparseable or missing files are simply
/// no data — the nfo is an optional enhancement, never a failure.
pub fn read_sidecar(media_path: &Path) -> Option<NfoData> {
    read_file(&media_path.with_extension("nfo"))
}

/// Read an .nfo by its own path (directory-level tvshow.nfo / season.nfo).
pub fn read_file(nfo_path: &Path) -> Option<NfoData> {
    let text = media_db::sidecar::read_text_capped(nfo_path, media_db::sidecar::MAX_TEXT).ok()?;
    match parse(&text) {
        Ok(data) => Some(data),
        Err(err) => {
            tracing::warn!("ignoring malformed nfo {}: {err}", nfo_path.display());
            None
        }
    }
}

/// A `&…;` reference as text: numeric character references and the five
/// predefined entities resolve; a DTD-defined entity is never expanded
/// (this parser reads no DTDs, by design) and stays literal.
fn resolve_ref(r: &BytesRef) -> Result<String> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Ok(c.to_string());
    }
    let name = r.decode()?;
    Ok(match &*name {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        other => format!("&{other};"),
    })
}

/// Text from a sidecar, fit to store: control characters dropped (they
/// are never meant, and one in a title makes the DIDL/SOAP response for
/// the whole container unparseable to strict clients — line breaks and
/// tabs in plots stay), and bounded in length.
fn clean(value: &str) -> String {
    const MAX_CHARS: usize = 8 * 1024;
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .take(MAX_CHARS)
        .collect()
}

fn parse(text: &str) -> Result<NfoData> {
    let mut reader = Reader::from_str(text);
    let mut data = NfoData::default();
    let mut current: Option<String> = None;
    let mut uniqueid_is_imdb = false;
    // The reader hands character and entity references over as events of
    // their own, so an element's text is reassembled until its end tag.
    let mut buf = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if name == "uniqueid" {
                    uniqueid_is_imdb = e
                        .try_get_attribute("type")
                        .ok()
                        .flatten()
                        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok())
                        .is_some_and(|v| v == "imdb");
                }
                current = Some(name);
                buf.clear();
            }
            Event::Text(t) if current.is_some() => buf.push_str(&t.xml10_content()?),
            Event::CData(c) if current.is_some() => buf.push_str(&c.decode()?),
            Event::GeneralRef(r) if current.is_some() => buf.push_str(&resolve_ref(&r)?),
            Event::End(_) => {
                let Some(element) = current.take() else { continue };
                let value = clean(buf.trim());
                buf.clear();
                if value.is_empty() {
                    continue;
                }
                match element.as_str() {
                    "title" => {
                        data.title.get_or_insert(value);
                    }
                    "showtitle" => {
                        data.show_title.get_or_insert(value);
                    }
                    "year" => {
                        data.year.get_or_insert(value.parse().unwrap_or(0));
                    }
                    // <season> in episode nfos, <seasonnumber> in Kodi's
                    // season.nfo — both name the season.
                    "season" | "seasonnumber" => {
                        data.season.get_or_insert(value.parse().unwrap_or(-1));
                    }
                    "episode" => {
                        data.episode.get_or_insert(value.parse().unwrap_or(-1));
                    }
                    "plot" => {
                        data.plot.get_or_insert(value);
                    }
                    // Only a real title id ("tt" + digits) is kept: the
                    // value ends up in links on the web pages.
                    "uniqueid" if uniqueid_is_imdb => {
                        let digits = value.strip_prefix("tt").unwrap_or("");
                        if (7..=9).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
                            data.imdb_id.get_or_insert(value);
                        }
                    }
                    "set" => {
                        data.set.get_or_insert(value);
                    }
                    "genre" => data.genres.push(value),
                    "director" => data.directors.push(value),
                    "rating" => {
                        // "NaN", "inf" and 1e308 all parse; only a real
                        // 0–10 rating is a rating.
                        if let Ok(rating) = value.parse::<f64>() {
                            if rating.is_finite() && (0.0..=10.0).contains(&rating) {
                                data.rating.get_or_insert(rating);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    data.year = data.year.filter(|y| (1801..2200).contains(y));
    data.season = data.season.filter(|s| *s >= 0);
    data.episode = data.episode.filter(|e| *e >= 0);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_tvshow_nfo() {
        let data = parse(
            "<?xml version=\"1.0\"?>\n<!-- generated by media-enrich (TMDB) -->\n<tvshow>\n\
             <title>Fake Show</title>\n<plot>About things.</plot>\n<rating>8.7</rating>\n\
             <uniqueid type=\"imdb\">tt9999999</uniqueid>\n</tvshow>\n",
        )
        .unwrap();
        assert_eq!(data.title.as_deref(), Some("Fake Show"));
        assert_eq!(data.plot.as_deref(), Some("About things."));
        assert_eq!(data.rating, Some(8.7));
        assert_eq!(data.imdb_id.as_deref(), Some("tt9999999"));
    }

    #[test]
    fn reassembles_text_split_by_references_and_cdata() {
        let data = parse(
            "<movie><title>Tom &amp; Jerry &#x2014; &quot;Redux&quot;</title>\n\
             <plot><![CDATA[a < b & c]]> then &lt;more&gt;</plot>\n\
             <uniqueid type=\"imdb\">tt0000001</uniqueid><genre>&unknown;</genre></movie>",
        )
        .unwrap();
        assert_eq!(data.title.as_deref(), Some("Tom & Jerry \u{2014} \"Redux\""));
        assert_eq!(data.plot.as_deref(), Some("a < b & c then <more>"));
        assert_eq!(data.imdb_id.as_deref(), Some("tt0000001"));
        assert_eq!(data.genres, vec!["&unknown;".to_string()]);
    }

    #[test]
    fn parses_season_nfo() {
        let data = parse(
            "<season>\n<showtitle>Fake Show</showtitle>\n<seasonnumber>3</seasonnumber>\n\
             <plot>A season.</plot>\n</season>\n",
        )
        .unwrap();
        assert_eq!(data.show_title.as_deref(), Some("Fake Show"));
        assert_eq!(data.season, Some(3));
        assert_eq!(data.plot.as_deref(), Some("A season."));
    }
}
