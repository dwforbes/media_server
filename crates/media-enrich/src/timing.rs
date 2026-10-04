//! Frame timing: HEVC MP4s whose muxer wrote no composition offsets.
//!
//! Some x265 releases arrive with B-frames but no `ctts` box in the video
//! track, so every frame's presentation time equals its decode time. VLC
//! and ffmpeg re-sort frames by picture order count and never notice.
//! Browsers hand the decoder display-order timestamps in decode order
//! and drop whatever comes out "late" — a sixth of the frames, seen as
//! judder — while the same file is flawless in VLC.
//!
//! The repair is lossless. The picture order counts are read from the
//! slice headers (ffmpeg's trace_headers bitstream filter: a parse, no
//! decode), the display order derived from them per coded video
//! sequence (H.265 8.3.1), and a `ctts` box inserted into the sample
//! table; the edit list starts presentation at the first displayed frame
//! and the durations follow. Not a byte of the elementary streams
//! changes. The file is rewritten beside itself (the moov grows, so it
//! cannot be patched in place), verified, and renamed over the original
//! with its mtime kept.
//!
//! Detection is cheap enough to run over the library every time: the
//! moov is parsed for an HEVC video track without `ctts`, and only then
//! does ffprobe say whether the stream reorders at all.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use media_db::container::{atoms_in, descend, trak_handler, Atom};

/// What a look at a file found.
#[derive(Debug, PartialEq)]
pub enum Check {
    /// Nothing to do, and why ("has ctts", "not HEVC", "no reordering").
    Fine(&'static str),
    /// HEVC, reorders frames, and no composition offsets anywhere.
    Missing { reorder: u32 },
}

pub enum Outcome {
    Fine(&'static str),
    WouldRepair { reorder: u32 },
    Repaired(Repair),
}

/// What a repair did, for the log.
#[derive(Debug)]
pub struct Repair {
    pub pictures: usize,
    pub sequences: usize,
    /// Presentation delay, media ticks (the largest offset needed).
    pub delay: u64,
    pub timescale: u32,
    pub runs: usize,
}

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["mp4", "m4v", "mov"].iter().any(|a| e.eq_ignore_ascii_case(a)))
}

/// The video trak and its stbl body range, by file offsets.
fn video_stbl(file: &mut std::fs::File) -> Result<Option<(Atom, (u64, u64))>> {
    let len = file.metadata()?.len();
    let Some(moov) = atoms_in(file, 0, len)?.into_iter().find(|a| a.kind == *b"moov") else {
        return Ok(None);
    };
    let (lo, hi) = moov.body();
    for trak in atoms_in(file, lo, hi)?.into_iter().filter(|a| a.kind == *b"trak") {
        if trak_handler(file, &trak)? != Some(*b"vide") {
            continue;
        }
        let (tlo, thi) = trak.body();
        let Some(stbl) = descend(file, &[b"mdia", b"minf", b"stbl"], tlo, thi)? else { continue };
        return Ok(Some((trak, stbl)));
    }
    Ok(None)
}

/// Is this an HEVC MP4 that reorders frames yet carries no ctts?
pub fn check(ffprobe: &str, path: &Path) -> Result<Check> {
    if !is_mp4(path) {
        return Ok(Check::Fine("not an MP4"));
    }
    let mut file = std::fs::File::open(path)?;
    let Some((_, (lo, hi))) = video_stbl(&mut file)? else {
        return Ok(Check::Fine("no video track"));
    };
    let kids = atoms_in(&mut file, lo, hi)?;
    if kids.iter().any(|a| a.kind == *b"ctts") {
        return Ok(Check::Fine("has ctts"));
    }
    // The sample entry's type names the codec: hvc1/hev1 for HEVC.
    let Some(stsd) = kids.iter().find(|a| a.kind == *b"stsd") else {
        return Ok(Check::Fine("no sample description"));
    };
    let (slo, shi) = stsd.body();
    let hevc = atoms_in(&mut file, slo + 8, shi)?
        .iter()
        .any(|a| a.kind == *b"hvc1" || a.kind == *b"hev1");
    if !hevc {
        return Ok(Check::Fine("not HEVC"));
    }
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=has_b_frames", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .with_context(|| format!("running {ffprobe}"))?;
    let reorder: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
    if reorder == 0 {
        return Ok(Check::Fine("no reordering"));
    }
    Ok(Check::Missing { reorder })
}

