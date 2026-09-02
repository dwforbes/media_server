//! Inspect or remove embedded container junk — scene-filename titles and
//! subtitle tracks — in MP4 and Matroska files, in place. See
//! media_db::container for the mechanism. Title stripping also runs
//! automatically inside media-enrich when `strip_titles` is enabled.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Parser;
use media_db::container::{self, HevcTag, SubtitleStatus, TitleStatus};

#[derive(Parser)]
#[command(
    about = "Show or remove embedded titles and subtitle tracks in MP4/MKV files \
             (in place, header-only patch)"
)]
struct Args {
    /// MP4/M4V/M4A/MKV/WebM files to process.
    files: Vec<PathBuf>,
    /// Neutralize embedded container titles.
    #[arg(long)]
    strip: bool,
    /// Remove embedded subtitle tracks (MP4 only — Matroska needs a remux).
    #[arg(long)]
    strip_subs: bool,
    /// Retag hev1 HEVC video as hvc1 in place (MP4 only), which Safari,
    /// QuickTime and iOS require; possible whenever the header already
    /// carries the parameter sets, which is nearly always.
    #[arg(long)]
    hvc1: bool,
}

fn show(path: &Path) -> Result<()> {
    match container::inspect(path)? {
        TitleStatus::Unsupported => println!("{}: not MP4/MKV or no metadata", path.display()),
        TitleStatus::NoTitle => println!("{}: no embedded title", path.display()),
        TitleStatus::Titles(titles) => {
            for text in titles {
                println!("{}: title {text:?}", path.display());
            }
        }
    }
    match container::subtitle_tracks(path)? {
        SubtitleStatus::Tracks(handlers) => println!(
            "{}: {} embedded subtitle track(s) [{}]",
            path.display(),
            handlers.len(),
            handlers.join(", ")
        ),
        SubtitleStatus::None => println!("{}: no embedded subtitle tracks", path.display()),
        // Matroska: reporting "none" would be a lie, and this tool cannot
        // enumerate them without a demux.
        SubtitleStatus::Unsupported => {}
    }
    match container::hevc_tag(path)? {
        HevcTag::None => {}
        HevcTag::Hvc1 => println!("{}: HEVC tagged hvc1 (plays on Apple devices)", path.display()),
        HevcTag::Hev1Fixable => println!(
            "{}: HEVC tagged hev1 — Safari, QuickTime and iOS refuse it; --hvc1 retags it in place",
            path.display()
        ),
        HevcTag::Hev1InBandOnly => println!(
            "{}: HEVC tagged hev1 with in-band parameter sets only — needs a remux \
             (ffmpeg -i in.mp4 -c copy -tag:v hvc1 out.mp4)",
            path.display()
        ),
    }
    // Captions media-enrich muxed in from the .srt sidecar carry a record
    // of it; a changed sidecar makes the next enrichment replace them.
    if let Ok(Some(tool)) = container::encoding_tool(path) {
        if let Some(hash) = media_db::captions::recorded_hash(&tool) {
            println!(
                "{}: captions embedded from the .srt sidecar (sha256 {}…)",
                path.display(),
                &hash[..12]
            );
        }
    }
    Ok(())
}

fn process(path: &Path, args: &Args) -> Result<()> {
    if args.hvc1 {
        match container::retag_hvc1(path)? {
            HevcTag::Hvc1 => println!("{}: HEVC now tagged hvc1", path.display()),
            HevcTag::Hev1InBandOnly => println!(
                "{}: not retagged — the header carries no parameter sets; remux instead",
                path.display()
            ),
            _ => println!("{}: no HEVC video to retag", path.display()),
        }
    }
    if args.strip {
        let removed = container::strip(path)?;
        if removed.is_empty() {
            println!("{}: no embedded title", path.display());
        }
        for text in removed {
            println!("{}: neutralized title {text:?}", path.display());
        }
    }
    if args.strip_subs {
        match container::strip_subtitles(path)? {
            SubtitleStatus::Tracks(handlers) => println!(
                "{}: removed {} subtitle track(s) [{}]",
                path.display(),
                handlers.len(),
                handlers.join(", ")
            ),
            SubtitleStatus::None => {
                println!("{}: no embedded subtitle tracks", path.display())
            }
            SubtitleStatus::Unsupported => println!(
                "{}: subtitle removal needs a remux for this container — \
                 ffmpeg -i IN -map 0:v -map \"0:a?\" -c copy OUT",
                path.display()
            ),
        }
    }
    if !args.strip && !args.strip_subs {
        show(path)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.files.is_empty() {
        bail!("no files given; usage: media-title [--strip] [--strip-subs] <files...>");
    }
    let mut failures = 0;
    for path in &args.files {
        if let Err(err) = process(path, &args) {
            eprintln!("{}: {err:#}", path.display());
            failures += 1;
        }
    }
    if failures > 0 {
        bail!("{failures} file(s) failed");
    }
    Ok(())
}
