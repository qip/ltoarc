//! A later copy of a session made from the source disks instead of from tape (a deferred
//! copy): the session's records are written again to another tape set, the data read back
//! from the original files and checked, chunk by chunk, against the BLAKE3 recorded when the
//! session was written. The new copy's records cut the data exactly as the first copy's did
//! (same chunks, same packs), so the new placements join the session's existing chunks.
//!
//! A file that has changed since cannot give its original data: its chunks get no placement
//! on the new copy (restores of it use the other copies) and it is reported.

use std::fs::File;
use std::os::unix::fs::FileExt;

use super::copy::CopyReport;
use super::*;
use ltoarc_format::record::from_cbor;

pub struct FromSourceOptions {
    pub ring: ringbuf::Config,
    pub keys: Vec<Key>,
    /// zstd level for records the first copy compressed (default 3).
    pub zstd_level: Option<i32>,
}

/// The FILE record of a catalogued entry.
fn file_meta(cat: &Catalog, o: &ObjectRow, path: &[u8], hashes: &[HashRow], sessions: &HashMap<i64, String>) -> Result<FileMeta> {
    let kind = FileKind::parse(&o.kind).ok_or_else(|| anyhow!("unknown kind {}", o.kind))?;
    let same_as = match o.data_from {
        Some(d) => {
            let h = cat.object(d)?.ok_or_else(|| anyhow!("{}: its data's file is missing from the catalog", String::from_utf8_lossy(path)))?;
            let content = hashes.iter().find(|x| x.kind == "content");
            Some(SameAs {
                session: sessions.get(&h.session_id).and_then(|u| u.parse().ok()).ok_or_else(|| anyhow!("session {} missing", h.session_id))?,
                id: h.seq as u64,
                algo: content.map(|x| x.algo.clone()).unwrap_or_default(),
                digest: content.map(|x| x.digest.clone()).unwrap_or_default(),
            })
        }
        None => None,
    };
    Ok(FileMeta {
        id: o.seq as u64,
        path: ByteBuf::from(path.to_vec()),
        kind,
        mode: o.mode as u32,
        uid: o.uid as u32,
        gid: o.gid as u32,
        user: o.user.clone(),
        group: o.grp.clone(),
        size: o.size as u64,
        mtime_ns: o.mtime_ns,
        atime_ns: o.atime_ns,
        ctime_ns: o.ctime_ns,
        dev: o.dev as u64,
        ino: o.ino as u64,
        nlink: o.nlink as u64,
        rdev: o.rdev as u64,
        target: o.target.clone().map(ByteBuf::from),
        xattrs: o.xattrs.as_deref().map(from_cbor).transpose()?.unwrap_or_default(),
        extents: o.extents.as_deref().map(from_cbor).transpose()?,
        fingerprints: hashes.iter().filter(|h| h.kind == "fingerprint").map(|h| (h.profile.clone(), h.digest.clone())).collect(),
        same_as,
    })
}

fn file_end(o: &ObjectRow, hashes: &[HashRow]) -> FileEnd {
    FileEnd {
        id: o.seq as u64,
        size: o.size as u64,
        status: o.status.clone(),
        hashes: hashes.iter().filter(|h| h.kind == "content").map(|h| (h.profile.clone(), h.algo.clone(), h.digest.clone())).collect(),
        xattrs: Vec::new(),
    }
}

