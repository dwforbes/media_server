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
  duration, resolution and frame rate ("1920 × 1080 @ 23.976 fps"), codecs,
  container, file size, and when it was added.
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
every video detail page) skips 10 seconds on ← / → and toggles play/pause on space —
wherever focus is, not only with the video focused; text fields and focused buttons
keep the keys — offers a "Skip intro" /
"Skip credits" button when the catalog knows the segments (see "Skip intro /
skip credits" below), and stamps the
playback position into the URL fragment once a second while playing (and on pause or
scrub) — `…/play/596#128s` — so the address bar is always a resumable, shareable link; opening such a URL seeks
there once metadata loads and notes "Resuming at m:ss — start from the beginning"
beside the skip hint for 30 seconds. Fragments are
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
subtitle track, heading, hover card, next-up note and CC button, with the URL updated
via `pushState` so reload, bookmark and resume keep working. The `<video>` element
itself never changes, so fullscreen survives the hand-over. Music plays on the detail
page itself — an `<audio>` player sits under the title, and the same in-place
hand-over steps through the album — and with a player on the page the "Prior" /
"Next" / "Next up" links swap the neighbour in the same way instead of reloading.

A **CC** button at the right border of the skip-hint line under the video — the line
whose centre carries the resume notes — (present whenever the program has captions: an
`.srt` sidecar or an embedded text track) opens a captions panel down
the right side of the page, the video narrowing to make room; on phones it sits below
the controls instead. Every line of the subtitle track is listed with its time, but
placed along the program's running time rather than simply stacked: each line sits at
its start time on a pixels-per-second scale and is pushed down only as far as the line
above it needs, so a silent stretch is blank space and a fast exchange packs solid —
the column is the program's timeline, with a rule running on through the gaps. The
scale is the program's own (the median of line height over seconds to the next line,
so a typical pair of lines just touches). A bar marks the current position, drifting
through the silent stretches; the current line is highlighted; the list follows
playback unless you have just scrolled it yourself (it resumes after six seconds); and
clicking a line seeks there and plays (a modifier-click opens the same `#position`
resume link in a new tab). The lines come from the WebVTT track the browser already
loaded for the `<track>` element — the same conversion the on-video captions use,
embedded tracks included once their extraction finishes — so nothing is fetched twice
and no parsing happens in the page. The panel is remembered per browser like
auto-play: it re-opens on the next program with captions and follows in-place episode
swaps, hiding itself for a program that has none.

