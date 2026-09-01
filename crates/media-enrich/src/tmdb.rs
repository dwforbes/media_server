use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const BASE: &str = "https://api.themoviedb.org/3";

#[derive(Clone)]
pub struct MovieInfo {
    pub tmdb_id: i64,
    pub title: String,
    pub year: Option<i64>,
    pub plot: Option<String>,
    pub genres: Vec<String>,
    pub directors: Vec<String>,
    pub poster_path: Option<String>,
    pub imdb_id: Option<String>,
    /// TMDB collection ("Harry Potter Collection") — franchise membership.
    pub collection: Option<String>,
}

pub struct SeriesInfo {
    pub tmdb_id: i64,
    pub name: String,
    pub poster_path: Option<String>,
    pub plot: Option<String>,
    pub imdb_id: Option<String>,
}

/// One season of a series: its own overview plus every episode's
/// (title, overview) keyed by episode number.
#[derive(Default)]
pub struct SeasonInfo {
    pub overview: Option<String>,
    pub episodes: HashMap<i64, (String, String)>,
}

pub struct Tmdb {
    client: reqwest::blocking::Client,
    key: String,
    /// A TMDB "API Read Access Token" (v4, a JWT) goes in the Authorization
    /// header; a classic v3 key goes in the api_key query parameter.
    bearer: bool,
}

impl Tmdb {
    pub fn new(key: String) -> Self {
        let bearer = key.starts_with("eyJ");
        Tmdb {
            client: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("building HTTP client"),
            key,
            bearer,
        }
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        match self.get_opt(path, params)? {
            Some(json) => Ok(json),
            None => bail!("TMDB {path} returned 404 Not Found"),
        }
    }

