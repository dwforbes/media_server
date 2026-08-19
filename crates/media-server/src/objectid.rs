use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;

/// Every node in the ContentDirectory tree has a stable string id encoding
/// what it is; browsing parses the id back into a query. "0" is the UPnP
/// mandated root.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectId {
    Root,
    // Movies
    Movies,
    MoviesAll,
    MoviesRecent,
    MoviesByYear,
    MoviesYear(i64),
    MoviesByDecade,
    MoviesDecade(i64),
    MoviesByGenre,
    MoviesGenre(i64),
    MoviesByDirector,
    MoviesDirector(i64),
    MoviesByRating,
    MoviesRating(usize),
    MoviesFolders,
    // Music
    Music,
    MusicRecent,
    MusicArtists,
    MusicArtist(String),
    MusicAlbums,
    MusicAlbum { artist: String, album: String },
    MusicByGenre,
    MusicGenre(i64),
    MusicFolders,
    // TV
    Tv,
    TvRecent,
    TvSeries(String),
    TvSeason { series: String, season: i64 },
    TvFolders,
    // Folder view: rel_dir is "" at the top of a source root.
    Dir { root_id: i64, rel_dir: String },
    // A playable file.
    Item(i64),
}

fn enc(s: &str) -> String {
    B64.encode(s.as_bytes())
}

fn dec(s: &str) -> Option<String> {
    let bytes = B64.decode(s.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

impl ObjectId {
    pub fn to_id(&self) -> String {
        use ObjectId::*;
        match self {
            Root => "0".into(),
            Movies => "mv".into(),
            MoviesAll => "mv:all".into(),
            MoviesRecent => "mv:recent".into(),
            MusicRecent => "mu:recent".into(),
            TvRecent => "tv:recent".into(),
            MoviesByYear => "mv:year".into(),
            MoviesYear(y) => format!("mv:year:{y}"),
            MoviesByDecade => "mv:decade".into(),
            MoviesDecade(d) => format!("mv:decade:{d}"),
            MoviesByGenre => "mv:genre".into(),
            MoviesGenre(g) => format!("mv:genre:{g}"),
            MoviesByDirector => "mv:director".into(),
            MoviesDirector(d) => format!("mv:director:{d}"),
            MoviesByRating => "mv:rating".into(),
            MoviesRating(bucket) => format!("mv:rating:{bucket}"),
            MoviesFolders => "mv:dir".into(),
            Music => "mu".into(),
            MusicArtists => "mu:artist".into(),
            MusicArtist(a) => format!("mu:artist:{}", enc(a)),
            MusicAlbums => "mu:album".into(),
            MusicAlbum { artist, album } => format!("mu:album:{}:{}", enc(artist), enc(album)),
            MusicByGenre => "mu:genre".into(),
            MusicGenre(g) => format!("mu:genre:{g}"),
            MusicFolders => "mu:dir".into(),
            Tv => "tv".into(),
            TvSeries(s) => format!("tv:ser:{}", enc(s)),
            TvSeason { series, season } => format!("tv:ser:{}:{season}", enc(series)),
            TvFolders => "tv:dir".into(),
            Dir { root_id, rel_dir } if rel_dir.is_empty() => format!("dir:{root_id}"),
            Dir { root_id, rel_dir } => format!("dir:{root_id}:{}", enc(rel_dir)),
            Item(f) => format!("it:{f}"),
        }
    }

    pub fn parse(id: &str) -> Option<ObjectId> {
        use ObjectId::*;
        if id == "0" {
            return Some(Root);
        }
        let parts: Vec<&str> = id.split(':').collect();
        Some(match parts.as_slice() {
            ["mv"] => Movies,
            ["mv", "all"] => MoviesAll,
            ["mv", "recent"] => MoviesRecent,
            ["mu", "recent"] => MusicRecent,
            ["tv", "recent"] => TvRecent,
            ["mv", "year"] => MoviesByYear,
            ["mv", "year", y] => MoviesYear(y.parse().ok()?),
            ["mv", "decade"] => MoviesByDecade,
            ["mv", "decade", d] => MoviesDecade(d.parse().ok()?),
            ["mv", "genre"] => MoviesByGenre,
            ["mv", "genre", g] => MoviesGenre(g.parse().ok()?),
            ["mv", "director"] => MoviesByDirector,
            ["mv", "director", d] => MoviesDirector(d.parse().ok()?),
            ["mv", "rating"] => MoviesByRating,
            ["mv", "rating", b] => MoviesRating(b.parse().ok()?),
            ["mv", "dir"] => MoviesFolders,
            ["mu"] => Music,
            ["mu", "artist"] => MusicArtists,
            ["mu", "artist", a] => MusicArtist(dec(a)?),
            ["mu", "album"] => MusicAlbums,
            ["mu", "album", a, al] => MusicAlbum { artist: dec(a)?, album: dec(al)? },
            ["mu", "genre"] => MusicByGenre,
            ["mu", "genre", g] => MusicGenre(g.parse().ok()?),
            ["mu", "dir"] => MusicFolders,
            ["tv"] => Tv,
            ["tv", "ser", s] => TvSeries(dec(s)?),
            ["tv", "ser", s, n] => TvSeason { series: dec(s)?, season: n.parse().ok()? },
            ["tv", "dir"] => TvFolders,
            ["dir", r] => Dir { root_id: r.parse().ok()?, rel_dir: String::new() },
            ["dir", r, d] => Dir { root_id: r.parse().ok()?, rel_dir: dec(d)? },
            ["it", f] => Item(f.parse().ok()?),
            _ => return None,
        })
    }
}
