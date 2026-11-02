//! Writing a backup session to one cartridge, as a pipeline:
//!
//! ```text
//! walker ──► small-file workers ──(PACK units)──┐
//!        └─► large-file workers ──(chunk streams)┼─► sequencer ──blocks──► ring buffer ──► tape writer
//!        └─► directories, links, devices ────────┘   (catalog,             (hysteresis)     (owns drive)
//!                                                      records)
//! ```
//!
//! Small files are read whole and packed (with compression decided per pack); large files
//! stream in chunks, read ahead by one thread and hashed by one thread per content
//! algorithm. The sequencer is the only thread that touches the catalog: it numbers records,
//! records every chunk's tape position, and keeps each large file contiguous on tape.
//! Layout written per cartridge (FORMAT.md): session stream, filemark, volume catalog stream,
//! filemark.
//!
//! A session that outgrows its cartridge continues on the next one of the tape set: the
//! stream on the full cartridge ends with VOLUME_END, the ring is written out, the cartridge
//! gets its volume catalog and is marked clean, and the next cartridge starts with
//! SESSION_BEGIN (volume_seq + 1) and, if a large file was cut, a CONTINUED copy of its FILE
//! record. Cartridge memory marks each cartridge open while the session writes to it
//! (FORMAT.md §8.2), so a crash leaves a trail for `recover`.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use ltoarc_catalog::{Catalog, ChunkRow, HashRow, ObjectRow, Placement, SessionRow, SessionVolume, VolumeRow, ROOT_PATH};
use ltoarc_device::{Attribute, Position, TapeDrive};
use ltoarc_format::record::{
    codec, encode_pack, flags, rtype, to_cbor, ChunkHeader, FileEnd, FileKind, FileMeta, PackMember, SameAs, SessionBegin,
    SessionEnd, VolumeEnd, CHUNK_HEADER_LEN,
};
use ltoarc_format::stream::{RecordPos, StreamWriter};
use ltoarc_format::{BlockKind, VolumeLabel};
use serde_bytes::ByteBuf;
use uuid::Uuid;

use crate::compress::{self, Decision};
use crate::incr;
use crate::crypt::{self, Key, Sealer};
use crate::hash::{self, Algo, Kind, Mode, Policy};
use crate::meta::{self, Names};
use crate::ratelimit::RateLimit;
use crate::ringbuf::{self, Ring, SharedDrive, WriterStats};
use crate::tapeio::{self, read_label};
use crate::vci;
use crate::walk::{WalkOptions, Walker};

pub mod copy;
pub mod from_source;

pub const DEFAULT_CHUNK: usize = 16 << 20;

pub struct Options {
    pub job: Option<String>,
    pub sources: Vec<PathBuf>,
    pub walk: WalkOptions,
    pub hash: Policy,
    pub compression: compress::Policy,
    pub chunk_size: usize,
    pub copy: u8,
    pub ring: ringbuf::Config,
    /// Files up to this size are packed together into PACK records.
    pub pack_max: u64,
    /// Raw size at which a pack is closed.
    pub pack_target: usize,
    pub small_workers: usize,
    pub large_workers: usize,
    /// Read sources at no more than this many bytes per second.
    pub read_limit: Option<u64>,
    /// Let the drive compress (hardware). Off by default: incompressible media gains
    /// nothing, and with it off capacity and throughput are predictable.
    pub drive_compression: bool,
    /// Packed small files held in memory while large files stream (default: a quarter of
    /// the ring buffer). Stacked packs are written in one go between large files.
    pub stack: Option<u64>,
    /// At the start, how long small files wait for the walker to find large files.
    pub startup_window: Duration,
    /// Encrypt with this key (from a key file): every record but SESSION_BEGIN, and the
    /// volume catalogs.
    pub encrypt: Option<Key>,
    /// What the session compares against (incremental sessions need `job`).
    pub level: incr::Level,
    /// Which earlier data a new or changed file may refer to instead of being written.
    pub dedup: incr::Dedup,
    /// Continue the newest session of the job (or of these sources) that ended partial.
    pub resume: bool,
    /// Counters the caller shows progress from (bytes buffered and written to tape).
    pub meter: Option<std::sync::Arc<ringbuf::Meter>>,
    pub trust: incr::Trust,
    /// Never refer to data in other sessions (restores need no other session's tapes).
    pub self_contained: bool,
    /// Append-only sessions: leave files modified this recently for the next session.
    pub settle: Option<Duration>,
    /// Copies the session writes (copy..copy+copies-1): a file refers to earlier data only
    /// when every one of them holds it.
    pub copies: u8,
    /// Archive paths under another name: (real prefix, archived prefix), e.g. a ZFS snapshot
    /// directory recorded as the live tree it is a snapshot of, so every run of a job names
    /// its files the same way.
    pub map: Vec<(PathBuf, PathBuf)>,
    /// Commands whose output is archived as a file (a stream source: `zfs send`,
    /// `pg_basebackup -Ft -D -`, ...).
    pub streams: Vec<StreamSource>,
}

/// A command whose standard output is archived as a file at `path`.
#[derive(Debug, Clone)]
pub struct StreamSource {
    pub path: PathBuf,
    /// Run with /bin/sh -c.
    pub command: String,
}

impl Options {
    /// The name `path` is archived under.
    pub fn archived(&self, path: &Path) -> PathBuf {
        swap_prefix(path, self.map.iter().map(|(r, a)| (r.as_path(), a.as_path())))
    }

    /// Where an archived path is on disk for this session.
    pub fn real(&self, archived: &Path) -> PathBuf {
        swap_prefix(archived, self.map.iter().map(|(r, a)| (a.as_path(), r.as_path())))
    }

    pub fn new(sources: Vec<PathBuf>) -> Self {
        Options {
            job: None,
            sources,
            walk: WalkOptions::default(),
            hash: Policy { profiles: hash::resolve_profiles(&["blake3".into()], &Default::default()).unwrap(), ..Default::default() },
            compression: compress::Policy::default(),
            chunk_size: DEFAULT_CHUNK,
            copy: 1,
            ring: ringbuf::Config::default_for_host(),
            pack_max: 1 << 20,
            pack_target: 16 << 20,
            small_workers: 8,
            large_workers: 2,
            read_limit: None,
            drive_compression: false,
            stack: None,
            startup_window: Duration::from_secs(3),
            encrypt: None,
            level: incr::Level::Full,
            dedup: incr::Dedup::Moves,
            resume: false,
            meter: None,
            trust: incr::Trust::Trust,
            self_contained: false,
            settle: None,
            copies: 1,
            map: Vec::new(),
            streams: Vec::new(),
        }
    }
}

/// Progress callbacks (all optional).
pub trait Progress {
    fn entry(&mut self, _path: &Path, _size: u64) {}
    fn data(&mut self, _raw: u64, _stored: u64) {}
    fn warn(&mut self, _msg: &str) {}
    /// Copy `copy` of the session starts writing to a cartridge (`seq` 1 for the copy's first).
    fn volume(&mut self, _barcode: &str, _copy: u8, _seq: u32) {}
}

pub struct NoProgress;
impl Progress for NoProgress {}

/// The cartridge a session continues on.
pub struct Next {
    pub volume: VolumeRow,
    /// The drive holding it, when not the one the session was writing to.
    pub drive: Option<Box<dyn TapeDrive>>,
    /// Pacing for it (None: full speed).
    pub pace: Option<ringbuf::Pace>,
}

/// Supplies further cartridges when a session outgrows the one it is writing.
pub trait NextVolume {
    /// Whether the tape set has another cartridge for lane `lane` (one drive of the session) to
    /// continue on.
    fn available(&mut self, lane: usize) -> bool;
    /// Put away the full cartridge in lane `lane`'s `drive` and get the next one of the set
    /// ready to append to, in that drive or another. The catalog is outside a transaction.
    fn next(&mut self, lane: usize, drive: &mut dyn TapeDrive, full: &VolumeRow) -> Result<Next>;
}

/// Sessions that end when their one cartridge is full.
pub struct SingleVolume;

impl NextVolume for SingleVolume {
    fn available(&mut self, _lane: usize) -> bool {
        false
    }

    fn next(&mut self, _lane: usize, _drive: &mut dyn TapeDrive, full: &VolumeRow) -> Result<Next> {
        bail!("{} is full and no further cartridge was given", full.barcode)
    }
}

/// One cartridge's part of a session.
#[derive(Debug, Clone, Default)]
pub struct VolumeUse {
    /// Which copy of the session the cartridge holds.
    pub copy: u8,
    /// The lane (drive of the session) that wrote it; lane 0 is copy 1's first drive.
    pub lane: usize,
    pub barcode: String,
    pub volume_id: i64,
    pub start_object: u64,
    pub end_object: u64,
    pub catalog_object: Option<u64>,
    pub catalog_bytes: u64,
}

#[derive(Debug, Default)]
pub struct Report {
    pub session_uuid: Uuid,
    pub session_id: i64,
    pub status: String,
    pub files: u64,
    pub dirs: u64,
    pub others: u64,
    pub packs: u64,
    pub bytes: u64,
    pub stored: u64,
    /// Cartridges written, in order.
    pub volumes: Vec<VolumeUse>,
    /// Stopped early: the last cartridge reached its end and the set had no further one.
    pub early_warning: bool,
    pub warnings: Vec<String>,
    /// Files whose computed content hash differs from the digest in their xattr (verify mode).
    pub mismatches: Vec<String>,
    /// Files that changed while being read.
    pub changed: Vec<String>,
    /// Writer statistics of the first drive, and of each drive.
    pub writer: WriterStats,
    pub writers: Vec<WriterStats>,
    /// Drive-reported tape starts while writing the session's data, all drives (IBM LP 37h).
    pub drive_write_cycles: Option<u64>,
    /// Level the session ran at, and the session it builds on.
    pub level: String,
    pub parent: Option<Uuid>,
    /// Entries unchanged since the session it builds on (not read, not written).
    pub unchanged: u64,
    /// Paths recorded as deleted.
    pub whiteouts: u64,
    /// Files stored by reference to data already on tape (moved or duplicate), and their size.
    pub referenced: u64,
    pub referenced_bytes: u64,
    /// Append-only: files left for later because they were modified too recently, and
    /// archived files that have changed since (not saved again).
    pub settling: u64,
    pub changed_archived: Vec<String>,
    pub elapsed: Duration,
}

/// A file read completely by a worker (small, empty, unreadable, or the end of a large one).
struct Prepared {
    path: PathBuf,
    meta: FileMeta,
    size: u64,
    status: &'static str,
    digests: Vec<(String, String, String)>,
    mismatches: Vec<String>,
    /// Stored by reference: the object whose chunks hold the same data.
    data_from: Option<i64>,
}

struct PackUnit {
    members: Vec<(Prepared, u64, [u8; 32])>,
    raw_len: u64,
    codec: u8,
    stored: Vec<u8>,
    blake3: [u8; 32],
}

enum LargeMsg {
    Chunk { seq: u32, offset: u64, raw_len: u64, codec: u8, stored: Vec<u8>, blake3: [u8; 32] },
    End(Box<Prepared>),
}

enum Unit {
    /// Directory, symlink, hard link, device: a FILE record only.
    Meta(Box<FileMeta>),
    /// File with no data records (empty or unreadable): FILE and FILE_END.
    Done(Box<Prepared>),
    Pack(Box<PackUnit>),
    Large(Box<FileMeta>, PathBuf, Receiver<LargeMsg>),
    Warn(String),
}

struct Job {
    path: PathBuf,
    md: Metadata,
    id: u64,
    /// Look for the same data on tape before writing the file.
    probe: Option<Probe>,
}

/// Where a new or changed file's data may already be on tape.
struct Probe {
    /// Archived files of its size whose paths are gone (it may have moved from one).
    moves: Vec<incr::Candidate>,
    /// Any archived file with the same content will do.
    all: bool,
}

struct WriteBack {
    path: PathBuf,
    object_id: i64,
    mtime_ns: i64,
    size: u64,
    xattrs: Vec<(ByteBuf, ByteBuf)>,
}

/// Put `extra` over `base` (same name: replaced).
fn merge_xattrs(base: &[(ByteBuf, ByteBuf)], extra: &[(ByteBuf, ByteBuf)]) -> Vec<(ByteBuf, ByteBuf)> {
    let mut out: Vec<(ByteBuf, ByteBuf)> = base.iter().filter(|(n, _)| !extra.iter().any(|(e, _)| e == n)).cloned().collect();
    out.extend(extra.iter().cloned());
    out
}

fn swap_prefix<'p>(path: &Path, pairs: impl Iterator<Item = (&'p Path, &'p Path)>) -> PathBuf {
    for (from, to) in pairs {
        if let Ok(rest) = path.strip_prefix(from) {
            return if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
        }
    }
    path.to_path_buf()
}

