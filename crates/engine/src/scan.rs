//! Rebuild catalog entries from what is on tape, without using any catalog: every FILE record
//! carries full metadata and every CHUNK record its own position.
//!
//! A session spanning cartridges is rebuilt one cartridge at a time, in any order: a stream
//! whose session is already known adds its part to it, matching files by their record id
//! (a CONTINUED FILE record names the file whose data carries on from the previous one).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use ltoarc_catalog::{Catalog, ChunkRow, HashRow, ObjectRow, Placement, SessionRow, SessionVolume, VolumeRow};
use ltoarc_device::{Error as DevError, Position, ReadStatus, TapeDrive};
use ltoarc_format::record::{
    codec, decode_pack, flags, from_cbor, rtype, to_cbor, ChunkHeader, FileEnd, FileMeta, SessionBegin, SessionEnd,
    CHUNK_HEADER_LEN,
};
use ltoarc_format::stream::StreamReader;
use ltoarc_format::{parse_block, BlockKind, VolumeLabel};

use crate::crypt::{self, Key};
use crate::tapeio::{read_label, TapeBlocks};

#[derive(Debug, Default)]
pub struct Report {
    pub label: Option<VolumeLabel>,
    pub added: Vec<String>,
    pub skipped: Vec<String>,
    pub files: u64,
    pub chunks: u64,
    pub bytes: u64,
    pub catalogs: u64,
    pub foreign_files: u64,
    /// The cartridge has the bootstrap file (FORMAT.md §5).
    pub bootstrap: bool,
    pub errors: Vec<String>,
}

/// Scan the whole cartridge. Sessions already in the catalog are skipped unless `replace`.
/// Encrypted sessions need the key they were written with among `keys`.
pub fn scan_volume(cat: &Catalog, drive: &mut dyn TapeDrive, volume: &VolumeRow, replace: bool, keys: &[Key]) -> Result<Report> {
    let label = read_label(drive)?;
    let mut report = Report { label: Some(label.clone()), ..Default::default() };
    let mut v = volume.clone();
    if v.uuid.is_none() {
        v.uuid = Some(label.volume_uuid.to_string());
        v.generation = Some(label.generation.clone());
        v.pool = label.pool.clone();
        if v.state == "new" || v.state == "blank" || v.state == "foreign" {
            v.state = "scratch".into();
        }
        v.block_size = Some(label.block_size as i64);
        cat.update_volume(&v)?;
    } else {
        crate::backup::check_label(&label, volume)?;
        if v.block_size != Some(label.block_size as i64) {
            v.block_size = Some(label.block_size as i64);
            cat.update_volume(&v)?;
        }
    }
    let bs = label.block_size as usize;
    // Position after the label and its filemark.
    drive.locate(Position { partition: 0, object: 0 })?;
    drive.space_filemarks(1)?;
    let mut last_session: Option<SessionVolume> = None;
    let mut buf = vec![0u8; bs];
    loop {
        let file_start = drive.position()?.pos.object as i64;
        let first = match drive.read_block(&mut buf) {
            Ok(ReadStatus::Data(n)) => buf[..n].to_vec(),
            Ok(ReadStatus::Filemark) => continue,
            Ok(ReadStatus::EndOfData) | Err(DevError::EndOfData) => break,
            Err(e) => return Err(e).context(format!("reading block {file_start}")),
        };
        let kind = parse_block(&first).ok().map(|(h, _)| h.kind);
        match kind {
            Some(BlockKind::Data) => {
                let src = TapeBlocks::new(drive, bs).with_pending(first);
                let mut rd = StreamReader::new(src)?;
                match scan_session(cat, &mut rd, v.id, file_start, replace, keys, &mut report) {
                    Ok(sv) => last_session = sv,
                    Err(e) => report.errors.push(format!("session at block {file_start}: {e:#}")),
                }
                let src = rd.into_source();
                if !src.hit_filemark && !src.hit_eod {
                    src.drive.space_filemarks(1)?;
                }
            }
            Some(BlockKind::Catalog) => {
                report.catalogs += 1;
                if let Some(mut sv) = last_session.take() {
                    sv.catalog_object = Some(file_start);
                    cat.add_session_volume(&sv)?;
                }
                drive.space_filemarks(1)?;
            }
            None if crate::bootstrap::is_tar(&first) => {
                report.bootstrap = true;
                drive.space_filemarks(1)?;
            }
            Some(BlockKind::Label) | None => {
                report.foreign_files += 1;
                drive.space_filemarks(1)?;
            }
        }
    }
    Ok(report)
}

