use std::path::Path;

use anyhow::Result;
use quick_xml::events::Event;
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
    pub show_title: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
}

/// Read the sidecar if it exists. Unparseable or missing files are simply
/// no data — the nfo is an optional enhancement, never a failure.
pub fn read_sidecar(media_path: &Path) -> Option<NfoData> {
    let nfo_path = media_path.with_extension("nfo");
    let text = std::fs::read_to_string(&nfo_path).ok()?;
    match parse(&text) {
        Ok(data) => Some(data),
        Err(err) => {
            tracing::warn!("ignoring malformed nfo {}: {err}", nfo_path.display());
            None
        }
    }
}

fn parse(text: &str) -> Result<NfoData> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut data = NfoData::default();
    let mut current: Option<String> = None;

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                current = Some(String::from_utf8_lossy(e.local_name().as_ref()).to_string());
            }
            Event::End(_) => current = None,
            Event::Text(t) => {
                let Some(element) = current.as_deref() else { continue };
                let value = t.unescape()?.trim().to_string();
                if value.is_empty() {
                    continue;
                }
                match element {
                    "title" => {
                        data.title.get_or_insert(value);
                    }
                    "showtitle" => {
                        data.show_title.get_or_insert(value);
                    }
                    "year" => {
                        data.year.get_or_insert(value.parse().unwrap_or(0));
                    }
                    "season" => {
                        data.season.get_or_insert(value.parse().unwrap_or(-1));
                    }
                    "episode" => {
                        data.episode.get_or_insert(value.parse().unwrap_or(-1));
                    }
                    "genre" => data.genres.push(value),
                    "director" => data.directors.push(value),
                    "rating" => {
                        if let Ok(rating) = value.parse::<f64>() {
                            data.rating.get_or_insert(rating);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    data.year = data.year.filter(|y| *y > 1800);
    data.season = data.season.filter(|s| *s >= 0);
    data.episode = data.episode.filter(|e| *e >= 0);
    Ok(data)
}