fn bytes(p: &Path) -> ByteBuf {
    ByteBuf::from(p.as_os_str().as_bytes().to_vec())
}

fn session_id64(u: &Uuid) -> u64 {
    u64::from_le_bytes(u.as_bytes()[..8].try_into().unwrap())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Workers

fn fingerprints(opts: &Options, path: &Path, m: &mut FileMeta, warn: &mut Vec<String>) {
    for p in opts.hash.profiles.iter().filter(|p| p.kind == Kind::Fingerprint) {
        let digest = match opts.hash.mode {
            Mode::Generate if p.command.is_some() => hash::run_command(p, path).map(Some),
            _ => match hash::fresh_xattr(path, p, m.mtime_ns, m.size) {
                Some(d) => Ok(Some(d)),
                None if p.command.is_some() => hash::run_command(p, path).map(Some),
                None => Ok(None),
            },
        };
        match digest {
            Ok(Some(d)) => {
                m.fingerprints.insert(p.name.clone(), d);
            }
            Ok(None) => {}
            Err(e) => warn.push(format!("{}: {e}", path.display())),
        }
    }
}

/// External-command content hashes and verify-mode comparison against xattrs.
fn finish_digests(opts: &Options, path: &Path, m: &FileMeta, prep: &mut Prepared, warn: &mut Vec<String>) {
    for p in opts.hash.profiles.iter().filter(|p| p.kind == Kind::Content && p.command.is_some()) {
        let existing = (opts.hash.mode == Mode::UseExisting).then(|| hash::fresh_xattr(path, p, m.mtime_ns, m.size)).flatten();
        match existing.map(Ok).unwrap_or_else(|| hash::run_command(p, path)) {
            Ok(d) => prep.digests.push((p.name.clone(), p.algo_name(), d)),
            Err(e) => warn.push(format!("{}: {e}", path.display())),
        }
    }
    if opts.hash.mode == Mode::Verify {
        for (profile, _, digest) in &prep.digests {
            let Some(p) = opts.hash.profiles.iter().find(|p| &p.name == profile) else { continue };
            if let Some(x) = hash::fresh_xattr(path, p, m.mtime_ns, m.size) {
                if &x != digest {
                    prep.mismatches.push(format!("{}: {profile} xattr {x} but data hashes to {digest}", path.display()));
                }
            }
        }
    }
}

/// What a file's xattrs say about its data being on tape already.
enum Known {
    /// Stored by reference.
    Same(Box<Prepared>),
    /// A fingerprint matched this archived file; its content hash has to confirm it.
    Confirm(incr::Candidate),
    Nothing,
}

/// An archived file with the given digest that a probe allows referring to.
fn lookup(opts: &Options, reader: Option<&Catalog>, probe: &Probe, size: u64, algo: &str, digest: &str) -> Option<incr::Candidate> {
    // Every copy this session writes must hold the data referred to.
    let on_all = |id: i64| match reader {
        Some(r) => (0..opts.copies.max(1)).all(|i| r.placed(id, (opts.copy + i) as i64).unwrap_or(false)),
        None => opts.copies <= 1,
    };
    if let Some(c) = probe.moves.iter().find(|c| c.matches(algo, digest) && on_all(c.data_object)) {
        return Some(c.clone());
    }
    if !probe.all {
        return None;
    }
    let r = reader?;
    let (o, uuid) = r.find_content(algo, digest, size as i64, opts.copy as i64).ok()??;
    if !on_all(o.id) {
        return None;
    }
    Some(incr::Candidate { data_object: o.id, session: uuid.parse().ok()?, seq: o.seq as u64, size, hashes: r.hashes(o.id).ok()? })
}

/// Look for the file's data on tape by the digests its xattrs hold (no read).
fn same_known(opts: &Options, reader: Option<&Catalog>, job: &Job, m: &FileMeta) -> Known {
    let Some(probe) = &job.probe else { return Known::Nothing };
    for p in &opts.hash.profiles {
        let digest = match p.kind {
            Kind::Fingerprint => m.fingerprints.get(&p.name).cloned(),
            Kind::Content => hash::fresh_xattr(&job.path, p, m.mtime_ns, m.size),
        };
        let Some(d) = digest else { continue };
        let Some(c) = lookup(opts, reader, probe, m.size, &p.algo_name(), &d) else { continue };
        if p.kind == Kind::Fingerprint && opts.trust == incr::Trust::Confirm {
            return Known::Confirm(c);
        }
        return Known::Same(Box::new(referenced(opts, job, m, c, Vec::new())));
    }
    Known::Nothing
}

/// Whether hashing the file may find its data on tape: a candidate of its size exists.
fn worth_hashing(job: &Job, reader: Option<&Catalog>, confirm: &Option<incr::Candidate>) -> bool {
    let Some(probe) = &job.probe else { return false };
    confirm.is_some() || !probe.moves.is_empty() || (probe.all && reader.is_some_and(|r| r.any_file_of_size(job.md.len() as i64).unwrap_or(false)))
}

/// Look for the file's data on tape by its content digests `computed`.
fn same_hashed(opts: &Options, reader: Option<&Catalog>, job: &Job, m: &FileMeta, confirm: Option<incr::Candidate>, computed: &[(String, String, String)]) -> Option<Prepared> {
    let probe = job.probe.as_ref()?;
    for (_, algo, digest) in computed {
        let c = match &confirm {
            Some(c) => c.matches(algo, digest).then(|| c.clone()),
            None => lookup(opts, reader, probe, m.size, algo, digest),
        };
        if let Some(c) = c {
            return Some(referenced(opts, job, m, c, computed.to_vec()));
        }
    }
    None
}

/// The file stored by reference to candidate `c`'s data, with the content digests known
/// for it (computed now, or the archived file's under this job's profiles).
fn referenced(opts: &Options, job: &Job, m: &FileMeta, c: incr::Candidate, computed: Vec<(String, String, String)>) -> Prepared {
    let mut digests = computed;
    for p in opts.hash.profiles.iter().filter(|p| p.kind == Kind::Content) {
        if digests.iter().any(|(n, _, _)| n == &p.name) {
            continue;
        }
        if let Some(h) = c.hashes.iter().find(|h| h.kind == "content" && h.algo == p.algo_name()) {
            digests.push((p.name.clone(), h.algo.clone(), h.digest.clone()));
        }
    }
    let (algo, digest) = digests
        .first()
        .map(|(_, a, d)| (a.clone(), d.clone()))
        .or_else(|| c.hashes.first().map(|h| (h.algo.clone(), h.digest.clone())))
        .unwrap_or_default();
    let mut meta = m.clone();
    meta.same_as = Some(SameAs { session: c.session, id: c.seq, algo, digest });
    Prepared { path: job.path.clone(), meta, size: m.size, status: "ok", digests, mismatches: Vec::new(), data_from: Some(c.data_object) }
}

/// Content digests of a whole file under `algos`: (profile, algorithm, hex digest).
fn hash_file(path: &Path, algos: &[(String, Algo)], limit: Option<&RateLimit>) -> std::io::Result<Vec<(String, String, String)>> {
    use std::io::Read;
    let mut f = File::open(path)?;
    let mut hashers: Vec<hash::Hasher> = algos.iter().map(|(_, a)| a.hasher()).collect();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if let Some(l) = limit {
            l.take(n);
        }
        for h in hashers.iter_mut() {
            h.update(&buf[..n]);
        }
    }
    Ok(algos.iter().zip(hashers).map(|((p, a), h)| (p.clone(), a.name().to_string(), h.finish())).collect())
}

fn changed_since(path: &Path, md: &Metadata) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(after) => meta::mtime_ns(&after) != meta::mtime_ns(md) || after.len() != md.len(),
        Err(_) => true,
    }
}

struct PackBuilder {
    compress: bool,
    data: Vec<u8>,
    members: Vec<(Prepared, u64, [u8; 32])>,
}

impl PackBuilder {
    fn new(compress: bool) -> Self {
        PackBuilder { compress, data: Vec::new(), members: Vec::new() }
    }

    fn finish(&mut self, policy: &compress::Policy) -> Option<PackUnit> {
        if self.members.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data);
        let blake3 = *blake3::hash(&data).as_bytes();
        let compressed = if self.compress { policy.compress(&data) } else { None };
        let (codec, stored) = match compressed {
            Some(c) => (codec::ZSTD, c),
            None => (codec::STORED, data.clone()),
        };
        Some(PackUnit { members: std::mem::take(&mut self.members), raw_len: data.len() as u64, codec, stored, blake3 })
    }
}

fn small_worker(opts: &Options, limit: Option<&RateLimit>, stop: &AtomicBool, reader: Option<Catalog>, jobs: Receiver<Job>, units: Sender<Unit>) {
    let reader = reader.as_ref();
    let mut names = Names::default();
    let mut packs = [PackBuilder::new(true), PackBuilder::new(false)];
    for job in jobs {
        if stop.load(Ordering::Relaxed) {
            continue;
        }
        let mut warn = Vec::new();
        let mut m = match meta::file_meta(&job.path, &job.md, job.id, &mut names) {
            Ok(m) => m,
            Err(e) => {
                let _ = units.send(Unit::Warn(format!("{}: {e}", job.path.display())));
                continue;
            }
        };
        m.path = bytes(&opts.archived(&job.path));
        fingerprints(opts, &job.path, &mut m, &mut warn);
        let confirm = match same_known(opts, reader, &job, &m) {
            Known::Same(r) => {
                let _ = units.send(Unit::Done(r));
                continue;
            }
            Known::Confirm(c) => Some(c),
            Known::Nothing => None,
        };
        let mut prep = Prepared {
            path: job.path.clone(),
            meta: m.clone(),
            size: 0,
            status: "ok",
            digests: Vec::new(),
            mismatches: Vec::new(),
            data_from: None,
        };
        if let Some(l) = limit {
            l.take(job.md.len() as usize);
        }
        let data = match std::fs::read(&job.path) {
            Ok(d) => d,
            Err(e) => {
                warn.push(format!("{}: {e}", job.path.display()));
                prep.status = "unreadable";
                for w in warn {
                    let _ = units.send(Unit::Warn(w));
                }
                let _ = units.send(Unit::Done(Box::new(prep)));
                continue;
            }
        };
        if data.len() as u64 != m.size || changed_since(&job.path, &job.md) {
            prep.status = "changed";
        }
        prep.size = data.len() as u64;
        prep.digests = opts.hash.inline_content().map(|(p, a)| (p.name.clone(), a.name().to_string(), a.digest(&data))).collect();
        if prep.status == "ok" && worth_hashing(&job, reader, &confirm) {
            if let Some(r) = same_hashed(opts, reader, &job, &m, confirm, &prep.digests) {
                let _ = units.send(Unit::Done(Box::new(r)));
                continue;
            }
        }
        finish_digests(opts, &job.path, &m, &mut prep, &mut warn);
        for w in warn {
            let _ = units.send(Unit::Warn(w));
        }
        if data.is_empty() {
            let _ = units.send(Unit::Done(Box::new(prep)));
            continue;
        }
        let blake = *blake3::hash(&data).as_bytes();
        let which = match opts.compression.decide(&job.path) {
            Decision::Store => 1,
            Decision::Compress | Decision::Probe => 0,
        };
        let pack = &mut packs[which];
        let offset = pack.data.len() as u64;
        pack.data.extend_from_slice(&data);
        pack.members.push((prep, offset, blake));
        if pack.data.len() >= opts.pack_target {
            if let Some(p) = pack.finish(&opts.compression) {
                if units.send(Unit::Pack(Box::new(p))).is_err() {
                    return;
                }
            }
        }
    }
    for pack in &mut packs {
        if let Some(p) = pack.finish(&opts.compression) {
            let _ = units.send(Unit::Pack(Box::new(p)));
        }
    }
}

enum HashMsg {
    Data(Arc<Vec<u8>>),
    Zeros(u64),
}

/// Decrements the count of outstanding large files when a job is finished.
struct Outstanding<'a>(&'a AtomicU64);