    /// Like get, but a 404 is Ok(None) — the id-addressed endpoints use
    /// it to tell "no such movie" apart from a real failure.
    fn get_opt(&self, path: &str, params: &[(&str, &str)]) -> Result<Option<Value>> {
        let mut request = self.client.get(format!("{BASE}{path}")).query(params);
        if self.bearer {
            request = request.bearer_auth(&self.key);
        } else {
            request = request.query(&[("api_key", self.key.as_str())]);
        }
        // without_url: reqwest's error text carries the request URL, and
        // with a v3 key that is the key itself, headed for the journal.
        let response = request
            .send()
            .map_err(reqwest::Error::without_url)
            .context("TMDB request failed")?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("TMDB rejected the API key (401) — check TMDB_API_KEY");
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("TMDB {path} returned {status}");
        }
        response
            .json()
            .map(Some)
            .map_err(reqwest::Error::without_url)
            .context("parsing TMDB response")
    }

    /// Best match for (title, year). Search precision matters: TMDB's
    /// "year" parameter is loose (any regional release year), which can
    /// rank a sibling film first — Deathly Hallows Part 1, with 2011 home
    /// releases, outranked Part 2 for year=2011. So: strict
    /// primary_release_year first, loose year as fallback (filename years
    /// are sometimes regional). A filename year is never dropped: a
    /// year-less search for a remake's title returns the original (The
    /// Mummy 2026 -> The Mummy 1999), and a wrong identity is worse than
    /// none — an unmatched file is listed at the end and can be pinned.
    pub fn find_movie(&mut self, title: &str, year: Option<i64>) -> Result<Option<MovieInfo>> {
        let result = match year {
            Some(y) => match self.search(title, Some(("primary_release_year", y)))? {
                Some(hit) => Some(hit),
                None => self.search(title, Some(("year", y)))?,
            },
            None => self.search(title, None)?,
        };
        let Some(hit) = result else { return Ok(None) };
        let tmdb_id = hit.get("id").and_then(Value::as_i64).context("result missing id")?;
        self.movie_details(tmdb_id, title)
    }

    /// A movie by its TMDB id — for sidecars that pin the identity.
    /// None when TMDB has no such id (a typo in the pin).
    pub fn movie_by_id(&self, tmdb_id: i64) -> Result<Option<MovieInfo>> {
        self.movie_details(tmdb_id, "")
    }

    /// One details call (credits and external ids appended) supplies
    /// genres, directors, imdb id, and collection membership together.
    fn movie_details(&self, tmdb_id: i64, fallback_title: &str) -> Result<Option<MovieInfo>> {
        let title = fallback_title;
        let Some(details) = self.get_opt(
            &format!("/movie/{tmdb_id}"),
            &[("append_to_response", "credits,external_ids")],
        )?
        else {
            return Ok(None);
        };
        let genres = details
            .get("genres")
            .and_then(|g| g.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|g| g.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let directors = details
            .pointer("/credits/crew")
            .and_then(|c| c.as_array())
            .map(|crew| {
                crew.iter()
                    .filter(|p| p.get("job").and_then(Value::as_str) == Some("Director"))
                    .filter_map(|p| p.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let imdb_id = details
            .get("imdb_id")
            .or_else(|| details.pointer("/external_ids/imdb_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let collection = details
            .pointer("/belongs_to_collection/name")
            .and_then(Value::as_str)
            .map(str::to_string);

        Ok(Some(MovieInfo {
            tmdb_id,
            title: details
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(title)
                .to_string(),
            year: details
                .get("release_date")
                .and_then(Value::as_str)
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse().ok()),
            plot: details
                .get("overview")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            genres,
            directors,
            poster_path: details
                .get("poster_path")
                .and_then(Value::as_str)
                .map(str::to_string),
            imdb_id,
            collection,
        }))
    }


    /// Best TV-series match by name. The search hit settles the identity;
    /// one follow-up details call supplies the overview and IMDb id (the
    /// search response has no external ids).
    pub fn find_series(&self, name: &str) -> Result<Option<SeriesInfo>> {
        let json = self.get("/search/tv", &[("query", name), ("include_adult", "false")])?;
        let Some(hit) = json
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
        else {
            return Ok(None);
        };
        let tmdb_id = hit.get("id").and_then(Value::as_i64).context("result missing id")?;
        let details = self
            .get_opt(&format!("/tv/{tmdb_id}"), &[("append_to_response", "external_ids")])
            .unwrap_or(None);
        let source = details.as_ref().unwrap_or(hit);
        Ok(Some(SeriesInfo {
            tmdb_id,
            name: source
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string(),
            poster_path: source
                .get("poster_path")
                .and_then(Value::as_str)
                .map(str::to_string),
            plot: source
                .get("overview")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            imdb_id: details
                .as_ref()
                .and_then(|d| d.pointer("/external_ids/imdb_id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }))
    }

    /// One season of a series — its overview and every episode's
    /// (title, overview). One API call covers the whole season.
    pub fn season_details(&self, series_id: i64, season: i64) -> Result<SeasonInfo> {
        let json = match self.get(&format!("/tv/{series_id}/season/{season}"), &[]) {
            Ok(j) => j,
            // A season TMDB doesn't know just yields no data.
            Err(_) => return Ok(SeasonInfo::default()),
        };
        let mut out = SeasonInfo {
            overview: json
                .get("overview")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            episodes: HashMap::new(),
        };
        if let Some(episodes) = json.get("episodes").and_then(|e| e.as_array()) {
            for episode in episodes {
                if let (Some(number), Some(name)) = (
                    episode.get("episode_number").and_then(Value::as_i64),
                    episode.get("name").and_then(Value::as_str),
                ) {
                    let overview = episode
                        .get("overview")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out.episodes.insert(number, (name.to_string(), overview));
                }
            }
        }
        Ok(out)
    }

    /// The IMDb id for one episode.
    pub fn episode_imdb_id(&self, series_id: i64, season: i64, episode: i64) -> Result<Option<String>> {
        let json = match self.get(
            &format!("/tv/{series_id}/season/{season}/episode/{episode}/external_ids"),
            &[],
        ) {
            Ok(j) => j,
            // Unknown episode: no id rather than a failed run.
            Err(_) => return Ok(None),
        };
        Ok(json
            .get("imdb_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string))
    }

    /// Download a poster (w500 rendition, ~500x750) to `dest`.
    pub fn download_poster(&self, poster_path: &str, dest: &std::path::Path) -> Result<()> {
        let url = format!("https://image.tmdb.org/t/p/w500{poster_path}");
        // A w500 poster is ~50 KB; read no more than this from the CDN.
        const MAX_POSTER_BYTES: u64 = 16 * 1024 * 1024;
        use std::io::Read;
        let mut bytes = Vec::new();
        self.client
            .get(url)
            .send()
            .context("downloading poster")?
            .error_for_status()?
            .take(MAX_POSTER_BYTES)
            .read_to_end(&mut bytes)
            .context("reading poster")?;
        media_db::sidecar::write_atomic(dest, &bytes)
            .with_context(|| format!("writing {}", dest.display()))?;
        Ok(())
    }

    fn search(&self, title: &str, year: Option<(&str, i64)>) -> Result<Option<Value>> {
        let year_string;
        let mut params = vec![("query", title), ("include_adult", "false")];
        if let Some((param, y)) = year {
            year_string = y.to_string();
            params.push((param, &year_string));
        }
        let json = self.get("/search/movie", &params)?;
        Ok(json
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .cloned())
    }
}