The same stripping runs automatically as part of enrichment when `strip_titles = true`
is set in the scanner config's `[enrich]` section (or with `media-enrich
--strip-titles`), so newly added releases are cleaned without a manual step. It is
opt-in because it is the one step that writes into media files rather than beside them.

### HEVC on Apple devices: hev1 vs hvc1

Safari, QuickTime and iOS decode HEVC in MP4 only from `hvc1` sample entries — the
variant whose parameter sets live in the header's `hvcC` box. `hev1`, which permits
in-band parameter sets, is refused outright, whatever the stream actually holds; VLC,
Windows and ffmpeg play both, which is why the failure looks random until you probe:
`ffprobe` prints the tag beside the codec (`hevc (Main 10) (hev1 / …)`). ffmpeg's
default for HEVC in MP4 is `hev1`, and most x265 MP4 rips carry it. Since `hvcC` nearly
always carries the parameter sets anyway, `hev1` is `hvc1` in all but name — and the
name is four bytes in the header. So:

- `media-title file.mp4` reports the tag; `media-title --hvc1 file.mp4` retags it in
  place, after checking that `hvcC` lists the VPS, SPS and PPS. A file whose header
  lacks them (rare) is reported as needing a remux: `ffmpeg -i in.mp4 -c copy -tag:v
  hvc1 out.mp4`.
- Enrichment does the same for every video under the movie and TV roots on each run
  (`fix_hevc_tags = true` by default; `--no-fix-hevc-tags` or `fix_hevc_tags = false`
  to leave files alone), reporting what it retagged and what needs a remux; the dry run
  lists what it would do. The MKV remux and the subtitle embed steps write `hvc1`
  themselves, so nothing they produce needs it.

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

The embedded track remembers where it came from. The file's `©too` ("encoding tool")
atom — the one atom meant to name the tool that wrote the file, which ffprobe reports
as `encoder` and `media-title` prints — records
`media-enrich; captions=srt:sha256:<hash of the embedded text>`. Every later run hashes
the sidecar and compares: replace or fix the `.srt`, and the next enrichment re-muxes
the file with the new track in place of the old one, under the same temp-file, verify,
rename discipline (`--dry-run` lists what would be replaced). A timestamp-only change
(a touch, a copy, a re-save in another encoding) is not a change. Captions are replaced
only when that record is present, the hashes differ and the file still has exactly that
one text track; captions of unknown provenance, or a file whose subtitle layout has
changed since, are left alone and mentioned in the output. Two consequences worth
knowing: deleting the sidecar of such a file lets the extraction step regenerate it
from the embedded track, whose text hashes differently, so the following run re-embeds
it once to re-anchor the record; and a plain `ffmpeg -c copy` of the file elsewhere
replaces the atom with ffmpeg's own name, after which the captions count as unknown
provenance. The remux step records the same thing when it embeds a sidecar.

None of this polls. The check runs inside an enrichment run, and a run is what the
scanner starts when new media settles — or, new with this feature, when an `.srt` is
written (after the same quiet period; sidecars a run itself writes, such as extracted
ones, do not re-trigger it). Within a run the embed step keeps a memory beside the
catalog, `enrich-captions.json`: for every video with a same-name `.srt`, the size and
mtime of both files at the last look and what was concluded. A pair whose stamps have
not moved is skipped without reading, hashing or probing anything, so a run over an
untouched library costs its directory walk and nothing more; a corrected sidecar, a
remux or a title strip moves a stamp and earns one fresh look. Dry runs read the memory
but never write it, and a file that fails is not recorded, so it is retried. A sidecar
the extraction step regenerates is recorded as it stands, so it is not re-embedded
merely for having been regenerated — only if you then change it.

**An existing library, and files embedded before records existed.** The first run
after this change looks at every pair once (the cost every run had before) and then
settles into the memory. A track with no record — embedded by an earlier version of
this tool, or by anything else — is not re-injected: when the file has exactly one text
track, the run reads that track and compares it with the sidecar cue by cue (timings
and words; indices, styling tags and line breaks do not count). If they match, the
sidecar is *adopted* as the track's source — remembered in the memory, the file
untouched — and from then on a corrected sidecar is embedded like any other; the run
reports these as "existing caption track matches the sidecar". If they differ, the
track is kept and the file is listed once as "they differ from the sidecar": either the
sidecar was corrected before this change existed, or the track came from somewhere
else. For a library where you know the sidecars are the truth, one manual
`media-enrich --config … --adopt-sidecar-captions` embeds those sidecars too; each file
gets the record and needs no further special treatment.

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

## Security posture

Everything here is unauthenticated by design — it is a LAN media server — so the
threat model is a compromised device on the LAN (any HTTP, SOAP or SSDP traffic) and
hostile content on the share (crafted media files, sidecars and filenames written by
anything that can reach it). The controls that follow from that:

- **Pages.** Every catalog-derived string is escaped into HTML/XML; the IMDb id is
  additionally validated (`tt` + digits) at ingest and at render. Every response
  carries `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`,
  `Referrer-Policy: no-referrer` and a Content-Security-Policy that allows exactly one
  inline script — the player's, by SHA-256 hash — so an escaping slip cannot run
  script. Art is served with a MIME type sniffed from the bytes (JPEG/PNG/GIF/WebP/BMP),
  never from a tag or extension; anything else is refused.
- **Share content.** Sidecars (.nfo, .srt, .edl, music.toml, posters) are read through
  `media_db::sidecar`: bounded in size (8 MiB text, 32 MiB images) and never through
  symlinks; the enricher writes them atomically (temp + rename) and refuses a symlink
  at the target; the watcher never catalogs a symlinked media file. NFO values are
  stripped of control characters and bounded; the MP4/Matroska title parser reads at
  most 64 KiB of a title and walks boxes with overflow-safe arithmetic; per-file
  extraction is wrapped so a parser panic marks the row error instead of stopping the
  scanner; preview-image decoding runs under pixel and allocation limits.
- **Requests.** SOAP bodies are capped at 2 MiB by the framework and parsed by a
  pull parser with no entity expansion; paging arithmetic saturates; search honours at
  most 12 distinct terms; object ids reject control characters; ffprobe/ffmpeg spawns
  share a two-permit semaphore and a "no captions" result is cached, so a burst of
  requests cannot fork a process apiece.
- **SSDP.** M-SEARCH is answered only for sources on a directly attached network (so a
  forged source cannot use the responder as an amplifier) and within a reply budget
  (25/s, bursts of 100); the announcer also validates the UUID it relays and caps the
  device.xml it reads.
- **Services.** The systemd units in `deploy/` run as an unprivileged user with
  `ProtectSystem=strict`, no capabilities, a syscall allow-list, no device or kernel
  access, and a memory ceiling. Re-copy them after pulling this change. Run
  `cargo audit` now and then; the RustSec advisories against quick-xml 0.37 are what
  prompted the move to 0.41.

## Not implemented (v1)

- GENA eventing (clients poll instead).
- DLNA.ORG_PN media profiles — protocolInfo is the permissive generic form, which VLC,
  BubbleUPnP, and most TVs accept.
- Transcoding.
