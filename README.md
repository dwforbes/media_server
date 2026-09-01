# mediaserver

Two Rust applications sharing one SQLite catalog:

- **media-server** — a UPnP AV MediaServer (ContentDirectory:1) that serves audio/video
  straight from disk over HTTP (byte-range seeking, no transcoding). Its entire catalog
  comes from the database; it never walks the filesystem.
- **media-scanner** — a daemon that watches source folders (FSEvents + periodic full
  reconcile) and maintains the database: the raw file list plus locally-extracted
  attributes (title, year, genre, artist/album, series/season/episode, duration,
  resolution, codecs), artwork, and skippable intro/credits segments (see
  "Skip intro / skip credits" below).

They are deliberately separate binaries. The shared-database complexity is handled by the
`media-db` workspace crate — single source of truth for schema, migrations, and queries —
while the two apps build, deploy, and restart independently.

## Layout

```
crates/media-db        shared library: schema, migrations, models, queries
crates/media-scanner   the watcher/extractor daemon
crates/media-server    the UPnP server
crates/media-enrich    optional: TMDB/IMDb enrichment (.nfo sidecars, posters, ...)
crates/media-title     inspect/neutralize embedded container titles (see below)
crates/media-announcer SSDP relay beacon for other network segments (see below)
media-scanner.example.toml   example configs — copy to media-scanner.toml /
media-server.example.toml    media-server.toml (gitignored) and edit
deploy/                systemd units for scanner, server, and announcer
```

## Build & run

```sh
cargo build --release

# 1. Copy the example configs and set your source folders ([[roots]] with
#    kind movies / music / tv). The copies are gitignored — pulls never
#    touch them.
cp media-scanner.example.toml media-scanner.toml
cp media-server.example.toml media-server.toml

# 2. Populate the catalog:
./target/release/media-scanner --once          # single pass, then exits
./target/release/media-scanner                 # or run as the daemon

# 3. Start the server (defaults: port 8200, auto-detected LAN IP):
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
Movies    → All Movies / Recently Added / 4K / By Year / By Decade / By Genre /
            By Director / By Franchise / By Rating / Folders
Music     → Recently Added / Artists / Albums / Genres / Folders
TV Shows  → Recently Added / 4K / Series → Season → Episodes, plus Folders
```

Metadata is local-only: filename parsing (`Heat (1995).mkv`, `Show S01E02 Title.mkv`),
embedded tags (ID3/Vorbis/FLAC/MP4 via lofty), and Kodi-style `.nfo` sidecars
(`<movie><title><year><genre><director>`, `<episodedetails>`), which override
name-parsed values.

**Search** is available from every browse page and scoped to the container you are
in *and everything below it* — search within a genre, a series, a decade, or from the
top for the whole library. Terms are matched case-insensitively (all terms must
match) against title, series, artist, album, genre, director, and year, and results
can be taken as a playlist. The same engine backs UPnP **ContentDirectory Search**,
so capable clients (BubbleUPnP, many TVs) can search the server directly:
`SearchCriteria` clauses with `contains` become terms, `upnp:class` constraints
filter audio vs video, `*` matches everything, and paging is honoured.

**Recently Added** lists the newest catalogued items per type (`recent_count` in
`media-server.toml`, default 25), by the time the file entered the catalog — not its
mtime, and untouched by later re-extraction. TV and music entries are fully qualified
("The Wire S01E03 - The Buys", "Artist - Album - Track") since a bare title means
little out of context.

**Directory-level music overrides**: tagless audio (courseware, audiobooks,
rips) falls back to path guessing, which can shred one collection into many junk
"artists". Drop a `music.toml` anywhere in a music root:

```toml
artist = "Headspace"
# album_artist = "..."   # optional
# album = "..."          # optional — omit to keep per-folder albums
# genre = "Meditation"   # optional
# track_number_prefix = false   # leading digits are part of the title
#                               # ("30 Minutes"), not track numbers
```