impl Drop for Outstanding<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn large_worker(
    opts: &Options,
    limit: Option<&RateLimit>,
    stop: &AtomicBool,
    pending: &AtomicU64,
    reader: Option<Catalog>,
    jobs: Receiver<Job>,
    units: Sender<Unit>,
) {
    let reader = reader.as_ref();
    let mut names = Names::default();
    for job in jobs {
        let _done = Outstanding(pending);
        if stop.load(Ordering::Relaxed) {
            continue;
        }
        let mut warn = Vec::new();
        let mut m = match meta::file_meta(&job.path, &job.md, job.id, &mut names) {
            Ok(m) => m,
            Err(e) => {
                let _ = units.send(Unit::Warn(format!("{}: {e}", job.path.display())));
                continue;
            }
        };
        m.path = bytes(&opts.archived(&job.path));
        if job.probe.is_some() {
            let mut warn = Vec::new();
            fingerprints(opts, &job.path, &mut m, &mut warn);
            for w in warn {
                let _ = units.send(Unit::Warn(w));
            }
            let confirm = match same_known(opts, reader, &job, &m) {
                Known::Same(r) => {
                    let _ = units.send(Unit::Done(r));
                    continue;
                }
                Known::Confirm(c) => Some(c),
                Known::Nothing => None,
            };
            if worth_hashing(&job, reader, &confirm) {
                let algos: Vec<(String, Algo)> = opts.hash.inline_content().map(|(p, a)| (p.name.clone(), a)).collect();
                match hash_file(&job.path, &algos, limit) {
                    Ok(computed) => {
                        if let Some(r) = same_hashed(opts, reader, &job, &m, confirm, &computed) {
                            let _ = units.send(Unit::Done(Box::new(r)));
                            continue;
                        }
                    }
                    Err(e) => {
                        let _ = units.send(Unit::Warn(format!("{}: {e}", job.path.display())));
                    }
                }
            }
        }
        let file = match File::open(&job.path) {
            Ok(f) => f,
            Err(e) => {
                let _ = units.send(Unit::Warn(format!("{}: {e}", job.path.display())));
                let prep = Prepared { path: job.path, meta: m, size: 0, status: "unreadable", digests: Vec::new(), mismatches: Vec::new(), data_from: None };
                let _ = units.send(Unit::Done(Box::new(prep)));
                continue;
            }
        };
        m.extents = meta::data_extents(&file, &job.md).unwrap_or(None);
        // SAFETY: advice on a valid descriptor; failure only loses the hint.
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        if job.probe.is_none() {
            fingerprints(opts, &job.path, &mut m, &mut warn);
        }
        for w in warn.drain(..) {
            let _ = units.send(Unit::Warn(w));
        }
        let (ctx, crx) = bounded(4);
        if units.send(Unit::Large(Box::new(m.clone()), job.path.clone(), crx)).is_err() {
            return;
        }
        let mut prep = stream_file(opts, limit, &job, &m, &file, &ctx, &mut warn);
        if let Some(p) = prep.as_mut() {
            finish_digests(opts, &job.path, &m, p, &mut warn);
        }
        for w in warn {
            let _ = units.send(Unit::Warn(w));
        }
        if let Some(p) = prep {
            let _ = ctx.send(LargeMsg::End(Box::new(p)));
        }
    }
}

/// Run a stream source and send its output as a large file: chunks as they come, the size
/// known at the end. A command that fails leaves the file "unreadable" (what it wrote is
/// kept, and the error is reported).
fn stream_source(opts: &Options, limit: Option<&RateLimit>, stop: &AtomicBool, pending: &AtomicU64, src: &StreamSource, id: u64, units: Sender<Unit>) {
    use std::io::Read;
    let _done = Outstanding(pending);
    let now_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    // SAFETY: getuid/getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let meta = FileMeta {
        id,
        path: bytes(&src.path),
        kind: FileKind::File,
        mode: 0o100600,
        uid,
        gid,
        user: None,
        group: None,
        size: 0,
        mtime_ns: now_ns,
        atime_ns: now_ns,
        ctime_ns: now_ns,
        dev: 0,
        ino: 0,
        nlink: 1,
        rdev: 0,
        target: None,
        xattrs: vec![(ByteBuf::from(b"user.ltoarc.stream".to_vec()), ByteBuf::from(src.command.as_bytes().to_vec()))],
        extents: None,
        fingerprints: Default::default(),
        same_as: None,
    };
    let mut child = match std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&src.command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = units.send(Unit::Warn(format!("{}: cannot run `{}`: {e}", src.path.display(), src.command)));
            return;
        }
    };
    let (ctx, crx) = bounded(4);
    if units.send(Unit::Large(Box::new(meta.clone()), src.path.clone(), crx)).is_err() {
        let _ = child.kill();
        return;
    }
    let mut out = child.stdout.take().expect("piped");
    let mut errout = child.stderr.take().expect("piped");
    let err_reader = std::thread::spawn(move || {
        let mut e = String::new();
        let _ = errout.read_to_string(&mut e);
        e
    });
    let algos: Vec<(String, Algo)> = opts.hash.inline_content().map(|(p, a)| (p.name.clone(), a)).collect();
    let mut hashers: Vec<hash::Hasher> = algos.iter().map(|(_, a)| a.hasher()).collect();
    let compress = opts.compression.decide(&src.path) != Decision::Store;
    let mut total = 0u64;
    let mut seq = 0u32;
    let mut status = "ok";
    let mut cut = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            cut = true;
            break;
        }
        let mut buf = vec![0u8; opts.chunk_size];
        let mut got = 0;
        while got < buf.len() {
            match out.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    let _ = units.send(Unit::Warn(format!("{}: reading the command's output: {e}", src.path.display())));
                    status = "unreadable";
                    break;
                }
            }
        }
        if got == 0 {
            break;
        }
        buf.truncate(got);
        if let Some(l) = limit {
            l.take(got);
        }
        for h in hashers.iter_mut() {
            h.update(&buf);
        }
        let blake = *blake3::hash(&buf).as_bytes();
        let compressed = if compress { opts.compression.compress(&buf) } else { None };
        let (cdc, stored) = match compressed {
            Some(c) => (codec::ZSTD, c),
            None => (codec::STORED, buf.clone()),
        };
        if ctx.send(LargeMsg::Chunk { seq, offset: total, raw_len: got as u64, codec: cdc, stored, blake3: blake }).is_err() {
            cut = true;
            break;
        }
        seq += 1;
        total += got as u64;
        if status != "ok" {
            break;
        }
    }
    if cut {
        let _ = child.kill();
    }
    let exit = child.wait();
    let stderr = err_reader.join().unwrap_or_default();
    match exit {
        Ok(st) if st.success() => {}
        Ok(st) => {
            status = "unreadable";
            let tail: String = stderr.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
            let _ = units.send(Unit::Warn(format!("{}: `{}` failed ({st}): {tail}", src.path.display(), src.command)));
        }
        Err(e) => {
            status = "unreadable";
            let _ = units.send(Unit::Warn(format!("{}: `{}`: {e}", src.path.display(), src.command)));
        }
    }
    let digests = if status == "ok" { algos.iter().zip(hashers).map(|((p, a), h)| (p.clone(), a.name().to_string(), h.finish())).collect() } else { Vec::new() };
    // The FILE record said size 0 (unknown then); FILE_END gives the size.
    let p = Prepared { path: src.path.clone(), meta, size: total, status, digests, mismatches: Vec::new(), data_from: None };
    let _ = ctx.send(LargeMsg::End(Box::new(p)));
}

/// Read, hash and send one large file. None when the sequencer stopped taking its chunks.
fn stream_file(
    opts: &Options,
    limit: Option<&RateLimit>,
    job: &Job,
    m: &FileMeta,
    file: &File,
    out: &Sender<LargeMsg>,
    warn: &mut Vec<String>,
) -> Option<Prepared> {
    let algos: Vec<(String, Algo)> = opts.hash.inline_content().map(|(p, a)| (p.name.clone(), a)).collect();
    let extents = m.extents.clone().unwrap_or_else(|| vec![(0, m.size)]);
    let chunk = opts.chunk_size;
    std::thread::scope(|s| {
        // Read ahead in its own thread.
        let (rtx, rrx) = bounded::<std::io::Result<(u64, Vec<u8>)>>(3);
        s.spawn(move || {
            for (start, len) in extents {
                let end = start + len;
                let mut pos = start;
                while pos < end {
                    let want = ((end - pos) as usize).min(chunk);
                    if let Some(l) = limit {
                        l.take(want);
                    }
                    let mut buf = vec![0u8; want];
                    let mut got = 0;
                    while got < want {
                        match file.read_at(&mut buf[got..], pos + got as u64) {
                            Ok(0) => break,
                            Ok(n) => got += n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(e) => {
                                let _ = rtx.send(Err(e));
                                return;
                            }
                        }
                    }
                    buf.truncate(got);
                    if rtx.send(Ok((pos, buf))).is_err() || got < want {
                        return;
                    }
                    pos += got as u64;
                }
            }
        });
        // One hashing thread per content algorithm.
        let mut hashers = Vec::new();
        for (profile, algo) in &algos {
            let (htx, hrx) = bounded::<HashMsg>(4);
            let algo = *algo;
            let h = s.spawn(move || {
                let mut h = algo.hasher();
                let zeros = vec![0u8; 1 << 20];
                for msg in hrx {
                    match msg {
                        HashMsg::Data(d) => h.update(&d),
                        HashMsg::Zeros(mut n) => {
                            while n > 0 {
                                let k = n.min(zeros.len() as u64) as usize;
                                h.update(&zeros[..k]);
                                n -= k as u64;
                            }
                        }
                    }
                }
                h.finish()
            });
            hashers.push((profile.clone(), algo, htx, h));
        }
        let decision = opts.compression.decide(&job.path);
        let mut compress_on: Option<bool> = match decision {
            Decision::Store => Some(false),
            Decision::Compress => Some(true),
            Decision::Probe => None,
        };
        let mut status = "ok";
        let mut hashed = 0u64;
        let mut total = 0u64;
        for (seq, item) in rrx.iter().enumerate() {
            let seq = seq as u32;
            let (offset, data) = match item {
                Ok(x) => x,
                Err(e) => {
                    warn.push(format!("{}: read error: {e}", job.path.display()));
                    status = "unreadable";
                    break;
                }
            };
            if data.is_empty() {
                status = "changed";
                break;
            }
            let data = Arc::new(data);
            for (_, _, htx, _) in &hashers {
                if offset > hashed {
                    let _ = htx.send(HashMsg::Zeros(offset - hashed));
                }
                let _ = htx.send(HashMsg::Data(data.clone()));
            }
            hashed = offset + data.len() as u64;
            let blake = *blake3::hash(&data).as_bytes();
            let on = *compress_on.get_or_insert_with(|| opts.compression.probe(&data));
            let compressed = if on { opts.compression.compress(&data) } else { None };
            let (cdc, stored) = match compressed {
                Some(c) => (codec::ZSTD, c),
                None => (codec::STORED, data.to_vec()),
            };
            let msg = LargeMsg::Chunk { seq, offset, raw_len: data.len() as u64, codec: cdc, stored, blake3: blake };
            if out.send(msg).is_err() {
                return None; // sequencer stopped (cartridge full or error)
            }
            total = hashed;
        }
        // A sparse file may end in a hole, so only dense files must have been read to the end.
        if status == "ok" && ((m.extents.is_none() && total != m.size) || changed_since(&job.path, &job.md)) {
            status = "changed";
        }
        let mut digests = Vec::new();
        let size = if status == "ok" { m.size } else { total };
        for (profile, algo, htx, h) in hashers {
            if size > hashed {
                let _ = htx.send(HashMsg::Zeros(size - hashed));
            }
            drop(htx);
            let d = h.join().unwrap_or_default();
            if status == "ok" || status == "changed" {
                digests.push((profile, algo.name().to_string(), d));
            }
        }
        Some(Prepared { path: job.path.clone(), meta: m.clone(), size, status, digests, mismatches: Vec::new(), data_from: None })
    })
}

// ---------------------------------------------------------------------------------------
// Sequencer

/// A large file being written to a lane.
struct OpenLarge {
    meta: Box<FileMeta>,
    path: PathBuf,
    oid: i64,
    rx: Receiver<LargeMsg>,
    written: u64,
}

/// One drive's share of a session: its ring buffer and writer thread, the cartridges it
/// fills, and its session stream on the current one.
struct Lane<'a, 'd> {
    ring: &'a Ring,
    drive: &'a SharedDrive<'d>,
    w: StreamWriter,
    volume: VolumeRow,
    label: VolumeLabel,
    /// SESSION_BEGIN on this lane's current cartridge.
    begin: SessionBegin,
    /// First block of the session stream on the current cartridge.
    start: u64,
    budget: Option<u64>,
    /// The drive's motion counters when the current cartridge started.
    perf_before: Option<DrivePerf>,
    /// What the drive reported while writing data on the lane's cartridges so far.
    motion: Option<Motion>,
    /// No cartridge left for this lane.
    full: bool,
    open: Option<OpenLarge>,
    /// Cartridges holding the open large file's data so far.
    file_volumes: Vec<String>,
    /// Pacing for the lane's drive (None: full speed).
    pace: Option<ringbuf::Pace>,
    /// Stored bytes routed to the lane, for balancing lanes by size.
    bytes: u64,
    /// Data records written to the current cartridge.
    data_records: u64,
    /// Copying: the FILE record (flags, body as on tape) of the file whose data is being
    /// copied, repeated with CONTINUED on a continuation cartridge.
    repeat: Option<(u16, Vec<u8>)>,
    /// The copy of the session this lane writes.
    copy: u8,
}