/// Per packet (= per sample, the edit list ignored so every sample comes
/// through), the picture it carries as (nal_unit_type, poc_lsb) from its
/// slice header — IDR pictures carry no lsb and have POC 0 — or None for
/// a packet with no slice at all (parameter sets or SEI on their own,
/// an end-of-sequence marker), which some muxers write as a sample of
/// its own. A packet with slices but no first slice segment is a
/// damaged picture and refused.
/// (nal_unit_type, poc_lsb) of a picture.
type Picture = (u32, u32);
/// While parsing: saw a slice header; the picture, its lsb still owed.
type PacketTrace = (bool, Option<(u32, Option<u32>)>);

fn picture_order(ffmpeg: &str, path: &Path) -> Result<(u32, Vec<Option<Picture>>)> {
    let mut child = Command::new(ffmpeg)
        .args(["-v", "info", "-nostdin", "-nostats", "-ignore_editlist", "1", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {ffmpeg}"))?;
    let stderr = child.stderr.take().context("no stderr")?;
    let mut log2_max: Option<u32> = None;
    let mut nal: u32 = 0;
    // Per packet: (saw a slice header, the picture).
    let mut packets: Vec<PacketTrace> = Vec::new();
    let mut pending = false;
    for line in std::io::BufReader::new(stderr).split(b'\n') {
        let line = line?;
        // "[trace_headers @ 0x…] Packet: 2773 bytes, key frame, pts …"
        // "[trace_headers @ 0x…] Slice Segment Header"
        // "[trace_headers @ 0x…] 171  log2_max_pic_order_cnt_lsb_minus4  00101 = 4"
        // Anything ffmpeg wrote before it on the same line (a progress
        // line ends in a carriage return, not a newline) is skipped.
        let text = String::from_utf8_lossy(&line);
        let Some(at) = text.rfind("[trace_headers") else { continue };
        let Some(rest) = text[at..].split_once("] ").map(|(_, r)| r) else { continue };
        if rest.starts_with("Packet:") {
            packets.push((false, None));
            pending = false;
            continue;
        }
        if rest.starts_with("Slice Segment Header") {
            if let Some(p) = packets.last_mut() {
                p.0 = true;
            }
            continue;
        }
        let mut parts = rest.split_whitespace();
        let Some(name) = parts.nth(1) else { continue };
        let Some(value) = text.rsplit("= ").next().and_then(|v| v.trim().parse::<i64>().ok()) else { continue };
        match name {
            "log2_max_pic_order_cnt_lsb_minus4" => {
                let v = value as u32 + 4;
                if log2_max.is_some_and(|l| l != v) {
                    bail!("the SPS changes log2_max_pic_order_cnt_lsb mid-stream; not handled");
                }
                log2_max = Some(v);
            }
            "nal_unit_type" => nal = value as u32,
            "first_slice_segment_in_pic_flag" if value == 1 => {
                let Some(p) = packets.last_mut() else { continue };
                if let Some((first, _)) = p.1 {
                    bail!("sample {}: two pictures in one sample (nal types {first} and {nal}); not handled", packets.len() - 1);
                }
                if nal == 19 || nal == 20 {
                    p.1 = Some((nal, Some(0)));
                    pending = false;
                } else {
                    p.1 = Some((nal, None));
                    pending = true;
                }
            }
            "slice_pic_order_cnt_lsb" if pending => {
                if let Some((_, Some((_, lsb)))) = packets.last_mut() {
                    *lsb = Some(value as u32);
                }
                pending = false;
            }
            _ => {}
        }
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg could not parse the stream ({status})");
    }
    let log2_max = log2_max.context("no SPS seen in the stream")?;
    let mut out = Vec::with_capacity(packets.len());
    for (i, (sliced, pic)) in packets.into_iter().enumerate() {
        out.push(match pic {
            Some((nt, Some(lsb))) => Some((nt, lsb)),
            Some((_, None)) => bail!("sample {i}: a slice without a picture order count"),
            None if sliced => bail!("sample {i}: slices but no first slice segment (a damaged picture)"),
            None => None,
        });
    }
    Ok((log2_max, out))
}

/// Display rank of every picture (H.265 8.3.1 for the POC msb), and the
/// number of coded video sequences.
pub fn display_ranks(log2_max: u32, pics: &[(u32, u32)]) -> (Vec<usize>, usize) {
    let max_lsb = 1i64 << log2_max;
    let mut poc = Vec::with_capacity(pics.len());
    let mut cvs = Vec::with_capacity(pics.len());
    let mut cvs_id: i64 = -1;
    let (mut prev_lsb, mut prev_msb) = (0i64, 0i64);
    for (i, &(nt, lsb)) in pics.iter().enumerate() {
        let lsb = lsb as i64;
        let irap = (16..=23).contains(&nt);
        let msb = if nt == 19 || nt == 20 || (irap && i == 0) {
            cvs_id += 1;
            0
        } else if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        poc.push(msb + lsb);
        cvs.push(cvs_id);
        // prevTid0Pic: not RASL/RADL (6–9), not a sub-layer non-reference
        // picture (even types below 16).
        let rasl_radl = (6..=9).contains(&nt);
        let sub_layer_non_ref = nt < 16 && nt % 2 == 0;
        if !rasl_radl && !sub_layer_non_ref {
            prev_lsb = lsb;
            prev_msb = msb;
        }
    }
    let mut order: Vec<usize> = (0..pics.len()).collect();
    order.sort_by_key(|&i| (cvs[i], poc[i]));
    let mut rank = vec![0usize; pics.len()];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }
    (rank, (cvs_id + 1).max(0) as usize)
}

/// Composition offsets from decode times and display ranks. `picture`
/// says which samples carry a picture (the rest — a parameter-set or
/// end-of-sequence sample — are not displayed and keep offset 0); the
/// j-th picture shows at the decode time of the picture of rank j, plus
/// the smallest delay keeping every offset non-negative. Returns
/// (delay, offsets).
pub fn offsets(dts: &[u64], picture: &[bool], rank: &[usize]) -> (u64, Vec<u64>) {
    let samples: Vec<usize> = (0..dts.len()).filter(|&i| picture[i]).collect();
    debug_assert_eq!(samples.len(), rank.len());
    let delay = samples
        .iter()
        .enumerate()
        .map(|(j, &i)| dts[i] as i64 - dts[samples[rank[j]]] as i64)
        .max()
        .unwrap_or(0)
        .max(0) as u64;
    let mut offs = vec![0u64; dts.len()];
    for (j, &i) in samples.iter().enumerate() {
        offs[i] = dts[samples[rank[j]]] + delay - dts[i];
    }
    (delay, offs)
}

/// The ctts box for a run of offsets.
pub fn ctts_box(offs: &[u64]) -> (Vec<u8>, usize) {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &o in offs {
        match runs.last_mut() {
            Some((count, last)) if *last as u64 == o => *count += 1,
            _ => runs.push((1, o as u32)),
        }
    }
    let mut body = Vec::with_capacity(8 + 8 * runs.len());
    body.extend_from_slice(&0u32.to_be_bytes()); // version 0, flags 0
    body.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    for (c, o) in &runs {
        body.extend_from_slice(&c.to_be_bytes());
        body.extend_from_slice(&o.to_be_bytes());
    }
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(b"ctts");
    out.extend_from_slice(&body);
    (out, runs.len())
}

// ---- In-memory box walking over the moov, by (pos, header_len, size) ----

fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], usize, usize, usize)> {
    let mut out = Vec::new();
    let mut p = start;
    while p + 8 <= end {
        let size32 = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = buf[p + 4..p + 8].try_into().unwrap();
        let (size, hl) = match size32 {
            0 => (end - p, 8),
            1 if p + 16 <= end => (u64::from_be_bytes(buf[p + 8..p + 16].try_into().unwrap()) as usize, 16),
            s => (s, 8),
        };
        if size < hl || p + size > end {
            break;
        }
        out.push((kind, p, hl, size));
        p += size;
    }
    out
}

