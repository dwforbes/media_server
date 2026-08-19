//! Inspect or neutralize the embedded title of MP4 and Matroska files, in
//! place. See media_db::container_title for the mechanism. The same logic
//! runs automatically inside media-enrich when `strip_titles` is enabled.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Parser;
use media_db::container_title::{self, TitleStatus};

#[derive(Parser)]
#[command(
    about = "Show or neutralize the embedded title of MP4/MKV files (in place, header-only patch)"
)]
struct Args {
    /// MP4/M4V/M4A/MKV/WebM files to process.
    files: Vec<PathBuf>,
    /// Neutralize found titles (default is show-only).
    #[arg(long)]
    strip: bool,
}

fn process(path: &Path, strip: bool) -> Result<()> {
    if strip {
        let removed = container_title::strip(path)?;
        if removed.is_empty() {
            println!("{}: no embedded title", path.display());
        }
        for text in removed {
            println!("{}: neutralized title {text:?}", path.display());
        }
        return Ok(());
    }
    match container_title::inspect(path)? {
        TitleStatus::Unsupported => println!("{}: not MP4/MKV or no metadata", path.display()),
        TitleStatus::NoTitle => println!("{}: no embedded title", path.display()),
        TitleStatus::Titles(titles) => {
            for text in titles {
                println!("{}: title {text:?}", path.display());
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.files.is_empty() {
        bail!("no files given; usage: media-title [--strip] <files...>");
    }
    let mut failures = 0;
    for path in &args.files {
        if let Err(err) = process(path, args.strip) {
            eprintln!("{}: {err:#}", path.display());
            failures += 1;
        }
    }
    if failures > 0 {
        bail!("{failures} file(s) failed");
    }
    Ok(())
}
