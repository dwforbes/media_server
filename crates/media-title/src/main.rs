//! Inspect or neutralize the embedded title of MP4 and Matroska files,
//! in place.
//!
//! Release files often carry the scene filename as the container title
//! (MP4: moov/udta/meta/ilst/©nam; MKV/WebM: Segment Info Title), which
//! players like VLC prefer over the title a UPnP server supplies once
//! playback starts. Removing the element would mean rewriting the file
//! (sizes are baked into both formats), but each has a sanctioned "skip
//! this" form occupying identical bytes: MP4 title atoms are renamed to
//! `free`, Matroska title elements are rewritten as Void. A header-sized
//! patch, no re-muxing, instant on any file size. Formats are detected by
//! magic bytes, not extension.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

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

// ---------------------------------------------------------------- Matroska

/// EBML element IDs, as read (marker bits included).
const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const CLUSTER: u32 = 0x1F43_B675;
const TITLE: u32 = 0x7BA9;

#[derive(Debug, Clone, Copy)]
struct EbmlElement {
    /// File offset of the element's first ID byte.
    offset: u64,
    id: u32,
    id_len: u8,
    size_len: u8,
    data_len: u64,
}

impl EbmlElement {
    fn data_offset(&self) -> u64 {
        self.offset + self.id_len as u64 + self.size_len as u64
    }
    fn end(&self) -> u64 {
        self.data_offset() + self.data_len
    }
}

fn read_ebml_element(file: &mut File, offset: u64) -> Result<EbmlElement> {
    file.seek(SeekFrom::Start(offset))?;
    let mut first = [0u8; 1];
    file.read_exact(&mut first)?;
    let id_len = first[0].leading_zeros() as u8 + 1;
    if id_len > 4 {
        bail!("invalid EBML id at offset {offset}");
    }
    let mut id = first[0] as u32;
    for _ in 1..id_len {
        let mut b = [0u8; 1];
        file.read_exact(&mut b)?;
        id = (id << 8) | b[0] as u32;
    }

    file.read_exact(&mut first)?;
    let size_len = first[0].leading_zeros() as u8 + 1;
    if size_len > 8 {
        bail!("invalid EBML size at offset {offset}");
    }
    // For an 8-byte VINT the first byte holds no value bits at all.
    let mask = 0xFFu8.checked_shr(size_len as u32).unwrap_or(0);
    let mut data_len = (first[0] & mask) as u64;
    let mut all_ones = data_len == mask as u64;
    for _ in 1..size_len {
        let mut b = [0u8; 1];
        file.read_exact(&mut b)?;
        data_len = (data_len << 8) | b[0] as u64;
        all_ones &= b[0] == 0xFF;
    }
    if all_ones {
        // "Unknown size" — legal for Segment in live streams.
        data_len = u64::MAX;
    }
    Ok(EbmlElement { offset, id, id_len, size_len, data_len })
}

/// Title elements in a Matroska file: walk Segment children up to the
/// first Cluster (Info always precedes media data in practice).
fn matroska_titles(file: &mut File, file_len: u64) -> Result<Vec<EbmlElement>> {
    let header = read_ebml_element(file, 0)?;
    if header.id != EBML_HEADER {
        bail!("not a Matroska file");
    }
    let segment = read_ebml_element(file, header.end())?;
    if segment.id != SEGMENT {
        bail!("no Segment element");
    }
    let segment_end = if segment.data_len == u64::MAX {
        file_len
    } else {
        segment.end().min(file_len)
    };

    let mut titles = Vec::new();
    let mut pos = segment.data_offset();
    while pos + 2 <= segment_end {
        let child = read_ebml_element(file, pos)?;
        if child.data_len == u64::MAX {
            bail!("unexpected unknown-size element at offset {pos}");
        }
        match child.id {
            CLUSTER => break,
            INFO => {
                let mut inner = child.data_offset();
                while inner + 2 <= child.end() {
                    let grand = read_ebml_element(file, inner)?;
                    if grand.data_len == u64::MAX {
                        bail!("unexpected unknown-size element at offset {inner}");
                    }
                    if grand.id == TITLE {
                        titles.push(grand);
                    }
                    inner = grand.end();
                }
            }
            _ => {}
        }
        pos = child.end();
    }
    Ok(titles)
}

/// Big-endian EBML VINT of exactly `len` bytes encoding `value`.
fn encode_vint(value: u64, len: u8) -> Result<Vec<u8>> {
    let capacity = (1u64 << (7 * len as u32)) - 1;
    if value >= capacity {
        bail!("value {value} does not fit a {len}-byte VINT");
    }
    let mut bytes = vec![0u8; len as usize];
    let mut v = value;
    for slot in bytes.iter_mut().rev() {
        *slot = (v & 0xFF) as u8;
        v >>= 8;
    }
    bytes[0] |= 0x80 >> (len - 1);
    Ok(bytes)
}

/// Rewrite a Title element as a Void of identical total span: Void's
/// one-byte ID frees up header bytes, absorbed by a longer size VINT.
/// The old title text is zeroed for good measure.
fn void_element(file: &mut File, element: &EbmlElement) -> Result<()> {
    let new_size_len = element.id_len + element.size_len - 1;
    let mut header = vec![0xECu8];
    header.extend(encode_vint(element.data_len, new_size_len)?);
    file.seek(SeekFrom::Start(element.offset))?;
    file.write_all(&header)?;
    file.write_all(&vec![0u8; element.data_len as usize])?;
    Ok(())
}

fn process_matroska(path: &Path, file: &mut File, len: u64, strip: bool) -> Result<()> {
    let titles = matroska_titles(file, len)?;
    if titles.is_empty() {
        println!("{}: no embedded title", path.display());
        return Ok(());
    }
    for title in titles {
        let mut text = vec![0u8; title.data_len.min(512) as usize];
        file.seek(SeekFrom::Start(title.data_offset()))?;
        file.read_exact(&mut text)?;
        let text = String::from_utf8_lossy(&text).trim_end_matches('\0').to_string();
        if strip {
            void_element(file, &title)?;
            println!("{}: neutralized title {text:?}", path.display());
        } else {
            println!("{}: title {text:?}", path.display());
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ driver

fn process(path: &Path, strip: bool) -> Result<()> {
    let mut file = if strip {
        OpenOptions::new().read(true).write(true).open(path)
    } else {
        File::open(path)
    }
    .with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();

    // Detect by magic: EBML files open with 0x1A45DFA3.
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && u32::from_be_bytes(magic) == EBML_HEADER {
        return process_matroska(path, &mut file, len, strip);
    }

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