fn find(buf: &[u8], start: usize, end: usize, path: &[&[u8; 4]]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (kind, p, hl, size) in boxes(buf, start, end) {
        if kind == *path[0] {
            if path.len() == 1 {
                out.push((p, hl, size));
            } else {
                out.extend(find(buf, p + hl, p + size, &path[1..]));
            }
        }
    }
    out
}

fn be32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
}
fn be64(buf: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(buf[at..at + 8].try_into().unwrap())
}

/// Grow a box's size field (32-bit, or the 64-bit largesize) in place.
fn grow_box(buf: &mut [u8], at: usize, by: usize) {
    if be32(buf, at) == 1 {
        let v = be64(buf, at + 8) + by as u64;
        buf[at + 8..at + 16].copy_from_slice(&v.to_be_bytes());
    } else {
        let v = be32(buf, at) + by as u32;
        buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
    }
}

/// A new moov with the ctts in the video track's stbl, sizes, edit list,
/// durations and (if the mdat follows the moov) chunk offsets adjusted.
/// Pure: `moov` is the box's bytes including its header.
pub fn rebuild_moov(moov: &[u8], picture: &[bool], rank: &[usize], mdat_after_moov: bool) -> Result<(Vec<u8>, Repair)> {
    let pics = picture.len();
    let top = boxes(moov, 0, moov.len());
    let &(_, mp, mhl, msize) = top.first().filter(|(k, ..)| k == b"moov").context("not a moov box")?;
    let video = find(moov, mp + mhl, mp + msize, &[b"trak"])
        .into_iter()
        .find(|&(tp, thl, tsize)| {
            find(moov, tp + thl, tp + tsize, &[b"mdia", b"hdlr"])
                .first()
                .is_some_and(|&(hp, hhl, _)| &moov[hp + hhl + 8..hp + hhl + 12] == b"vide")
        })
        .context("no video track")?;
    let (tp, thl, tsize) = video;
    let mdia = *find(moov, tp + thl, tp + tsize, &[b"mdia"]).first().context("no mdia")?;
    let minf = *find(moov, tp + thl, tp + tsize, &[b"mdia", b"minf"]).first().context("no minf")?;
    let stbl = *find(moov, tp + thl, tp + tsize, &[b"mdia", b"minf", b"stbl"]).first().context("no stbl")?;
    let kids = boxes(moov, stbl.0 + stbl.1, stbl.0 + stbl.2);
    if kids.iter().any(|(k, ..)| k == b"ctts") {
        bail!("the video track already has a ctts box");
    }
    let &(_, sp, _, ssize) = kids.iter().find(|(k, ..)| k == b"stts").context("no stts")?;
    let n = be32(moov, sp + 12) as usize;
    let mut durs: Vec<u64> = Vec::with_capacity(pics);
    for k in 0..n {
        let count = be32(moov, sp + 16 + 8 * k) as usize;
        let d = be32(moov, sp + 20 + 8 * k) as u64;
        durs.extend(std::iter::repeat_n(d, count));
    }
    if durs.len() != pics {
        bail!("{} samples in the track but {pics} packets in the stream", durs.len());
    }
    let mut dts = Vec::with_capacity(pics);
    let mut t = 0u64;
    for d in &durs {
        dts.push(t);
        t += d;
    }
    let total = t;
    let (delay, offs) = offsets(&dts, picture, rank);
    if offs.iter().any(|&o| o > u32::MAX as u64) {
        bail!("a composition offset does not fit 32 bits");
    }
    let (ctts, runs) = ctts_box(&offs);

    let mut new = moov.to_vec();
    let ins = sp + ssize;
    new.splice(ins..ins, ctts.iter().copied());
    let grow = ctts.len();
    for at in [mp, tp, mdia.0, minf.0, stbl.0] {
        grow_box(&mut new, at, grow);
    }
    // Timescales: the media's (for the delay) and the movie's (durations).
    let mdhd = *find(moov, tp + thl, tp + tsize, &[b"mdia", b"mdhd"]).first().context("no mdhd")?;
    let mver = new[mdhd.0 + 8];
    let media_ts = be32(&new, mdhd.0 + if mver == 1 { 28 } else { 20 });
    let mvhd = *find(moov, mp + mhl, mp + msize, &[b"mvhd"]).first().context("no mvhd")?;
    let vver = new[mvhd.0 + 8];
    let movie_ts = be32(&new, mvhd.0 + if vver == 1 { 28 } else { 20 }) as u64;
    // The presentation now spans every frame: the edit, the track and (if
    // it is the longest) the movie say so, in the movie's timescale.
    let pres = (total * movie_ts).div_ceil(media_ts.max(1) as u64);
    if let Some(&(ep, _, _)) = find(moov, tp + thl, tp + tsize, &[b"edts", b"elst"]).first() {
        let ver = new[ep + 8];
        let count = be32(&new, ep + 12);
        if count != 1 {
            bail!("video edit list with {count} entries; not handled");
        }
        if ver == 1 {
            new[ep + 16..ep + 24].copy_from_slice(&pres.to_be_bytes());
            new[ep + 24..ep + 32].copy_from_slice(&(delay as i64).to_be_bytes());
        } else {
            new[ep + 16..ep + 20].copy_from_slice(&(pres as u32).to_be_bytes());
            new[ep + 20..ep + 24].copy_from_slice(&(delay as i32).to_be_bytes());
        }
    }
    let tkhd = *find(moov, tp + thl, tp + tsize, &[b"tkhd"]).first().context("no tkhd")?;
    if new[tkhd.0 + 8] == 1 {
        new[tkhd.0 + 36..tkhd.0 + 44].copy_from_slice(&pres.to_be_bytes());
    } else {
        new[tkhd.0 + 28..tkhd.0 + 32].copy_from_slice(&(pres as u32).to_be_bytes());
    }
    if vver == 1 {
        if pres > be64(&new, mvhd.0 + 32) {
            new[mvhd.0 + 32..mvhd.0 + 40].copy_from_slice(&pres.to_be_bytes());
        }
    } else if pres > be32(&new, mvhd.0 + 24) as u64 {
        new[mvhd.0 + 24..mvhd.0 + 28].copy_from_slice(&(pres as u32).to_be_bytes());
    }
    // Chunk offsets move only if the mdat sits after the (now larger) moov.
    if mdat_after_moov {
        let path_stco: [&[u8; 4]; 5] = [b"trak", b"mdia", b"minf", b"stbl", b"stco"];
        let path_co64: [&[u8; 4]; 5] = [b"trak", b"mdia", b"minf", b"stbl", b"co64"];
        for (p, _, _) in find(&new, mhl, new.len(), &path_stco) {
            let count = be32(&new, p + 12) as usize;
            for k in 0..count {
                let q = p + 16 + 4 * k;
                let v = be32(&new, q) + grow as u32;
                new[q..q + 4].copy_from_slice(&v.to_be_bytes());
            }
        }
        for (p, _, _) in find(&new, mhl, new.len(), &path_co64) {
            let count = be32(&new, p + 12) as usize;
            for k in 0..count {
                let q = p + 16 + 8 * k;
                let v = be64(&new, q) + grow as u64;
                new[q..q + 8].copy_from_slice(&v.to_be_bytes());
            }
        }
    }
    Ok((new, Repair { pictures: rank.len(), sequences: 0, delay, timescale: media_ts, runs }))
}