impl Lane<'_, '_> {
    /// The drive is writing and its buffer is getting low.
    fn starving(&self) -> bool {
        self.ring.started() && self.ring.buffered() < self.ring.config().high / 2
    }
}

struct Sequencer<'a, 'd> {
    cat: &'a Catalog,
    opts: &'a Options,
    next: &'a mut dyn NextVolume,
    stop: &'a AtomicBool,
    progress: &'a mut dyn Progress,
    lanes: Vec<Lane<'a, 'd>>,
    /// Lanes that take the same units: one lane per copy of the session (one slot per drive
    /// of a copy, when each copy is written through several drives).
    slots: Vec<Vec<usize>>,
    /// Ring buffer size and start threshold per lane.
    lane_ring: (u64, u64),
    sid64: u64,
    session_id: i64,
    /// Cartridges started so far by each copy (numbers each one's part within its copy).
    volume_seq: HashMap<u8, u32>,
    /// Every lane is out of cartridges: the session is closing.
    full: bool,
    /// A file was cut short because its lane ran out of cartridges.
    truncated: bool,
    /// Writing another copy of an existing session: its catalog row stays as it is.
    copying: bool,
    /// Seals record bodies with the session's data key when encrypting.
    sealer: Option<Sealer>,
    dir_ids: HashMap<PathBuf, i64>,
    writeback: Vec<WriteBack>,
    report: Report,
    /// Small-file units waiting while large files stream.
    small_rx: Receiver<Unit>,
    small_open: bool,
    stack: VecDeque<Unit>,
    stack_bytes: u64,
    stack_limit: u64,
}

fn unit_bytes(u: &Unit) -> u64 {
    match u {
        Unit::Pack(p) => p.stored.len() as u64,
        _ => 0,
    }
}

impl Sequencer<'_, '_> {
    /// Append a record to lane `l` (sealed when the session is encrypted).
    fn record(&mut self, l: usize, rtype: u16, rflags: u16, parts: &[&[u8]]) -> RecordPos {
        let w = &mut self.lanes[l].w;
        match &mut self.sealer {
            Some(s) => {
                let body = s.seal_parts(&crypt::record_aad(rtype), parts);
                w.append(rtype, rflags | flags::ENCRYPTED, &[&body])
            }
            None => w.append(rtype, rflags, parts),
        }
    }

    fn warn(&mut self, msg: String) {
        self.progress.warn(&msg);
        self.report.warnings.push(msg);
    }

    /// The slot without a large file in progress that has taken the fewest bytes.
    fn free_slot(&self) -> Option<usize> {
        (0..self.slots.len())
            .filter(|&s| self.slots[s].iter().all(|&l| !self.lanes[l].full && self.lanes[l].open.is_none()))
            .min_by_key(|&s| self.lanes[self.slots[s][0]].bytes)
    }

    /// The slot (with cartridges left) that has taken the fewest bytes.
    fn least_slot(&self) -> Option<usize> {
        (0..self.slots.len()).filter(|&s| self.slots[s].iter().all(|&l| !self.lanes[l].full)).min_by_key(|&s| self.lanes[self.slots[s][0]].bytes)
    }

    /// The slot lane `l` belongs to.
    fn slot_of(&self, l: usize) -> usize {
        self.slots.iter().position(|s| s.contains(&l)).expect("every lane is in a slot")
    }

    /// Get the cartridge in lane `l`'s drive ready for the session: check its label and
    /// coherency record, find the end of data, size the ring and the capacity budget, mark
    /// it open, and start its session stream (with the file in progress, on a continuation).
    fn start_volume(&mut self, l: usize, volume: VolumeRow) -> Result<()> {
        let shared = self.lanes[l].drive;
        let mut guard = shared.lock().unwrap();
        let drive: &mut dyn TapeDrive = guard.get();
        // A cartridge stays encrypted (with one key) or plaintext, as its first session was.
        let want = self.lanes[l].begin.encryption.as_ref().map(|w| w.key_id.clone());
        for e in self.cat.volume_encryption(volume.id)?.unwrap_or_default() {
            let have = e.map(|j| crypt::from_json(&j).map(|w| w.key_id)).transpose()?;
            if have != want {
                bail!(
                    "{} holds {} sessions; this one is {}",
                    volume.barcode,
                    have.map_or("plaintext".to_string(), |k| format!("encrypted (key {k})")),
                    want.map_or("plaintext".to_string(), |k| format!("encrypted with key {k}"))
                );
            }
        }
        match vci::state(drive)? {
            vci::State::Open(v) => bail!(
                "{0} has an unfinished session (from block {1}); run `ltoarc tape recover {0}`",
                volume.barcode,
                v.open.map_or(0, |o| o.start)
            ),
            vci::State::Stale(_) => {
                self.warn(format!("{}: something other than ltoarc wrote to it after its last catalog", volume.barcode))
            }
            _ => {}
        }
        // Which volume this is and its block size: from the end of its last volume catalog
        // when it has one (no trip back to the label), else from the label.
        let at_end = match volume.block_size {
            Some(bs) => match tapeio::identify_at_end(drive)? {
                Some((uuid, eod)) if volume.uuid.as_deref() == Some(uuid.to_string().as_str()) => {
                    let mut label = VolumeLabel::new(&volume.barcode, volume.generation.as_deref().unwrap_or(""), volume.pool.clone(), String::new());
                    label.volume_uuid = uuid;
                    label.block_size = bs as u32;
                    Some((label, eod))
                }
                _ => None,
            },
            None => None,
        };
        let (label, start) = match at_end {
            Some(found) => found,
            None => {
                let label = read_label(drive)?;
                check_label(&label, &volume)?;
                // Remembered, so the next append need not come back here.
                if let Some(mut v) = self.cat.volume(&volume.barcode)? {
                    if v.block_size != Some(label.block_size as i64) {
                        v.block_size = Some(label.block_size as i64);
                        self.cat.update_volume(&v)?;
                    }
                }
                (label, drive.locate_eod(0).context("locating end of data")?.pos.object)
            }
        };
        if start < 2 {
            bail!("{}: end of data at block {start}, so the cartridge has no label file", volume.barcode);
        }
        if drive.compression().ok() != Some(self.opts.drive_compression) {
            drive.set_compression(self.opts.drive_compression).with_context(|| {
                format!("turning drive compression {}", if self.opts.drive_compression { "on" } else { "off" })
            })?;
        }
        // Never buffer more than a quarter of what still fits on the cartridge.
        let capacity = remaining_capacity(drive);
        let (ring_cap, ring_high) = self.lane_ring;
        let (cap, high) = match capacity {
            Some((remaining, _)) => {
                let cap = ring_cap.min((remaining / 4).max(4 << 20));
                (cap, ring_high.min(cap / 2))
            }
            None => (ring_cap, ring_high),
        };
        let lane = &mut self.lanes[l];
        lane.ring.new_volume(cap, high, lane.pace);
        // Take data while there is still room for the buffer and for the trailer and volume
        // catalog after it (at least 1% of the cartridge). Data records are checked before
        // they are appended (`make_room`), so none crosses the line.
        let tail = 4 * label.block_size as u64 + (1 << 20);
        lane.budget = capacity.map(|(remaining, max)| remaining.saturating_sub(cap + tail.max(max / 100)));
        lane.data_records = 0;
        lane.perf_before = drive_perf(drive);
        if let Err(e) = vci::mark_open(drive, label.volume_uuid, self.sid64, start) {
            self.warn(format!("{}: cannot mark the session open in cartridge memory: {e:#}", volume.barcode));
        }
        drop(guard);
        let copy = self.lanes[l].copy;
        let seqno = {
            let n = self.volume_seq.entry(copy).or_default();
            *n += 1;
            *n
        };
        self.progress.volume(&volume.barcode, copy, seqno);
        let lane = &mut self.lanes[l];
        lane.begin.volume = label.volume_uuid;
        lane.begin.volume_seq = seqno;
        lane.w = StreamWriter::new(label.block_size, label.volume_uuid, self.sid64, BlockKind::Data);
        lane.w.append(rtype::SESSION_BEGIN, 0, &[&to_cbor(&lane.begin)]);
        if lane.open.is_some() {
            lane.file_volumes.push(volume.barcode.clone());
        }
        lane.volume = volume;
        lane.label = label;
        lane.start = start;
        if let Some(m) = self.lanes[l].open.as_ref().map(|o| o.meta.clone()) {
            self.record(l, rtype::FILE, flags::CONTINUED, &[&to_cbor(&*m)]);
        } else if let Some((f, body)) = self.lanes[l].repeat.clone() {
            self.lanes[l].w.append(rtype::FILE, f | flags::CONTINUED, &[&body]);
        }
        Ok(())
    }

    /// Close lane `l`'s session stream on its current cartridge (its last record already
    /// appended): write out the ring, end the file, commit the catalog, append the volume
    /// catalog, and mark the cartridge clean in cartridge memory.
    fn end_volume(&mut self, l: usize, status: &str, full: bool) -> Result<()> {
        if !self.cat.in_transaction() {
            self.cat.begin()?;
        }
        let lane = &mut self.lanes[l];
        lane.w.finish();
        while let Some(b) = lane.w.take_block() {
            lane.ring.push(b).map_err(|e| anyhow!(e))?;
        }
        lane.ring.drain().map_err(|e| anyhow!(e))?;
        let shared = lane.drive;
        let mut guard = shared.lock().unwrap();
        let drive: &mut dyn TapeDrive = guard.get();
        drive.write_filemarks(1).context("writing filemark")?;
        let lane = &self.lanes[l];
        let (start, end_object) = (lane.start, lane.start + lane.w.blocks());
        let (volume, label, seqno) = (lane.volume.clone(), lane.label.clone(), lane.begin.volume_seq);
        check_position(drive, end_object + 1)?;
        if let (Some(a), Some(b)) = (lane.perf_before, drive_perf(drive)) {
            let m = Motion::between(&a.write, &b.write);
            *self.report.drive_write_cycles.get_or_insert(0) += m.cycles;
            self.lanes[l].motion.get_or_insert_with(Motion::default).merge(&m);
        }
        if !self.copying {
            self.cat.update_session(&SessionRow {
                id: self.session_id,
                ended: Some(now()),
                status: status.into(),
                files: self.report.files as i64,
                bytes: self.report.bytes as i64,
                stored_bytes: self.report.stored as i64,
                ..Default::default()
            })?;
        }
        let mut sv = SessionVolume {
            session_id: self.session_id,
            copy: self.lanes[l].copy as i64,
            seq: seqno as i64,
            volume_id: volume.id,
            partition: 0,
            start_object: start as i64,
            end_object: Some(end_object as i64),
            catalog_object: None,
            verified_at: None,
        };
        self.cat.add_session_volume(&sv)?;
        self.cat.commit()?;
        let mut used = VolumeUse { copy: self.lanes[l].copy, lane: l, barcode: volume.barcode.clone(), volume_id: volume.id, start_object: start, end_object, ..Default::default() };
        // The volume catalog needs the committed rows.
        match write_volume_catalog(self.cat, drive, &volume, &label, self.sid64, end_object + 1, self.opts.encrypt.as_ref()) {
            Ok(bytes) => {
                sv.catalog_object = Some(end_object as i64 + 1);
                self.cat.add_session_volume(&sv)?;
                used.catalog_object = Some(end_object + 1);
                used.catalog_bytes = bytes;
                if let Err(e) = vci::mark_clean(drive, label.volume_uuid, end_object + 1) {
                    self.warn(format!("{}: cannot mark the cartridge clean in cartridge memory: {e:#}", volume.barcode));
                }
            }
            Err(e) => self.warn(format!("{}: volume catalog not written to tape: {e:#}", volume.barcode)),
        }
        let mut v = self.cat.volume(&volume.barcode)?.unwrap_or_else(|| volume.clone());
        if v.state == "scratch" || v.state == "new" {
            v.state = "filling".into();
        }
        if full {
            v.state = "full".into();
        }
        self.cat.update_volume(&v)?;
        // Best effort: DATE AND TIME LAST WRITTEN in cartridge memory.
        let _ = drive.write_attributes(0, &[Attribute::ascii(0x0804, &chrono::Utc::now().format("%Y%m%d%H%M").to_string(), 12)]);
        self.report.volumes.push(used);
        Ok(())
    }

