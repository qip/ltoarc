//! Tape-to-tape copy: another copy of a session already on tape, read from the cartridges of
//! one copy in one drive and written to a tape set in another (FORMAT.md §6.2 applies on both
//! sides; each may span cartridges).
//!
//! Records are copied as they are on tape, compressed and encrypted bodies included; they are
//! decoded only to be checked (chunk and pack BLAKE3) and to know which file is open when the
//! destination moves to its next cartridge. Placements of the new copy are found by the
//! source record's position. The source is read through the read-ahead thread; the
//! destination is written by the backup's lane: ring buffer, writer thread, cartridge budget,
//! spanning, coherency marks, and a volume catalog on every cartridge. An encrypted session
//! needs its key: it opens the records for checking and seals the new volume catalogs.

use super::*;
use crate::readahead::{Blocks, Extent, ReadAhead, ReadStats};
use ltoarc_format::record::{decode_pack, from_cbor};
use ltoarc_format::stream::StreamReader;

/// Loads the cartridges holding the session's source copy.
pub trait SourceVolumes {
    /// Load `volume` and return the drive holding it.
    fn load(&mut self, volume: &VolumeRow) -> Result<&mut dyn TapeDrive>;
}

pub struct CopyOptions {
    /// Destination buffer (as for backups).
    pub ring: ringbuf::Config,
    /// Read-ahead memory on the source side.
    pub read_buffer: u64,
    /// Speed-matching range of the source drive: it reads at the pace the destination
    /// writes, so the faster drive slows down to the slower one instead of stopping.
    /// None reads at full speed, in bursts.
    pub source_pace: Option<ringbuf::Pace>,
    /// Rate the source starts at before the destination's is known (its expected speed).
    pub match_rate: Option<f64>,
    /// Keys for encrypted sessions.
    pub keys: Vec<Key>,
}

#[derive(Debug, Default)]
pub struct CopyReport {
    pub session_uuid: String,
    pub copy: i64,
    pub records: u64,
    /// CHUNK and PACK records, and the stored bytes they hold.
    pub chunks: u64,
    pub bytes: u64,
    /// Chunk and pack records whose BLAKE3 matched.
    pub verified: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub volumes: Vec<VolumeUse>,
    /// How the source was read: speed, the rate it was matched to, stops.
    pub read: ReadStats,
    /// How the destination was written, including its drive's stops.
    pub writer: WriterStats,
}

/// BLAKE3 of the raw data in a CHUNK or PACK body (in the clear), against its header.
fn check_data(rtype: u16, plain: &[u8]) -> Result<bool> {
    let (h, stored) = if rtype == rtype::PACK {
        let (h, _, stored) = decode_pack(plain)?;
        (h, stored)
    } else {
        (ChunkHeader::decode(plain)?, &plain[CHUNK_HEADER_LEN..])
    };
    let raw = match h.codec {
        codec::STORED => std::borrow::Cow::Borrowed(stored),
        codec::ZSTD => std::borrow::Cow::Owned(zstd::bulk::decompress(stored, h.raw_len as usize)?),
        other => bail!("unknown codec {other}"),
    };
    Ok(blake3::hash(&raw).as_bytes() == &h.blake3)
}