fn read_range(path: &[u8], offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
    let f = File::open(meta::os(path))?;
    let mut buf = vec![0u8; len as usize];
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], offset + got as u64)? {
            0 => break,
            n => got += n,
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// Write copy `to_copy` of session `session_id` from the source files, starting on the
/// cartridge in `dest` and continuing on those `next` supplies. The record layout follows
/// the session's lowest existing copy.
pub fn copy_from_source(
    cat: &Catalog,
    session_id: i64,
    to_copy: i64,
    dest: LaneStart<'_>,
    fo: &FromSourceOptions,
    next: &mut dyn NextVolume,
    progress: &mut dyn Progress,
) -> Result<CopyReport> {
    let sessions = cat.sessions()?;
    let session = sessions.iter().find(|s| s.id == session_id).cloned().ok_or_else(|| anyhow!("no session {session_id}"))?;
    let uuids: HashMap<i64, String> = sessions.iter().map(|s| (s.id, s.uuid.clone())).collect();
    let svs = cat.session_volumes(session_id)?;
    if svs.iter().any(|sv| sv.copy == to_copy) {
        bail!("session {} already has a copy {to_copy}", session.uuid);
    }
    let layout = svs.iter().map(|sv| sv.copy).min().ok_or_else(|| anyhow!("session {} is on no cartridge", session.uuid))?;
    let wrapped = session.encryption.as_deref().map(crypt::from_json).transpose()?;
    let (dek, kek) = match &wrapped {
        Some(w) => {
            let dek = crypt::unwrap(&fo.keys, w).with_context(|| format!("session {} is encrypted; copying it needs its key", session.uuid))?;
            (Some(dek), fo.keys.iter().find(|k| k.id() == w.key_id).cloned())
        }
        None => (None, None),
    };
    let uuid: Uuid = session.uuid.parse().map_err(|e| anyhow!("session uuid: {e}"))?;
    let sid64 = session_id64(&uuid);
    let mut opts = Options::new(Vec::new());
    opts.ring = fo.ring;
    opts.copy = to_copy as u8;
    opts.encrypt = kek;
    let opts = &opts;
    let ring = Ring::new(ringbuf::Config { pace: None, ..opts.ring });
    let first_dest = dest.volume.clone();
    let drives: Vec<SharedDrive> = vec![Mutex::new(ringbuf::DriveSlot::new(dest.drive))];
    let stop = AtomicBool::new(false);
    let mut report = CopyReport { session_uuid: session.uuid.clone(), copy: to_copy, ..Default::default() };
    let sources: Vec<String> = serde_json::from_str(&session.sources).unwrap_or_default();
    let begin = SessionBegin {
        session: uuid,
        volume: Uuid::nil(),
        job: session.job.clone(),
        level: session.level.clone(),
        started: session.started,
        host: session.host.clone(),
        tool: concat!("ltoarc ", env!("CARGO_PKG_VERSION")).into(),
        sources: sources.iter().map(|p| ByteBuf::from(p.as_bytes().to_vec())).collect(),
        copy: to_copy as u8,
        volume_seq: 0,
        parent: session.parent_id.and_then(|p| uuids.get(&p)).and_then(|u| u.parse().ok()),
        encryption: wrapped.clone(),
    };
    let lane = Lane {
        ring: &ring,
        drive: &drives[0],
        w: StreamWriter::new(64 << 10, Uuid::nil(), sid64, BlockKind::Data),
        volume: first_dest.clone(),
        label: VolumeLabel::new(&first_dest.barcode, "", None, String::new()),
        begin,
        start: 0,
        budget: None,
        perf_before: None,
        motion: None,
        full: false,
        open: None,
        file_volumes: Vec::new(),
        pace: dest.pace,
        bytes: 0,
        data_records: 0,
        repeat: None,
        copy: to_copy as u8,
    };
    let mut seq = Sequencer {
        cat,
        opts,
        next,
        stop: &stop,
        progress,
        lanes: vec![lane],
        slots: vec![vec![0]],
        lane_ring: (opts.ring.capacity, opts.ring.high),
        sid64,
        session_id,
        volume_seq: HashMap::new(),
        full: false,
        truncated: false,
        copying: true,
        // Fresh seals under the session's key, in a nonce space of this copy's own.
        sealer: dek.as_ref().map(|k| Sealer::for_copy(k, to_copy as u8)),
        dir_ids: HashMap::new(),
        writeback: Vec::new(),
        report: Report::default(),
        small_rx: crossbeam_channel::never(),
        small_open: false,
        stack: VecDeque::new(),
        stack_bytes: 0,
        stack_limit: 0,
    };
    // What to write: every entry, and the data records in the order the layout copy has them.
    let objects = cat.session_objects(session_id)?;
    let paths = cat.full_paths(objects.iter().map(|o| o.path_id))?;
    let mut hashes: HashMap<i64, Vec<HashRow>> = cat.session_hashes(&[session_id])?;
    let records = cat.session_records(session_id, layout)?;
    let mut chunks_left: HashMap<i64, usize> = HashMap::new();
    for (c, _) in &records {
        *chunks_left.entry(c.object_id).or_default() += 1;
    }
    let by_id: HashMap<i64, &ObjectRow> = objects.iter().map(|o| (o.id, o)).collect();
    let level = fo.zstd_level.unwrap_or(3);
    cat.begin()?;
    let result = std::thread::scope(|s| -> Result<WriterStats> {
        let writer = s.spawn(|| ring.write_to(&drives[0]));
        let run = (|| -> Result<()> {
            seq.start_volume(0, first_dest.clone())?;
            let meta = |seq: &Sequencer, o: &ObjectRow, h: &HashMap<i64, Vec<HashRow>>| -> Result<FileMeta> {
                file_meta(seq.cat, o, &paths[&o.path_id], h.get(&o.id).map(|v| v.as_slice()).unwrap_or(&[]), &uuids)
            };
            // Entries without data records here: directories, links, empty files, references.
            for o in objects.iter().filter(|o| !chunks_left.contains_key(&o.id)) {
                let m = meta(&seq, o, &hashes)?;
                seq.record(0, rtype::FILE, 0, &[&to_cbor(&m)]);
                if o.kind == "file" {
                    let e = file_end(o, hashes.get(&o.id).map(|v| v.as_slice()).unwrap_or(&[]));
                    seq.record(0, rtype::FILE_END, 0, &[&to_cbor(&e)]);
                }
                report.records += 1;
                seq.flush(0)?;
            }
            let mut written: std::collections::HashSet<i64> = std::collections::HashSet::new();
            // Files whose source no longer holds the session's data.
            let mut failed: std::collections::HashSet<i64> = std::collections::HashSet::new();
            let mut i = 0;
            while i < records.len() {
                // One record: a chunk, or the members of a pack (same position).
                let key = (records[i].1.volume_id, records[i].1.block, records[i].1.offset);
                let mut j = i + 1;
                while j < records.len() && (records[j].1.volume_id, records[j].1.block, records[j].1.offset) == key {
                    j += 1;
                }
                let group = &records[i..j];
                i = j;
                // FILE records first; a large file's is repeated on a continuation cartridge.
                for (c, _) in group {
                    if written.insert(c.object_id) {
                        let o = by_id[&c.object_id];
                        let body = to_cbor(&meta(&seq, o, &hashes)?);
                        let (f, b) = match &mut seq.sealer {
                            Some(sl) => (flags::ENCRYPTED, sl.seal(&crypt::record_aad(rtype::FILE), &body)),
                            None => (0, body),
                        };
                        seq.lanes[0].w.append(rtype::FILE, f, &[&b]);
                        if group[0].0.pack_offset.is_none() {
                            seq.lanes[0].repeat = Some((f, b));
                        }
                        report.records += 1;
                    }
                }
                // The data, read again from the source and checked.
                let first = &group[0].0;
                let mut good: Vec<bool> = Vec::new();
                let raw: Vec<u8> = if first.pack_offset.is_some() {
                    let mut raw = Vec::new();
                    for (c, _) in group {
                        let o = by_id[&c.object_id];
                        let path = &paths[&o.path_id];
                        let at = c.pack_offset.unwrap_or(0) as usize;
                        raw.resize(raw.len().max(at), 0);
                        let data = read_range(path, 0, c.raw_len as u64).ok().filter(|d| d.len() as i64 == c.raw_len && blake3::hash(d).as_bytes()[..] == c.blake3[..]);
                        match data {
                            Some(d) => {
                                raw.truncate(at);
                                raw.extend_from_slice(&d);
                                good.push(true);
                            }
                            None => {
                                // Its place in the pack stays (zeros), so the others' offsets hold.
                                report.errors.push(format!("{}: changed or unreadable since the session; not in copy {to_copy}", String::from_utf8_lossy(path)));
                                raw.truncate(at);
                                raw.resize(at + c.raw_len as usize, 0);
                                good.push(false);
                                failed.insert(c.object_id);
                            }
                        }
                    }
                    raw
                } else {
                    let o = by_id[&first.object_id];
                    let path = &paths[&o.path_id];
                    let data = if failed.contains(&first.object_id) { None } else { read_range(path, first.offset as u64, first.raw_len as u64).ok() };
                    match data {
                        _ if failed.contains(&first.object_id) => {
                            good.push(false);
                            Vec::new()
                        }
                        Some(d) if d.len() as i64 == first.raw_len && blake3::hash(&d).as_bytes()[..] == first.blake3[..] => {
                            good.push(true);
                            d
                        }
                        _ => {
                            report.errors.push(format!(
                                "{}: bytes {}..{} changed or unreadable since the session; not in copy {to_copy}",
                                String::from_utf8_lossy(path),
                                first.offset,
                                first.offset + first.raw_len
                            ));
                            good.push(false);
                            failed.insert(first.object_id);
                            // The copy holds all of a file or none of it.
                            cat.drop_placements(first.object_id, to_copy)?;
                            Vec::new()
                        }
                    }
                };
                // A chunk that cannot be had is left out.
                if first.pack_offset.is_none() && !good[0] {
                    let left = chunks_left.get_mut(&first.object_id).expect("counted");
                    *left -= 1;
                    if *left == 0 {
                        let o = by_id[&first.object_id];
                        let mut e = file_end(o, hashes.remove(&o.id).as_deref().unwrap_or(&[]));
                        e.status = "unreadable".into();
                        seq.record(0, rtype::FILE_END, 0, &[&to_cbor(&e)]);
                        seq.lanes[0].repeat = None;
                    }
                    continue;
                }
                let (cdc, stored) = match first.codec as u8 {
                    codec::ZSTD => match zstd::bulk::compress(&raw, level) {
                        Ok(c) if c.len() < raw.len() => (codec::ZSTD, c),
                        _ => (codec::STORED, raw.clone()),
                    },
                    _ => (codec::STORED, raw.clone()),
                };
                let rflags = if cdc == codec::ZSTD { flags::COMPRESSED } else { 0 };
                if !seq.make_room(0, stored.len() as u64 + 4096 * group.len() as u64)? {
                    bail!("the destination tape set ran out of cartridges");
                }
                let pos = if first.pack_offset.is_some() {
                    let members: Vec<PackMember> = group
                        .iter()
                        .map(|(c, _)| PackMember { object: by_id[&c.object_id].seq as u64, offset: c.pack_offset.unwrap_or(0) as u64, len: c.raw_len as u64, blake3: c.blake3.clone() })
                        .collect();
                    let header = ChunkHeader {
                        object: 0,
                        seq: report.chunks as u32,
                        codec: cdc,
                        flags: 0,
                        offset: 0,
                        raw_len: raw.len() as u64,
                        stored_len: stored.len() as u64,
                        blake3: *blake3::hash(&raw).as_bytes(),
                    };
                    seq.record(0, rtype::PACK, rflags, &[&encode_pack(&header, &members, &stored)])
                } else {
                    let o = by_id[&first.object_id];
                    let header = ChunkHeader {
                        object: o.seq as u64,
                        seq: first.seq as u32,
                        codec: cdc,
                        flags: 0,
                        offset: first.offset as u64,
                        raw_len: first.raw_len as u64,
                        stored_len: stored.len() as u64,
                        blake3: first.blake3.clone().try_into().map_err(|_| anyhow!("chunk {} has no BLAKE3", first.id))?,
                    };
                    seq.record(0, rtype::CHUNK, rflags, &[&header.encode(), &stored])
                };
                report.records += 1;
                report.chunks += 1;
                report.bytes += stored.len() as u64;
                seq.lanes[0].bytes += stored.len() as u64;
                seq.progress.data(raw.len() as u64, stored.len() as u64);
                for ((c, layout_p), ok) in group.iter().zip(&good) {
                    if *ok {
                        report.verified += 1;
                        let mut p = seq.placement(0, c.id, pos);
                        let size = stored.len() as i64;
                        p.stored_len = (size != layout_p.stored_len.unwrap_or(c.stored_len)).then_some(size);
                        cat.insert_placement(&p)?;
                    }
                }
                // FILE_END after a file's last chunk.
                for (c, _) in group {
                    let left = chunks_left.get_mut(&c.object_id).expect("counted");
                    *left -= 1;
                    if *left == 0 {
                        let o = by_id[&c.object_id];
                        let mut e = file_end(o, hashes.remove(&o.id).as_deref().unwrap_or(&[]));
                        if failed.contains(&o.id) {
                            e.status = "unreadable".into();
                        }
                        seq.record(0, rtype::FILE_END, 0, &[&to_cbor(&e)]);
                        report.records += 1;
                        seq.lanes[0].repeat = None;
                    }
                }
                seq.flush(0)?;
            }
            let end = SessionEnd {
                session: uuid,
                files: session.files as u64,
                bytes: session.bytes as u64,
                stored: session.stored_bytes as u64,
                ended: session.ended.unwrap_or_else(now),
                status: session.status.clone(),
            };
            seq.record(0, rtype::SESSION_END, 0, &[&to_cbor(&end)]);
            seq.end_volume(0, &session.status, false)
        })();
        ring.close();
        let wr = writer.join().map_err(|_| anyhow!("writer thread panicked")).and_then(|r| r.map_err(|e| anyhow!(e)));
        run?;
        wr
    });
    match result {
        Ok(mut w) => {
            w.drive = seq.lanes[0].motion;
            report.writer = w;
            report.volumes = std::mem::take(&mut seq.report.volumes);
            report.warnings = std::mem::take(&mut seq.report.warnings);
            Ok(report)
        }
        Err(e) => {
            if cat.in_transaction() {
                let _ = cat.rollback();
            }
            Err(e)
        }
    }
}