    /// Lane `l`'s cartridge is full: end this part of the session and continue on the next.
    fn switch_volume(&mut self, l: usize) -> Result<()> {
        let end = VolumeEnd {
            session: self.lanes[l].begin.session,
            volume_seq: self.lanes[l].begin.volume_seq,
            files: self.report.files,
            bytes: self.report.bytes,
            stored: self.report.stored,
            ended: now(),
        };
        self.record(l, rtype::VOLUME_END, 0, &[&to_cbor(&end)]);
        self.end_volume(l, "partial", true)?;
        let full = self.lanes[l].volume.clone();
        let shared = self.lanes[l].drive;
        let next = {
            let mut guard = shared.lock().unwrap();
            let next = self.next.next(l, guard.get(), &full).with_context(|| format!("continuing after {}", full.barcode))?;
            if let Some(d) = next.drive {
                guard.replace(d);
            }
            self.lanes[l].pace = next.pace;
            next.volume
        };
        self.cat.begin()?;
        self.start_volume(l, next)
    }

    /// Take whatever small-file units are ready into the stack (never blocks).
    fn absorb_small(&mut self) {
        while self.small_open && self.stack_bytes < self.stack_limit {
            match self.small_rx.try_recv() {
                Ok(u) => {
                    self.stack_bytes += unit_bytes(&u);
                    self.stack.push_back(u);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.small_open = false,
            }
        }
    }

    /// Write stacked units, each to the least loaded free lane, until at most `keep` bytes
    /// remain stacked (or no lane is free).
    fn write_stack(&mut self, keep: u64) -> Result<()> {
        while self.stack_bytes > keep || (keep == 0 && !self.stack.is_empty()) {
            let Some(slot) = self.free_slot() else { break };
            let Some(u) = self.stack.pop_front() else { break };
            self.stack_bytes -= unit_bytes(&u);
            match u {
                Unit::Pack(p) => self.pack(slot, *p)?,
                other => self.route(other)?,
            }
        }
        Ok(())
    }

    /// Hand lane `l`'s finished blocks to its ring; switch cartridges near the end.
    fn flush(&mut self, l: usize) -> Result<()> {
        let lane = &mut self.lanes[l];
        while let Some(b) = lane.w.take_block() {
            lane.ring.push(b).map_err(|e| anyhow!(e))?;
        }
        if !lane.full && (lane.ring.early_warning() || lane.budget.is_some_and(|b| lane.w.payload_bytes() >= b)) {
            self.cartridge_full(l)?;
        }
        Ok(())
    }

    /// Before appending a data record of `len` bytes to lane `l`: if it would cross the
    /// cartridge's budget, continue on the next cartridge first. False if the lane has no
    /// cartridge left.
    fn make_room(&mut self, l: usize, len: u64) -> Result<bool> {
        let lane = &self.lanes[l];
        if !lane.full && lane.data_records > 0 && lane.budget.is_some_and(|b| lane.w.payload_bytes() + len > b) {
            self.cartridge_full(l)?;
        }
        self.lanes[l].data_records += 1;
        Ok(!self.lanes[l].full)
    }

    /// Lane `l`'s cartridge has no room left: continue on the next one, or retire the lane.
    fn cartridge_full(&mut self, l: usize) -> Result<()> {
        if self.next.available(l) {
            return self.switch_volume(l);
        }
        self.lanes[l].full = true;
        let bc = self.lanes[l].volume.barcode.clone();
        if self.slots.iter().any(|s| s.len() > 1) {
            // Copies written together hold the same: when one runs out, the session ends.
            self.full = true;
            self.stop.store(true, Ordering::Relaxed);
            self.warn(format!("{bc} is nearly full and copy {}'s tape set has no further cartridge; ending the session for every copy", self.lanes[l].copy));
        } else if self.lanes.iter().all(|x| x.full) {
            self.full = true;
            self.stop.store(true, Ordering::Relaxed);
            self.warn(format!("{bc} is nearly full and the tape set has no further cartridge; ending the session"));
        } else {
            self.warn(format!("{bc} is nearly full and no further cartridge fits its drive; the other drives carry on"));
        }
        Ok(())
    }

    fn placement(&self, l: usize, chunk_id: i64, pos: RecordPos) -> Placement {
        let lane = &self.lanes[l];
        Placement {
            chunk_id,
            copy: lane.copy as i64,
            volume_id: lane.volume.id,
            partition: 0,
            block: (lane.start + pos.block_seq) as i64,
            offset: pos.offset as i64,
            stored_len: None,
        }
    }

    fn path_id(&mut self, path: &Path, is_dir: bool) -> Result<i64> {
        let id = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                let pid = match self.dir_ids.get(parent) {
                    Some(&id) => id,
                    None => {
                        let id = self.cat.intern_path(parent.as_os_str().as_bytes())?;
                        self.dir_ids.insert(parent.to_path_buf(), id);
                        id
                    }
                };
                self.cat.path_child(pid, name.as_bytes())?
            }
            _ => ROOT_PATH,
        };
        if is_dir {
            self.dir_ids.insert(path.to_path_buf(), id);
        }
        Ok(id)
    }

    /// Catalog row for an entry. Returns the object id.
    fn object(&mut self, m: &FileMeta) -> Result<i64> {
        self.object_from(m, None)
    }

    /// Catalog row for an entry whose data may be that of an earlier object.
    fn object_from(&mut self, m: &FileMeta, data_from: Option<i64>) -> Result<i64> {
        let path = meta::os(&m.path).to_path_buf();
        let path_id = self.path_id(&path, m.kind == FileKind::Dir)?;
        let object_id = self.cat.insert_object(&ObjectRow {
            id: 0,
            session_id: self.session_id,
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
            status: "ok".into(),
            data_from,
        })?;
        for (profile, digest) in &m.fingerprints {
            let algo = self.opts.hash.profiles.iter().find(|p| &p.name == profile).map_or(profile.clone(), |p| p.algo_name());
            self.cat.insert_hash(object_id, &HashRow { profile: profile.clone(), kind: "fingerprint".into(), algo, digest: digest.clone() })?;
        }
        self.progress.entry(&path, m.size);
        match m.kind {
            FileKind::File => self.report.files += 1,
            FileKind::Dir => self.report.dirs += 1,
            FileKind::Whiteout => self.report.whiteouts += 1,
            _ => self.report.others += 1,
        }
        if data_from.is_some() {
            self.report.referenced += 1;
            self.report.referenced_bytes += m.size;
        }
        Ok(object_id)
    }

    /// Catalog row, and the FILE record on every lane of `slot`. Returns the object id.
    fn file_record(&mut self, slot: usize, m: &FileMeta) -> Result<i64> {
        self.file_record_from(slot, m, None)
    }

    fn file_record_from(&mut self, slot: usize, m: &FileMeta, data_from: Option<i64>) -> Result<i64> {
        let oid = self.object_from(m, data_from)?;
        let body = to_cbor(m);
        for l in self.slots[slot].clone() {
            self.record(l, rtype::FILE, 0, &[&body]);
        }
        Ok(oid)
    }

    /// Hashes and status of a file, and its FILE_END record on every lane of `slot`.
    fn file_end(&mut self, slot: usize, object_id: i64, p: &Prepared) -> Result<()> {
        for (profile, algo, digest) in &p.digests {
            self.cat.insert_hash(
                object_id,
                &HashRow { profile: profile.clone(), kind: "content".into(), algo: algo.clone(), digest: digest.clone() },
            )?;
        }
        if p.status != "ok" || p.size != p.meta.size {
            self.cat.set_object_status(object_id, p.status, p.size as i64)?;
        }
        if p.status == "changed" {
            self.report.changed.push(p.path.display().to_string());
        }
        self.report.mismatches.extend(p.mismatches.iter().cloned());
        let xattrs = if p.status == "ok" { self.written_xattrs(slot, p) } else { Vec::new() };
        if !xattrs.is_empty() {
            self.cat.set_object_xattrs(object_id, Some(&to_cbor(&merge_xattrs(&p.meta.xattrs, &xattrs))))?;
            self.writeback.push(WriteBack { path: p.path.clone(), object_id, mtime_ns: p.meta.mtime_ns, size: p.size, xattrs: xattrs.clone() });
        }
        let end = to_cbor(&FileEnd { id: p.meta.id, size: p.size, status: p.status.into(), hashes: p.digests.clone(), xattrs });
        for l in self.slots[slot].clone() {
            self.record(l, rtype::FILE_END, 0, &[&end]);
        }
        Ok(())
    }

    /// Extended attributes this session gives a file: content digests and their freshness
    /// record (write-back), and --set-xattr templates. Written to the source after the session
    /// and part of the archived copy.
    fn written_xattrs(&self, slot: usize, p: &Prepared) -> Vec<(ByteBuf, ByteBuf)> {
        let policy = &self.opts.hash;
        let mut out = Vec::new();
        let x = |n: &str, v: &str| (ByteBuf::from(n.as_bytes().to_vec()), ByteBuf::from(v.as_bytes().to_vec()));
        if policy.write_back {
            let mut algos = Vec::new();
            for (profile, _, digest) in &p.digests {
                let Some(pr) = policy.profiles.iter().find(|pr| &pr.name == profile) else { continue };
                if let Some(name) = &pr.xattr {
                    out.push(x(name, digest));
                    algos.push(pr.algo_name());
                }
            }
            let metas: std::collections::BTreeSet<&String> =
                policy.profiles.iter().filter(|p| p.kind == Kind::Content).filter_map(|p| p.meta_xattr.as_ref()).collect();
            for m in metas {
                out.push(x(m, &hash::meta_value(&algos.join(","), p.meta.mtime_ns, p.size)));
            }
        }
        if !policy.extra_xattrs.is_empty() {
            // The cartridges holding the file, every copy's.
            let mut tapes: Vec<String> = Vec::new();
            for &l in &self.slots[slot] {
                let lane = &self.lanes[l];
                let large = lane.open.as_ref().is_some_and(|o| o.meta.id == p.meta.id);
                if large {
                    tapes.extend(lane.file_volumes.iter().cloned());
                } else {
                    tapes.push(lane.volume.barcode.clone());
                }
            }
            let barcodes = tapes.join(",");
            let lane = &self.lanes[self.slots[slot][0]];
            let date = chrono::DateTime::from_timestamp(lane.begin.started, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            for (name, template) in &policy.extra_xattrs {
                let mut v = template
                    .replace("{session}", &lane.begin.session.to_string())
                    .replace("{date}", &date)
                    .replace("{barcodes}", &barcodes)
                    .replace("{job}", self.opts.job.as_deref().unwrap_or(""));
                for (profile, _, digest) in &p.digests {
                    v = v.replace(&format!("{{{profile}}}"), digest);
                }
                out.push(x(name, &v));
            }
        }
        out
    }

    /// Send a unit where it goes: metadata to every lane (so each lane's cartridges describe
    /// the whole tree), other files to the least loaded lane, large files to a free lane.
    fn route(&mut self, u: Unit) -> Result<()> {
        if self.full {
            return Ok(()); // drain without writing; the session is closing
        }
        match u {
            Unit::Warn(w) => self.warn(w),
            Unit::Meta(m) => {
                self.object(&m)?;
                let body = to_cbor(&*m);
                for l in 0..self.lanes.len() {
                    if !self.lanes[l].full {
                        self.record(l, rtype::FILE, 0, &[&body]);
                        self.flush(l)?;
                    }
                }
            }
            Unit::Done(p) => {
                if let Some(slot) = self.least_slot() {
                    let oid = self.file_record_from(slot, &p.meta, p.data_from)?;
                    self.file_end(slot, oid, &p)?;
                    for l in self.slots[slot].clone() {
                        self.flush(l)?;
                    }
                }
            }
            Unit::Pack(p) => match self.free_slot() {
                Some(slot) => self.pack(slot, *p)?,
                None => {
                    self.stack_bytes += p.stored.len() as u64;
                    self.stack.push_front(Unit::Pack(p));
                }
            },
            Unit::Large(m, path, rx) => match self.free_slot() {
                Some(slot) => {
                    let oid = self.file_record(slot, &m)?;
                    // The slot's first lane receives the file's chunks and writes each to
                    // every lane of the slot.
                    let lanes = self.slots[slot].clone();
                    let mut rx = Some(rx);
                    for &l in &lanes {
                        let lane = &mut self.lanes[l];
                        lane.file_volumes = vec![lane.volume.barcode.clone()];
                        let rx = rx.take().unwrap_or_else(crossbeam_channel::never);
                        lane.open = Some(OpenLarge { meta: m.clone(), path: path.clone(), oid, rx, written: 0 });
                    }
                    for &l in &lanes {
                        self.flush(l)?;
                    }
                }
                None => bail!("internal: a large file arrived with no lane free"),
            },
        }
        Ok(())
    }

    fn pack(&mut self, slot: usize, pack: PackUnit) -> Result<()> {
        let lanes = self.slots[slot].clone();
        for &l in &lanes {
            if !self.make_room(l, pack.stored.len() as u64 + 4096 * pack.members.len() as u64)? {
                // No cartridge left on this lane: another slot takes it (or the session closes).
                self.stack_bytes += pack.stored.len() as u64;
                self.stack.push_front(Unit::Pack(Box::new(pack)));
                return Ok(());
            }
        }
        let mut oids = Vec::new();
        for (p, _, _) in &pack.members {
            oids.push(self.file_record(slot, &p.meta)?);
        }
        let members: Vec<PackMember> =
            pack.members.iter().map(|(p, off, b)| PackMember { object: p.meta.id, offset: *off, len: p.size, blake3: b.to_vec() }).collect();
        let header = ChunkHeader {
            object: 0,
            seq: self.report.packs as u32,
            codec: pack.codec,
            flags: 0,
            offset: 0,
            raw_len: pack.raw_len,
            stored_len: pack.stored.len() as u64,
            blake3: pack.blake3,
        };
        let rflags = if pack.codec == codec::ZSTD { flags::COMPRESSED } else { 0 };
        let body = encode_pack(&header, &members, &pack.stored);
        let positions: Vec<(usize, RecordPos)> = lanes.iter().map(|&l| (l, self.record(l, rtype::PACK, rflags, &[&body]))).collect();
        for ((p, off, b), oid) in pack.members.iter().zip(&oids) {
            let chunk_id = self.cat.insert_chunk(&ChunkRow {
                id: 0,
                object_id: *oid,
                seq: 0,
                offset: 0,
                raw_len: p.size as i64,
                stored_len: pack.stored.len() as i64,
                codec: pack.codec as i64,
                blake3: b.to_vec(),
                pack_offset: Some(*off as i64),
            })?;
            for &(l, pos) in &positions {
                self.cat.insert_placement(&self.placement(l, chunk_id, pos))?;
            }
        }
        self.report.packs += 1;
        self.report.bytes += pack.raw_len;
        self.report.stored += pack.stored.len() as u64;
        for &l in &lanes {
            self.lanes[l].bytes += pack.stored.len() as u64;
        }
        self.progress.data(pack.raw_len, pack.stored.len() as u64);
        for ((p, _, _), oid) in pack.members.iter().zip(&oids) {
            self.file_end(slot, *oid, p)?;
        }
        for &l in &lanes {
            self.flush(l)?;
        }
        Ok(())
    }

    /// The next piece of the large file open on lane `l` (the first of its slot), written to
    /// every lane of the slot.
    fn chunk(&mut self, l: usize, msg: LargeMsg) -> Result<()> {
        let Some(open) = &self.lanes[l].open else { return Ok(()) };
        let (oid, id) = (open.oid, open.meta.id);
        let slot = self.slot_of(l);
        let lanes = self.slots[slot].clone();
        match msg {
            LargeMsg::Chunk { seq, offset, raw_len, codec: cdc, stored, blake3 } => {
                for &k in &lanes {
                    if !self.make_room(k, stored.len() as u64 + CHUNK_HEADER_LEN as u64)? {
                        return self.close_large(l);
                    }
                }
                let header = ChunkHeader { object: id, seq, codec: cdc, flags: 0, offset, raw_len, stored_len: stored.len() as u64, blake3 };
                let rflags = if cdc == codec::ZSTD { flags::COMPRESSED } else { 0 };
                let head = header.encode();
                let positions: Vec<(usize, RecordPos)> = lanes.iter().map(|&k| (k, self.record(k, rtype::CHUNK, rflags, &[&head, &stored]))).collect();
                let chunk_id = self.cat.insert_chunk(&ChunkRow {
                    id: 0,
                    object_id: oid,
                    seq: seq as i64,
                    offset: offset as i64,
                    raw_len: raw_len as i64,
                    stored_len: stored.len() as i64,
                    codec: cdc as i64,
                    blake3: blake3.to_vec(),
                    pack_offset: None,
                })?;
                for &(k, pos) in &positions {
                    self.cat.insert_placement(&self.placement(k, chunk_id, pos))?;
                }
                self.report.bytes += raw_len;
                self.report.stored += stored.len() as u64;
                self.progress.data(raw_len, stored.len() as u64);
                for &k in &lanes {
                    let lane = &mut self.lanes[k];
                    lane.bytes += stored.len() as u64;
                    if let Some(o) = lane.open.as_mut() {
                        o.written = offset + raw_len;
                    }
                }
                for &k in &lanes {
                    self.flush(k)?;
                }
                if lanes.iter().any(|&k| self.lanes[k].full) {
                    self.close_large(l)?;
                }
                Ok(())
            }
            LargeMsg::End(p) => {
                self.file_end(slot, oid, &p)?;
                for &k in &lanes {
                    self.lanes[k].open = None;
                }
                for &k in &lanes {
                    self.flush(k)?;
                }
                Ok(())
            }
        }
    }

    /// The large file on lane `l`'s slot ends without its FILE_END from the worker: a lane
    /// ran out of cartridges (truncated) or the worker failed (unreadable).
    fn close_large(&mut self, l: usize) -> Result<()> {
        let slot = self.slot_of(l);
        let lanes = self.slots[slot].clone();
        let Some(o) = self.lanes[l].open.take() else { return Ok(()) };
        for &k in &lanes {
            self.lanes[k].open = None;
        }
        let full = lanes.iter().any(|&k| self.lanes[k].full);
        let status = if full { "truncated" } else { "unreadable" };
        self.truncated |= full;
        let p = Prepared { path: o.path, meta: *o.meta, size: o.written, status, digests: Vec::new(), mismatches: Vec::new(), data_from: None };
        // The FILE_END names the file's cartridges, so the lanes still know it is open.
        self.file_end(slot, o.oid, &p)?;
        for &k in &lanes {
            if !self.lanes[k].full {
                self.flush(k)?;
            }
        }
        Ok(())
    }
}

