//! Inspect or neutralize the embedded title of MP4 files, in place.
//!
//! Release files often carry the scene filename as the container title
//! (moov/udta/meta/ilst/©nam), which players like VLC prefer over the
//! title a UPnP server supplies once playback starts. Removing an atom
//! would mean rewriting the file (atom sizes are baked in), but renaming
//! its type to `free` — the standard padding atom — makes every parser
//! skip it. A four-byte patch, no re-muxing, instant on any file size.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Show or neutralize the embedded title of MP4 files (in place, 4-byte patch)")]
struct Args {
    /// MP4/M4V/M4A files to process.
    files: Vec<PathBuf>,
    /// Neutralize found titles (default is show-only).
    #[arg(long)]
    strip: bool,
}

#[derive(Debug, Clone, Copy)]
struct Atom {
    /// File offset of the atom's first byte (the size field).
    offset: u64,
    size: u64,
    header_len: u64,
    kind: [u8; 4],
}

impl Atom {
    fn body(&self) -> (u64, u64) {
        (self.offset + self.header_len, self.offset + self.size)
    }
}

/// Parse the atoms in [start, end).
fn atoms_in(file: &mut File, start: u64, end: u64) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 8 <= end {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let size32 = u32::from_be_bytes(header[0..4].try_into().unwrap());
        let kind = [header[4], header[5], header[6], header[7]];
        let (size, header_len) = match size32 {
            0 => (end - pos, 8),
            1 => {
                let mut large = [0u8; 8];
                file.read_exact(&mut large)?;
                (u64::from_be_bytes(large), 16)
            }
            s => (s as u64, 8),
        };
        if size < header_len || pos + size > end {
            bail!("malformed atom at offset {pos}");
        }
        out.push(Atom { offset: pos, size, header_len, kind });
        pos += size;
    }
    Ok(out)
}

/// Descend a path of container atoms from [start, end), returning the body
/// range of the final one. `meta` is a FullBox: its children start 4 bytes
/// (version/flags) into the body.
fn descend(file: &mut File, path: &[&[u8; 4]], start: u64, end: u64) -> Result<Option<(u64, u64)>> {
    let (mut lo, mut hi) = (start, end);
    for want in path {
        let Some(found) = atoms_in(file, lo, hi)?.into_iter().find(|a| a.kind == **want)
        else {
            return Ok(None);
        };
        let (body_lo, body_hi) = found.body();
        lo = if found.kind == *b"meta" { body_lo + 4 } else { body_lo };
        hi = body_hi;
    }
    Ok(Some((lo, hi)))
}

const TITLE_KIND: [u8; 4] = [0xA9, b'n', b'a', b'm']; // ©nam

/// The current title text inside a ©nam atom, if readable.
fn title_text(file: &mut File, title: &Atom) -> Result<Option<String>> {
    let (lo, hi) = title.body();
    let Some(data) = atoms_in(file, lo, hi)?.into_iter().find(|a| &a.kind == b"data") else {
        return Ok(None);
    };
    let (data_lo, data_hi) = data.body();
    // data atom body: 4 bytes type/version, 4 bytes locale, then payload.
    if data_hi <= data_lo + 8 {
        return Ok(None);
    }
    let mut payload = vec![0u8; (data_hi - data_lo - 8) as usize];
    file.seek(SeekFrom::Start(data_lo + 8))?;
    file.read_exact(&mut payload)?;
    Ok(Some(String::from_utf8_lossy(&payload).to_string()))
}

fn process(path: &Path, strip: bool) -> Result<()> {
    let mut file = if strip {
        OpenOptions::new().read(true).write(true).open(path)
    } else {
        File::open(path)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();

    let Some((ilst_lo, ilst_hi)) =
        descend(&mut file, &[b"moov", b"udta", b"meta", b"ilst"], 0, len)?
    else {
        println!("{}: no metadata item list", path.display());
        return Ok(());
    };
    let titles: Vec<Atom> = atoms_in(&mut file, ilst_lo, ilst_hi)?
        .into_iter()
        .filter(|a| a.kind == TITLE_KIND)
        .collect();
    if titles.is_empty() {
        println!("{}: no embedded title", path.display());
        return Ok(());
    }
    for title in titles {
        let text = title_text(&mut file, &title)?.unwrap_or_default();
        if strip {
            // Rename ©nam -> free: the type field sits 4 bytes into the
            // atom (after the 32-bit size), even for 64-bit-size atoms.
            file.seek(SeekFrom::Start(title.offset + 4))?;
            file.write_all(b"free")?;
            println!("{}: neutralized title {text:?}", path.display());
        } else {
            println!("{}: title {text:?}", path.display());
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.files.is_empty() {
        bail!("no files given; usage: mp4-title [--strip] <files...>");
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