/// Copy session `session_id` from its copy `from_copy` to a new copy `to_copy`, starting on
/// the destination cartridge in `dest` and continuing on those `next` supplies.
#[allow(clippy::too_many_arguments)]
pub fn copy_session(
    cat: &Catalog,
    session_id: i64,
    from_copy: i64,
    to_copy: i64,
    source: &mut dyn SourceVolumes,
    dest: LaneStart<'_>,
    copts: &CopyOptions,
    next: &mut dyn NextVolume,
    progress: &mut dyn Progress,
) -> Result<CopyReport> {
    let session = cat.sessions()?.into_iter().find(|s| s.id == session_id).ok_or_else(|| anyhow!("no session {session_id}"))?;
    let all = cat.session_volumes(session_id)?;
    if all.iter().any(|sv| sv.copy == to_copy) {
        bail!("session {} already has a copy {to_copy}", session.uuid);
    }
    let mut parts: Vec<SessionVolume> = all.into_iter().filter(|sv| sv.copy == from_copy).collect();
    parts.sort_by_key(|sv| sv.seq);
    if parts.is_empty() {
        bail!("session {} has no copy {from_copy}", session.uuid);
    }
    let wrapped = session.encryption.as_deref().map(crypt::from_json).transpose()?;
    let (dek, kek) = match &wrapped {
        Some(w) => {
            let dek = crypt::unwrap(&copts.keys, w)
                .with_context(|| format!("session {} is encrypted; copying it needs its key", session.uuid))?;
            (Some(dek), copts.keys.iter().find(|k| k.id() == w.key_id).cloned())
        }
        None => (None, None),
    };
    let uuid: Uuid = session.uuid.parse().map_err(|e| anyhow!("session uuid: {e}"))?;
    let sid64 = session_id64(&uuid);
    let mut opts = Options::new(Vec::new());
    opts.ring = copts.ring;
    opts.copy = to_copy as u8;
    opts.encrypt = kek;
    let opts = &opts;
    let ring = Ring::new(ringbuf::Config { pace: None, ..opts.ring });
    let first_dest = dest.volume.clone();
    let drives: Vec<SharedDrive> = vec![Mutex::new(ringbuf::DriveSlot::new(dest.drive))];
    let stop = AtomicBool::new(false);
    let mut report = CopyReport { session_uuid: session.uuid.clone(), copy: to_copy, ..Default::default() };
    let placeholder = SessionBegin {
        session: uuid,
        volume: Uuid::nil(),
        job: None,
        level: String::new(),
        started: session.started,
        host: String::new(),
        tool: String::new(),
        sources: Vec::new(),
        copy: to_copy as u8,
        volume_seq: 0,
        parent: None,
        encryption: wrapped.clone(),
    };
    let lane = Lane {
        ring: &ring,
        drive: &drives[0],
        w: StreamWriter::new(64 << 10, Uuid::nil(), sid64, BlockKind::Data),
        volume: first_dest.clone(),
        label: VolumeLabel::new(&first_dest.barcode, "", None, String::new()),
        begin: placeholder,
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
        sealer: None,
        dir_ids: HashMap::new(),
        writeback: Vec::new(),
        report: Report::default(),
        small_rx: crossbeam_channel::never(),
        small_open: false,
        stack: VecDeque::new(),
        stack_bytes: 0,
        stack_limit: 0,
    };
    cat.begin()?;
    let result = std::thread::scope(|s| -> Result<WriterStats> {
        let writer = s.spawn(|| ring.write_to(&drives[0]));
        let run = (|| -> Result<()> {
            let mut started = false;
            // FILE records (flags, body) of files whose FILE_END has not come yet, and the
            // one whose chunks are being copied.
            let mut files: HashMap<u64, (u16, Vec<u8>)> = HashMap::new();
            let mut open: Option<u64> = None;
            for part in &parts {
                let vol = cat.volume_by_id(part.volume_id)?.ok_or_else(|| anyhow!("volume {} missing", part.volume_id))?;
                let drive = source.load(&vol)?;
                let label = read_label(drive)?;
                check_label(&label, &vol)?;
                let extent = [Extent { partition: part.partition as u8, start: part.start_object as u64, end: part.end_object.map(|e| e as u64) }];
                let ra = ReadAhead::paced(copts.read_buffer, copts.source_pace, copts.match_rate);
                let bs = label.block_size as usize;
                let (stats, r) = std::thread::scope(|s2| {
                    let reader = s2.spawn(|| ra.read(drive, bs, &extent));
                    let r = (|| -> Result<()> {
                        let mut src = Blocks::new(&ra);
                        if !src.skip_to(part.start_object as u64) {
                            bail!("{}: block {} could not be read{}", vol.barcode, part.start_object, src.error.take().map(|e| format!(": {e}")).unwrap_or_default());
                        }
                        let mut rd = StreamReader::at(src, 0)?;
                        loop {
                            let rec = match rd.next_record() {
                                Ok(Some(r)) => r,
                                Ok(None) => break,
                                // A recovered session ends where the crash cut it.
                                Err(_) if rd.ended() && session.status != "complete" => break,
                                Err(e) => bail!("{} near block {:?}: {e}", vol.barcode, rd.position()),
                            };
                            let (rt, rf) = (rec.header.rtype, rec.header.flags);
                            match rt {
                                rtype::SESSION_BEGIN => {
                                    if !started {
                                        let mut b: SessionBegin = from_cbor(&rec.body)?;
                                        b.copy = to_copy as u8;
                                        b.volume_seq = 0;
                                        seq.lanes[0].begin = b;
                                        seq.start_volume(0, first_dest.clone())?;
                                        started = true;
                                    }
                                    continue;
                                }
                                // The destination has cartridge boundaries of its own.
                                rtype::VOLUME_END => continue,
                                rtype::FILE if rf & flags::CONTINUED != 0 => continue,
                                _ => {}
                            }
                            if !started {
                                bail!("{}: session stream does not start with SESSION_BEGIN", vol.barcode);
                            }
                            let plain = crypt::record_body(dek.as_ref(), rt, rf, &rec.body)?;
                            let data = rt == rtype::CHUNK || rt == rtype::PACK;
                            match rt {
                                rtype::FILE => {
                                    let m: FileMeta = from_cbor(&plain)?;
                                    if m.kind == FileKind::File {
                                        files.insert(m.id, (rf, rec.body.clone()));
                                    }
                                }
                                rtype::CHUNK => {
                                    let h = ChunkHeader::decode(&plain)?;
                                    open = Some(h.object);
                                    seq.lanes[0].repeat = files.get(&h.object).cloned();
                                }
                                rtype::FILE_END => {
                                    let e: FileEnd = from_cbor(&plain)?;
                                    files.remove(&e.id);
                                    if open == Some(e.id) {
                                        open = None;
                                        seq.lanes[0].repeat = None;
                                    }
                                }
                                _ => {}
                            }
                            let src_block = part.start_object + rec.pos.block_seq as i64;
                            if data {
                                match check_data(rt, &plain) {
                                    Ok(true) => report.verified += 1,
                                    Ok(false) => report.errors.push(format!("{}: record at block {src_block} fails its BLAKE3 check", vol.barcode)),
                                    Err(e) => report.errors.push(format!("{}: record at block {src_block}: {e}", vol.barcode)),
                                }
                                if !seq.make_room(0, rec.body.len() as u64 + 24)? {
                                    bail!("the destination tape set ran out of cartridges");
                                }
                            }
                            let pos = seq.lanes[0].w.append(rt, rf, &[&rec.body]);
                            report.records += 1;
                            if data {
                                for chunk_id in cat.chunks_at(part.volume_id, from_copy, src_block, rec.pos.offset as i64)? {
                                    cat.insert_placement(&seq.placement(0, chunk_id, pos))?;
                                }
                                report.chunks += 1;
                                report.bytes += rec.body.len() as u64;
                                seq.lanes[0].bytes += rec.body.len() as u64;
                                seq.progress.data(rec.body.len() as u64, rec.body.len() as u64);
                            }
                            seq.flush(0)?;
                        }
                        Ok(())
                    })();
                    ra.cancel();
                    (reader.join(), r)
                });
                report.read.merge(&stats.map_err(|_| anyhow!("tape reader thread panicked"))?);
                r?;
            }
            if !started {
                bail!("no session stream found for session {}", session.uuid);
            }
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