/// What the walker found against the job's earlier sessions.
#[derive(Default)]
struct WalkCounts {
    unchanged: u64,
    settling: u64,
    changed_archived: Vec<String>,
}

struct WalkDone<'a>(&'a AtomicBool);

impl Drop for WalkDone<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// What woke the scheduler.
enum Event {
    None,
    Meta(Option<Unit>),
    Large(Option<Unit>),
    Small(Option<Unit>),
    Chunk(usize, Option<LargeMsg>),
}

/// Feed the sequencer: metadata as it comes; large files first and whole, one per lane at a
/// time; small-file packs stacked in memory and written to lanes between large files: when
/// the stack is full, when no large file is outstanding, or when a lane's drive is running
/// and its buffer gets low.
fn schedule(
    seq: &mut Sequencer<'_, '_>,
    meta_rx: &Receiver<Unit>,
    large_rx: &Receiver<Unit>,
    pending: &AtomicU64,
    walk_done: &AtomicBool,
    startup_window: Duration,
) -> Result<()> {
    use crossbeam_channel::{Select, TryRecvError};
    let started = Instant::now();
    let (mut meta_open, mut large_open) = (true, true);
    loop {
        while meta_open {
            match meta_rx.try_recv() {
                Ok(u) => seq.route(u)?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => meta_open = false,
            }
        }
        seq.absorb_small();
        // Chunks of the large files being written, a few per lane at a time.
        let mut progressed = false;
        for l in 0..seq.lanes.len() {
            for _ in 0..4 {
                let msg = match &seq.lanes[l].open {
                    Some(o) => o.rx.try_recv(),
                    None => break,
                };
                match msg {
                    Ok(m) => {
                        seq.chunk(l, m)?;
                        progressed = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        seq.close_large(l)?;
                        progressed = true;
                        break;
                    }
                }
            }
        }
        // A free lane takes the next large file (closing: drop them).
        while large_open && (seq.free_slot().is_some() || seq.full) {
            match large_rx.try_recv() {
                Ok(u) => {
                    seq.route(u)?;
                    progressed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => large_open = false,
            }
        }
        // Stacked small files go to free lanes (also while other lanes stream large files).
        let large_outstanding = large_open && pending.load(Ordering::Relaxed) > 0;
        let discovering = !walk_done.load(Ordering::Relaxed) && started.elapsed() < startup_window;
        if !seq.stack.is_empty() && seq.free_slot().is_some() {
            if seq.stack_bytes >= seq.stack_limit {
                seq.write_stack(seq.stack_limit / 2)?;
                continue;
            }
            let starving = seq.lanes.iter().any(|l| !l.full && l.open.is_none() && l.starving());
            if starving || (!large_outstanding && !discovering) {
                seq.write_stack(0)?;
                continue;
            }
        }
        if progressed {
            continue;
        }
        if seq.full && !large_outstanding {
            // Closing: drain the channels without writing.
            seq.stack.clear();
            seq.stack_bytes = 0;
        }
        let busy = seq.lanes.iter().any(|l| l.open.is_some());
        if !meta_open && !large_open && !seq.small_open && seq.stack.is_empty() && !busy {
            return Ok(());
        }
        let event = {
            let mut sel = Select::new();
            let mut ops: Vec<(usize, Option<usize>)> = Vec::new(); // (operation, lane) ; lane None: see below
            let mut kinds: Vec<u8> = Vec::new();
            if meta_open {
                ops.push((sel.recv(meta_rx), None));
                kinds.push(0);
            }
            if large_open && (seq.free_slot().is_some() || seq.full) {
                ops.push((sel.recv(large_rx), None));
                kinds.push(1);
            }
            if seq.small_open && seq.stack_bytes < seq.stack_limit {
                ops.push((sel.recv(&seq.small_rx), None));
                kinds.push(2);
            }
            for (l, lane) in seq.lanes.iter().enumerate() {
                if let Some(o) = &lane.open {
                    ops.push((sel.recv(&o.rx), Some(l)));
                    kinds.push(3);
                }
            }
            match sel.select_timeout(Duration::from_millis(20)) {
                Err(_) => Event::None,
                Ok(op) => {
                    let i = ops.iter().position(|(k, _)| *k == op.index()).expect("selected operation");
                    match (kinds[i], ops[i].1) {
                        (0, _) => Event::Meta(op.recv(meta_rx).ok()),
                        (1, _) => Event::Large(op.recv(large_rx).ok()),
                        (2, _) => Event::Small(op.recv(&seq.small_rx).ok()),
                        (_, Some(l)) => Event::Chunk(l, op.recv(&seq.lanes[l].open.as_ref().expect("open").rx).ok()),
                        _ => unreachable!(),
                    }
                }
            }
        };
        match event {
            Event::None => {}
            Event::Meta(Some(u)) | Event::Large(Some(u)) => seq.route(u)?,
            Event::Meta(None) => meta_open = false,
            Event::Large(None) => large_open = false,
            Event::Small(Some(u)) => {
                seq.stack_bytes += unit_bytes(&u);
                seq.stack.push_back(u);
            }
            Event::Small(None) => seq.small_open = false,
            Event::Chunk(l, Some(m)) => seq.chunk(l, m)?,
            Event::Chunk(l, None) => seq.close_large(l)?,
        }
    }
}

/// Write one backup session to the cartridge loaded in `drive` (registered as `volume`),
/// ending it when the cartridge is full.
pub fn backup(
    cat: &Catalog,
    drive: &mut dyn TapeDrive,
    volume: &VolumeRow,
    opts: &Options,
    progress: &mut dyn Progress,
) -> Result<Report> {
    backup_spanning(cat, drive, volume, opts, &mut SingleVolume, progress)
}

/// Write one backup session starting on the cartridge loaded in `drive`, continuing on the
/// cartridges `next` supplies whenever one is full.
pub fn backup_spanning(
    cat: &Catalog,
    drive: &mut dyn TapeDrive,
    volume: &VolumeRow,
    opts: &Options,
    next: &mut dyn NextVolume,
    progress: &mut dyn Progress,
) -> Result<Report> {
    backup_parallel(cat, vec![LaneStart { drive, volume: volume.clone(), pace: opts.ring.pace }], opts, next, progress)
}

/// Where one drive starts in a parallel session.
pub struct LaneStart<'d> {
    pub drive: &'d mut dyn TapeDrive,
    pub volume: VolumeRow,
    /// Pacing for this drive (None: full speed).
    pub pace: Option<ringbuf::Pace>,
}

/// Write one backup session through several drives at once, each with its own cartridges
/// (from `next` as they fill); files are spread over the drives by size. The memory buffer
/// in `opts.ring` is shared out between them, and the source must keep all of them busy.
pub fn backup_parallel(
    cat: &Catalog,
    starts: Vec<LaneStart<'_>>,
    opts: &Options,
    next: &mut dyn NextVolume,
    progress: &mut dyn Progress,
) -> Result<Report> {
    backup_copies(cat, vec![starts], opts, next, progress)
}

