//! Restoring files: plan from the catalog, read chunks in tape order, verify, apply metadata.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ltoarc_catalog::{Catalog, ChunkRow, HashRow, ObjectRow, Placement, View};
use ltoarc_device::TapeDrive;
use ltoarc_format::record::{codec, decode_pack, from_cbor, rtype, ChunkHeader, PackMember, CHUNK_HEADER_LEN};
use ltoarc_format::stream::{RecordPos, StreamReader};
use serde_bytes::ByteBuf;

use crate::crypt::{self, Key};
use crate::hash::{Algo, Hasher};
use crate::meta;
use crate::readahead::{self, Blocks, ReadAhead};
use crate::tapeio::read_label;

/// Read-ahead memory: 2 GiB, or a quarter of available memory if that is less.
pub fn default_read_buffer() -> u64 {
    (crate::ringbuf::Config::default_for_host().capacity).min(2 << 30)
}

/// Reading through up to this many bytes is cheaper than a LOCATE (which can take a minute).
const READ_THROUGH_BYTES: u64 = 4 << 30;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dest: PathBuf,
    /// Remove this prefix from archived paths before placing them under `dest`.
    pub strip_prefix: Option<PathBuf>,
    pub overwrite: bool,
    pub view: View,
    /// Restore from this copy only (default: the lowest-numbered copy that holds each chunk).
    pub copy: Option<i64>,
    /// Set owner and group (default: only when running as root).
    pub owners: Option<bool>,
    /// Keys for encrypted sessions.
    pub keys: Vec<Key>,
    /// Memory for reading ahead of the restore (default: 2 GiB or a quarter of free memory).
    pub read_buffer: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub archived: Vec<u8>,
    pub dest: PathBuf,
    pub obj: ObjectRow,
    /// Restore as a hard link to this already-restored path.
    pub link_to: Option<PathBuf>,
    pub hashes: Vec<HashRow>,
    pub chunks: usize,
}

#[derive(Debug, Clone)]
pub struct Piece {
    pub item: usize,
    pub chunk: ChunkRow,
    pub placement: Placement,
    /// First block of the session stream holding this chunk.
    pub stream_start: i64,
    /// FILE record id of the object whose data this is.
    pub obj_seq: i64,
    /// Session that wrote it.
    pub session_id: i64,
    /// Upper bound of the length of the record holding it (for planning reads).
    pub record_len: u64,
}

/// The last PACK read: (partition, block, offset), decoded bytes, members.
type PackCache = ((i64, i64, i64), Vec<u8>, Vec<PackMember>);

#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Pieces per cartridge barcode, in tape order.
    pub volumes: BTreeMap<String, Vec<Piece>>,
    pub bytes: u64,
    /// Wrapped data keys of the encrypted sessions involved, by session id.
    pub encryption: HashMap<i64, String>,
}

impl Plan {
    pub fn barcodes(&self) -> Vec<String> {
        self.volumes.keys().cloned().collect()
    }
}

fn dest_for(archived: &[u8], opts: &Options) -> Result<PathBuf> {
    let p = meta::os(archived);
    let rel = match &opts.strip_prefix {
        Some(prefix) => p.strip_prefix(prefix).map_err(|_| anyhow!("{} is not under {}", p.display(), prefix.display()))?,
        None => p.strip_prefix("/").unwrap_or(p),
    };
    Ok(opts.dest.join(rel))
}

