/// Schema version stored in SQLite's `user_version` pragma. Bump when adding
/// a migration below; the server refuses to open a mismatched database.
pub const SCHEMA_VERSION: i32 = 15;

/// Migrations indexed by target version: MIGRATIONS[0] takes 0 -> 1, etc.
pub const MIGRATIONS: &[&str] = &[
    // 0 -> 1
    r#"
    CREATE TABLE roots (
        id      INTEGER PRIMARY KEY,
        path    TEXT NOT NULL UNIQUE,
        kind    TEXT NOT NULL CHECK (kind IN ('movies','music','tv'))
    );

    CREATE TABLE files (
        id          INTEGER PRIMARY KEY,
        root_id     INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
        rel_path    TEXT NOT NULL,
        size        INTEGER NOT NULL,
        mtime       INTEGER NOT NULL,
        kind        TEXT NOT NULL CHECK (kind IN ('movies','music','tv')),
        mime        TEXT NOT NULL,
        container   TEXT,
        duration_ms INTEGER,
        width       INTEGER,
        height      INTEGER,
        video_codec TEXT,
        audio_codec TEXT,
        status      TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending','ready','error')),
        updated_at  INTEGER NOT NULL,
        UNIQUE (root_id, rel_path)
    );
    CREATE INDEX idx_files_kind_status ON files(kind, status);

    CREATE TABLE movies (
        file_id    INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        title      TEXT NOT NULL,
        sort_title TEXT NOT NULL,
        year       INTEGER
    );
    CREATE INDEX idx_movies_year ON movies(year);
    CREATE INDEX idx_movies_sort ON movies(sort_title);

    CREATE TABLE genres (
        id   INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );

    CREATE TABLE movie_genres (
        file_id  INTEGER NOT NULL REFERENCES movies(file_id) ON DELETE CASCADE,
        genre_id INTEGER NOT NULL REFERENCES genres(id),
        PRIMARY KEY (file_id, genre_id)
    );
    CREATE INDEX idx_movie_genres_genre ON movie_genres(genre_id);

    CREATE TABLE tv_episodes (
        file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        series  TEXT NOT NULL,
        season  INTEGER NOT NULL,
        episode INTEGER NOT NULL,
        title   TEXT NOT NULL
    );
    CREATE INDEX idx_tv_series ON tv_episodes(series, season, episode);

    CREATE TABLE music_tracks (
        file_id      INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        title        TEXT NOT NULL,
        artist       TEXT,
        album_artist TEXT,
        album        TEXT,
        track_no     INTEGER,
        disc_no      INTEGER,
        year         INTEGER
    );
    CREATE INDEX idx_music_album ON music_tracks(album_artist, album, disc_no, track_no);
    CREATE INDEX idx_music_artist ON music_tracks(artist);

    CREATE TABLE track_genres (
        file_id  INTEGER NOT NULL REFERENCES music_tracks(file_id) ON DELETE CASCADE,
        genre_id INTEGER NOT NULL REFERENCES genres(id),
        PRIMARY KEY (file_id, genre_id)
    );
    CREATE INDEX idx_track_genres_genre ON track_genres(genre_id);
    "#,
    // 1 -> 2: remember the .nfo sidecar mtime seen at extraction time, so
    // reconcile can detect sidecars that changed while the daemon was down.
    "ALTER TABLE files ADD COLUMN nfo_mtime INTEGER;",
    // 2 -> 3: movie directors (many-to-many, same shape as genres). The
    // nfo_mtime poke forces re-extraction of movies that already have a
    // sidecar, so existing <director> data is picked up.
    r#"
    CREATE TABLE directors (
        id   INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );
    CREATE TABLE movie_directors (
        file_id     INTEGER NOT NULL REFERENCES movies(file_id) ON DELETE CASCADE,
        director_id INTEGER NOT NULL REFERENCES directors(id),
        PRIMARY KEY (file_id, director_id)
    );
    CREATE INDEX idx_movie_directors_director ON movie_directors(director_id);
    UPDATE files SET nfo_mtime = -1 WHERE kind = 'movies' AND nfo_mtime IS NOT NULL;
    "#,
    // 3 -> 4: artwork. Either a root-relative path to a sidecar image
    // (poster/cover file) or the literal 'embedded' for pictures inside
    // audio tags. Reconcile re-extracts files whose sidecar art changed.
    "ALTER TABLE files ADD COLUMN art TEXT;",
    // 4 -> 5: IMDb rating (0-10), sourced from .nfo sidecars.
    "ALTER TABLE movies ADD COLUMN rating REAL;",
    // 5 -> 6: when a file first entered the catalog (never rewritten, unlike
    // updated_at), for the Recently Added views. Existing rows inherit their
    // file mtime as the best available approximation.
    r#"
    ALTER TABLE files ADD COLUMN added_at INTEGER NOT NULL DEFAULT 0;
    UPDATE files SET added_at = mtime;
    CREATE INDEX idx_files_added ON files(kind, status, added_at DESC);
    "#,
    // 6 -> 7: plot/description from .nfo sidecars. Movie sidecars have
    // carried <plot> since the first enrichment, so the nfo_mtime poke
    // re-extracts them into the new column with no network involved.
    r#"
    ALTER TABLE movies ADD COLUMN plot TEXT;
    ALTER TABLE tv_episodes ADD COLUMN plot TEXT;
    UPDATE files SET nfo_mtime = -1 WHERE kind = 'movies' AND nfo_mtime IS NOT NULL;
    "#,
    // 7 -> 8: IMDb id (tt0083658) from .nfo uniqueid, for external links.
    "ALTER TABLE movies ADD COLUMN imdb_id TEXT;",
    // 8 -> 9: per-episode IMDb identity and rating (the ratings dataset
    // covers episode tconsts too).
    r#"
    ALTER TABLE tv_episodes ADD COLUMN rating REAL;
    ALTER TABLE tv_episodes ADD COLUMN imdb_id TEXT;
    "#,
    // 9 -> 10: TMDB collection ("Harry Potter Collection") from .nfo <set>,
    // for the Franchises view.
    "ALTER TABLE movies ADD COLUMN collection TEXT;",
    // 10 -> 11: audio stream details — codec label ("AAC LC", "FLAC"),
    // bitrate (kbps), sample rate (Hz), bit depth, channel count. Music
    // files go back to pending so the next reconcile re-reads their
    // properties; video files pick the fields up whenever they are next
    // extracted.
    "ALTER TABLE files ADD COLUMN audio_profile TEXT;
     ALTER TABLE files ADD COLUMN audio_bitrate INTEGER;
     ALTER TABLE files ADD COLUMN audio_sample_rate INTEGER;
     ALTER TABLE files ADD COLUMN audio_bit_depth INTEGER;
     ALTER TABLE files ADD COLUMN audio_channels INTEGER;
     UPDATE files SET status = 'pending' WHERE kind = 'music' AND status = 'ready';",
    // 11 -> 12: series- and season-level metadata, ingested from
    // directory-level tvshow.nfo / season.nfo sidecars. Series and seasons
    // stay virtual groupings of tv_episodes (matched by name, NOCASE like
    // every series query); these tables only decorate them, so an orphan
    // row for a removed series is harmless and never shown.
    r#"
    CREATE TABLE tv_series (
        name    TEXT PRIMARY KEY COLLATE NOCASE,
        plot    TEXT,
        rating  REAL,
        imdb_id TEXT
    );
    CREATE TABLE tv_seasons (
        series TEXT NOT NULL COLLATE NOCASE,
        season INTEGER NOT NULL,
        plot   TEXT,
        PRIMARY KEY (series, season)
    );
    "#,
    // 12 -> 13: skippable segments (intro / credits / recap / commercial
    // breaks) per video file, ingested from named chapter markers and .edl
    // sidecars; source says which ('chapters'/'edl') so later detectors
    // can defer to deliberate sidecars. edl_mtime mirrors nfo_mtime for
    // sidecar staleness, and the -1 poke makes every TV file look
    // edl-stale so the next reconcile re-probes it for chapters (never
    // seen before this version) without taking it out of 'ready' — the
    // same trick the nfo_mtime migrations use. Movies gain nothing from
    // skip buttons, so they wait for their next natural re-extraction.
    r#"
    CREATE TABLE segments (
        file_id  INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        start_ms INTEGER NOT NULL,
        end_ms   INTEGER NOT NULL,
        kind     TEXT NOT NULL CHECK (kind IN ('intro','credits','recap','commercial')),
        source   TEXT NOT NULL,
        PRIMARY KEY (file_id, start_ms)
    );
    ALTER TABLE files ADD COLUMN edl_mtime INTEGER;
    UPDATE files SET edl_mtime = -1 WHERE kind = 'tv';
    "#,
    // 13 -> 14: cached chromaprint fingerprints for the automatic
    // intro/credits detector — the head and tail audio windows of each TV
    // episode, keyed to size+mtime (plus the analyzer version) so a
    // changed file re-fingerprints. A row also marks its file as
    // analyzed, even with empty prints (undecodable audio), which is what
    // stops re-analysis loops; a season is stale while any ready episode
    // lacks a current row.
    r#"
    CREATE TABLE segment_prints (
        file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
        size    INTEGER NOT NULL,
        mtime   INTEGER NOT NULL,
        ver     INTEGER NOT NULL,
        head    BLOB NOT NULL,
        tail    BLOB NOT NULL
    );
    "#,
    // 14 -> 15: chapter ingestion gained a sanity cap (real files carry
    // garbage chapter tables — a recognizably named chapter spanning the
    // whole episode became a skip-to-the-end "intro"). Re-derive every
    // file whose segments came from chapters via the edl-stale poke, and
    // drop those files' fingerprints so their seasons re-analyze and the
    // audio detector fills in wherever the filtered chapters now leave
    // nothing.
    r#"
    DELETE FROM segment_prints WHERE file_id IN
        (SELECT DISTINCT file_id FROM segments WHERE source = 'chapters');
    UPDATE files SET edl_mtime = -1 WHERE id IN
        (SELECT DISTINCT file_id FROM segments WHERE source = 'chapters');
    "#,
];