/// Restore the composition offsets of one file if it needs them.
pub fn repair_if_needed(ffmpeg: &str, ffprobe: &str, path: &Path, dry_run: bool) -> Result<Outcome> {
    let reorder = match check(ffprobe, path)? {
        Check::Fine(why) => return Ok(Outcome::Fine(why)),
        Check::Missing { reorder } => reorder,
    };
    if dry_run {
        return Ok(Outcome::WouldRepair { reorder });
    }
    let (log2_max, packets) = picture_order(ffmpeg, path)?;
    let picture: Vec<bool> = packets.iter().map(|p| p.is_some()).collect();
    let pics: Vec<Picture> = packets.iter().flatten().copied().collect();
    if pics.len() < packets.len() {
        tracing::debug!("{}: {} sample(s) carry no picture", path.display(), packets.len() - pics.len());
    }
    let (rank, sequences) = display_ranks(log2_max, &pics);

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let top = atoms_in(&mut file, 0, len)?;
    let moov = *top.iter().find(|a| a.kind == *b"moov").context("no moov")?;
    let mdat_after = top.iter().any(|a| a.kind == *b"mdat" && a.offset > moov.offset);
    if moov.size > 256 << 20 {
        bail!("moov of {} bytes; not handled", moov.size);
    }
    let mut moov_bytes = vec![0u8; moov.size as usize];
    file.seek(SeekFrom::Start(moov.offset))?;
    file.read_exact(&mut moov_bytes)?;
    let (new_moov, mut repair) = rebuild_moov(&moov_bytes, &picture, &rank, mdat_after)?;
    repair.sequences = sequences;

    // Write beside, byte for byte but for the moov.
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
    let temp = crate::remux::temp_beside(dir, &stem, "timing-tmp", ext);
    let written = (|| -> Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&temp)?);
        file.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut (&mut file).take(moov.offset), &mut out)?;
        out.write_all(&new_moov)?;
        file.seek(SeekFrom::Start(moov.offset + moov.size))?;
        std::io::copy(&mut file, &mut out)?;
        out.flush()?;
        // Best effort: a full sync is refused on some network mounts.
        let _ = out.get_ref().sync_all();
        Ok(())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(err.context("writing the repaired file"));
    }

    // Verify: same size but for the ctts, the offsets now in effect
    // (presentation times no longer equal decode times), the duration
    // unchanged, and ffprobe content with the sample table.
    let verified = (|| -> Result<()> {
        let new_len = std::fs::metadata(&temp)?.len();
        if new_len != len + (new_moov.len() as u64 - moov.size) {
            bail!("size {new_len}, expected {}", len + (new_moov.len() as u64 - moov.size));
        }
        let out = Command::new(ffprobe)
            .args(["-v", "error", "-select_streams", "v:0", "-read_intervals", "%+#48", "-show_entries", "packet=pts_time,dts_time", "-of", "csv=p=0"])
            .arg(&temp)
            .output()?;
        if !out.status.success() {
            bail!("ffprobe refused the result: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let packets: Vec<&str> = text.lines().collect();
        let differing = packets.iter().filter(|l| l.split(',').next() != l.split(',').nth(1)).count();
        if packets.len() < 8 || differing == 0 {
            bail!("the offsets are not in effect ({} packets, {differing} with pts != dts)", packets.len());
        }
        let duration = |p: &Path| -> Result<f64> {
            let out = Command::new(ffprobe)
                .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
                .arg(p)
                .output()?;
            Ok(String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0))
        };
        let (before, after) = (duration(path)?, duration(&temp)?);
        if (after - before).abs() > 1.0 + before * 0.01 {
            bail!("duration {before:.1} -> {after:.1}");
        }
        Ok(())
    })();
    if let Err(err) = verified {
        let _ = std::fs::remove_file(&temp);
        return Err(err.context("verification failed; original untouched"));
    }
    if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&temp) {
            let _ = f.set_modified(modified);
        }
    }
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(Outcome::Repaired(repair))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_follow_poc_within_each_sequence() {
        // IDR, then a typical B-pyramid: decode order I P B b b, POC lsb 0 4 2 1 3.
        let pics = [(19, 0), (1, 4), (1, 2), (0, 1), (0, 3), (19, 0), (1, 2), (0, 1)];
        let (rank, sequences) = display_ranks(8, &pics);
        assert_eq!(rank, vec![0, 4, 2, 1, 3, 5, 7, 6]);
        assert_eq!(sequences, 2);
        // POC lsb wrapping around the modulus (16 here) continues upward:
        // 0, 6, 12, then lsb 2 reads as 18 and lsb 4 as 20.
        let pics = [(19, 0), (1, 6), (1, 12), (1, 2), (1, 4)];
        let (rank, _) = display_ranks(4, &pics);
        assert_eq!(rank, vec![0, 1, 2, 3, 4]);
        // And a backward jump of less than half the range is a real step back.
        let pics = [(19, 0), (1, 8), (0, 4), (0, 6)];
        let (rank, _) = display_ranks(4, &pics);
        assert_eq!(rank, vec![0, 3, 1, 2]);
    }

    #[test]
    fn offsets_delay_just_enough() {
        let dts = [0, 10, 20, 30, 40];
        let rank = [0, 3, 1, 2, 4];
        let (delay, offs) = offsets(&dts, &[true; 5], &rank);
        // Frame 2 (rank 1) shows at 10 + delay but is decoded at 20 → delay ≥ 10.
        assert_eq!(delay, 10);
        assert_eq!(offs, vec![10, 30, 0, 0, 10]);
        // A trailing sample with no picture (an end-of-sequence marker)
        // is left out of the ranking and keeps offset 0.
        let dts = [0, 10, 20, 30, 40, 50];
        let (delay, offs) = offsets(&dts, &[true, true, true, true, true, false], &rank);
        assert_eq!(delay, 10);
        assert_eq!(offs, vec![10, 30, 0, 0, 10, 0]);
        // One in the middle: the pictures around it keep their ranks, and
        // the delay grows to cover the gap (picture 2, decoded at 30, is
        // shown second, at 10 + delay).
        let (delay, offs) = offsets(&[0, 10, 20, 30, 40, 50], &[true, true, false, true, true, true], &rank);
        assert_eq!(delay, 20);
        assert_eq!(offs, vec![20, 50, 0, 0, 10, 20]);
        let (ctts, runs) = ctts_box(&offs);
        assert_eq!(runs, 5, "20 | 50 | 0 0 | 10 | 20");
        assert_eq!(&ctts[4..8], b"ctts");
        assert_eq!(u32::from_be_bytes(ctts[0..4].try_into().unwrap()) as usize, ctts.len());
    }

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn rebuilds_the_moov_with_a_ctts_and_adjusted_offsets() {
        // mvhd v0: version/flags, ctime, mtime, timescale 1000, duration 40.
        let mut mvhd = vec![0u8; 4 + 4 + 4];
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&40u32.to_be_bytes());
        mvhd.extend_from_slice(&[0; 80]);
        // tkhd v0: version/flags, ctime, mtime, track id, reserved, duration.
        let mut tkhd = vec![0u8; 4 + 4 + 4 + 4 + 4];
        tkhd.extend_from_slice(&40u32.to_be_bytes());
        tkhd.extend_from_slice(&[0; 60]);
        // elst v0: one entry, segment_duration 40, media_time 0, rate 1.
        let mut elst = vec![0u8; 4];
        elst.extend_from_slice(&1u32.to_be_bytes());
        elst.extend_from_slice(&40u32.to_be_bytes());
        elst.extend_from_slice(&0i32.to_be_bytes());
        elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        // mdhd v0: version/flags, ctime, mtime, timescale 100, duration 50.
        let mut mdhd = vec![0u8; 4 + 4 + 4];
        mdhd.extend_from_slice(&100u32.to_be_bytes());
        mdhd.extend_from_slice(&50u32.to_be_bytes());
        mdhd.extend_from_slice(&[0; 4]);
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 13]);
        // stts: 5 samples of 10 ticks.
        let mut stts = vec![0u8; 4];
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&5u32.to_be_bytes());
        stts.extend_from_slice(&10u32.to_be_bytes());
        // stco: one chunk at 1000.
        let mut stco = vec![0u8; 4];
        stco.extend_from_slice(&1u32.to_be_bytes());
        stco.extend_from_slice(&1000u32.to_be_bytes());
        let stbl = bx(b"stbl", &[bx(b"stts", &stts), bx(b"stco", &stco)].concat());
        let minf = bx(b"minf", &stbl);
        let mdia = bx(b"mdia", &[bx(b"mdhd", &mdhd), bx(b"hdlr", &hdlr), minf].concat());
        let trak = bx(b"trak", &[bx(b"tkhd", &tkhd), bx(b"edts", &bx(b"elst", &elst)), mdia].concat());
        let moov = bx(b"moov", &[bx(b"mvhd", &mvhd), trak].concat());

        let rank = [0, 3, 1, 2, 4];
        let (new, repair) = rebuild_moov(&moov, &[true; 5], &rank, true).unwrap();
        assert_eq!(repair.delay, 10);
        assert_eq!(repair.runs, 4);
        assert_eq!(new.len(), moov.len() + 8 + 8 + 8 * 4);
        assert_eq!(be32(&new, 0) as usize, new.len(), "moov size grew");
        let stbl = find(&new, 8, new.len(), &[b"trak", b"mdia", b"minf", b"stbl"])[0];
        let kids: Vec<[u8; 4]> = boxes(&new, stbl.0 + stbl.1, stbl.0 + stbl.2).iter().map(|k| k.0).collect();
        assert_eq!(kids, vec![*b"stts", *b"ctts", *b"stco"]);
        let stco = find(&new, 8, new.len(), &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])[0];
        assert_eq!(be32(&new, stco.0 + 16), 1000 + 48, "chunk offsets moved by the ctts size");
        let elst = find(&new, 8, new.len(), &[b"trak", b"edts", b"elst"])[0];
        assert_eq!(be32(&new, elst.0 + 20), 10, "media_time = delay");
        assert_eq!(be32(&new, elst.0 + 16), 500, "50 media ticks at 100/s = 500 movie ticks at 1000/s");
        let tkhd = find(&new, 8, new.len(), &[b"trak", b"tkhd"])[0];
        assert_eq!(be32(&new, tkhd.0 + 28), 500);
        let mvhd = find(&new, 8, new.len(), &[b"mvhd"])[0];
        assert_eq!(be32(&new, mvhd.0 + 24), 500, "the movie grows to the longest track");
        // Done once is refused.
        assert!(rebuild_moov(&new, &[true; 5], &rank, true).is_err());
        assert!(rebuild_moov(&moov, &[true; 4], &rank[..4], true).is_err(), "sample count must match the packets");
        // A moov before an mdat leaves chunk offsets alone.
        let (new2, _) = rebuild_moov(&moov, &[true; 5], &rank, false).unwrap();
        let stco = find(&new2, 8, new2.len(), &[b"trak", b"mdia", b"minf", b"stbl", b"stco"])[0];
        assert_eq!(be32(&new2, stco.0 + 16), 1000);
    }
}