/// Where a file's data is: one piece per chunk, from the lowest copy that holds it (or copy
/// `copy` only), with the cartridge it is on. A file stored by reference reads the chunks of
/// the file that holds its data. `item` tags the pieces; `stream_starts` caches where
/// session streams start, by (session, volume).
pub fn object_pieces(
    cat: &Catalog,
    src: &ObjectRow,
    item: usize,
    copy: Option<i64>,
    stream_starts: &mut HashMap<(i64, i64), i64>,
) -> Result<Vec<(String, Piece)>> {
    let holder;
    let src = match src.data_from {
        Some(d) => {
            holder = cat.object(d)?.ok_or_else(|| anyhow!("the file holding its data is missing from the catalog"))?;
            &holder
        }
        None => src,
    };
    let mut by_chunk: BTreeMap<i64, Vec<ltoarc_catalog::ChunkPlacement>> = BTreeMap::new();
    for cp in cat.chunk_placements(src.id)? {
        if copy.is_none_or(|c| c == cp.placement.copy) {
            by_chunk.entry(cp.chunk.id).or_default().push(cp);
        }
    }
    if src.size > 0 && by_chunk.is_empty() {
        bail!("no tape copy holds its data");
    }
    let mut out = Vec::new();
    for (_, mut cps) in by_chunk {
        cps.sort_by_key(|c| c.placement.copy);
        let cp = cps.remove(0);
        let key = (src.session_id, cp.placement.volume_id);
        let start = match stream_starts.get(&key) {
            Some(&s) => s,
            None => {
                let s = cat
                    .session_volumes(src.session_id)?
                    .into_iter()
                    .find(|v| v.volume_id == cp.placement.volume_id && v.copy == cp.placement.copy)
                    .map(|v| v.start_object)
                    .ok_or_else(|| anyhow!("session stream of chunk {} not recorded", cp.chunk.id))?;
                stream_starts.insert(key, s);
                s
            }
        };
        // Record header, CHUNK header, encryption overhead; a pack adds its member table.
        let mut record_len = 24 + 72 + 4 + crypt::OVERHEAD as u64 + cp.placement.stored_len.unwrap_or(cp.chunk.stored_len) as u64;
        if cp.chunk.pack_offset.is_some() {
            let pl = &cp.placement;
            record_len += 160 * cat.record_members(pl.volume_id, pl.copy, pl.block, pl.offset)?.max(1) as u64;
        }
        out.push((
            cp.barcode.clone(),
            Piece { item, chunk: cp.chunk, placement: cp.placement, stream_start: start, obj_seq: src.seq, session_id: src.session_id, record_len },
        ));
    }
    Ok(out)
}

/// Data keys of the encrypted sessions among `sessions`.
pub fn session_keys(cat: &Catalog, sessions: &HashSet<i64>, keys: &[Key]) -> Result<HashMap<i64, Key>> {
    let mut deks = HashMap::new();
    for s in cat.sessions()? {
        if let (true, Some(e)) = (sessions.contains(&s.id), &s.encryption) {
            let dek = crypt::unwrap(keys, &crypt::from_json(e)?).with_context(|| format!("session {} is encrypted", &s.uuid[..8]))?;
            deks.insert(s.id, dek);
        }
    }
    Ok(deks)
}