Set fields apply to **every track beneath that directory**, overriding tags and
path fallback; absent fields resolve as usual. Files **inherit field-wise**: each
field takes the nearest ancestor that sets it, so a top-level music.toml can declare
the artist and parsing rules once while deeper files override only the album. Edits
are picked up live by the watcher, or at the next reconcile.

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

**Identity is by TMDB id once known.** A sidecar carrying
`<uniqueid type="tmdb">ID</uniqueid>` (every generated one does) is refreshed by that
id — never re-searched — so a match cannot drift between runs. A filename's year is
never dropped from the search either: `The.Mummy.2026.mp4` with no 2026 hit is reported
as unmatched rather than becoming *The Mummy* (1999).

**Correcting a misidentified movie**: find the right entry on themoviedb.org (the id is
in its URL, e.g. `…/movie/1304313-lee-cronin-s-the-mummy`), delete the wrong
`<stem>-poster.jpg`, and save a one-line pin as `<stem>.nfo`:

```xml
<movie><uniqueid type="tmdb">1304313</uniqueid></movie>
```

The next run (automatic, or `media-enrich`) fetches that film by id and replaces the
pin with a full sidecar and poster. A hand-written `.nfo` that has a `<title>` is not
a pin: it is yours and stays as written (its tmdb id is still honoured under
`--refresh --force`).

Test with VLC (View → Playlist → Universal Plug'n'Play) or BubbleUPnP. The server
announces itself over SSDP (every `ssdp_alive_secs`, default 120s, each announcement
sent twice — multicast is lossy, wifi especially) and sends `ssdp:byebye` on shutdown.

### When discovery misbehaves

UPnP's weakness is that multicast discovery is the only standard entrance, and
routers/APs mistreat multicast in creative ways. Two escape hatches:

- **Unicast announcements**: list stubborn devices in `ssdp_unicast_clients` and every
  announcement is also delivered straight to them — no multicast involved. Works for
  clients that are dropping packets; a device whose SSDP stack is fully wedged (some
  Apple TVs until rebooted) needs the second hatch.
- **A relay beacon on another segment**: run `media-announcer` on an always-on box
  in that network — see "Remote announcers" below.
