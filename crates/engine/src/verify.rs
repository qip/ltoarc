//! Read a cartridge back and check it against the catalog: block and record checksums, chunk
//! BLAKE3 hashes, file content hashes, chunk positions, and the volume catalog stream.

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context, Result};
use ltoarc_catalog::{Catalog, VolumeRow};
use ltoarc_device::TapeDrive;
use ltoarc_format::record::{
    codec, decode_pack, flags, from_cbor, rtype, ChunkHeader, FileEnd, FileMeta, SessionBegin, CHUNK_HEADER_LEN,
};
use ltoarc_format::stream::StreamReader;

use crate::crypt::{self, Key};
use crate::hash::{Algo, HashPool};
use crate::readahead::{Blocks, Extent, ReadAhead, ReadStats};
use crate::ringbuf::Pace;
use crate::tapeio::read_label;

#[derive(Debug, Default)]
pub struct Report {
    pub sessions: u64,
    pub files: u64,
    pub chunks: u64,
    pub bytes: u64,
    pub file_hashes: u64,
    pub catalogs: u64,
    pub errors: Vec<String>,
    /// Chunks the catalog places on this volume that were not found where expected.
    pub missing: u64,
    /// How the tape was read: speed, pacing, locates, stops.
    pub read: ReadStats,
}

struct FileState {
    path: String,
    hashed_to: u64,
    /// Content hash algorithms to check, and the file's id in the hash pool.
    algos: Vec<Algo>,
    pid: u64,
}

/// Hash pool, ids for files fed to it, and the digests to compare at the end:
/// (pool id, path, (profile, algorithm, digest) recorded on tape).
#[derive(Default)]
struct Hashing {
    pool: HashPool,
    next: u64,
    pending: Vec<(u64, String, Vec<Digest>)>,
}

/// (profile, algorithm, hex digest).
type Digest = (String, String, String);

/// Encrypted sessions need their key among `keys`.
pub fn verify_volume(cat: &Catalog, drive: &mut dyn TapeDrive, volume: &VolumeRow, keys: &[Key]) -> Result<Report> {
    verify_volume_paced(cat, drive, volume, keys, None)
}

/// As `verify_volume`, matching the drive's speed to the checks within `pace` (the drive's
/// speed-matching range) instead of reading at full speed.
pub fn verify_volume_paced(cat: &Catalog, drive: &mut dyn TapeDrive, volume: &VolumeRow, keys: &[Key], pace: Option<Pace>) -> Result<Report> {
    let label = read_label(drive)?;
    crate::backup::check_label(&label, volume)?;
    let mut report = Report::default();
    // Several packed files share one position.
    let mut expected: HashMap<(i64, i64), Vec<Vec<u8>>> = HashMap::new();
    for (p, c, _) in cat.volume_placements(volume.id)? {
        expected.entry((p.block, p.offset)).or_default().push(c.blake3);
    }
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let bs = label.block_size as usize;
    // Read every session stream and its catalog through the read-ahead thread; they lie
    // one after the other, so the drive streams through the cartridge.
    let svs = cat.volume_sessions(volume.id)?;
    let mut extents = Vec::new();
    for sv in &svs {
        let partition = sv.partition as u8;
        extents.push(Extent { partition, start: sv.start_object as u64, end: sv.end_object.map(|e| e as u64) });
        if let Some(c) = sv.catalog_object {
            extents.push(Extent { partition, start: c as u64, end: None });
        }
    }
    let ra = ReadAhead::paced(crate::restore::default_read_buffer(), pace, None);
    let mut hashing = Hashing::default();
    let (stats, result) = std::thread::scope(|s| {
        let reader = s.spawn(|| ra.read(drive, bs, &extents));
        let r = check_sessions(cat, &ra, svs, keys, &mut report, &mut expected, &mut seen, &mut hashing);
        ra.cancel();
        (reader.join(), r)
    });
    let stats = stats.map_err(|_| anyhow!("tape reader thread panicked"))?;
    result?;
    report.read = stats;
    let digests = hashing.pool.finish();
    for (pid, path, hashes) in hashing.pending {
        let got = digests.get(&pid).cloned().unwrap_or_default();
        for (profile, algo, digest) in &hashes {
            if let Some((_, g)) = got.iter().find(|(a, _)| a == algo) {
                if g == digest {
                    report.file_hashes += 1;
                } else {
                    report.errors.push(format!("{path}: {profile} hash mismatch on tape"));
                }
            }
        }
    }
    report.missing = expected.values().map(|v| v.len() as u64).sum();
    for ((block, offset), _) in expected.iter().take(10) {
        report.errors.push(format!("catalog places a chunk at block {block} offset {offset}, but it was not found there"));
    }
    Ok(report)
}