/// Write one backup session as several copies at once (tee): the source is read once and
/// every record goes to each copy, `copies[i]` being the drives (and first cartridges) of
/// copy `opts.copy + i`. Each copy may span cartridges of its own tape set (`next` is asked
/// by lane: copy 1's lanes first, then copy 2's). Every copy needs the same number of
/// drives; files are spread over them by size as with `backup_parallel`. The slowest drive
/// sets the pace for all, and the others are matched to it. When one copy runs out of
/// cartridges, the session ends for every copy, so they hold the same.
pub fn backup_copies(
    cat: &Catalog,
    copies: Vec<Vec<LaneStart<'_>>>,
    opts: &Options,
    next: &mut dyn NextVolume,
    progress: &mut dyn Progress,
) -> Result<Report> {
    let started = Instant::now();
    if copies.is_empty() || copies[0].is_empty() {
        bail!("no drive to write to");
    }
    let per_copy = copies[0].len();
    if copies.iter().any(|c| c.len() != per_copy) {
        bail!("every copy needs the same number of drives");
    }
    if opts.copies as usize != copies.len() {
        bail!("options say {} copies, but drives were given for {}", opts.copies, copies.len());
    }
    let lane_copy: Vec<u8> = copies.iter().enumerate().flat_map(|(i, c)| std::iter::repeat_n(opts.copy + i as u8, c.len())).collect();
    let slots: Vec<Vec<usize>> = (0..per_copy).map(|i| (0..copies.len()).map(|c| c * per_copy + i).collect()).collect();
    let starts: Vec<LaneStart<'_>> = copies.into_iter().flatten().collect();
    let sources: Vec<PathBuf> = opts
        .sources
        .iter()
        .map(|s| std::fs::canonicalize(s).with_context(|| format!("source {}", s.display())))
        .collect::<Result<_>>()?;
    let mut walker = Walker::new(&opts.walk)?;
    let archived_sources: Vec<PathBuf> = sources.iter().map(|s| opts.archived(s)).collect();
    // What the session compares against (empty for a full session); a resumed session
    // builds on the partial one it continues.
    let (mut prev, level) = incr::previous_for(cat, opts.job.as_deref(), opts.level, opts.resume, &archived_sources)?;
    let parent = prev.base.clone();
    let session_uuid = Uuid::new_v4();
    let sid64 = session_id64(&session_uuid);
    let session_started = now();
    // Encrypted sessions get a fresh data key, wrapped with the user's key.
    let (dek, wrapped) = match &opts.encrypt {
        Some(kek) => {
            let dek = Key::generate();
            let w = crypt::wrap(kek, &dek);
            (Some(dek), Some(w))
        }
        None => (None, None),
    };
    let n = starts.len() as u64;
    let lane_ring = ((opts.ring.capacity / n).max(4 << 20), (opts.ring.high / n).max(1 << 20));
    let rings: Vec<Ring> = starts
        .iter()
        .map(|_| {
            let cfg = ringbuf::Config { capacity: lane_ring.0, high: lane_ring.1, pace: None };
            match &opts.meter {
                Some(m) => Ring::metered(cfg, m.clone()),
                None => Ring::new(cfg),
            }
        })
        .collect();
    let mut first: Vec<(VolumeRow, Option<ringbuf::Pace>)> = Vec::new();
    let drives: Vec<SharedDrive> = starts
        .into_iter()
        .map(|s| {
            first.push((s.volume, s.pace));
            Mutex::new(ringbuf::DriveSlot::new(s.drive))
        })
        .collect();
    let limit = opts.read_limit.map(RateLimit::new);
    let pending = AtomicU64::new(0);
    let walk_done = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    cat.begin()?;
    let result = (|| -> Result<(Report, Vec<WriteBack>, Vec<WriterStats>)> {
        let session_id = cat.create_session(&SessionRow {
            uuid: session_uuid.to_string(),
            job: opts.job.clone(),
            level: level.as_str().into(),
            parent_id: parent.as_ref().map(|p| p.id),
            started: session_started,
            status: "running".into(),
            host: hostname(),
            sources: serde_json::to_string(&archived_sources.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>())?,
            encryption: wrapped.as_ref().map(crypt::to_json),
            ..Default::default()
        })?;
        let begin = SessionBegin {
            session: session_uuid,
            volume: Uuid::nil(),
            job: opts.job.clone(),
            level: level.as_str().into(),
            started: session_started,
            host: hostname(),
            tool: concat!("ltoarc ", env!("CARGO_PKG_VERSION")).into(),
            sources: archived_sources.iter().map(|p| bytes(p)).collect(),
            copy: opts.copy,
            volume_seq: 0,
            parent: parent.as_ref().and_then(|p| p.uuid.parse().ok()),
            encryption: wrapped.clone(),
        };
        let lanes = rings
            .iter()
            .zip(&drives)
            .zip(&first)
            .zip(&lane_copy)
            .map(|(((ring, drive), (volume, pace)), &copy)| Lane {
                ring,
                drive,
                w: StreamWriter::new(64 << 10, Uuid::nil(), sid64, BlockKind::Data),
                volume: volume.clone(),
                label: VolumeLabel::new(&volume.barcode, "", None, String::new()),
                begin: SessionBegin { copy, ..begin.clone() },
                start: 0,
                budget: None,
                perf_before: None,
                motion: None,
                full: false,
                open: None,
                file_volumes: Vec::new(),
                pace: *pace,
                bytes: 0,
                data_records: 0,
                repeat: None,
                copy,
            })
            .collect();
        let mut seq = Sequencer {
            cat,
            opts,
            next,
            stop: &stop,
            progress: &mut *progress,
            lanes,
            slots,
            lane_ring,
            sid64,
            session_id,
            volume_seq: HashMap::new(),
            full: false,
            truncated: false,
            copying: false,
            sealer: dek.as_ref().map(Sealer::new),
            dir_ids: HashMap::new(),
            writeback: Vec::new(),
            report: Report {
                session_uuid,
                session_id,
                level: level.as_str().into(),
                parent: parent.as_ref().and_then(|p| p.uuid.parse().ok()),
                ..Default::default()
            },
            small_rx: crossbeam_channel::never(),
            small_open: true,
            stack: VecDeque::new(),
            stack_bytes: 0,
            stack_limit: 0,
        };
        for (l, (volume, _)) in first.iter().enumerate() {
            seq.start_volume(l, volume.clone())?;
        }
        let total_ring: u64 = rings.iter().map(|r| r.config().capacity).sum();
        seq.stack_limit = opts.stack.unwrap_or(total_ring / 4).max(opts.pack_target as u64);

        let (writer_results, run) = std::thread::scope(|s| {
            let writers: Vec<_> = rings.iter().zip(&drives).map(|(ring, drive)| s.spawn(move || ring.write_to(drive))).collect();
            let (small_tx, small_rx) = bounded::<Job>(4096);
            let (large_tx, large_rx) = bounded::<Job>(16);
            let (meta_tx, meta_rx) = bounded::<Unit>(1024);
            let (small_u_tx, small_u_rx) = bounded::<Unit>(16);
            let (large_u_tx, large_u_rx) = bounded::<Unit>(8);
            let limit = limit.as_ref();
            let pending = &pending;
            // Workers looking for any archived copy of a file's data read the catalog
            // through connections of their own.
            let reader = || if opts.dedup != incr::Dedup::Off && !opts.self_contained { cat.reader().ok().flatten() } else { None };
            for _ in 0..opts.small_workers.max(1) {
                let (rx, tx, stop, r) = (small_rx.clone(), small_u_tx.clone(), &stop, reader());
                s.spawn(move || small_worker(opts, limit, stop, r, rx, tx));
            }
            // At least one large-file reader per drive, so every drive can stream a file.
            for _ in 0..opts.large_workers.max(seq.lanes.len()) {
                let (rx, tx, stop, r) = (large_rx.clone(), large_u_tx.clone(), &stop, reader());
                s.spawn(move || large_worker(opts, limit, stop, pending, r, rx, tx));
            }
            // Stream sources, alongside the files (ids above any the walker hands out).
            for (k, src) in opts.streams.iter().enumerate() {
                pending.fetch_add(1, Ordering::Relaxed);
                let (tx, stop) = (large_u_tx.clone(), &stop);
                s.spawn(move || stream_source(opts, limit, stop, pending, src, (1 << 40) + k as u64, tx));
            }
            drop((small_rx, large_rx, small_u_tx, large_u_tx));
            seq.small_rx = small_u_rx;
            let walk_units = meta_tx;
            let stop_ref = &stop;
            let sources_ref = &sources;
            let walk_done = &walk_done;
            let walker_thread = s.spawn(move || -> Result<(Vec<String>, WalkCounts)> {
                let _done = WalkDone(walk_done);
                let mut names = Names::default();
                let mut hardlinks: HashMap<(u64, u64), ByteBuf> = HashMap::new();
                let mut next = 0u64;
                let mut counts = WalkCounts::default();
                let settled_before = opts.settle.map(|d| now() * 1_000_000_000 - d.as_nanos() as i64);
                let probing = !opts.self_contained && opts.dedup != incr::Dedup::Off;
                walker.walk(sources_ref, &mut |path, md| {
                    if stop_ref.load(Ordering::Relaxed) {
                        return Ok(false);
                    }
                    let kind = meta::kind_of(md);
                    // Against the job's earlier sessions: unchanged entries are neither read
                    // nor recorded again; an append-only job saves new files only.
                    let apath = opts.archived(path);
                    match prev.classify(&apath, md) {
                        incr::Found::Unchanged => {
                            counts.unchanged += 1;
                            if kind == FileKind::File && md.nlink() > 1 {
                                hardlinks.entry((md.dev(), md.ino())).or_insert_with(|| bytes(&apath));
                            }
                            return Ok(true);
                        }
                        incr::Found::Changed(_) if level == incr::Level::Append => {
                            if kind == FileKind::File {
                                counts.changed_archived.push(path.display().to_string());
                            }
                            return Ok(true);
                        }
                        _ => {}
                    }
                    if level == incr::Level::Append && kind == FileKind::File && settled_before.is_some_and(|t| meta::mtime_ns(md) > t) {
                        counts.settling += 1;
                        return Ok(true);
                    }
                    let id = next;
                    next += 1;
                    if kind == FileKind::File && md.nlink() > 1 {
                        let key = (md.dev(), md.ino());
                        if let Some(first) = hardlinks.get(&key) {
                            let mut m = meta::file_meta(path, md, id, &mut names)?;
                            m.path = bytes(&apath);
                            m.kind = FileKind::Hardlink;
                            m.target = Some(first.clone());
                            m.size = 0;
                            return Ok(walk_units.send(Unit::Meta(Box::new(m))).is_ok());
                        }
                        hardlinks.insert(key, bytes(&apath));
                    }
                    if kind == FileKind::File {
                        // Its data may be on tape already (moved, or a duplicate).
                        let probe = (probing && md.len() > 0)
                            .then(|| Probe { moves: prev.moved_candidates(md.len(), &|a| opts.real(a)), all: opts.dedup == incr::Dedup::All })
                            .filter(|p| p.all || !p.moves.is_empty());
                        let job = Job { path: path.to_path_buf(), md: md.clone(), id, probe };
                        let sent = if md.len() <= opts.pack_max {
                            small_tx.send(job).is_ok()
                        } else {
                            pending.fetch_add(1, Ordering::Relaxed);
                            let ok = large_tx.send(job).is_ok();
                            if !ok {
                                pending.fetch_sub(1, Ordering::Relaxed);
                            }
                            ok
                        };
                        return Ok(sent);
                    }
                    match meta::file_meta(path, md, id, &mut names) {
                        Ok(mut m) => {
                            m.path = bytes(&apath);
                            Ok(walk_units.send(Unit::Meta(Box::new(m))).is_ok())
                        }
                        Err(e) => Ok(walk_units.send(Unit::Warn(format!("{}: {e}", path.display()))).is_ok()),
                    }
                })?;
                // Paths gone since the session this one builds on (not those below a
                // directory that could not be read this time).
                if matches!(level, incr::Level::Incremental | incr::Level::Differential) && !stop_ref.load(Ordering::Relaxed) {
                    let unknown: Vec<PathBuf> = walker.unknown.iter().map(|u| opts.archived(u)).collect();
                    for (path, _) in prev.deleted(&archived_sources, &unknown) {
                        let m = meta::whiteout(path, next);
                        next += 1;
                        if walk_units.send(Unit::Meta(Box::new(m))).is_err() {
                            break;
                        }
                    }
                }
                Ok((std::mem::take(&mut walker.warnings), counts))
            });
            let run = schedule(&mut seq, &meta_rx, &large_u_rx, pending, walk_done, opts.startup_window);
            // Whatever happened, let every thread finish. A queued large-file unit holds the
            // receiver its worker may be blocked sending chunks to, and channels keep queued
            // messages while any sender lives, so drain them until the workers are gone.
            stop.store(true, Ordering::Relaxed);
            drop(meta_rx);
            seq.small_rx = crossbeam_channel::never();
            for l in seq.lanes.iter_mut() {
                l.open = None;
            }
            for _ in large_u_rx.iter() {}
            let walked = walker_thread.join().map_err(|_| anyhow!("walker thread panicked")).and_then(|r| r);
            let run = run.and_then(|_| {
                let (warnings, counts) = walked?;
                for w in warnings {
                    seq.warn(w);
                }
                seq.report.unchanged = counts.unchanged;
                seq.report.settling = counts.settling;
                seq.report.changed_archived = counts.changed_archived;
                Ok(())
            });
            let run = run.and_then(|_| {
                let status = if seq.full || seq.truncated { "partial" } else { "complete" };
                let end = SessionEnd {
                    session: session_uuid,
                    files: seq.report.files,
                    bytes: seq.report.bytes,
                    stored: seq.report.stored,
                    ended: now(),
                    status: status.into(),
                };
                seq.report.status = status.into();
                seq.report.early_warning = seq.full;
                for l in 0..seq.lanes.len() {
                    seq.record(l, rtype::SESSION_END, 0, &[&to_cbor(&end)]);
                    let full = seq.lanes[l].full;
                    seq.end_volume(l, status, full)?;
                }
                Ok(())
            });
            for r in &rings {
                r.close();
            }
            let wr: Result<Vec<WriterStats>> = writers
                .into_iter()
                .map(|w| w.join().map_err(|_| anyhow!("writer thread panicked")).and_then(|r| r.map_err(|e| anyhow!(e))))
                .collect();
            (wr, run)
        });
        run?;
        let wstats = writer_results?;
        let write_seconds: f64 = wstats.iter().map(|w| w.write_time.as_secs_f64()).sum();
        // Per drive: the rate it was paced at and what it reported about its tape motion.
        let writers: Vec<serde_json::Value> = wstats
            .iter()
            .zip(&seq.lanes)
            .map(|(w, lane)| {
                let mut j = serde_json::json!({
                    "bytes": w.bytes, "write_seconds": w.write_time.as_secs_f64(), "underruns": w.underruns,
                    "matched_mbps": w.matched.average().map(|r| r / 1e6),
                    "matched_min_mbps": (w.matched.bytes > 0).then(|| w.matched.min / 1e6),
                    "matched_max_mbps": (w.matched.bytes > 0).then(|| w.matched.max / 1e6),
                });
                if let Some(m) = lane.motion {
                    j["pauses"] = m.pauses.into();
                    j["paused_seconds"] = m.paused_secs.into();
                    j["cycles"] = m.cycles.into();
                    j["speed_changes"] = m.speed_changes.into();
                    j["streaming_seconds"] = m.streaming_secs.into();
                }
                j
            })
            .collect();
        cat.update_session(&SessionRow {
            id: session_id,
            ended: Some(now()),
            status: seq.report.status.clone(),
            files: seq.report.files as i64,
            bytes: seq.report.bytes as i64,
            stored_bytes: seq.report.stored as i64,
            stats: Some(
                serde_json::json!({
                    "dirs": seq.report.dirs, "others": seq.report.others, "packs": seq.report.packs,
                    "changed": seq.report.changed.len(), "warnings": seq.report.warnings.len(),
                    "write_seconds": write_seconds, "bursts": wstats.iter().map(|w| w.bursts).sum::<u64>(),
                    "underruns": wstats.iter().map(|w| w.underruns).sum::<u64>(), "volumes": seq.report.volumes.len(),
                    "drives": wstats.len(), "writers": writers,
                })
                .to_string(),
            ),
            ..Default::default()
        })?;
        let wb = std::mem::take(&mut seq.writeback);
        let mut wstats = wstats;
        for (w, lane) in wstats.iter_mut().zip(&seq.lanes) {
            w.drive = lane.motion;
        }
        Ok((std::mem::take(&mut seq.report), wb, wstats))
    })();
    let (mut report, writeback, wstats) = match result {
        Ok(r) => r,
        Err(e) => {
            // Cartridges already finished keep their committed part of the session.
            if cat.in_transaction() {
                let _ = cat.rollback();
            }
            return Err(e);
        }
    };
    report.writer = wstats.first().cloned().unwrap_or_default();
    report.writers = wstats;
    write_back(cat, &writeback);
    report.elapsed = started.elapsed();
    Ok(report)
}