/// Build a restore plan for the given archived paths (each restored with everything below it).
pub fn plan(cat: &Catalog, selections: &[PathBuf], opts: &Options) -> Result<Plan> {
    let mut seen = HashSet::new();
    let mut rows: Vec<(Vec<u8>, ObjectRow)> = Vec::new();
    for sel in selections {
        let id = cat
            .lookup_path(sel.as_os_str().as_bytes())?
            .ok_or_else(|| anyhow!("{} is not in the catalog", sel.display()))?;
        let found = cat.resolve(id, opts.view)?;
        if found.is_empty() {
            bail!("nothing archived at or below {} in the selected view", sel.display());
        }
        for (path, o) in found {
            if seen.insert(o.id) {
                rows.push((path, o));
            }
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let index: HashMap<Vec<u8>, usize> = rows.iter().enumerate().map(|(i, (p, _))| (p.clone(), i)).collect();
    let mut plan = Plan::default();
    // Hard links whose first path is not selected: the first such link receives the data.
    let mut data_holder: HashMap<Vec<u8>, PathBuf> = HashMap::new();
    let mut data_from: Vec<Option<ObjectRow>> = Vec::new();
    for (path, obj) in &rows {
        let dest = dest_for(path, opts)?;
        let mut item = Item { archived: path.clone(), dest: dest.clone(), obj: obj.clone(), link_to: None, hashes: Vec::new(), chunks: 0 };
        let mut source = None;
        match obj.kind.as_str() {
            "file" => source = Some(obj.clone()),
            "hardlink" => {
                let target = obj.target.clone().unwrap_or_default();
                if let Some(&ti) = index.get(&target) {
                    item.link_to = Some(dest_for(&rows[ti].0, opts)?);
                } else if let Some(holder) = data_holder.get(&target) {
                    item.link_to = Some(holder.clone());
                } else {
                    let tid = cat.lookup_path(&target)?.ok_or_else(|| anyhow!("hard link target {} missing", String::from_utf8_lossy(&target)))?;
                    let tobj = cat.latest(tid, opts.view)?.ok_or_else(|| anyhow!("hard link target has no version"))?;
                    data_holder.insert(target, dest);
                    item.obj = ObjectRow { kind: "file".into(), ..tobj.clone() };
                    source = Some(tobj);
                }
            }
            _ => {}
        }
        if let Some(src) = &source {
            item.hashes = cat.hashes(src.id)?.into_iter().filter(|h| h.kind == "content").collect();
        }
        data_from.push(source);
        plan.items.push(item);
    }
    let mut stream_starts: HashMap<(i64, i64), i64> = HashMap::new();
    for (i, src) in data_from.iter().enumerate() {
        let Some(src) = src else { continue };
        let pieces = object_pieces(cat, src, i, opts.copy, &mut stream_starts)
            .with_context(|| String::from_utf8_lossy(&plan.items[i].archived).to_string())?;
        for (barcode, p) in pieces {
            plan.bytes += p.chunk.raw_len as u64;
            plan.items[i].chunks += 1;
            plan.volumes.entry(barcode).or_default().push(p);
        }
    }
    for pieces in plan.volumes.values_mut() {
        pieces.sort_by_key(|p| (p.placement.partition, p.placement.block, p.placement.offset));
    }
    let needed: HashSet<i64> = plan.volumes.values().flatten().map(|p| p.session_id).collect();
    for s in cat.sessions()? {
        if let (true, Some(e)) = (needed.contains(&s.id), s.encryption) {
            plan.encryption.insert(s.id, e);
        }
    }
    Ok(plan)
}

#[derive(Debug, Default)]
pub struct Report {
    pub files: u64,
    pub dirs: u64,
    pub others: u64,
    pub bytes: u64,
    pub chunks: u64,
    pub locates: u64,
    /// How the tape was read (all cartridges): speed, pacing, locates, stops.
    pub read: crate::readahead::ReadStats,
    pub verified_hashes: u64,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

struct Streaming {
    next_seq: i64,
    hashed_to: u64,
    /// Content hashes computed on the way (in the hash pool, by item index).
    algos: Vec<Algo>,
}

pub struct Restorer {
    plan: Plan,
    opts: Options,
    open: HashMap<usize, File>,
    remaining: Vec<usize>,
    stream: HashMap<usize, Streaming>,
    rehash: HashSet<usize>,
    done_hash: HashMap<usize, Vec<(String, String)>>,
    pool: crate::hash::HashPool,
    /// Data keys of encrypted sessions.
    deks: HashMap<i64, Key>,
    /// Speed-matching range of the drive reading the next cartridge (None: full speed).
    pace: Option<crate::ringbuf::Pace>,
    pub report: Report,
}

fn content_algos(hashes: &[HashRow]) -> Vec<Algo> {
    let mut algos: Vec<Algo> = hashes.iter().filter_map(|h| Algo::parse(&h.algo)).collect();
    algos.sort();
    algos.dedup();
    algos
}

fn content_hashers(hashes: &[HashRow]) -> Vec<(String, Hasher)> {
    content_algos(hashes).into_iter().map(|a| (a.name().to_string(), a.hasher())).collect()
}


impl Restorer {
    /// Check destinations and create directories.
    pub fn new(plan: Plan, opts: Options) -> Result<Restorer> {
        let mut clashes = Vec::new();
        for it in &plan.items {
            if it.obj.kind != "dir" && std::fs::symlink_metadata(&it.dest).is_ok() {
                clashes.push(it.dest.display().to_string());
            }
        }
        if !clashes.is_empty() && !opts.overwrite {
            bail!(
                "{} destination(s) already exist, e.g. {} (use --overwrite)",
                clashes.len(),
                clashes.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            );
        }
        for it in &plan.items {
            let dir = if it.obj.kind == "dir" { it.dest.as_path() } else { it.dest.parent().unwrap_or(Path::new("/")) };
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // Every key needed, before any cartridge moves.
        let mut deks = HashMap::new();
        for (sid, e) in &plan.encryption {
            deks.insert(*sid, crypt::unwrap(&opts.keys, &crypt::from_json(e)?)?);
        }
        let remaining = plan.items.iter().map(|i| i.chunks).collect();
        let stream = plan
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| it.chunks > 0)
            .map(|(i, it)| (i, Streaming { next_seq: 0, hashed_to: 0, algos: content_algos(&it.hashes) }))
            .collect();
        Ok(Restorer {
            plan,
            opts,
            open: HashMap::new(),
            remaining,
            stream,
            rehash: HashSet::new(),
            done_hash: HashMap::new(),
            pool: crate::hash::HashPool::new(),
            deks,
            pace: None,
            report: Report::default(),
        })
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Match the drive reading the next cartridge to the restore's pace, within `pace`
    /// (that drive's speed-matching range), instead of reading at full speed.
    pub fn set_pace(&mut self, pace: Option<crate::ringbuf::Pace>) {
        self.pace = pace;
    }

    /// Restore every piece stored on the cartridge `barcode`, which is loaded in `drive`.
    /// A reader thread reads the needed stretches of tape ahead into memory (see
    /// `readahead`), so the drive streams while files are written out.
    pub fn volume(&mut self, barcode: &str, drive: &mut dyn TapeDrive) -> Result<()> {
        let pieces = self.plan.volumes.get(barcode).cloned().unwrap_or_default();
        if pieces.is_empty() {
            return Ok(());
        }
        let deks = std::mem::take(&mut self.deks);
        let mut errors = Vec::new();
        let buffer = self.opts.read_buffer.unwrap_or_else(default_read_buffer);
        let pace = self.pace;
        let r = read_volume(drive, barcode, &pieces, buffer, pace, &deks, &mut errors, &mut |p, off, data| self.write_piece(p, off, data));
        self.deks = deks;
        for (item, what) in errors {
            let dest = self.plan.items[item].dest.display().to_string();
            self.report.errors.push(format!("{dest}: {what}"));
        }
        let stats = r?;
        self.report.locates += stats.locates;
        self.report.read.merge(&stats);
        Ok(())
    }

    fn write_piece(&mut self, p: &Piece, offset: u64, data: &[u8]) -> Result<()> {
        let item = &self.plan.items[p.item];
        if !self.open.contains_key(&p.item) {
            if self.opts.overwrite {
                let _ = std::fs::remove_file(&item.dest);
            }
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&item.dest)
                .with_context(|| format!("creating {}", item.dest.display()))?;
            self.open.insert(p.item, f);
        }
        self.open[&p.item].write_all_at(data, offset).with_context(|| format!("writing {}", item.dest.display()))?;
        if let Some(s) = self.stream.get_mut(&p.item) {
            if p.chunk.seq == s.next_seq && offset >= s.hashed_to {
                if !s.algos.is_empty() {
                    self.pool.zeros(p.item as u64, &s.algos, offset - s.hashed_to);
                    self.pool.data(p.item as u64, &s.algos, &std::sync::Arc::new(data.to_vec()));
                }
                s.hashed_to = offset + data.len() as u64;
                s.next_seq += 1;
            } else {
                self.rehash.insert(p.item);
            }
        }
        self.report.bytes += data.len() as u64;
        self.report.chunks += 1;
        self.remaining[p.item] -= 1;
        if self.remaining[p.item] == 0 {
            let f = self.open.remove(&p.item).expect("open file");
            f.set_len(item.obj.size as u64)?;
            if let Some(s) = self.stream.remove(&p.item) {
                if !self.rehash.contains(&p.item) && !s.algos.is_empty() {
                    self.pool.zeros(p.item as u64, &s.algos, (item.obj.size as u64).saturating_sub(s.hashed_to));
                    self.pool.end(p.item as u64, &s.algos);
                }
            }
        }
        Ok(())
    }

    /// Create entries without data, verify file hashes, and apply metadata.
    pub fn finish(mut self) -> Result<Report> {
        if let Some((&i, _)) = self.open.iter().next() {
            bail!("{} was not completely restored (missing chunks)", self.plan.items[i].dest.display());
        }
        // Digests computed while restoring.
        for (i, d) in std::mem::take(&mut self.pool).finish() {
            if !self.rehash.contains(&(i as usize)) {
                self.done_hash.insert(i as usize, d);
            }
        }
        let owners = self.opts.owners.unwrap_or_else(|| unsafe { libc::geteuid() } == 0);
        let items = std::mem::take(&mut self.plan.items);
        for (i, it) in items.iter().enumerate() {
            let r = self.create(i, it);
            if let Err(e) = r {
                self.report.errors.push(format!("{}: {e:#}", it.dest.display()));
            }
        }
        for (i, it) in items.iter().enumerate().filter(|(_, it)| it.obj.kind == "file" && it.link_to.is_none()) {
            self.check_hashes(i, it);
        }
        let mut dirs = Vec::new();
        for it in &items {
            if it.obj.kind == "dir" {
                dirs.push(it);
            } else if it.link_to.is_none() {
                self.apply_meta(it, owners);
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.dest.components().count()));
        for d in dirs {
            self.apply_meta(d, owners);
        }
        Ok(self.report)
    }

    fn create(&mut self, _i: usize, it: &Item) -> Result<()> {
        let o = &it.obj;
        if self.opts.overwrite && o.kind != "dir" && o.kind != "file" {
            let _ = std::fs::remove_file(&it.dest);
        }
        if let Some(target) = &it.link_to {
            if self.opts.overwrite {
                let _ = std::fs::remove_file(&it.dest);
            }
            std::fs::hard_link(target, &it.dest)?;
            self.report.files += 1;
            return Ok(());
        }
        match o.kind.as_str() {
            "dir" => self.report.dirs += 1,
            "file" => {
                if it.chunks == 0 {
                    if self.opts.overwrite {
                        let _ = std::fs::remove_file(&it.dest);
                    }
                    let f = File::create(&it.dest)?;
                    f.set_len(o.size as u64)?;
                }
                self.report.files += 1;
            }
            "symlink" => {
                std::os::unix::fs::symlink(meta::os(o.target.as_deref().unwrap_or_default()), &it.dest)?;
                self.report.others += 1;
            }
            "fifo" | "char" | "block" => {
                meta::mknod(&it.dest, o.mode as u32, o.rdev as u64)?;
                self.report.others += 1;
            }
            "socket" => self.report.warnings.push(format!("{}: sockets are not restored", it.dest.display())),
            other => self.report.warnings.push(format!("{}: unknown kind {other}", it.dest.display())),
        }
        Ok(())
    }

    fn check_hashes(&mut self, i: usize, it: &Item) {
        if it.hashes.is_empty() {
            return;
        }
        let computed: Vec<(String, String)> = match self.done_hash.remove(&i) {
            Some(c) => c,
            None => {
                let mut hs = content_hashers(&it.hashes);
                match std::fs::read(&it.dest) {
                    Ok(data) => {
                        for (_, h) in hs.iter_mut() {
                            h.update(&data);
                        }
                        hs.into_iter().map(|(a, h)| (a, h.finish())).collect()
                    }
                    Err(e) => {
                        self.report.errors.push(format!("{}: cannot re-read for hashing: {e}", it.dest.display()));
                        return;
                    }
                }
            }
        };
        for h in &it.hashes {
            if let Some((_, got)) = computed.iter().find(|(a, _)| *a == h.algo) {
                if *got == h.digest {
                    self.report.verified_hashes += 1;
                } else {
                    self.report.errors.push(format!(
                        "{}: {} {} hash mismatch (expected {}, got {})",
                        it.dest.display(),
                        h.profile,
                        h.algo,
                        h.digest,
                        got
                    ));
                }
            }
        }
    }

    fn apply_meta(&mut self, it: &Item, owners: bool) {
        let o = &it.obj;
        let p = &it.dest;
        if let Some(x) = &o.xattrs {
            if let Ok(list) = from_cbor::<Vec<(ByteBuf, ByteBuf)>>(x) {
                for (name, value) in list {
                    if let Err(e) = meta::set_xattr(p, &name, &value) {
                        let n = String::from_utf8_lossy(&name);
                        if !n.starts_with("security.") && !n.starts_with("trusted.") {
                            self.report.warnings.push(format!("{}: xattr {n}: {e}", p.display()));
                        }
                    }
                }
            }
        }
        if owners {
            if let Err(e) = meta::lchown(p, o.uid as u32, o.gid as u32) {
                self.report.warnings.push(format!("{}: chown: {e}", p.display()));
            }
        }
        if o.kind != "symlink" {
            if let Err(e) = std::fs::set_permissions(p, std::fs::Permissions::from_mode(o.mode as u32 & 0o7777)) {
                self.report.warnings.push(format!("{}: chmod: {e}", p.display()));
            }
        }
        if let Err(e) = meta::set_times(p, o.atime_ns, o.mtime_ns) {
            self.report.warnings.push(format!("{}: times: {e}", p.display()));
        }
    }
}

/// Takes a piece's bytes (with the file offset they start at).
pub type PieceSink<'a> = dyn FnMut(&Piece, u64, &[u8]) -> Result<()> + 'a;

/// Read the pieces stored on cartridge `barcode` (loaded in `drive`, sorted by position)
/// through the read-ahead thread, check each against its BLAKE3, and hand its bytes to
/// `sink` (with the file offset they start at). Integrity failures are collected in
/// `errors` by piece item; anything else stops the read.
#[allow(clippy::too_many_arguments)]
pub fn read_volume(
    drive: &mut dyn TapeDrive,
    barcode: &str,
    pieces: &[Piece],
    read_buffer: u64,
    pace: Option<crate::ringbuf::Pace>,
    deks: &HashMap<i64, Key>,
    errors: &mut Vec<(usize, String)>,
    sink: &mut PieceSink,
) -> Result<crate::readahead::ReadStats> {
    let label = read_label(drive)?;
    if label.barcode != barcode {
        bail!("loaded cartridge is labelled {}, expected {barcode}", label.barcode);
    }
    let gap_blocks = (READ_THROUGH_BYTES / label.block_size as u64).max(1);
    let payload = (label.block_size as u64).saturating_sub(ltoarc_format::HEADER_LEN as u64).max(1);
    let ranges: Vec<(u8, u64, u64)> = pieces
        .iter()
        .map(|p| {
            let first = p.placement.block as u64;
            (p.placement.partition as u8, first, first + (p.placement.offset as u64 + p.record_len).div_ceil(payload))
        })
        .collect();
    let extents = readahead::extents(&ranges, gap_blocks);
    let mut ext_of = Vec::with_capacity(ranges.len());
    let mut k = 0;
    for r in &ranges {
        while !(extents[k].partition == r.0 && r.1 >= extents[k].start && r.1 <= extents[k].end.unwrap_or(u64::MAX)) {
            k += 1;
        }
        ext_of.push(k);
    }
    let ra = ReadAhead::paced(read_buffer, pace, None);
    let bs = label.block_size as usize;
    let (stats, result) = std::thread::scope(|s| {
        let reader = s.spawn(|| ra.read(drive, bs, &extents));
        let r = consume(pieces, &ext_of, &ra, deks, errors, sink);
        ra.cancel();
        (reader.join(), r)
    });
    let stats = stats.map_err(|_| anyhow!("tape reader thread panicked"))?;
    result?;
    Ok(stats)
}

fn consume(
    pieces: &[Piece],
    ext_of: &[usize],
    ra: &ReadAhead,
    deks: &HashMap<i64, Key>,
    errors: &mut Vec<(usize, String)>,
    sink: &mut PieceSink,
) -> Result<()> {
    let mut rd = StreamReader::idle(Blocks::new(ra));
    // (extent, stream start) the reader is in.
    let mut cur: Option<(usize, i64)> = None;
    // Last PACK read: several files usually come out of one record.
    let mut pack: Option<PackCache> = None;
    for (i, p) in pieces.iter().enumerate() {
        let key = (p.placement.partition, p.placement.block, p.placement.offset);
        if let (Some(off), Some((k, raw, members))) = (p.chunk.pack_offset, &pack) {
            if *k == key {
                let data = pack_member(p, off, raw, members, errors)?;
                sink(p, 0, &data)?;
                continue;
            }
        }
        let target = RecordPos { block_seq: (p.placement.block - p.stream_start) as u64, offset: p.placement.offset as u32 };
        let same = cur == Some((ext_of[i], p.stream_start)) && rd.position().is_some_and(|c| c <= target);
        if same {
            rd.seek_forward(target)?;
        } else {
            if !rd.source_mut().skip_to(p.placement.block as u64) {
                let why = rd.source_mut().error.take().unwrap_or_else(|| "the tape reader did not reach it".into());
                bail!("block {}: {why}", p.placement.block);
            }
            rd.restart_at(target.offset)?;
            cur = Some((ext_of[i], p.stream_start));
        }
        let rec = rd.next_record()?.ok_or_else(|| anyhow!("no record at block {}", p.placement.block))?;
        let body = crypt::record_body(deks.get(&p.session_id), rec.header.rtype, rec.header.flags, &rec.body)
            .with_context(|| format!("record at block {}", p.placement.block))?;
        if rec.header.rtype == rtype::PACK {
            let (h, members, stored) = decode_pack(&body)?;
            let raw = match h.codec {
                codec::STORED => stored.to_vec(),
                codec::ZSTD => zstd::bulk::decompress(stored, h.raw_len as usize)?,
                other => bail!("unknown codec {other}"),
            };
            if blake3::hash(&raw).as_bytes() != &h.blake3 {
                errors.push((p.item, format!("pack at block {} fails its BLAKE3 check", p.placement.block)));
            }
            let off = p.chunk.pack_offset.ok_or_else(|| anyhow!("catalog expects a chunk but tape holds a pack at block {}", p.placement.block))?;
            let data = pack_member(p, off, &raw, &members, errors)?;
            pack = Some((key, raw, members));
            sink(p, 0, &data)?;
            continue;
        }
        if rec.header.rtype != rtype::CHUNK {
            bail!("expected a chunk at block {} offset {}, found record type {}", p.placement.block, p.placement.offset, rec.header.rtype);
        }
        let h = ChunkHeader::decode(&body)?;
        if h.object as i64 != p.obj_seq || h.seq as i64 != p.chunk.seq {
            bail!("chunk at block {} belongs to object {} seq {}, expected {} seq {}", p.placement.block, h.object, h.seq, p.obj_seq, p.chunk.seq);
        }
        let stored = &body[CHUNK_HEADER_LEN..];
        let data = match h.codec {
            codec::STORED => stored.to_vec(),
            codec::ZSTD => zstd::bulk::decompress(stored, h.raw_len as usize)?,
            other => bail!("unknown codec {other}"),
        };
        if blake3::hash(&data).as_bytes() != &h.blake3 || h.blake3.as_slice() != p.chunk.blake3.as_slice() {
            errors.push((p.item, format!("chunk {} fails its BLAKE3 check", p.chunk.seq)));
        }
        sink(p, h.offset, &data)?;
    }
    Ok(())
}

/// One file's bytes out of a decoded pack, checked against tape and catalog.
fn pack_member(p: &Piece, off: i64, raw: &[u8], members: &[PackMember], errors: &mut Vec<(usize, String)>) -> Result<Vec<u8>> {
    let m = members
        .iter()
        .find(|m| m.object as i64 == p.obj_seq)
        .ok_or_else(|| anyhow!("pack at block {} has no member for object {}", p.placement.block, p.obj_seq))?;
    if m.offset as i64 != off || m.len as i64 != p.chunk.raw_len {
        bail!("pack member for object {} disagrees with the catalog", p.obj_seq);
    }
    let end = (m.offset + m.len) as usize;
    let data = raw.get(m.offset as usize..end).ok_or_else(|| anyhow!("pack member outside the pack"))?.to_vec();
    if blake3::hash(&data).as_bytes().as_slice() != m.blake3.as_slice() || m.blake3 != p.chunk.blake3 {
        errors.push((p.item, "packed data fails its BLAKE3 check".into()));
    }
    Ok(data)
}