/// Scan the one session stream starting at `start` (crash recovery). None if nothing there,
/// or the session part is already in the catalog.
pub fn scan_stream_at(
    cat: &Catalog,
    drive: &mut dyn TapeDrive,
    volume: &VolumeRow,
    start: u64,
    keys: &[Key],
    report: &mut Report,
) -> Result<Option<SessionVolume>> {
    let label = read_label(drive)?;
    let bs = label.block_size as usize;
    drive.locate(Position { partition: 0, object: start })?;
    let mut buf = vec![0u8; bs];
    let first = match drive.read_block(&mut buf) {
        Ok(ReadStatus::Data(n)) => buf[..n].to_vec(),
        Ok(_) | Err(DevError::EndOfData) => return Ok(None),
        Err(e) => return Err(e).context(format!("reading block {start}")),
    };
    let mut rd = StreamReader::new(TapeBlocks::new(drive, bs).with_pending(first))?;
    scan_session(cat, &mut rd, volume.id, start as i64, false, keys, report)
}

fn scan_session(
    cat: &Catalog,
    rd: &mut StreamReader<TapeBlocks<'_>>,
    volume_id: i64,
    start: i64,
    replace: bool,
    keys: &[Key],
    report: &mut Report,
) -> Result<Option<SessionVolume>> {
    let first = rd.next_record()?.context("empty session stream")?;
    if first.header.rtype != rtype::SESSION_BEGIN {
        anyhow::bail!("stream does not start with SESSION_BEGIN");
    }
    let begin: SessionBegin = from_cbor(&first.body)?;
    let uuid = begin.session.to_string();
    let dek = begin.encryption.as_ref().map(|w| crypt::unwrap(keys, w)).transpose().with_context(|| format!("session {uuid}"))?;
    // A session already known from another cartridge gets this cartridge's part added.
    let mut known = None;
    if let Some(existing) = cat.session(&uuid)? {
        let parts = cat.session_volumes(existing.id)?;
        let here = parts.iter().any(|sv| sv.volume_id == volume_id && sv.copy == begin.copy as i64 && sv.start_object == start);
        if here && !replace {
            report.skipped.push(uuid);
            return Ok(None);
        }
        if here {
            if parts.iter().any(|sv| sv.volume_id != volume_id || sv.copy != begin.copy as i64) {
                anyhow::bail!("session {uuid} spans other cartridges too; --replace only re-reads single-cartridge sessions");
            }
            cat.delete_session(existing.id)?;
        } else {
            known = Some(existing);
        }
    }
    cat.begin()?;
    let r = (|| -> Result<SessionVolume> {
        let session_id = match &known {
            Some(s) => s.id,
            None => cat.create_session(&SessionRow {
                uuid: uuid.clone(),
                job: begin.job.clone(),
                level: begin.level.clone(),
                started: begin.started,
                status: "partial".into(),
                host: begin.host.clone(),
                sources: serde_json::to_string(
                    &begin.sources.iter().map(|s| String::from_utf8_lossy(s).into_owned()).collect::<Vec<_>>(),
                )?,
                stats: Some(r#"{"recovered_by":"scan"}"#.into()),
                encryption: begin.encryption.as_ref().map(crypt::to_json),
                ..Default::default()
            })?,
        };
        let mut objects: HashMap<u64, i64> = HashMap::new();
        // Files without FILE_END yet, and how much of each this stream holds.
        let mut open: HashSet<u64> = HashSet::new();
        let mut held: HashMap<u64, u64> = HashMap::new();
        let (mut files, mut bytes, mut stored) = (0i64, 0i64, 0i64);
        let mut end: Option<SessionEnd> = None;
        let mut continues = false;
        loop {
            let rec = match rd.next_record() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    report.errors.push(format!("session {uuid}: {e}; resynchronising"));
                    rd.resync()?;
                    if rd.ended() {
                        break;
                    }
                    continue;
                }
            };
            let body = match crypt::record_body(dek.as_ref(), rec.header.rtype, rec.header.flags, &rec.body) {
                Ok(b) => b,
                Err(e) => {
                    report.errors.push(format!("session {uuid}: record at block {}: {e}", start + rec.pos.block_seq as i64));
                    continue;
                }
            };
            match rec.header.rtype {
                rtype::FILE => {
                    let m: FileMeta = from_cbor(&body)?;
                    if m.kind.as_str() == "file" {
                        open.insert(m.id);
                    }
                    // Known from another cartridge (always so for a CONTINUED record when the
                    // previous cartridge was read first).
                    if known.is_some() || rec.header.flags & flags::CONTINUED != 0 {
                        if let Some(o) = cat.object_by_seq(session_id, m.id as i64)? {
                            objects.insert(m.id, o.id);
                            continue;
                        }
                    }
                    let path_id = cat.intern_path(&m.path)?;
                    // A file stored by reference: its data is that of a file of an earlier
                    // session, which has to be in the catalog (scan that cartridge first).
                    let (data_from, status) = match &m.same_as {
                        Some(r) => match cat.session(&r.session.to_string())? {
                            Some(s) => match cat.object_by_seq(s.id, r.id as i64)? {
                                Some(o) => (Some(o.data_object()), "ok"),
                                None => (None, "unresolved"),
                            },
                            None => (None, "unresolved"),
                        },
                        None => (None, "ok"),
                    };
                    if status == "unresolved" {
                        report.errors.push(format!(
                            "{}: its data is that of file {} of session {}, which this catalog does not have; scan the cartridges of that session, then this one again",
                            String::from_utf8_lossy(&m.path),
                            m.same_as.as_ref().map_or(0, |r| r.id),
                            m.same_as.as_ref().map(|r| r.session.to_string()).unwrap_or_default()
                        ));
                    }
                    let oid = cat.insert_object(&ObjectRow {
                        id: 0,
                        session_id,
                        path_id,
                        seq: m.id as i64,
                        kind: m.kind.as_str().into(),
                        mode: m.mode as i64,
                        uid: m.uid as i64,
                        gid: m.gid as i64,
                        user: m.user.clone(),
                        grp: m.group.clone(),
                        size: m.size as i64,
                        mtime_ns: m.mtime_ns,
                        atime_ns: m.atime_ns,
                        ctime_ns: m.ctime_ns,
                        dev: m.dev as i64,
                        ino: m.ino as i64,
                        nlink: m.nlink as i64,
                        rdev: m.rdev as i64,
                        target: m.target.as_ref().map(|t| t.to_vec()),
                        xattrs: (!m.xattrs.is_empty()).then(|| to_cbor(&m.xattrs)),
                        extents: m.extents.as_ref().map(to_cbor),
                        status: status.into(),
                        data_from,
                    })?;
                    for (profile, digest) in &m.fingerprints {
                        cat.insert_hash(
                            oid,
                            &HashRow { profile: profile.clone(), kind: "fingerprint".into(), algo: profile.clone(), digest: digest.clone() },
                        )?;
                    }
                    objects.insert(m.id, oid);
                    if m.kind.as_str() == "file" {
                        files += 1;
                    }
                    report.files += 1;
                }
                rtype::CHUNK => {
                    let h = ChunkHeader::decode(&body)?;
                    let data_ok = {
                        let stored_bytes = &body[CHUNK_HEADER_LEN..];
                        let raw = match h.codec {
                            codec::ZSTD => zstd::bulk::decompress(stored_bytes, h.raw_len as usize).ok(),
                            _ => Some(stored_bytes.to_vec()),
                        };
                        raw.is_some_and(|d| blake3::hash(&d).as_bytes() == &h.blake3)
                    };
                    if !data_ok {
                        report.errors.push(format!("session {uuid}: corrupt chunk at block {}", start + rec.pos.block_seq as i64));
                    }
                    let Some(&oid) = objects.get(&h.object) else {
                        report.errors.push(format!("session {uuid}: chunk for unknown object {}", h.object));
                        continue;
                    };
                    let chunk_id = cat.insert_chunk(&ChunkRow {
                        id: 0,
                        object_id: oid,
                        seq: h.seq as i64,
                        offset: h.offset as i64,
                        raw_len: h.raw_len as i64,
                        stored_len: h.stored_len as i64,
                        codec: h.codec as i64,
                        blake3: h.blake3.to_vec(),
                        pack_offset: None,
                    })?;
                    cat.insert_placement(&Placement {
                        chunk_id,
                        copy: begin.copy as i64,
                        volume_id,
                        partition: 0,
                        block: start + rec.pos.block_seq as i64,
                        offset: rec.pos.offset as i64,
                        stored_len: None,
                    })?;
                    bytes += h.raw_len as i64;
                    stored += h.stored_len as i64;
                    report.chunks += 1;
                    report.bytes += h.raw_len;
                    let e = held.entry(h.object).or_default();
                    *e = (*e).max(h.offset + h.raw_len);
                }
                rtype::PACK => {
                    let (h, members, stored_bytes) = decode_pack(&body)?;
                    let raw = match h.codec {
                        codec::ZSTD => zstd::bulk::decompress(stored_bytes, h.raw_len as usize).ok(),
                        _ => Some(stored_bytes.to_vec()),
                    };
                    let block = start + rec.pos.block_seq as i64;
                    if !raw.as_ref().is_some_and(|d| blake3::hash(d).as_bytes() == &h.blake3) {
                        report.errors.push(format!("session {uuid}: corrupt pack at block {block}"));
                    }
                    for m in &members {
                        let Some(&oid) = objects.get(&m.object) else {
                            report.errors.push(format!("session {uuid}: packed data for unknown object {}", m.object));
                            continue;
                        };
                        let chunk_id = cat.insert_chunk(&ChunkRow {
                            id: 0,
                            object_id: oid,
                            seq: 0,
                            offset: 0,
                            raw_len: m.len as i64,
                            stored_len: h.stored_len as i64,
                            codec: h.codec as i64,
                            blake3: m.blake3.clone(),
                            pack_offset: Some(m.offset as i64),
                        })?;
                        cat.insert_placement(&Placement {
                            chunk_id,
                            copy: begin.copy as i64,
                            volume_id,
                            partition: 0,
                            block,
                            offset: rec.pos.offset as i64,
                            stored_len: None,
                        })?;
                        held.insert(m.object, m.len);
                    }
                    bytes += h.raw_len as i64;
                    stored += h.stored_len as i64;
                    report.chunks += 1;
                    report.bytes += h.raw_len;
                }
                rtype::FILE_END => {
                    let e: FileEnd = from_cbor(&body)?;
                    open.remove(&e.id);
                    if let Some(&oid) = objects.get(&e.id) {
                        for (profile, algo, digest) in &e.hashes {
                            cat.insert_hash(
                                oid,
                                &HashRow { profile: profile.clone(), kind: "content".into(), algo: algo.clone(), digest: digest.clone() },
                            )?;
                        }
                        // Stream sources learn their size only at the end.
                        if e.status != "ok" || cat.object(oid)?.is_some_and(|o| o.size != e.size as i64) {
                            cat.set_object_status(oid, &e.status, e.size as i64)?;
                        }
                        if !e.xattrs.is_empty() {
                            let base: Vec<(serde_bytes::ByteBuf, serde_bytes::ByteBuf)> = match cat.object(oid)?.and_then(|o| o.xattrs) {
                                Some(c) => from_cbor(&c)?,
                                None => Vec::new(),
                            };
                            let mut merged: Vec<_> = base.into_iter().filter(|(n, _)| !e.xattrs.iter().any(|(x, _)| x == n)).collect();
                            merged.extend(e.xattrs.iter().cloned());
                            cat.set_object_xattrs(oid, Some(&to_cbor(&merged)))?;
                        }
                    }
                }
                rtype::SESSION_END => end = Some(from_cbor(&body)?),
                rtype::VOLUME_END => continues = true,
                _ => {}
            }
        }
        // A stream cut short (crash) leaves the files it was writing incomplete; a file still
        // open at VOLUME_END carries on on the next cartridge.
        if end.is_none() && !continues {
            for id in &open {
                if let Some(&oid) = objects.get(id) {
                    cat.set_object_status(oid, "truncated", held.get(id).copied().unwrap_or(0) as i64)?;
                }
            }
        }
        let blocks = rd.source_mut().blocks as i64;
        let (base, status, ended) = match &known {
            Some(s) => ((s.files, s.bytes, s.stored_bytes), s.status.clone(), s.ended),
            None => ((0, 0, 0), "partial".to_string(), None),
        };
        cat.update_session(&SessionRow {
            id: session_id,
            ended: end.as_ref().map(|e| e.ended).or(ended),
            status: end.as_ref().map_or(status, |e| e.status.clone()),
            files: base.0 + files,
            bytes: base.1 + bytes,
            stored_bytes: base.2 + stored,
            stats: Some(r#"{"recovered_by":"scan"}"#.into()),
            ..Default::default()
        })?;
        let sv = SessionVolume {
            session_id,
            copy: begin.copy as i64,
            seq: begin.volume_seq as i64,
            volume_id,
            partition: 0,
            start_object: start,
            end_object: Some(start + blocks),
            catalog_object: None,
            verified_at: None,
        };
        cat.add_session_volume(&sv)?;
        Ok(sv)
    })();
    match r {
        Ok(sv) => {
            cat.commit()?;
            report.added.push(uuid);
            Ok(Some(sv))
        }
        Err(e) => {
            let _ = cat.rollback();
            Err(e)
        }
    }
}