/// (remaining, maximum) native capacity of partition 0 in bytes, from cartridge memory.
/// The session stops writing new data once it would cut into the buffered data plus a tail
/// reserve (at least 1% of the cartridge), well before the drive's early warning.
fn remaining_capacity(drive: &mut dyn TapeDrive) -> Option<(u64, u64)> {
    let attrs = drive.read_attributes(0).ok()?;
    let mib = |id: u16| attrs.iter().find(|a| a.id == id).and_then(|a| a.as_u64()).map(|v| v << 20);
    // MAM 0000h REMAINING CAPACITY IN PARTITION, 0001h MAXIMUM CAPACITY IN PARTITION (MiB).
    let remaining = mib(0x0000)?;
    Some((remaining, mib(0x0001).unwrap_or(remaining)))
}

/// The drive's own record of tape motion in one mode (writing or reading) since the
/// cartridge was loaded: IBM LP 37h subpage 60h ("Mode Phase Timing Windows"). All are
/// running counts, so what happened between two snapshots is their difference.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModePerf {
    /// Times the drive entered the mode (1000h, 1200h).
    pub cycles: u64,
    /// Times it paused in the mode: its buffer ran dry while writing, or filled while
    /// reading (1001h, 1201h).
    pub pauses: u64,
    /// Milliseconds streaming in the mode (media phase timing 2010h, 2030h) and paused
    /// (2020h, 2040h).
    pub streaming_ms: u64,
    pub paused_ms: u64,
    /// "Speed Changes" (21F1h, 23F1h).
    pub speed_changes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DrivePerf {
    pub write: ModePerf,
    pub read: ModePerf,
}

pub fn drive_perf(drive: &mut dyn TapeDrive) -> Option<DrivePerf> {
    let page = drive.log_page(0x37, 0x60).ok()?;
    // Media phase aspects: 1 write, 2 write pause, 3 read, 4 read pause.
    let mode = |base: u16, phase: u16| -> Option<ModePerf> {
        Some(ModePerf {
            cycles: page.u64(base)?,
            pauses: page.u64(base + 1).unwrap_or(0),
            streaming_ms: page.u64(0x2000 | phase << 4).unwrap_or(0),
            paused_ms: page.u64(0x2000 | (phase + 1) << 4).unwrap_or(0),
            // 2?F1h counts them (2?F0h, a time, stays 0 on the IBM LTO-8).
            speed_changes: page.u64(0x20F1 | phase << 8).unwrap_or(0),
        })
    };
    Some(DrivePerf { write: mode(0x1000, 1)?, read: mode(0x1200, 3)? })
}

/// What the drive did in one mode between two snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Motion {
    pub cycles: u64,
    pub pauses: u64,
    pub streaming_secs: f64,
    pub paused_secs: f64,
    pub speed_changes: u64,
}

impl Motion {
    pub fn between(a: &ModePerf, b: &ModePerf) -> Motion {
        Motion {
            cycles: b.cycles.saturating_sub(a.cycles),
            pauses: b.pauses.saturating_sub(a.pauses),
            streaming_secs: b.streaming_ms.saturating_sub(a.streaming_ms) as f64 / 1e3,
            paused_secs: b.paused_ms.saturating_sub(a.paused_ms) as f64 / 1e3,
            speed_changes: b.speed_changes.saturating_sub(a.speed_changes),
        }
    }

    pub fn merge(&mut self, o: &Motion) {
        self.cycles += o.cycles;
        self.pauses += o.pauses;
        self.streaming_secs += o.streaming_secs;
        self.paused_secs += o.paused_secs;
        self.speed_changes += o.speed_changes;
    }

    /// Writing: "streaming 21.0 s at about 152 MB/s; 0 pause(s) (0.0 s stopped); 1
    /// cycle(s), 0 speed change(s)". A write pause is the drive stopping because its
    /// buffer ran dry.
    pub fn describe_write(&self, bytes: u64) -> String {
        let mut out = format!("streaming {:.1} s", self.streaming_secs);
        if self.streaming_secs > 0.0 {
            out += &format!(" at about {:.0} MB/s", bytes as f64 / self.streaming_secs / 1e6);
        }
        out += &format!(
            "; {} pause(s) ({:.1} s stopped); {} cycle(s), {} speed change(s)",
            self.pauses, self.paused_secs, self.cycles, self.speed_changes
        );
        out
    }

    /// Reading: the tape starts once per locate, and every further read cycle is a restart
    /// after a stop. (The drive's read-pause count is not one: on an LTO-8 it rose by
    /// about 80 per GB even while the drive delivered its full 300 MB/s.)
    pub fn describe_read(&self, bytes: u64, locates: u64) -> String {
        let secs = self.streaming_secs + self.paused_secs;
        let mut out = format!("{} stop(s) (tape restarts beyond one per locate)", self.cycles.saturating_sub(locates));
        if secs > 0.0 {
            out += &format!(", about {:.0} MB/s over {:.1} s in read mode", bytes as f64 / secs / 1e6, secs);
        }
        out += &format!(", {} read pause(s) logged, {} speed change(s)", self.pauses, self.speed_changes);
        out
    }
}

/// (write cycles, write pauses) for the current mount, from IBM LP 37h subpage 60h.
pub fn perf_counters(drive: &mut dyn TapeDrive) -> Option<(u64, u64)> {
    let page = drive.log_page(0x37, 0x60).ok()?;
    Some((page.u64(0x1000)?, page.u64(0x1001)?))
}

pub fn check_label(label: &VolumeLabel, volume: &VolumeRow) -> Result<()> {
    if let Some(u) = &volume.uuid {
        if *u != label.volume_uuid.to_string() {
            bail!(
                "loaded cartridge holds volume {} ({}), but the catalog expects {} for {}",
                label.volume_uuid,
                label.barcode,
                u,
                volume.barcode
            );
        }
    } else if label.barcode != volume.barcode {
        bail!("loaded cartridge is labelled {}, not {}", label.barcode, volume.barcode);
    }
    Ok(())
}

pub fn check_position(drive: &mut dyn TapeDrive, expect: u64) -> Result<()> {
    let p = drive.position()?.pos;
    if p.object != expect {
        bail!("tape position is block {} but ltoarc counted {expect}; refusing to record wrong positions", p.object);
    }
    Ok(())
}

/// Export this volume's catalog and append it as a catalog stream plus filemark.
pub fn write_volume_catalog(
    cat: &Catalog,
    drive: &mut dyn TapeDrive,
    volume: &VolumeRow,
    label: &VolumeLabel,
    sid64: u64,
    at: u64,
    encrypt: Option<&Key>,
) -> Result<u64> {
    let tmp = tempfile::Builder::new().prefix("ltoarc-volcat").tempdir()?;
    let path = tmp.path().join("volume.db");
    cat.export_volume(volume.id, &path)?;
    let raw = std::fs::read(&path)?;
    let z = zstd::bulk::compress(&raw, 3)?;
    let mut w = StreamWriter::new(label.block_size, label.volume_uuid, sid64, BlockKind::Catalog);
    match encrypt {
        Some(kek) => w.append(rtype::CATALOG, flags::COMPRESSED | flags::ENCRYPTED, &[&crypt::seal_catalog(kek, &z)]),
        None => w.append(rtype::CATALOG, flags::COMPRESSED, &[&z]),
    };
    w.finish();
    check_position(drive, at)?;
    while let Some(b) = w.take_block() {
        drive.write_block(&b).context("writing volume catalog")?;
    }
    drive.write_filemarks(1)?;
    check_position(drive, at + w.blocks() + 1)?;
    Ok(z.len() as u64)
}

/// After a successful session: write the session's xattrs (digests, --set-xattr) to the
/// sources that did not change since, then record their new ctime so the next run does not
/// see them as modified.
fn write_back(cat: &Catalog, items: &[WriteBack]) {
    for wb in items {
        let Ok(md) = std::fs::symlink_metadata(&wb.path) else { continue };
        if meta::mtime_ns(&md) != wb.mtime_ns || md.len() != wb.size {
            continue;
        }
        let mut wrote = false;
        for (name, value) in &wb.xattrs {
            wrote |= meta::set_xattr(&wb.path, name, value).is_ok();
        }
        if wrote {
            if let Ok(after) = std::fs::symlink_metadata(&wb.path) {
                let _ = cat.set_object_ctime(wb.object_id, meta::ctime_ns(&after));
            }
        }
    }
}

/// Position of a session start, for callers that need to locate it.
pub fn session_start(cat: &Catalog, session_id: i64, volume_id: i64) -> Result<Option<Position>> {
    Ok(cat
        .session_volumes(session_id)?
        .into_iter()
        .find(|v| v.volume_id == volume_id)
        .map(|v| Position { partition: v.partition as u8, object: v.start_object as u64 }))
}