- **Playlists, no discovery at all**: `/playlist.m3u` (or `/playlist/movies.m3u`,
  `/playlist/tv.m3u`, `/playlist/music.m3u`) exposes the whole catalog with proper
  display titles to anything that can open a URL — VLC's "Open Network Stream", a
  browser, a car head unit. Same streaming endpoints as UPnP, zero SSDP. Every
  container of the virtual tree — a genre, a decade, a rating bucket, a series or
  season, an artist or album — has a playlist too (`/playlist/id/<objectid>.m3u`,
  recursive with per-file dedup, so a series playlist spans its seasons), and a
  search has one at `/playlist/search?mq=<terms>`; the web pages no longer link
  these, since the in-browser player covers playback. The full virtual tree is
  browsable as HTML at `http://<server>:8200/` (also `/browse`). Every container
  link shows how many playable items live beneath it, and listing rows carry a 4K
  chip and the IMDb rating as a colour-coded box. Every item links to a **detail
  page** (`/item/<id>`) showing the poster, plot, IMDb rating, genres, director,
  duration, resolution, codecs, container, file size, and when it was added.
  Series and season pages are decorated too: the poster (promoted from an
  episode's artwork), the description from a Kodi-style `tvshow.nfo` /
  `season.nfo` (series pages add the IMDb rating and a link to the IMDb entry),
  and — on a series page — a season × episode **ratings grid**, every cell a
  colour-coded rating box linking to that episode.

### Remote announcers (media-announcer)

Discovery and serving are deliberately bifurcated: the **server** announces only on
the interfaces it should actually serve from (`ssdp_addrs`), while **announcers** —
small relay daemons on always-on hosts in other network segments — carry discovery
where the server's own multicast doesn't reach reliably.

The motivating topology: the server wired on one network (its `advertise_ip`, the
canonical address every URL carries), with clients on an inner NAT'd network that can
reach it through their router. Rather than giving the server a (tenuous) wifi leg
onto the inner network, an announcer on any wired inner-network box relays discovery.
Note that a server interface on a client network is not just a discovery matter:
a directly-connected route makes *reply traffic* (the actual media) egress via that
interface — so "serve only over the wired link" means the server host should have no
address on the client network at all, with announcers covering discovery there.

`media-announcer` reads the same `media-server.toml` (`advertise_ip` required):

```sh
media-announcer --config media-server.toml    # optional: --interval-secs 120
```

Each interval it health-checks the server by fetching `device.xml` — which also
supplies the device UUID, so announcements always carry the server's true identity —
and while healthy it multicasts the standard alive set on the local networks and
answers M-SEARCH queries (so clients opening their UPnP view get an instant local
response). If the server disappears it sends one `byebye` and falls silent until the
server returns; it can never advertise a dead server. Multiple announcers are
harmless — clients deduplicate by device UUID.

Deployment: `deploy/media-announcer.service` for Linux (systemd). On macOS, run it
as a **root LaunchDaemon** (`/Library/LaunchDaemons/`) with the binary and config
copied to the internal disk (e.g. `/usr/local/bin`, `/etc/mediaserver/`): user-session
LaunchAgents are blocked by macOS Local Network privacy (multicast fails with a
misleading "No route to host"), system processes cannot read external volumes, and
the binary should be ad-hoc signed with a stable identifier
(`codesign -s - -i com.mediaserver.announcer --force <binary>`) — re-sign after
rebuilds. The announcer needs no privileges, no state, and no database — just the
config and the network.

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

### media-title: fixing embedded container junk

Some releases embed the scene filename as the container title, which players like
VLC prefer over the server-supplied name once playback starts. `media-title`
inspects and neutralizes that in place — a header-only patch, no re-muxing: MP4
title atoms are renamed to `free` (the standard padding atom); MKV/WebM Title
elements are rewritten as Void. Format is detected by magic bytes, not extension.

It also removes **embedded subtitle tracks** from MP4s the same way (the subtitle
`trak` box is renamed to `free`), which is how you undo a wrongly-matched `.srt`
that got embedded by enrichment: strip it, fix the sidecar, and the next enrich run
embeds the corrected one. Matroska subtitle removal would orphan cluster data, so
there it prints the `ffmpeg` remux command instead of guessing.

```sh
media-title file.mp4 episode.mkv     # show embedded titles and subtitle tracks
media-title --strip *.mp4 *.mkv      # neutralize titles (titleless files untouched)
media-title --strip-subs file.mp4    # remove embedded subtitle tracks

# audit a whole library:
find /mnt/media -type f \( -iname "*.mp4" -o -iname "*.mkv" \) -print0 \
  | xargs -0 media-title | grep ': title '
```

The running scanner re-extracts touched files automatically (a harmless no-op).

The detail and player pages name what you are watching — an episode shows
"Series — Season N, Episode M" under its title, with the series and season linked to
their browse pages, a track shows artist and album linked likewise (so anything found
via search is one click from its siblings), and the detail page ends with
"« Prior episode" / "Next episode »" links (songs likewise) that step through the
whole series (or album) in order, the last episode of a season continuing into the
next season that exists — and
hovering (or tab-focusing) the "details" link beside the player's title reveals a
card with the poster, plot, IMDb rating (linked to the IMDb entry when known),
genres, director, duration, resolution, codec, size and the same
"« Prior episode" / "Next episode »" links, so you rarely
need to navigate back for context. The in-browser player (`/play/{id}`, linked from
every video detail page) skips 10 seconds on ← / →, offers a "Skip intro" /
"Skip credits" button when the catalog knows the segments (see "Skip intro /
skip credits" below), and stamps the
playback position into the URL fragment once a second while playing (and on pause or
scrub) — `…/play/596#128s` — so the address bar is always a resumable, shareable link; opening such a URL seeks
there once metadata loads and offers a "start from the beginning" link. Fragments are
never sent to the server, so this is purely client side and nothing is stored
server-side. The position is also stashed in the browser's sessionStorage per file,
covering the return trip the fragment cannot: leaving through links and coming back to
the program some way other than Back. In that case a "resume at m:ss" link appears
beside the skip hint for 30 seconds — while it shows, the stored position is protected
from being overwritten by the fresh playback, and clicking it jumps there; unclicked,
it removes itself and normal position tracking resumes. Finishing a program clears its
stored position, and sessionStorage means it is per-tab and gone when the browser
closes.

Episodes and album tracks auto-play into the next one when they end ("Auto-play next
episode", a checkbox under the player, remembered per browser; a "Next up" link sits
beside it). Rather than navigating — which would drop a fullscreen player — the page
fetches the next episode's page and swaps its pieces into place: source, poster,
subtitle track, heading, hover card and next-up note, with the URL updated via
`pushState` so reload, bookmark and resume keep working. The `<video>` element itself
never changes, so fullscreen survives the hand-over. Music plays on the detail page
itself — an `<audio>` player sits under the title, and the same in-place hand-over
steps through the album — and with a player on the page the "Prior" / "Next" /
"Next up" links swap the neighbour in the same way instead of reloading.

The same stripping runs automatically as part of enrichment when `strip_titles = true`
is set in the scanner config's `[enrich]` section (or with `media-enrich
--strip-titles`), so newly added releases are cleaned without a manual step. It is
opt-in because it is the one step that writes into media files rather than beside them.

### Embedding sidecar subtitles

Some MP4 releases ship subtitles only as a same-name `.srt` beside the file, which
many UPnP clients can't use. With `embed_subtitles = true` in the `[enrich]` section
(or `media-enrich --embed-subtitles`), enrichment muxes that sidecar in as a
`mov_text` track — the equivalent of `ffmpeg -i in.mp4 -i in.srt -c copy -c:s mov_text`
— for MP4s that have **no** subtitle stream and exactly that one `.srt`. Existing
streams are copied, never re-encoded. It is the most invasive step (a whole-file
replacement), so it is conservative: mux to a temp file in the same directory, verify
the result with ffprobe (stream count, subtitle present, duration unchanged), then a
single atomic rename. Opt-in; needs `ffmpeg` on the PATH (`ffmpeg_path` otherwise).

### Extracting embedded subtitles to sidecars

The reverse direction runs by default: for every video that has **no** same-name
`.srt`, enrichment extracts the best embedded text subtitle track to `{stem}.srt`
(`ffmpeg -map 0:s:N -f srt`), so VLC, Infuse, Kodi and UPnP renderers can all use it
and the web player has captions the moment playback starts. "Best" means full
captions over a **forced** track (foreign-language passages only — often listed
first and a poor default): English is preferred, SDH/CC (full dialogue plus sound
cues) over plain, forced tracks last, judged from the container's `forced` /
`hearing_impaired` flags and the track title. Bitmap tracks (PGS, VobSub) are not
text and are left alone, as is any `.srt` that already exists — hand-made sidecars
are never overwritten. Disable with `extract_subtitles = false` (or
`media-enrich --no-extract-subtitles`).

The web player falls back to extracting on demand — same track choice — into a
`vtt-cache` directory beside the catalog, for files enrichment has not reached yet
(or could not write beside). Concurrent viewers of the same file share one
extraction.

### Skip intro / skip credits

The catalog stores **skippable segments** per video file (intro, credits, recap,
commercial break), and the web player overlays a "Skip intro" / "Skip credits"
button while playback is inside one — a nudge, never an auto-skip. Credits
usually run to the end of the file, so skipping them fires the normal
end-of-media path and auto-play-next carries straight into the next episode.
(Web player only; UPnP has no vocabulary for this.) Three sources feed the
segments, in order of authority:

- **Chapter markers**: chapters named like "Opening", "Intro", "End Credits",
  "Recap" / "Previously on" are recognized during extraction (conservatively —
  unnamed or ordinary chapters yield nothing).
- **`.edl` sidecars**: a Kodi-style edit decision list beside the file (what
  comskip emits) — one `start stop action` line per segment, seconds. Cut (0)
  and commercial-break (3) actions become segments; EDL carries no
  intro/credits notion, so the kind is inferred from position (reaching the
  file's tail = credits, starting in the first five minutes = intro, anything
  else a commercial break). The sidecar is deliberate, so its presence — even
  empty — silences the other two sources; like `.nfo` files it is tracked by
  mtime, and edits are picked up by the watcher or the next reconcile.
- **Automatic audio detection**: episodes of a season share nearly identical
  intro and credits audio, so the scanner decodes each episode's first ten and
  last five minutes (ffmpeg, mono 11 kHz), fingerprints them with chromaprint,
  and cross-matches episode pairs — candidate alignments by hash voting (intro
  and credits sit at different relative offsets between two episodes, so a
  single best alignment is not enough), then gap-bridged runs of near-identical
  items along each. Median consensus across pairings (two agreeing pairings,
  or one in a two-episode season) yields each episode's intro and credits,
  stored as source `audio` — never overwriting the deliberate sources above.

Detection is incremental: fingerprints are cached in the catalog keyed to the
file's size and mtime, so a newly arrived episode decodes only itself and is
matched against its siblings' cached prints. The daemon analyzes one season per
tick (keeping the watcher responsive), after new media settles and while
enrichment is idle — enrichment may rewrite files, which would immediately
re-stale fresh fingerprints; `--once` drains everything before exiting. Seasons
with unreachable files (an unmounted share) are skipped and retried later. On by
default; an optional `[segments]` section in `media-scanner.toml` turns it off
(`auto = false`) or points at a specific ffmpeg (`ffmpeg_path`, falling back to
the `[enrich]` one). Renditions of the same episode are never matched against
each other, and a near-total match between two "different" episodes is rejected
as mislabeled duplicate content rather than reported as an intro. If detection
misfires on a show, drop an `.edl` beside the episode — it wins outright.

### Link previews when sharing pages

Every page carries Open Graph tags, so a link pasted into a chat or feed unfurls
into a poster / title / description card: series, season, detail, and player
pages all participate, with the type-appropriate context a bare title lacks
("Series S01E03 — Episode Title", "Title (2023)", "Artist — Track"). The
`og:image` points at `/art/{id}/og.jpg`, the poster downscaled to at most 900 px
on the long edge and re-encoded as JPEG — preview scrapers silently drop
full-size posters — and every URL is absolute against the origin the visitor
actually used, HTTPS included. Whether a card appears depends on who fetches it:
iMessage fetches previews from the sender's device, so plain LAN links unfurl
fine there; Slack or Discord fetch from their servers and need the HTTPS
hostname to be reachable from outside.

### HTTPS for the web pages

The browser pages can also be served over TLS on a second port, leaving the UPnP side
untouched — SSDP `LOCATION`, `device.xml`, the SOAP endpoints and every DIDL `<res>` URL
stay plain HTTP on `bind`, because renderers and TVs neither speak HTTPS nor could
validate a private certificate. Add a `[tls]` section to `media-server.toml`:

```toml
[tls]
bind = "0.0.0.0:8443"
hostname = "media.example.net"          # the name the certificate is issued for
cert = "/etc/mediaserver/fullchain.pem"
key = "/etc/mediaserver/privkey.pem"
redirect_pages = false                  # true: plain-port page requests → https://hostname
reload_secs = 3600                      # re-read the files this often (renewals)
```

The same router serves both ports. Pages use relative media URLs, so nothing is
mixed-content over HTTPS; playlists fetched over HTTPS carry `https://` entries for the
host the client used, while the plain port keeps the canonical UPnP URLs. With
`redirect_pages = true`, only HTML pages (`/`, `/browse…`, `/item/…`, `/play/…`,
`/search`) are redirected — media, artwork, playlists, icons and the UPnP endpoints
never are. The certificate and key are re-read every `reload_secs`, so a renewed
certificate needs no restart — as long as the files it reads are the renewed ones (see
below); a missing file or an unbindable port fails at startup.
TLS is rustls with the `ring` provider (pure Rust, no cmake or C toolchain on the Pi).

For the certificate, a public name with a Let's Encrypt certificate obtained via a
DNS-01 challenge works well even for a LAN-only address (the name can resolve to a
private IP; nothing on port 80 needs exposing). Let's Encrypt's `live/` files are
root-only, so have a certbot deploy hook copy each renewal to files the service user can
read — the commented recipe in `deploy/media-server.service` — and point `cert`/`key` at
those; the hourly re-read then picks renewals up automatically. (systemd's
`LoadCredential=` is not a fit here: it copies files once at service start, so a
renewal would need a restart.) HTTPS is a secure context, which is what browser
features like the Media Session API and "Add to Home Screen" require.

To listen on the standard port 443 under the unit's unprivileged user, uncomment
`AmbientCapabilities=CAP_NET_BIND_SERVICE` / `CapabilityBoundingSet=CAP_NET_BIND_SERVICE`
in the unit (or add them as a drop-in with `systemctl edit media-server`), set
`bind = "0.0.0.0:443"`, then `daemon-reload` and restart; generated links omit `:443`.

### Remuxing MKV to MP4

Matroska is fine for TVs and VLC but not for browsers: Firefox won't range-stream it
and Safari won't open it, even when the streams inside are plain H.264/HEVC + AAC.
With `remux_mkv = true` in the `[enrich]` section (or `media-enrich --remux-mkv`),
enrichment rewrites eligible `.mkv` files as `.mp4` with the video and audio streams
copied bit-for-bit — the equivalent of `ffmpeg -i in.mkv -map 0 -c copy -c:s mov_text
-tag:v hvc1 -movflags +faststart out.mp4`. One thing is added: an AC-3 / E-AC-3 track
(which Chrome and Firefox cannot decode in any container) gains a **stereo AAC twin
inserted ahead of it as the default track**, so browsers play the file while receivers
and TVs still find the original Dolby Digital track behind it.

Only clean candidates are converted; everything else is listed as "kept as-is" with
the reason: video other than H.264/HEVC/AV1, audio MP4 can't carry natively (DTS,
TrueHD, FLAC, PCM, Vorbis), bitmap subtitles (PGS/VobSub have no MP4 form), Dolby
Vision, or a same-name `.mp4` already present. Bitmap subtitles stop disqualifying a
file when a usable same-stem `.srt` sidecar exists: the bitmap tracks are dropped
(the sidecar covers them), and whenever no text subtitle track survives — that case,
or an `.mkv` with no subtitles at all — the sidecar is embedded as the `mov_text`
track in the same pass. Text subtitles become `mov_text` (ASS
styling is flattened); attachments such as fonts and cover art are dropped; chapters
and language tags carry over. The file is written beside the original, verified with
ffprobe (stream counts, duration), given the original's mtime, renamed into place, and
only then is the `.mkv` removed — sidecars (`.nfo`, `-poster.jpg`, `.srt`) share the
stem and stay valid, and the catalog carries the original's added-at date across the
change so Recently Added doesn't fill with conversions. `--dry-run` shows the full plan
(it only probes). Opt-in; needs `ffmpeg` (`ffmpeg_path`) and `ffprobe`.

## Not implemented (v1)

- GENA eventing (clients poll instead).
- DLNA.ORG_PN media profiles — protocolInfo is the permissive generic form, which VLC,
  BubbleUPnP, and most TVs accept.
- Transcoding.
