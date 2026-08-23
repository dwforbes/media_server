//! Embedded container titles in MP4 and Matroska files: inspect, and
//! neutralize in place.
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
use std::path::Path;

use anyhow::{bail, Context, Result};

/// What a file's container says about titles.
#[derive(Debug, Clone, PartialEq)]
pub enum TitleStatus {
    /// Not an MP4 or Matroska file (or no metadata structure at all).
    Unsupported,
    NoTitle,
    /// One or more embedded titles, as text.
    Titles(Vec<String>),
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
                    // A Title claiming to extend past EOF is malformed; never
                    // trust its size for allocation or in-place rewriting.
                    if grand.id == TITLE && grand.end() <= file_len {
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
    // Zero the old title bytes in bounded chunks — never allocate the
    // element's declared size (a crafted file could claim a huge one).
    let zeros = [0u8; 8192];
    let mut remaining = element.data_len;
    while remaining > 0 {
        let n = remaining.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}


enum Container {
    Mp4,
    Matroska,
}

fn detect(file: &mut File) -> Result<Container> {
    let mut magic = [0u8; 4];
    file.seek(SeekFrom::Start(0))?;
    if file.read_exact(&mut magic).is_ok() && u32::from_be_bytes(magic) == EBML_HEADER {
        return Ok(Container::Matroska);
    }
    Ok(Container::Mp4)
}

fn mp4_title_atoms(file: &mut File, len: u64) -> Result<Option<Vec<Atom>>> {
    let Some((ilst_lo, ilst_hi)) =
        descend(file, &[b"moov", b"udta", b"meta", b"ilst"], 0, len)?
    else {
        return Ok(None);
    };
    Ok(Some(
        atoms_in(file, ilst_lo, ilst_hi)?
            .into_iter()
            .filter(|a| a.kind == TITLE_KIND)
            .collect(),
    ))
}

fn matroska_title_text(file: &mut File, title: &EbmlElement) -> Result<String> {
    let mut text = vec![0u8; title.data_len.min(512) as usize];
    file.seek(SeekFrom::Start(title.data_offset()))?;
    file.read_exact(&mut text)?;
    Ok(String::from_utf8_lossy(&text).trim_end_matches('\0').to_string())
}

/// Report the embedded title(s) of a file without modifying it.
pub fn inspect(path: &Path) -> Result<TitleStatus> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    match detect(&mut file)? {
        Container::Matroska => {
            let titles = matroska_titles(&mut file, len)?;
            if titles.is_empty() {
                return Ok(TitleStatus::NoTitle);
            }
            let mut out = Vec::new();
            for t in &titles {
                out.push(matroska_title_text(&mut file, t)?);
            }
            Ok(TitleStatus::Titles(out))
        }
        Container::Mp4 => match mp4_title_atoms(&mut file, len) {
            Ok(Some(atoms)) if atoms.is_empty() => Ok(TitleStatus::NoTitle),
            Ok(Some(atoms)) => {
                let mut out = Vec::new();
                for a in &atoms {
                    out.push(title_text(&mut file, a)?.unwrap_or_default());
                }
                Ok(TitleStatus::Titles(out))
            }
            Ok(None) => Ok(TitleStatus::Unsupported),
            // Not an MP4 either: malformed-atom errors mean "not ours".
            Err(_) => Ok(TitleStatus::Unsupported),
        },
    }
}

/// Neutralize embedded titles in place. Returns the titles that were
/// removed (empty if there were none); files of other formats are left
/// untouched and report Unsupported-equivalent empty results.
pub fn strip(path: &Path) -> Result<Vec<String>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} for writing", path.display()))?;
    let len = file.metadata()?.len();
    let mut removed = Vec::new();
    match detect(&mut file)? {
        Container::Matroska => {
            for title in matroska_titles(&mut file, len)? {
                removed.push(matroska_title_text(&mut file, &title)?);
                void_element(&mut file, &title)?;
            }
        }
        Container::Mp4 => {
            let Ok(Some(atoms)) = mp4_title_atoms(&mut file, len) else {
                return Ok(removed);
            };
            for atom in atoms {
                removed.push(title_text(&mut file, &atom)?.unwrap_or_default());
                // Rename ©nam -> free: the type field sits 4 bytes into the
                // atom (after the 32-bit size), even for 64-bit-size atoms.
                file.seek(SeekFrom::Start(atom.offset + 4))?;
                file.write_all(b"free")?;
            }
        }
    }
    Ok(removed)
}
