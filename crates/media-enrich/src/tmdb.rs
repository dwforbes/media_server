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
}

pub struct SeriesInfo {
    pub tmdb_id: i64,
    pub name: String,
    pub poster_path: Option<String>,
}

pub struct Tmdb {
    client: reqwest::blocking::Client,
    key: String,
    /// A TMDB "API Read Access Token" (v4, a JWT) goes in the Authorization
    /// header; a classic v3 key goes in the api_key query parameter.
    bearer: bool,
    genre_names: Option<HashMap<i64, String>>,
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
            genre_names: None,
        }
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        let mut request = self.client.get(format!("{BASE}{path}")).query(params);
        if self.bearer {
            request = request.bearer_auth(&self.key);
        } else {
            request = request.query(&[("api_key", self.key.as_str())]);
        }
        let response = request.send().context("TMDB request failed")?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("TMDB rejected the API key (401) — check TMDB_API_KEY");
        }
        if !status.is_success() {
            bail!("TMDB {path} returned {status}");
        }
        response.json().context("parsing TMDB response")
    }

    fn genre_name(&mut self, id: i64) -> Result<Option<String>> {
        if self.genre_names.is_none() {
            let json = self.get("/genre/movie/list", &[])?;
            let mut map = HashMap::new();
            if let Some(genres) = json.get("genres").and_then(|g| g.as_array()) {
                for genre in genres {
                    if let (Some(gid), Some(name)) =
                        (genre.get("id").and_then(Value::as_i64), genre.get("name").and_then(Value::as_str))
                    {
                        map.insert(gid, name.to_string());
                    }
                }
            }
            self.genre_names = Some(map);
        }
        Ok(self.genre_names.as_ref().unwrap().get(&id).cloned())
    }

    /// Best match for (title, year); retries without the year if nothing hits.
    pub fn find_movie(&mut self, title: &str, year: Option<i64>) -> Result<Option<MovieInfo>> {
        let mut result = self.search(title, year)?;
        if result.is_none() && year.is_some() {
            result = self.search(title, None)?;
        }
        let Some(hit) = result else { return Ok(None) };

        let tmdb_id = hit.get("id").and_then(Value::as_i64).context("result missing id")?;
        let mut genres = Vec::new();
        for gid in hit
            .get("genre_ids")
            .and_then(|g| g.as_array())
            .map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
            .unwrap_or_default()
        {
            if let Some(name) = self.genre_name(gid)? {
                genres.push(name);
            }
        }

        let credits = self.get(&format!("/movie/{tmdb_id}/credits"), &[])?;
        let directors = credits
            .get("crew")
            .and_then(|c| c.as_array())
            .map(|crew| {
                crew.iter()
                    .filter(|p| p.get("job").and_then(Value::as_str) == Some("Director"))
                    .filter_map(|p| p.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Some(MovieInfo {
            tmdb_id,
            title: hit
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(title)
                .to_string(),
            year: hit
                .get("release_date")
                .and_then(Value::as_str)
                .and_then(|d| d.get(..4))
                .and_then(|y| y.parse().ok()),
            plot: hit
                .get("overview")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            genres,
            directors,
            poster_path: hit
                .get("poster_path")
                .and_then(Value::as_str)
                .map(str::to_string),
        }))
    }

    /// The IMDb id ("tt0083658") for a TMDB movie, used to join against the
    /// IMDb ratings dataset.
    pub fn imdb_id(&self, movie_id: i64) -> Result<Option<String>> {
        let json = self.get(&format!("/movie/{movie_id}/external_ids"), &[])?;
        Ok(json
            .get("imdb_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string))
    }

    /// Best TV-series match by name.
    pub fn find_series(&self, name: &str) -> Result<Option<SeriesInfo>> {
        let json = self.get("/search/tv", &[("query", name), ("include_adult", "false")])?;
        let Some(hit) = json
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
        else {
            return Ok(None);
        };
        Ok(Some(SeriesInfo {
            tmdb_id: hit.get("id").and_then(Value::as_i64).context("result missing id")?,
            name: hit
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(name)
                .to_string(),
            poster_path: hit
                .get("poster_path")
                .and_then(Value::as_str)
                .map(str::to_string),
        }))
    }

    /// Episode titles for one season, keyed by episode number. One API call
    /// covers the whole season.
    pub fn season_episode_titles(
        &self,
        series_id: i64,
        season: i64,
    ) -> Result<HashMap<i64, String>> {
        let json = match self.get(&format!("/tv/{series_id}/season/{season}"), &[]) {
            Ok(j) => j,
            // A season TMDB doesn't know just yields no titles.
            Err(_) => return Ok(HashMap::new()),
        };
        let mut out = HashMap::new();
        if let Some(episodes) = json.get("episodes").and_then(|e| e.as_array()) {
            for episode in episodes {
                if let (Some(number), Some(name)) = (
                    episode.get("episode_number").and_then(Value::as_i64),
                    episode.get("name").and_then(Value::as_str),
                ) {
                    out.insert(number, name.to_string());
                }
            }
        }
        Ok(out)
    }

    /// Download a poster (w500 rendition, ~500x750) to `dest`.
    pub fn download_poster(&self, poster_path: &str, dest: &std::path::Path) -> Result<()> {
        let url = format!("https://image.tmdb.org/t/p/w500{poster_path}");
        let bytes = self
            .client
            .get(url)
            .send()
            .context("downloading poster")?
            .error_for_status()?
            .bytes()?;
        std::fs::write(dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
        Ok(())
    }

    fn search(&self, title: &str, year: Option<i64>) -> Result<Option<Value>> {
        let year_string;
        let mut params = vec![("query", title), ("include_adult", "false")];
        if let Some(y) = year {
            year_string = y.to_string();
            params.push(("year", &year_string));
        }
        let json = self.get("/search/movie", &params)?;
        Ok(json
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|r| r.first())
            .cloned())
    }
}
