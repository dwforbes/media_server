# mediaserver

Two Rust applications sharing one SQLite catalog:

- **media-server** — a UPnP AV MediaServer (ContentDirectory:1) that serves audio/video
  straight from disk over HTTP (byte-range seeking, no transcoding). Its entire catalog
  comes from the database; it never walks the filesystem.
- **media-scanner** — a daemon that watches source folders (FSEvents + periodic full
  reconcile) and maintains the database: the raw file list plus locally-extracted
  attributes (title, year, genre, artist/album, series/season/episode, duration,
  resolution, codecs).

They are deliberately separate binaries. The shared-database complexity is handled by the
`media-db` workspace crate — single source of truth for schema, migrations, and queries —
while the two apps build, deploy, and restart independently.

## Layout

```
crates/media-db        shared library: schema, migrations, models, queries
crates/media-scanner   the watcher/extractor daemon
crates/media-server    the UPnP server
crates/media-enrich    optional: writes TMDB-sourced .nfo sidecars (see below)
media-scanner.toml     example configs (copy & edit)
media-server.toml
```

## Build & run

```sh
cargo build --release

# 1. Edit media-scanner.toml: set your source folders ([[roots]] with kind
#    movies / music / tv). Then populate the catalog:
./target/release/media-scanner --once          # single pass, then exits
./target/release/media-scanner                 # or run as the daemon

# 2. Start the server (defaults: port 8200, auto-detected LAN IP):
./target/release/media-server
```

Both read TOML configs (`--config <path>`, defaults to the file in the working
directory). The database defaults to `~/Library/Application Support/mediaserver/media.db`
on the **internal disk** — keep it off external/network volumes; SQLite WAL's
multi-process locking is only reliable on a local filesystem. The scanner opens the
database read-write and runs migrations; the server opens it read-only.

`ffprobe` (from `brew install ffmpeg`) supplies video duration/resolution/codecs. Without
it, videos are still catalogued from their names — just without technical attributes.

## What clients see

```
Movies    → All Movies / Recently Added / By Year / By Decade / By Genre /
            By Director / By Rating / Folders
Music     → Recently Added / Artists / Albums / Genres / Folders
TV Shows  → Recently Added / Series → Season → Episodes, plus Folders
```

Metadata is local-only: filename parsing (`Heat (1995).mkv`, `Show S01E02 Title.mkv`),
embedded tags (ID3/Vorbis/FLAC/MP4 via lofty), and Kodi-style `.nfo` sidecars
(`<movie><title><year><genre><director>`, `<episodedetails>`), which override
name-parsed values.

**Recently Added** lists the newest catalogued items per type (`recent_count` in
`media-server.toml`, default 25), by the time the file entered the catalog — not its
mtime, and untouched by later re-extraction. TV and music entries are fully qualified
("The Wire S01E03 - The Buys", "Artist - Album - Track") since a bare title means
little out of context.

**Multiple qualities of the same item** (same movie title + year, or the same
episode number) are merged into a single entry carrying one `<res>` per file,
best-first — naive clients that only read the first `<res>` stream the best copy,
capable ones offer the choice. The Folders view always mirrors the actual files.

**Cover art** is served at `/art/{id}` and advertised per item (and per music album
container) via `upnp:albumArtURI`, so capable clients show posters and album covers.
Sources, in order: a `<stem>-poster.jpg/png` sidecar (movies/TV — what media-enrich
downloads), a directory-level `cover.jpg`/`folder.jpg`/`front.jpg` (music), or a
picture embedded in the audio tags. Adding or changing artwork is picked up by the
watcher immediately, or by the next reconcile pass.

### Enriching genres with media-enrich