/// Check the session streams `svs` (and their catalogs) as the reader delivers them.
#[allow(clippy::too_many_arguments)]
fn check_sessions(
    cat: &Catalog,
    ra: &ReadAhead,
    svs: Vec<ltoarc_catalog::SessionVolume>,
    keys: &[Key],
    report: &mut Report,
    expected: &mut HashMap<(i64, i64), Vec<Vec<u8>>>,
    seen: &mut HashSet<(i64, i64)>,
    hashing: &mut Hashing,
) -> Result<()> {
    for sv in svs {
        let session = cat
            .sessions()?
            .into_iter()
            .find(|s| s.id == sv.session_id)
            .ok_or_else(|| anyhow!("session {} missing", sv.session_id))?;
        report.sessions += 1;
        let dek = session
            .encryption
            .as_deref()
            .map(|e| crypt::unwrap(keys, &crypt::from_json(e)?))
            .transpose()
            .with_context(|| format!("session {}", session.uuid))?;
        let start = sv.start_object;
        let mut src = Blocks::new(ra);
        if !src.skip_to(start as u64) {
            let why = src.error.take().map(|e| format!(": {e}")).unwrap_or_default();
            report.errors.push(format!("session {}: block {start} could not be read{why}", session.uuid));
            continue;
        }
        let mut rd = StreamReader::at(src, 0)?;
        let mut files: HashMap<u64, FileState> = HashMap::new();
        loop {
            let rec = match rd.next_record() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                // A recovered session ends where the crash cut it, often inside a record.
                Err(_) if rd.ended() && session.status != "complete" => break,
                Err(e) => {
                    report.errors.push(format!("session {}: {e} near block {:?}", session.uuid, rd.position()));
                    if rd.resync().is_err() || rd.ended() {
                        break;
                    }
                    continue;
                }
            };
            let body = match crypt::record_body(dek.as_ref(), rec.header.rtype, rec.header.flags, &rec.body) {
                Ok(b) => b,
                Err(e) => {
                    report.errors.push(format!("session {}: record at block {}: {e}", session.uuid, start + rec.pos.block_seq as i64));
                    continue;
                }
            };
            match rec.header.rtype {
                rtype::SESSION_BEGIN => {
                    let b: SessionBegin = from_cbor(&body)?;
                    if b.session.to_string() != session.uuid {
                        report.errors.push(format!("block {start}: session {} found where {} was expected", b.session, session.uuid));
                    }
                }
                rtype::FILE => {
                    let m: FileMeta = from_cbor(&body)?;
                    // A file continued from the previous cartridge, or stored by reference to
                    // data of an earlier session: its hash covers data this stream does not
                    // hold, so only chunks are checked here (restore checks the whole file).
                    if rec.header.flags & flags::CONTINUED != 0 || m.same_as.is_some() {
                        let path = String::from_utf8_lossy(&m.path).into_owned();
                        files.insert(m.id, FileState { path, hashed_to: 0, algos: Vec::new(), pid: 0 });
                        continue;
                    }
                    report.files += 1;
                    let hashes = match cat.object_by_seq(session.id, m.id as i64)? {
                        Some(o) => cat.hashes(o.id)?,
                        None => {
                            report.errors.push(format!("{}: on tape but not in the catalog", String::from_utf8_lossy(&m.path)));
                            Vec::new()
                        }
                    };
                    let mut algos: Vec<Algo> = hashes.iter().filter(|h| h.kind == "content").filter_map(|h| Algo::parse(&h.algo)).collect();
                    algos.sort();
                    algos.dedup();
                    hashing.next += 1;
                    files.insert(m.id, FileState { path: String::from_utf8_lossy(&m.path).into_owned(), hashed_to: 0, algos, pid: hashing.next });
                }
                rtype::CHUNK => {
                    let h = ChunkHeader::decode(&body)?;
                    let stored = &body[CHUNK_HEADER_LEN..];
                    let data = match h.codec {
                        codec::STORED => stored.to_vec(),
                        codec::ZSTD => zstd::bulk::decompress(stored, h.raw_len as usize)?,
                        other => {
                            report.errors.push(format!("unknown codec {other}"));
                            continue;
                        }
                    };
                    report.chunks += 1;
                    report.bytes += data.len() as u64;
                    let block = start + rec.pos.block_seq as i64;
                    let key = (block, rec.pos.offset as i64);
                    let name = files.get(&h.object).map_or("?".to_string(), |f| f.path.clone());
                    if blake3::hash(&data).as_bytes() != &h.blake3 {
                        report.errors.push(format!("{name}: chunk {} at block {block} is corrupt", h.seq));
                    }
                    match expected.remove(&key) {
                        Some(b) if b.len() == 1 && b[0] == h.blake3 => {
                            seen.insert(key);
                        }
                        Some(_) => report.errors.push(format!("{name}: chunk at block {block} differs from the catalog")),
                        None => report.errors.push(format!("{name}: chunk at block {block} is not in the catalog")),
                    }
                    if let Some(f) = files.get_mut(&h.object) {
                        hashing.pool.zeros(f.pid, &f.algos, h.offset.saturating_sub(f.hashed_to));
                        f.hashed_to = h.offset + data.len() as u64;
                        hashing.pool.data(f.pid, &f.algos, &std::sync::Arc::new(data));
                    }
                }
                rtype::PACK => {
                    let (h, members, stored) = decode_pack(&body)?;
                    let raw = match h.codec {
                        codec::STORED => stored.to_vec(),
                        codec::ZSTD => zstd::bulk::decompress(stored, h.raw_len as usize)?,
                        other => {
                            report.errors.push(format!("unknown codec {other}"));
                            continue;
                        }
                    };
                    report.chunks += 1;
                    report.bytes += raw.len() as u64;
                    let block = start + rec.pos.block_seq as i64;
                    if blake3::hash(&raw).as_bytes() != &h.blake3 {
                        report.errors.push(format!("pack at block {block} is corrupt"));
                    }
                    let mut want = expected.remove(&(block, rec.pos.offset as i64)).unwrap_or_default();
                    for m in &members {
                        let name = files.get(&m.object).map_or("?".to_string(), |f| f.path.clone());
                        let Some(data) = raw.get(m.offset as usize..(m.offset + m.len) as usize) else {
                            report.errors.push(format!("{name}: outside its pack at block {block}"));
                            continue;
                        };
                        if blake3::hash(data).as_bytes().as_slice() != m.blake3.as_slice() {
                            report.errors.push(format!("{name}: packed data at block {block} is corrupt"));
                        }
                        match want.iter().position(|b| *b == m.blake3) {
                            Some(i) => {
                                want.swap_remove(i);
                            }
                            None => report.errors.push(format!("{name}: packed at block {block} but not in the catalog")),
                        }
                        if let Some(f) = files.get_mut(&m.object) {
                            if !f.algos.is_empty() {
                                hashing.pool.data(f.pid, &f.algos, &std::sync::Arc::new(data.to_vec()));
                            }
                            f.hashed_to = m.len;
                        }
                    }
                    if want.is_empty() {
                        seen.insert((block, rec.pos.offset as i64));
                    } else {
                        expected.insert((block, rec.pos.offset as i64), want);
                    }
                }
                rtype::FILE_END => {
                    let e: FileEnd = from_cbor(&body)?;
                    if let Some(f) = files.remove(&e.id) {
                        if (e.status == "ok" || e.status == "changed") && !f.algos.is_empty() {
                            hashing.pool.zeros(f.pid, &f.algos, e.size.saturating_sub(f.hashed_to));
                            hashing.pool.end(f.pid, &f.algos);
                            hashing.pending.push((f.pid, f.path, e.hashes));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(c) = sv.catalog_object {
            let mut src = Blocks::new(ra);
            if !src.skip_to(c as u64) {
                report.errors.push(format!("catalog at block {c} could not be read"));
                continue;
            }
            let mut rd = StreamReader::at(src, 0)?;
            match rd.next_record() {
                Ok(Some(r)) if r.header.rtype == rtype::CATALOG => match catalog_db(keys, &r) {
                    Ok(db) if db.starts_with(b"SQLite format 3\0") => report.catalogs += 1,
                    Ok(_) => report.errors.push(format!("catalog at block {c} is not an SQLite database")),
                    Err(e) => report.errors.push(format!("catalog at block {c}: {e}")),
                },
                Ok(_) => report.errors.push(format!("no catalog record at block {c}")),
                Err(e) => report.errors.push(format!("catalog at block {c}: {e}")),
            }
        }
    }
    Ok(())
}

/// The SQLite database in a CATALOG record (decrypted when it is encrypted).
fn catalog_db(keys: &[Key], r: &ltoarc_format::stream::Record) -> Result<Vec<u8>> {
    let z = if r.header.flags & ltoarc_format::record::flags::ENCRYPTED != 0 {
        crypt::open_catalog(keys, &r.body)?
    } else {
        r.body.clone()
    };
    Ok(zstd::stream::decode_all(&z[..])?)
}