Libraries without `.nfo` files or embedded tags have no local source for genres. The
`media-enrich` tool fills that gap while keeping the scanner itself fully offline: it
looks movies up on TMDB (from the filename's title/year) and writes standard Kodi
`.nfo` sidecars plus `<stem>-poster.jpg` images, which the scanner then picks up like
any other local metadata. TV roots are enriched too: episodes are grouped by parsed
series, matched via TMDB's TV endpoints (one call per season), and get per-episode
`.nfo` files (canonical show name + real episode titles) and a `poster.jpg` in each
series folder.

```sh
export TMDB_API_KEY=...                        # free key: themoviedb.org/settings/api
./target/release/media-enrich --dry-run        # show the plan, no network/writes
./target/release/media-enrich                  # enrich files missing an .nfo
./target/release/media-enrich --refresh        # also re-fetch previously generated .nfo
```

Movie `.nfo` files also get the **real IMDb rating**: enrichment resolves each match's
IMDb id via TMDB, then joins against IMDb's official non-commercial ratings dataset
(`title.ratings.tsv.gz`, ~7 MB, cached beside the database and refreshed weekly, or per
`--ratings-max-age-days`). No scraping, no extra API keys, license-clean for personal
use. The server's "By Rating" view buckets movies (9+, 8–9, …, Unrated), best first,
with the score prefixed to the title. `--no-ratings` opts out.

**Automatic enrichment**: with an `[enrich]` section in `media-scanner.toml`, the
scanner daemon runs media-enrich itself whenever new media is catalogued — debounced
(one run per burst of files), serialized (one run in flight), rate-limited, and
triggered only by new media files, never by the sidecars enrichment writes (which
would loop). New files therefore appear in clients within seconds with parsed
metadata, then upgrade themselves with genres, directors, ratings, and artwork about
a minute later. Export `TMDB_API_KEY` in the daemon's environment. With `--once`,
enrichment runs synchronously followed by a second reconcile.

It is safe to re-run any time (e.g. after adding movies): files that already have an
`.nfo` are skipped by default. `--refresh` re-fetches only sidecars carrying the
tool's "generated by media-enrich" marker; hand-written `.nfo` files are never touched
unless you add `--force`. It reads the scanner's config (`--config`) to find the movie
roots. Sidecar changes are noticed by the running scanner immediately (watch events),
or at the next reconcile pass otherwise — the catalog tracks each sidecar's mtime.

Test with VLC (View → Playlist → Universal Plug'n'Play) or BubbleUPnP. The server
announces itself over SSDP and refreshes announcements every 5 minutes; on shutdown it
sends `ssdp:byebye`.

## How the pieces cooperate

- SQLite runs in WAL mode: the scanner is the only writer, the server only reads.
- Files enter the catalog as `pending` and only become `ready` in the same transaction
  that writes their attributes — the server never surfaces half-scanned files. Files
  still being copied are detected by size-stability checks and picked up on a later
  event or reconcile pass.
- The server polls `PRAGMA data_version` (2 s); any scanner commit bumps the
  ContentDirectory `SystemUpdateID`, so browsing clients refresh on their next poll.
- Every node in the UPnP tree has a stable, parseable object id (`mv:year:1995`,
  `mu:album:<b64>:<b64>`, `dir:<root>:<b64 path>`, `it:<file id>`) that maps directly to
  a query — no in-memory tree to invalidate.

### media-title: fixing scene-titled containers

Some releases embed the scene filename as the container title, which players like
VLC prefer over the server-supplied name once playback starts. `media-title`
inspects and neutralizes that in place — a header-only patch, no re-muxing: MP4
title atoms are renamed to `free` (the standard padding atom); MKV/WebM Title
elements are rewritten as Void. Format is detected by magic bytes, not extension.

```sh
media-title file.mp4 episode.mkv     # show embedded titles
media-title --strip *.mp4 *.mkv      # neutralize them (titleless files untouched)

# audit a whole library:
find /mnt/media -type f \( -iname "*.mp4" -o -iname "*.mkv" \) -print0 \
  | xargs -0 media-title | grep ': title '
```

The running scanner re-extracts touched files automatically (a harmless no-op).

## Not implemented (v1)

- ContentDirectory `Search` (returns UPnP error 602) and GENA eventing (clients poll).
- DLNA.ORG_PN media profiles — protocolInfo is the permissive generic form, which VLC,
  BubbleUPnP, and most TVs accept.
- Transcoding, playlists.
