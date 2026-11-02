//! File-backed simulator of tape libraries, drives and cartridges.
//!
//! A *world* is a directory. `world.json` holds libraries, drives and cartridges; each
//! cartridge partition is stored as `tapes/<id>/p<N>.idx` (one 13-byte entry per logical
//! object: kind u8, length u32 LE, data offset u64 LE) plus `p<N>.dat` (block payloads).
//! A process that opens a world holds an exclusive `flock` on `world.lock` while any handle
//! is alive and keeps the state in memory, writing `world.json` back lazily; other processes
//! wait for the lock.
//!
//! Behaviour follows the IBM LTO SCSI Reference where it matters to ltoarc: append-only
//! writes (writing truncates everything after the position), early warning before the end of
//! a partition, the drive/media compatibility table, MAM attributes, Type M (M8) creation
//! from new LTO-7 cartridges in LTO-8 drives, and write cycles/pauses (LP 37h) from a model
//! of tape motion against the drive buffer.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ltoarc_scsi::density::{self as dens, medium_type};
use ltoarc_scsi::log::{self as lg, LogParam};
use ltoarc_scsi::mam::{self, id as mam_id, Format};
use serde::{Deserialize, Serialize};

use crate::generation::{drive_access, drive_media};
use crate::{
    Access, Attribute, Backend, Changer, ChangerInfo, DensityDescriptor, DriveInfo, Element, ElementType, Error,
    Generation, Inventory, LogPage, MediaInfo, MediaKind, PartitionLayout, Position, PositionInfo, ReadStatus,
    Readiness, Result, TapeDrive, WriteStatus,
};

const IDX_ENTRY: u64 = 13;
const KIND_BLOCK: u8 = 0;
const KIND_FILEMARK: u8 = 1;
const MIB: u64 = 1 << 20;

pub const ADDR_TRANSPORT: u16 = 0x0001;
pub const ADDR_IE: u16 = 0x0010;
pub const ADDR_DRIVE: u16 = 0x0100;
pub const ADDR_SLOT: u16 = 0x1000;

/// IBM Performance Characteristics log page; ltoarc reads subpage 60h (this mount, advanced).
pub const PAGE_PERFORMANCE: u8 = 0x37;

// ---------------------------------------------------------------------------------------
// World specification (what to create)

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorldSpec {
    #[serde(default)]
    pub libraries: Vec<LibrarySpec>,
    /// Drives outside any library.
    #[serde(default)]
    pub standalone: Vec<DriveSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibrarySpec {
    pub name: String,
    pub slots: usize,
    #[serde(default)]
    pub ie_slots: usize,
    pub drives: Vec<DriveSpec>,
    #[serde(default)]
    pub tapes: Vec<TapeSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveSpec {
    pub generation: Generation,
    /// False models a drive installed in a library bay but not cabled to this host.
    #[serde(default = "yes")]
    pub attached: bool,
    #[serde(default)]
    pub serial: Option<String>,
    /// Native data rate in MB/s (default: typical for the generation).
    #[serde(default)]
    pub native_mbps: Option<f64>,
    /// Drive buffer in MiB (default 1024).
    #[serde(default)]
    pub buffer_mib: Option<u64>,
    /// Write speeds the drive cannot match (MB/s, exclusive), as measured on an IBM LTO-8
    /// half-height drive with LTO-7 media (155 to 280): fed in there, it streams at the next
    /// speed above and stops whenever its buffer runs dry.
    #[serde(default)]
    pub speed_gap_mbps: Option<(f64, f64)>,
}

impl DriveSpec {
    pub fn new(generation: Generation) -> Self {
        DriveSpec { generation, attached: true, serial: None, native_mbps: None, buffer_mib: None, speed_gap_mbps: None }
    }

    pub fn serial(mut self, s: &str) -> Self {
        self.serial = Some(s.into());
        self
    }

    pub fn unattached(mut self) -> Self {
        self.attached = false;
        self
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TapeSpec {
    pub barcode: Option<String>,
    /// Physical cartridge generation (an M8 is created from an LTO-7 cartridge).
    pub cartridge: Generation,
    #[serde(default = "data_kind")]
    pub kind: MediaKind,
    /// Total capacity in bytes; defaults to the generation's native capacity.
    #[serde(default)]
    pub capacity: Option<u64>,
    /// Storage slot index (0-based); the first free slot when absent.
    #[serde(default)]
    pub slot: Option<usize>,
}

fn data_kind() -> MediaKind {
    MediaKind::Data
}

// ---------------------------------------------------------------------------------------
// Persistent state

#[derive(Debug, Serialize, Deserialize, Default)]
struct WorldState {
    version: u32,
    libraries: Vec<LibState>,
    drives: Vec<DriveState>,
    tapes: BTreeMap<String, TapeState>,
    next_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LibState {
    name: String,
    serial: String,
    slots: Vec<Option<String>>,
    ie: Vec<Option<String>>,
    /// Drive serial installed in each bay.
    bays: Vec<Option<String>>,
    /// Storage element each cartridge was last taken from.
    sources: BTreeMap<String, u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DriveState {
    serial: String,
    generation: Generation,
    attached: bool,
    library: Option<String>,
    loaded: Option<String>,
    threaded: bool,
    partition: u8,
    object: u64,
    /// Preferred density for new LTO-7 cartridges (00h = native, 5Dh = make M8).
    preferred_l7: u8,
    created: u64,
    loads: u64,
    cleanings: u64,
    head_secs: f64,
    head_secs_at_clean: f64,
    meters: u64,
    hard_write_errors: u64,
    hard_read_errors: u64,
    bytes_written: u64,
    bytes_read: u64,
    tapealerts: Vec<u8>,
    #[serde(default)]
    native_mbps: Option<f64>,
    #[serde(default)]
    buffer_mib: Option<u64>,
    #[serde(default)]
    speed_gap_mbps: Option<(f64, f64)>,
    #[serde(default)]
    motion: Motion,
    /// Hardware compression (LTO drives power up with it enabled).
    #[serde(default = "yes")]
    compression: bool,
    /// Fault injection: blocks still accepted before every write fails (a crash mid-session).
    #[serde(default)]
    fail_after: Option<u64>,
}

/// Tape motion while writing, measured against wall-clock time between host writes: data
/// lands in the drive buffer, which drains at the speed-matched tape speed. When the buffer
/// runs dry while the tape is moving, the drive must stop and reposition (a write pause).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Motion {
    level: f64,
    last: f64,
    streaming: bool,
    tape_rate: f64,
    host_rate: f64,
    write_cycles: u64,
    write_pauses: u64,
    /// Reading: data the drive has read ahead of the host into its buffer.
    #[serde(default)]
    r_level: f64,
    #[serde(default)]
    r_last: f64,
    #[serde(default)]
    r_streaming: bool,
    #[serde(default)]
    r_rate: f64,
    #[serde(default)]
    r_host: f64,
    #[serde(default)]
    read_cycles: u64,
    #[serde(default)]
    read_pauses: u64,
    /// Modelled time in each mode and, of that, time lost to stopping and backhitching
    /// (seconds), and bytes moved: [write, read].
    #[serde(default)]
    mode_secs: [f64; 2],
    #[serde(default)]
    thrash_secs: [f64; 2],
    #[serde(default)]
    mode_bytes: [f64; 2],
    /// Tape speed changes while streaming ([write, read]) and when the last one was.
    #[serde(default)]
    speed_changes: [u64; 2],
    #[serde(default)]
    speed_set: [f64; 2],
}

/// Modelled time a stop costs: decelerate, backhitch, accelerate.
const BACKHITCH_SECS: f64 = 2.0;
/// Tape speeds a drive streams at (evenly spaced from its slowest to its native speed), as
/// real drives speed-match in steps.
const SPEEDS: usize = 12;
/// A drive changes speed at most this often (seconds).
const SPEED_SETTLE: f64 = 1.0;
/// Time constant (seconds) of the drive's view of the host's data rate.
const HOST_TAU: f64 = 1.0;

/// The speed a drive streams at next, from its speeds (evenly spaced from the slowest to
/// native, less those in its gap): the one nearest the host's rate when that is more than
/// three quarters of a step away (the next one above when the host's rate is in the gap);
/// else one slower when its buffer runs low (writing; high when reading) while the tape
/// outpaces the host, one faster (not across the gap) when the opposite holds; else the
/// current one.
fn pick_speed(current: f64, host: f64, vmin: f64, vmax: f64, gap: Option<(f64, f64)>, short: bool, long: bool) -> f64 {
    let step = (vmax - vmin) / (SPEEDS - 1) as f64;
    let speeds: Vec<f64> =
        (0..SPEEDS).map(|i| vmin + step * i as f64).filter(|v| gap.is_none_or(|(lo, hi)| *v <= lo || *v >= hi)).collect();
    let nearest = match gap {
        Some((lo, hi)) if host > lo && host < hi => speeds.iter().copied().find(|v| *v >= hi).unwrap_or(vmax),
        _ => speeds.iter().copied().min_by(|a, b| (a - host).abs().total_cmp(&(b - host).abs())).unwrap_or(vmax),
    };
    if (host - current).abs() > 0.75 * step {
        nearest
    } else if short && current > host {
        speeds.iter().copied().rev().find(|v| *v < current - 1.0).unwrap_or(vmin)
    } else if long && current < host {
        // Not up across the gap: below it the host just waits on a full buffer.
        let up = speeds.iter().copied().find(|v| *v > current + 1.0).unwrap_or(vmax);
        if gap.is_some_and(|(lo, hi)| current <= lo + 1.0 && up >= hi) { current } else { up }
    } else {
        current
    }
}

/// Smoothed host rate after `len` bytes `dt` seconds after the previous transfer.
fn host_ema(prev: f64, len: u64, dt: f64, vmax: f64) -> f64 {
    let inst = (len as f64 / dt).min(vmax);
    if prev == 0.0 { inst } else { prev + (1.0 - (-dt / HOST_TAU).exp()) * (inst - prev) }
}

impl Motion {
    /// Time in the mode and data moved, the way the drive accounts for it.
    fn account(&mut self, mode: usize, len: u64, rate: f64, stopped: bool) {
        if stopped {
            self.thrash_secs[mode] += BACKHITCH_SECS;
            self.mode_secs[mode] += BACKHITCH_SECS;
        }
        if len > 0 && rate > 0.0 {
            self.mode_secs[mode] += len as f64 / rate;
            self.mode_bytes[mode] += len as f64;
        }
    }

    /// (cycles, pauses, streaming ms, paused ms) for LP 37h.
    /// Change to `want` if it differs and the drive has settled since its last change.
    fn set_speed(&mut self, mode: usize, current: f64, want: f64, t: f64) -> f64 {
        if (want - current).abs() > 1.0 && t - self.speed_set[mode] >= SPEED_SETTLE {
            self.speed_changes[mode] += 1;
            self.speed_set[mode] = t;
            want
        } else {
            current
        }
    }

    fn page(&self, mode: usize) -> (u64, u64, u64, u64) {
        let (cycles, pauses) = if mode == 0 { (self.write_cycles, self.write_pauses) } else { (self.read_cycles, self.read_pauses) };
        let paused = self.thrash_secs[mode];
        (cycles, pauses, ((self.mode_secs[mode] - paused).max(0.0) * 1e3) as u64, (paused * 1e3) as u64)
    }
}

fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl DriveState {
    /// (native bytes/s, speed-matching floor bytes/s, buffer bytes)
    fn speed(&self) -> (f64, f64, f64) {
        let native = self.native_mbps.unwrap_or(match self.generation {
            Generation::Lto3 => 80.0,
            Generation::Lto4 => 120.0,
            Generation::Lto5 => 140.0,
            Generation::Lto6 => 160.0,
            _ => 300.0,
        }) * 1e6;
        (native, native / 3.0, self.buffer_mib.unwrap_or(1024) as f64 * MIB as f64)
    }

    fn motion_write(&mut self, len: u64) {
        let (vmax, vmin, cap) = self.speed();
        let gap = self.speed_gap_mbps.map(|(lo, hi)| (lo * 1e6, hi * 1e6));
        let t = now_f64();
        let m = &mut self.motion;
        m.r_last = 0.0;
        if m.last > 0.0 {
            let dt = (t - m.last).max(1e-6);
            m.host_rate = host_ema(m.host_rate, len, dt, vmax);
            if m.streaming {
                let drained = m.tape_rate * dt;
                if drained >= m.level {
                    m.level = 0.0;
                    m.streaming = false;
                    m.write_pauses += 1;
                    m.account(0, 0, 0.0, true);
                } else {
                    m.level -= drained;
                }
            }
        }
        m.level = (m.level + len as f64).min(cap);
        let host = if m.host_rate > 0.0 { m.host_rate } else { vmax };
        if !m.streaming && m.level >= cap / 2.0 {
            m.streaming = true;
            m.write_cycles += 1;
            m.tape_rate = pick_speed(0.0, host, vmin, vmax, gap, false, false);
            m.speed_set[0] = t;
        } else if m.streaming {
            let want = pick_speed(m.tape_rate, host, vmin, vmax, gap, m.level < cap / 4.0, m.level > cap * 0.75);
            m.tape_rate = m.set_speed(0, m.tape_rate, want, t);
        }
        let rate = if m.tape_rate > 0.0 { m.tape_rate } else { vmax };
        m.account(0, len, rate, false);
        m.last = t;
    }

    /// Tape motion while reading: the drive reads ahead into its buffer at the speed-matched
    /// tape speed, and the host takes from it. When the buffer fills (a host slower than the
    /// drive's slowest speed) the tape stops, and starts again (a read cycle) once the host
    /// has taken half of it.
    fn motion_read(&mut self, len: u64) {
        let (vmax, vmin, cap) = self.speed();
        let t = now_f64();
        let m = &mut self.motion;
        if m.r_last == 0.0 {
            m.r_streaming = true;
            m.read_cycles += 1;
            m.r_level = 0.0;
            m.r_host = 0.0;
            m.r_rate = vmax;
        } else {
            let dt = (t - m.r_last).max(1e-6);
            m.r_host = host_ema(m.r_host, len, dt, vmax);
            if m.r_streaming {
                m.r_level += m.r_rate * dt;
                if m.r_level >= cap {
                    m.r_level = cap;
                    m.r_streaming = false;
                    m.read_pauses += 1;
                    m.account(1, 0, 0.0, true);
                }
            }
        }
        m.r_level = (m.r_level - len as f64).max(0.0);
        if !m.r_streaming && m.r_level <= cap / 2.0 {
            // The tape starts again: another read cycle.
            m.r_streaming = true;
            m.read_cycles += 1;
        }
        if m.r_streaming && m.r_host > 0.0 {
            let want = pick_speed(m.r_rate, m.r_host, vmin, vmax, None, m.r_level > cap * 0.75, m.r_level < cap / 4.0);
            m.r_rate = m.set_speed(1, m.r_rate, want, t);
        }
        m.account(1, len, m.r_rate, false);
        m.r_last = t;
    }

    /// The host stopped writing on purpose (filemark, positioning, reading, unloading):
    /// whatever is buffered is written out, which is not a pause.
    fn motion_flush(&mut self) {
        self.motion.r_last = 0.0;
        self.motion.r_streaming = false;
        let m = &mut self.motion;
        if m.level > 0.0 && !m.streaming {
            m.write_cycles += 1;
        }
        m.level = 0.0;
        m.streaming = false;
        m.last = 0.0;
        m.host_rate = 0.0;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TapeState {
    id: String,
    barcode: Option<String>,
    cartridge: Generation,
    kind: MediaKind,
    /// Formatted density; None until first written.
    density: Option<u8>,
    capacity: u64,
    write_protect: bool,
    partitions: Vec<u64>,
    host_mam: BTreeMap<u16, (u8, Vec<u8>)>,
    load_count: u64,
    init_count: u64,
    bytes_written_life: u64,
    bytes_read_life: u64,
    bytes_written_load: u64,
    bytes_read_load: u64,
    last_drives: Vec<String>,
    serial: String,
    manufactured: String,
    change_ref: u32,
    cleaning_cycles_left: u32,
}

impl TapeState {
    fn media_generation(&self) -> Generation {
        self.density.and_then(Generation::from_density).unwrap_or(self.cartridge)
    }

    fn type_m_eligible(&self) -> bool {
        self.cartridge == Generation::Lto7
            && self.kind == MediaKind::Data
            && self.density.is_none()
            && self.bytes_written_life == 0
    }

    fn ew_reserve(&self, partition: u8) -> u64 {
        // About 1% of the partition, at least 4 MiB: room for a session trailer and catalog.
        (self.partitions.get(partition as usize).copied().unwrap_or(0) / 100).max(4 * MIB)
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn sim_err(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

fn medium_type_code(g: Generation, kind: MediaKind) -> u8 {
    let base = match g.cartridge() {
        Generation::Lto3 => 0x38,
        Generation::Lto4 => 0x48,
        Generation::Lto5 => medium_type::LTO5_DATA,
        Generation::Lto6 => medium_type::LTO6_DATA,
        Generation::Lto7 | Generation::M8 => medium_type::LTO7_DATA,
        Generation::Lto8 => medium_type::LTO8_DATA,
        Generation::Lto9 => 0x98,
    };
    match kind {
        MediaKind::Worm => base | 0x04,
        MediaKind::Cleaning => 0x01,
        MediaKind::Data => base,
    }
}

fn digit(g: Generation) -> char {
    g.suffix().chars().last().unwrap_or('0')
}

// ---------------------------------------------------------------------------------------
// Partition data files

#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: u8,
    len: u32,
    offset: u64,
}

struct Part {
    idx: PathBuf,
    dat: PathBuf,
}

impl Part {
    fn new(root: &Path, tape: &str, p: u8) -> Part {
        let dir = root.join("tapes").join(tape);
        Part { idx: dir.join(format!("p{p}.idx")), dat: dir.join(format!("p{p}.dat")) }
    }

    fn count(&self) -> Result<u64> {
        Ok(fs::metadata(&self.idx).map(|m| m.len() / IDX_ENTRY).unwrap_or(0))
    }

    fn used(&self) -> u64 {
        fs::metadata(&self.dat).map(|m| m.len()).unwrap_or(0)
    }

    fn entry(&self, k: u64) -> Result<Option<Entry>> {
        let Ok(mut f) = File::open(&self.idx) else { return Ok(None) };
        if (k + 1) * IDX_ENTRY > f.metadata()?.len() {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(k * IDX_ENTRY))?;
        let mut b = [0u8; IDX_ENTRY as usize];
        f.read_exact(&mut b)?;
        Ok(Some(Entry {
            kind: b[0],
            len: u32::from_le_bytes(b[1..5].try_into().unwrap()),
            offset: u64::from_le_bytes(b[5..13].try_into().unwrap()),
        }))
    }

    fn entries(&self) -> Result<Vec<Entry>> {
        let Ok(data) = fs::read(&self.idx) else { return Ok(Vec::new()) };
        Ok(data
            .chunks_exact(IDX_ENTRY as usize)
            .map(|b| Entry {
                kind: b[0],
                len: u32::from_le_bytes(b[1..5].try_into().unwrap()),
                offset: u64::from_le_bytes(b[5..13].try_into().unwrap()),
            })
            .collect())
    }

    /// Discard object `k` and everything after it (tape is append-only).
    fn truncate(&self, k: u64) -> Result<()> {
        if k >= self.count()? {
            return Ok(());
        }
        let data_end = self.entry(k)?.map(|e| e.offset).unwrap_or(0);
        OpenOptions::new().write(true).open(&self.idx)?.set_len(k * IDX_ENTRY)?;
        if let Ok(f) = OpenOptions::new().write(true).open(&self.dat) {
            f.set_len(data_end)?;
        }
        Ok(())
    }

    fn append(&self, kind: u8, data: &[u8]) -> Result<()> {
        if let Some(dir) = self.idx.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut dat = OpenOptions::new().create(true).append(true).open(&self.dat)?;
        let offset = dat.metadata()?.len();
        dat.write_all(data)?;
        let mut e = [0u8; IDX_ENTRY as usize];
        e[0] = kind;
        e[1..5].copy_from_slice(&(data.len() as u32).to_le_bytes());
        e[5..13].copy_from_slice(&offset.to_le_bytes());
        OpenOptions::new().create(true).append(true).open(&self.idx)?.write_all(&e)?;
        Ok(())
    }

    fn read(&self, e: &Entry, buf: &mut [u8]) -> Result<()> {
        let mut f = File::open(&self.dat)?;
        f.seek(SeekFrom::Start(e.offset))?;
        f.read_exact(&mut buf[..e.len as usize])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// World

/// In-process state of an open world. The process holds the world's lock while any handle
/// exists; state is written back lazily (and always when the last handle goes away).
struct Shared {
    root: PathBuf,
    _lock: File,
    state: WorldState,
    dirty: bool,
    lazy_ops: u32,
    last_save: Instant,
}

impl Shared {
    fn save(&mut self) -> Result<()> {
        let tmp = self.root.join("world.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.state).map_err(|e| sim_err(e.to_string()))?)?;
        fs::rename(tmp, self.root.join("world.json"))?;
        self.dirty = false;
        self.lazy_ops = 0;
        self.last_save = Instant::now();
        Ok(())
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.save();
        }
    }
}

type Registry = Mutex<HashMap<PathBuf, Weak<Mutex<Shared>>>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(Default::default)
}

#[derive(Clone)]
pub struct SimWorld {
    root: PathBuf,
    shared: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for SimWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimWorld").field("root", &self.root).finish()
    }
}

impl SimWorld {
    pub fn create(root: &Path, spec: &WorldSpec) -> Result<SimWorld> {
        if root.join("world.json").exists() {
            return Err(sim_err(format!("{} already holds a simulated world", root.display())));
        }
        fs::create_dir_all(root.join("tapes"))?;
        let mut st = WorldState { version: 1, ..Default::default() };
        let mut drive_no = 0;
        let mut add_drive = |st: &mut WorldState, d: &DriveSpec, lib: Option<&str>| -> String {
            drive_no += 1;
            let serial = d.serial.clone().unwrap_or_else(|| format!("SIM{}D{drive_no:04}", digit(d.generation)));
            st.drives.push(DriveState {
                serial: serial.clone(),
                generation: d.generation,
                attached: d.attached,
                library: lib.map(str::to_string),
                loaded: None,
                threaded: false,
                partition: 0,
                object: 0,
                preferred_l7: 0,
                created: now(),
                loads: 0,
                cleanings: 0,
                head_secs: 0.0,
                head_secs_at_clean: 0.0,
                meters: 0,
                hard_write_errors: 0,
                hard_read_errors: 0,
                bytes_written: 0,
                bytes_read: 0,
                tapealerts: Vec::new(),
                native_mbps: d.native_mbps,
                buffer_mib: d.buffer_mib,
                speed_gap_mbps: d.speed_gap_mbps,
                motion: Motion::default(),
                compression: true,
                fail_after: None,
            });
            serial
        };
        for (n, l) in spec.libraries.iter().enumerate() {
            let serial = format!("SIMLIB{:04}", n + 1);
            let mut lib = LibState {
                name: l.name.clone(),
                serial: serial.clone(),
                slots: vec![None; l.slots],
                ie: vec![None; l.ie_slots],
                bays: Vec::new(),
                sources: BTreeMap::new(),
            };
            for d in &l.drives {
                lib.bays.push(Some(add_drive(&mut st, d, Some(&serial))));
            }
            st.libraries.push(lib);
        }
        for d in &spec.standalone {
            add_drive(&mut st, d, None);
        }
        let w = Self::attach(root.canonicalize()?, Some(st))?;
        for (n, l) in spec.libraries.iter().enumerate() {
            for t in &l.tapes {
                w.add_tape(&format!("SIMLIB{:04}", n + 1), t)?;
            }
        }
        Ok(w)
    }

    pub fn open(root: &Path) -> Result<SimWorld> {
        if !root.join("world.json").exists() {
            return Err(sim_err(format!("{} is not a simulated world (no world.json)", root.display())));
        }
        Self::attach(root.canonicalize()?, None)
    }

    /// Share the process-wide state of a world, taking its lock on first use.
    fn attach(root: PathBuf, init: Option<WorldState>) -> Result<SimWorld> {
        let mut reg = registry().lock().unwrap();
        if let Some(shared) = reg.get(&root).and_then(Weak::upgrade) {
            return Ok(SimWorld { root, shared });
        }
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(root.join("world.lock"))?;
        // SAFETY: flock on a valid descriptor; released when the file is closed.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let fresh = init.is_some();
        let state = match init {
            Some(s) => s,
            None => serde_json::from_slice(&fs::read(root.join("world.json"))?)
                .map_err(|e| sim_err(format!("world.json: {e}")))?,
        };
        let mut sh = Shared { root: root.clone(), _lock: lock, state, dirty: false, lazy_ops: 0, last_save: Instant::now() };
        if fresh {
            sh.save()?;
        }
        let shared = Arc::new(Mutex::new(sh));
        reg.insert(root.clone(), Arc::downgrade(&shared));
        Ok(SimWorld { root, shared })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run `f` on the world state and write it back immediately.
    fn with<R>(&self, f: impl FnOnce(&mut WorldState, &Path) -> Result<R>) -> Result<R> {
        self.with_mode(true, f)
    }

    /// Run `f`; the state is written back later (data path: reads, writes, positioning).
    fn with_lazy<R>(&self, f: impl FnOnce(&mut WorldState, &Path) -> Result<R>) -> Result<R> {
        self.with_mode(false, f)
    }

    fn with_mode<R>(&self, now: bool, f: impl FnOnce(&mut WorldState, &Path) -> Result<R>) -> Result<R> {
        let mut sh = self.shared.lock().unwrap();
        let root = sh.root.clone();
        let r = f(&mut sh.state, &root);
        sh.dirty = true;
        sh.lazy_ops += 1;
        if now || sh.lazy_ops >= 4096 || sh.last_save.elapsed() > Duration::from_secs(2) {
            sh.save()?;
        }
        r
    }

    /// Write the state back now.
    pub fn flush(&self) -> Result<()> {
        self.shared.lock().unwrap().save()
    }

    /// Put a new cartridge into a library slot. Returns its internal id.
    pub fn add_tape(&self, library: &str, spec: &TapeSpec) -> Result<String> {
        self.with(|st, _| {
            st.next_id += 1;
            let id = format!("t{:05}", st.next_id);
            let lib = st
                .libraries
                .iter_mut()
                .find(|l| l.serial == library || l.name == library)
                .ok_or_else(|| sim_err(format!("no library {library}")))?;
            let slot = match spec.slot {
                Some(s) if s < lib.slots.len() && lib.slots[s].is_none() => s,
                Some(s) => return Err(sim_err(format!("slot {s} is not free"))),
                None => lib.slots.iter().position(Option::is_none).ok_or_else(|| sim_err("library is full"))?,
            };
            lib.slots[slot] = Some(id.clone());
            let capacity = spec.capacity.unwrap_or_else(|| spec.cartridge.native_capacity());
            let n = st.next_id;
            st.tapes.insert(
                id.clone(),
                TapeState {
                    id: id.clone(),
                    barcode: spec.barcode.clone(),
                    cartridge: spec.cartridge.cartridge(),
                    kind: spec.kind,
                    density: (spec.cartridge == Generation::M8).then_some(dens::LTO7_M8),
                    capacity,
                    write_protect: false,
                    partitions: vec![capacity],
                    host_mam: BTreeMap::new(),
                    load_count: 0,
                    init_count: 0,
                    bytes_written_life: 0,
                    bytes_read_life: 0,
                    bytes_written_load: 0,
                    bytes_read_load: 0,
                    last_drives: Vec::new(),
                    serial: format!("SIMCART{n:08}"),
                    manufactured: "20260101".into(),
                    change_ref: 0,
                    cleaning_cycles_left: if spec.kind == MediaKind::Cleaning { 50 } else { 0 },
                },
            );
            Ok(id)
        })
    }

    /// Fault injection: raise or clear a TapeAlert flag on a drive.
    pub fn set_tapealert(&self, drive_serial: &str, flag: u8, on: bool) -> Result<()> {
        self.with(|st, _| {
            let d = drive_state(st, drive_serial)?;
            d.tapealerts.retain(|&f| f != flag);
            if on {
                d.tapealerts.push(flag);
            }
            Ok(())
        })
    }

    /// Make a drive fail every write after `blocks` more blocks (None: stop failing), as if
    /// the host crashed or lost the drive mid-session.
    pub fn fail_writes_after(&self, drive_serial: &str, blocks: Option<u64>) -> Result<()> {
        self.with(|st, _| {
            drive_state(st, drive_serial)?.fail_after = blocks;
            Ok(())
        })
    }

    /// (write cycles, write pauses) of a drive's motion model since the current load.
    pub fn motion(&self, drive_serial: &str) -> Result<(u64, u64)> {
        self.with_lazy(|st, _| {
            let d = drive_state(st, drive_serial)?;
            Ok((d.motion.write_cycles, d.motion.write_pauses))
        })
    }

    pub fn changers(&self) -> Result<Vec<SimChanger>> {
        let st = self.with_lazy(|st, _| Ok(st.libraries.clone()))?;
        Ok(st
            .into_iter()
            .map(|l| SimChanger {
                world: self.clone(),
                info: ChangerInfo {
                    vendor: "IBM".into(),
                    product: "3573-TL".into(),
                    revision: "SIM1".into(),
                    serial: l.serial.clone(),
                    backend: Backend::Sim,
                    location: self.root.display().to_string(),
                },
                serial: l.serial,
            })
            .collect())
    }

    /// Drives that are cabled to the host (attached).
    pub fn drives(&self) -> Result<Vec<SimDrive>> {
        let ds = self.with_lazy(|st, _| Ok(st.drives.clone()))?;
        Ok(ds.into_iter().filter(|d| d.attached).map(|d| self.drive_handle(&d)).collect())
    }

    pub fn drive(&self, serial: &str) -> Result<SimDrive> {
        let d = self.with_lazy(|st, _| drive_state(st, serial).cloned())?;
        if !d.attached {
            return Err(sim_err(format!("drive {serial} is not attached to this host")));
        }
        Ok(self.drive_handle(&d))
    }

    pub fn changer(&self, serial: &str) -> Result<SimChanger> {
        self.changers()?
            .into_iter()
            .find(|c| c.serial == serial)
            .ok_or_else(|| sim_err(format!("no simulated library {serial}")))
    }

    fn drive_handle(&self, d: &DriveState) -> SimDrive {
        SimDrive {
            world: self.clone(),
            info: DriveInfo {
                vendor: "IBM".into(),
                product: format!("ULT3580-HH{}", digit(d.generation)),
                revision: "SIM1".into(),
                serial: d.serial.clone(),
                generation: Some(d.generation),
                backend: Backend::Sim,
                location: self.root.display().to_string(),
                st_path: None,
            },
        }
    }
}

fn drive_state<'a>(st: &'a mut WorldState, serial: &str) -> Result<&'a mut DriveState> {
    st.drives.iter_mut().find(|d| d.serial == serial).ok_or_else(|| sim_err(format!("no simulated drive {serial}")))
}

/// Drive with a threaded cartridge, plus that cartridge.
fn loaded<'a>(st: &'a mut WorldState, serial: &str) -> Result<(&'a mut DriveState, &'a mut TapeState)> {
    let WorldState { drives, tapes, .. } = st;
    let d = drives.iter_mut().find(|d| d.serial == serial).ok_or_else(|| sim_err("no such drive"))?;
    let id = d.loaded.clone().ok_or(Error::NoMedium)?;
    if !d.threaded {
        return Err(Error::NotReady("cartridge present but not loaded".into()));
    }
    let t = tapes.get_mut(&id).ok_or_else(|| sim_err("cartridge state missing"))?;
    Ok((d, t))
}

/// Bookkeeping when a cartridge is threaded (loaded) in a drive.
fn thread(d: &mut DriveState, t: &mut TapeState) {
    d.threaded = true;
    d.partition = 0;
    d.object = 0;
    d.loads += 1;
    d.head_secs += 15.0;
    d.meters += 30;
    // LP 37h mount values reset on load.
    d.motion = Motion::default();
    t.load_count += 1;
    t.bytes_read_load = 0;
    t.bytes_written_load = 0;
    t.last_drives.insert(0, d.serial.clone());
    t.last_drives.truncate(4);
    if t.kind == MediaKind::Cleaning {
        // A cleaning cartridge cleans, then the drive unloads it.
        d.cleanings += 1;
        d.head_secs_at_clean = d.head_secs;
        d.tapealerts.retain(|f| *f != lg::TAPEALERT_CLEAN_NOW && *f != lg::TAPEALERT_CLEAN_PERIODIC);
        t.cleaning_cycles_left = t.cleaning_cycles_left.saturating_sub(1);
        d.threaded = false;
    }
}

fn check_access(d: &DriveState, t: &TapeState, write: bool) -> Result<()> {
    if t.kind == MediaKind::Cleaning {
        return Err(sim_err("cleaning cartridge installed"));
    }
    let media = t.media_generation();
    match (drive_access(d.generation, media), write) {
        (Access::ReadWrite, _) | (Access::ReadOnly, false) => Ok(()),
        (_, op) => Err(Error::Incompatible { drive: d.generation, media, op: if op { "write" } else { "read" } }),
    }
}

/// Before the first write to a blank cartridge: decide its format (M8 or native).
fn format_on_first_write(d: &DriveState, t: &mut TapeState) {
    if t.density.is_some() {
        return;
    }
    if t.type_m_eligible() && d.generation == Generation::Lto8 && d.preferred_l7 == dens::LTO7_M8 {
        t.density = Some(dens::LTO7_M8);
        t.capacity = t.capacity / 2 * 3; // 6 TB -> 9 TB
        t.partitions = vec![t.capacity];
    } else {
        t.density = Some(t.cartridge.density());
    }
}

fn descriptor(code: u8, write_ok: bool, default: bool) -> DensityDescriptor {
    let g = Generation::from_density(code);
    DensityDescriptor {
        primary: code,
        secondary: code,
        write_ok,
        duplicate: false,
        default,
        bits_per_mm: 0,
        media_width: 127,
        tracks: 0,
        capacity_mb: g.map_or(0, |g| (g.native_capacity() / 1_000_000) as u32),
        assigning_org: "LTO-CVE".into(),
        name: g.map_or(String::new(), |g| match g {
            Generation::M8 => "U-832M".into(),
            other => format!("U-{}32", digit(other)),
        }),
        description: g.map_or(String::new(), |g| format!("Ultrium {}", g.name().trim_start_matches("LTO-"))),
    }
}

// ---------------------------------------------------------------------------------------
// Drive

pub struct SimDrive {
    world: SimWorld,
    info: DriveInfo,
}

impl SimDrive {
    fn with<R>(&self, f: impl FnOnce(&mut WorldState, &Path) -> Result<R>) -> Result<R> {
        self.world.with(f)
    }

    fn with_lazy<R>(&self, f: impl FnOnce(&mut WorldState, &Path) -> Result<R>) -> Result<R> {
        self.world.with_lazy(f)
    }
}

impl TapeDrive for SimDrive {
    fn info(&self) -> &DriveInfo {
        &self.info
    }

    fn readiness(&mut self) -> Result<Readiness> {
        let s = &self.info.serial;
        self.with_lazy(|st, _| {
            let d = drive_state(st, s)?;
            Ok(match (&d.loaded, d.threaded) {
                (None, _) => Readiness::NoMedium,
                (Some(_), false) => Readiness::NotReady("initializing command required".into()),
                (Some(_), true) => Readiness::Ready,
            })
        })
    }

    fn load(&mut self) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, _| {
            let WorldState { drives, tapes, .. } = st;
            let d = drives.iter_mut().find(|d| d.serial == s).ok_or_else(|| sim_err("no such drive"))?;
            let id = d.loaded.clone().ok_or(Error::NoMedium)?;
            if !d.threaded {
                thread(d, tapes.get_mut(&id).ok_or_else(|| sim_err("cartridge state missing"))?);
            }
            Ok(())
        })
    }

    fn unload(&mut self) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, _| {
            let d = drive_state(st, &s)?;
            if d.loaded.is_none() {
                return Err(Error::NoMedium);
            }
            if d.threaded {
                d.head_secs += 15.0;
            }
            d.motion_flush();
            d.threaded = false;
            d.partition = 0;
            d.object = 0;
            Ok(())
        })
    }

    fn media_info(&mut self) -> Result<MediaInfo> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, _| {
            let (_, t) = loaded(st, &s)?;
            let density = t.density.unwrap_or(0);
            Ok(MediaInfo {
                density,
                generation: Generation::from_density(density),
                medium_type: medium_type_code(t.cartridge, t.kind),
                write_protected: t.write_protect,
                block_length: 0,
            })
        })
    }

    fn max_block_size(&mut self) -> Result<usize> {
        Ok(8 << 20)
    }

    fn report_density(&mut self, media: bool) -> Result<Vec<DensityDescriptor>> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, _| {
            if !media {
                let d = drive_state(st, &s)?;
                let (rw, ro) = drive_media(d.generation);
                return Ok(rw
                    .iter()
                    .map(|g| descriptor(g.density(), true, *g == d.generation))
                    .chain(ro.iter().map(|g| descriptor(g.density(), false, false)))
                    .collect());
            }
            let (d, t) = loaded(st, &s)?;
            let rw = |g: Generation| drive_access(d.generation, g) == Access::ReadWrite;
            Ok(match t.density {
                Some(code) => vec![descriptor(code, Generation::from_density(code).is_some_and(rw), true)],
                None if t.type_m_eligible() && d.generation == Generation::Lto8 => {
                    let m = d.preferred_l7 == dens::LTO7_M8;
                    vec![descriptor(dens::LTO7, true, !m), descriptor(dens::LTO7_M8, true, m)]
                }
                None => vec![descriptor(t.cartridge.density(), rw(t.cartridge), true)],
            })
        })
    }

    fn position(&mut self) -> Result<PositionInfo> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let (d, t) = loaded(st, &s)?;
            let part = Part::new(root, &t.id, d.partition);
            let entries = part.entries()?;
            let file = entries.iter().take(d.object as usize).filter(|e| e.kind == KIND_FILEMARK).count() as u64;
            let used = part.used();
            Ok(PositionInfo {
                pos: Position { partition: d.partition, object: d.object },
                file,
                bop: d.object == 0,
                eop: used + t.ew_reserve(d.partition) > t.partitions[d.partition as usize]
                    && d.object == entries.len() as u64,
            })
        })
    }

    fn locate(&mut self, pos: Position) -> Result<()> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let (d, t) = loaded(st, &s)?;
            if pos.partition as usize >= t.partitions.len() {
                return Err(sim_err(format!("partition {} does not exist", pos.partition)));
            }
            d.motion_flush();
            let count = Part::new(root, &t.id, pos.partition).count()?;
            d.partition = pos.partition;
            d.head_secs += 30.0;
            if pos.object > count {
                d.object = count;
                return Err(Error::EndOfData);
            }
            d.object = pos.object;
            Ok(())
        })
    }

    fn locate_eod(&mut self, partition: u8) -> Result<PositionInfo> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let (d, t) = loaded(st, &s)?;
            if partition as usize >= t.partitions.len() {
                return Err(sim_err(format!("partition {partition} does not exist")));
            }
            d.motion_flush();
            d.partition = partition;
            d.object = Part::new(root, &t.id, partition).count()?;
            d.head_secs += 30.0;
            Ok(())
        })?;
        self.position()
    }

    fn rewind(&mut self) -> Result<()> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, _| {
            let (d, _) = loaded(st, &s)?;
            d.motion_flush();
            d.partition = 0;
            d.object = 0;
            d.head_secs += 10.0;
            Ok(())
        })
    }

    fn space_filemarks(&mut self, count: i32) -> Result<()> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let (d, t) = loaded(st, &s)?;
            d.motion_flush();
            let entries = Part::new(root, &t.id, d.partition).entries()?;
            if count >= 0 {
                let mut left = count;
                let mut k = d.object as usize;
                while left > 0 {
                    match entries.get(k) {
                        None => {
                            d.object = k as u64;
                            return Err(Error::EndOfData);
                        }
                        Some(e) => {
                            if e.kind == KIND_FILEMARK {
                                left -= 1;
                            }
                            k += 1;
                        }
                    }
                }
                d.object = k as u64;
            } else {
                let mut left = -count;
                let mut k = d.object as usize;
                while left > 0 {
                    if k == 0 {
                        d.object = 0;
                        return Err(sim_err("beginning of partition reached while spacing"));
                    }
                    k -= 1;
                    if entries[k].kind == KIND_FILEMARK {
                        left -= 1;
                    }
                }
                d.object = k as u64;
            }
            Ok(())
        })
    }

    fn write_block(&mut self, data: &[u8]) -> Result<WriteStatus> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| write_objects(st, root, &s, KIND_BLOCK, data, 1))
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteStatus> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| write_objects(st, root, &s, KIND_FILEMARK, &[], count))
    }

    fn read_block(&mut self, buf: &mut [u8]) -> Result<ReadStatus> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let (d, t) = loaded(st, &s)?;
            check_access(d, t, false)?;
            if d.motion.level > 0.0 {
                d.motion_flush();
            }
            let part = Part::new(root, &t.id, d.partition);
            let Some(e) = part.entry(d.object)? else { return Ok(ReadStatus::EndOfData) };
            d.object += 1;
            if e.kind == KIND_FILEMARK {
                d.motion_read(0);
                return Ok(ReadStatus::Filemark);
            }
            if e.len as usize > buf.len() {
                return Err(sim_err(format!("block of {} bytes does not fit a {}-byte buffer", e.len, buf.len())));
            }
            part.read(&e, buf)?;
            d.motion_read(e.len as u64);
            d.bytes_read += e.len as u64;
            t.bytes_read_life += e.len as u64;
            t.bytes_read_load += e.len as u64;
            Ok(ReadStatus::Data(e.len as usize))
        })
    }

    fn read_attributes(&mut self, partition: u8) -> Result<Vec<Attribute>> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let WorldState { drives, tapes, .. } = st;
            let d = drives.iter().find(|d| d.serial == s).ok_or_else(|| sim_err("no such drive"))?;
            let t = tapes.get(d.loaded.as_ref().ok_or(Error::NoMedium)?).ok_or_else(|| sim_err("cartridge missing"))?;
            Ok(mam_attributes(t, root, partition))
        })
    }

    fn write_attributes(&mut self, _partition: u8, attrs: &[Attribute]) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, _| {
            let WorldState { drives, tapes, .. } = st;
            let d = drives.iter().find(|d| d.serial == s).ok_or_else(|| sim_err("no such drive"))?;
            let t = tapes
                .get_mut(d.loaded.as_ref().ok_or(Error::NoMedium)?)
                .ok_or_else(|| sim_err("cartridge missing"))?;
            for a in attrs {
                if !(0x0800..=0x0bff).contains(&a.id) {
                    return Err(sim_err(format!("MAM attribute {:#06x} is read-only", a.id)));
                }
            }
            for a in attrs {
                let fmt = match a.format {
                    Format::Binary => 0,
                    Format::Ascii => 1,
                    Format::Text => 2,
                    Format::Reserved => 3,
                };
                t.host_mam.insert(a.id, (fmt, a.value.clone()));
            }
            Ok(())
        })
    }

    fn supported_log_pages(&mut self) -> Result<Vec<u8>> {
        Ok(vec![
            lg::PAGE_SUPPORTED,
            lg::PAGE_SEQUENTIAL_ACCESS,
            lg::PAGE_DEVICE_STATISTICS,
            lg::PAGE_VOLUME_STATISTICS,
            lg::PAGE_TAPEALERT,
            lg::PAGE_TAPE_CAPACITY,
            PAGE_PERFORMANCE,
        ])
    }

    fn log_page(&mut self, page: u8, _subpage: u8) -> Result<LogPage> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, root| {
            let WorldState { drives, tapes, .. } = st;
            let d = drives.iter().find(|d| d.serial == s).ok_or_else(|| sim_err("no such drive"))?;
            let tape = d.loaded.as_ref().filter(|_| d.threaded).and_then(|id| tapes.get(id));
            let u = |code: u16, v: u64, len: usize| LogParam::new(code, v.to_be_bytes()[8 - len..].to_vec());
            let params = match page {
                lg::PAGE_SEQUENTIAL_ACCESS => vec![
                    u(0x0000, d.bytes_written, 8),
                    u(0x0001, d.bytes_written, 8),
                    u(0x0002, d.bytes_read, 8),
                    u(0x0003, d.bytes_read, 8),
                    u(0x0100, d.tapealerts.contains(&lg::TAPEALERT_CLEAN_NOW) as u64, 1),
                ],
                lg::PAGE_DEVICE_STATISTICS => {
                    let hours = |secs: f64| (secs / 3600.0).ceil() as u64;
                    let mut serial = LogParam::new(0x0041, d.serial.as_bytes().to_vec());
                    serial.control = 0x01;
                    vec![
                        u(0x0000, d.loads, 4),
                        u(0x0001, d.cleanings, 4),
                        u(0x0002, now().saturating_sub(d.created).div_ceil(3600), 4),
                        u(0x0003, hours(d.head_secs), 4),
                        u(0x0004, d.meters, 4),
                        u(0x0008, hours(d.head_secs - d.head_secs_at_clean), 4),
                        u(0x000e, d.hard_write_errors, 4),
                        u(0x000f, d.hard_read_errors, 4),
                        serial,
                    ]
                }
                lg::PAGE_VOLUME_STATISTICS => {
                    let t = tape.ok_or(Error::NoMedium)?;
                    let used: u64 = (0..t.partitions.len() as u8).map(|p| Part::new(root, &t.id, p).used()).sum();
                    vec![
                        u(0x0001, t.load_count, 4),
                        u(0x0002, t.bytes_written_life / (400 * 1024), 4),
                        u(0x0007, t.bytes_read_life / (400 * 1024), 4),
                        u(0x0003, 0, 4),
                        u(0x0004, 0, 4),
                        u(0x0008, 0, 4),
                        u(0x0009, 0, 4),
                        u(0x000e, t.bytes_written_load / 1_000_000, 4),
                        u(0x000f, t.bytes_read_load / 1_000_000, 4),
                        u(0x0010, t.bytes_written_life / 1_000_000, 4),
                        u(0x0011, t.bytes_read_life / 1_000_000, 4),
                        u(0x0016, t.capacity / 1_000_000, 4),
                        u(0x0017, used / 1_000_000, 4),
                        u(0x0019, 100u64.saturating_sub(t.load_count / 200), 1),
                        u(0x0101, t.load_count, 4),
                        u(0x0102, t.load_count * 2, 4),
                    ]
                }
                lg::PAGE_TAPEALERT => {
                    (1..=64u16).map(|c| LogParam::new(c, vec![d.tapealerts.contains(&(c as u8)) as u8])).collect()
                }
                lg::PAGE_TAPE_CAPACITY => {
                    let t = tape.ok_or(Error::NoMedium)?;
                    let mut v = Vec::new();
                    for (p, cap) in t.partitions.iter().enumerate().take(2) {
                        let used = Part::new(root, &t.id, p as u8).used();
                        v.push(u(0x0001 + p as u16, cap.saturating_sub(used) / 1_000_000, 4));
                        v.push(u(0x0003 + p as u16, cap / 1_000_000, 4));
                    }
                    v.sort_by_key(|p| p.code);
                    v
                }
                PAGE_PERFORMANCE => {
                    let mut v = vec![u(0x0000, 1, 1)];
                    for (mode, base, phase) in [(0usize, 0x1000u16, 1u16), (1, 0x1200, 3)] {
                        let (cycles, pauses, streaming, paused) = d.motion.page(mode);
                        v.extend([
                            u(base, cycles, 4),
                            u(base + 1, pauses, 4),
                            u(base + 0x10, streaming + paused, 6),
                            u(0x2000 | phase << 4, streaming, 6),
                            u(0x2000 | (phase + 1) << 4, paused, 6),
                            u(0x20F1 | phase << 8, d.motion.speed_changes[mode], 4),
                        ]);
                    }
                    v.sort_by_key(|p| p.code);
                    v
                }
                lg::PAGE_SUPPORTED => Vec::new(),
                other => return Err(Error::Unsupported(format!("log page {other:#04x}"))),
            };
            Ok(LogPage { page, subpage: 0, params })
        })
    }

    fn compression(&mut self) -> Result<bool> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, _| Ok(drive_state(st, &s)?.compression))
    }

    fn set_compression(&mut self, on: bool) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, _| {
            drive_state(st, &s)?.compression = on;
            Ok(())
        })
    }

    fn format(&mut self, layout: PartitionLayout) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, root| {
            let (d, t) = loaded(st, &s)?;
            check_access(d, t, true)?;
            if t.write_protect {
                return Err(Error::WriteProtected);
            }
            format_on_first_write(d, t);
            let _ = fs::remove_dir_all(root.join("tapes").join(&t.id));
            t.partitions = match layout {
                PartitionLayout::Single => vec![t.capacity],
                PartitionLayout::Two { p0_mb } => {
                    let p0 = p0_mb as u64 * 1_000_000;
                    // Partitions are allocated in whole wraps; model guard space as one extra p0.
                    let rest = t.capacity.checked_sub(2 * p0).ok_or_else(|| sim_err("index partition too large"))?;
                    vec![p0, rest]
                }
            };
            t.init_count += 1;
            t.change_ref += 1;
            d.partition = 0;
            d.object = 0;
            d.head_secs += 60.0;
            Ok(())
        })
    }

    fn preferred_density(&mut self, medium_type: u8) -> Result<u8> {
        let s = self.info.serial.clone();
        self.with_lazy(|st, _| {
            let d = drive_state(st, &s)?;
            if d.generation != Generation::Lto8 || medium_type != medium_type::LTO7_DATA {
                return Err(Error::Unsupported("no preferred density entry".into()));
            }
            Ok(d.preferred_l7)
        })
    }

    fn set_preferred_density(&mut self, medium_type: u8, density: u8) -> Result<()> {
        let s = self.info.serial.clone();
        self.with(|st, _| {
            let d = drive_state(st, &s)?;
            if d.generation != Generation::Lto8 || medium_type != medium_type::LTO7_DATA {
                return Err(Error::Unsupported("no preferred density entry".into()));
            }
            if ![0, dens::LTO7, dens::LTO7_M8].contains(&density) {
                return Err(sim_err(format!("invalid preferred density {density:#04x}")));
            }
            d.preferred_l7 = density;
            Ok(())
        })
    }
}

fn write_objects(st: &mut WorldState, root: &Path, serial: &str, kind: u8, data: &[u8], n: u32) -> Result<WriteStatus> {
    let (d, t) = loaded(st, serial)?;
    if t.write_protect {
        return Err(Error::WriteProtected);
    }
    if let Some(left) = d.fail_after.as_mut() {
        if *left == 0 {
            return Err(sim_err("simulated failure: the drive stopped accepting writes"));
        }
        *left -= 1;
    }
    if t.density.is_none() {
        if d.partition != 0 || d.object != 0 {
            return Err(sim_err("blank cartridge must be written from the beginning"));
        }
        format_on_first_write(d, t);
    }
    check_access(d, t, true)?;
    let part = Part::new(root, &t.id, d.partition);
    part.truncate(d.object)?;
    let cap = t.partitions[d.partition as usize];
    let reserve = t.ew_reserve(d.partition);
    let mut status = WriteStatus::Ok;
    for _ in 0..n {
        let used = part.used();
        if used + data.len() as u64 > cap {
            return Err(Error::EndOfMedium);
        }
        part.append(kind, data)?;
        d.object += 1;
        if used + data.len() as u64 > cap - reserve {
            status = WriteStatus::EarlyWarning;
        }
    }
    let bytes = data.len() as u64 * n as u64;
    d.bytes_written += bytes;
    d.meters += bytes / (20 * MIB);
    if kind == KIND_BLOCK {
        d.motion_write(bytes);
    } else {
        d.motion_flush();
    }
    t.bytes_written_life += bytes;
    t.bytes_written_load += bytes;
    t.change_ref = t.change_ref.wrapping_add(1);
    Ok(status)
}

fn mam_attributes(t: &TapeState, root: &Path, partition: u8) -> Vec<Attribute> {
    let ro = |id: u16, format: Format, value: Vec<u8>| Attribute { id, read_only: true, format, value };
    let bin = |id: u16, v: u64, len: usize| ro(id, Format::Binary, v.to_be_bytes()[8 - len..].to_vec());
    let ascii = |id: u16, s: &str, len: usize| {
        let mut a = Attribute::ascii(id, s, len);
        a.read_only = true;
        a
    };
    let cap = t.partitions.get(partition as usize).copied().unwrap_or(0);
    let used = Part::new(root, &t.id, partition).used();
    let mut out = vec![
        bin(mam_id::REMAINING_CAPACITY, cap.saturating_sub(used) / MIB, 8),
        bin(mam_id::MAXIMUM_CAPACITY, cap / MIB, 8),
        bin(mam_id::TAPEALERT_FLAGS, 0, 8),
        bin(mam_id::LOAD_COUNT, t.load_count, 8),
        bin(mam_id::MAM_SPACE_REMAINING, 1024, 8),
        bin(mam_id::FORMAT_DENSITY_CODE, t.density.unwrap_or(0) as u64, 1),
        bin(mam_id::INITIALIZATION_COUNT, t.init_count, 2),
        bin(mam_id::VOLUME_CHANGE_REFERENCE, t.change_ref as u64, 4),
    ];
    for (i, serial) in t.last_drives.iter().enumerate().take(4) {
        out.push(ascii(mam_id::DEVICE_AT_LAST_LOAD + i as u16, &format!("IBM     {serial:<32}"), 40));
    }
    out.extend([
        bin(mam_id::TOTAL_MB_WRITTEN, t.bytes_written_life / MIB, 8),
        bin(mam_id::TOTAL_MB_READ, t.bytes_read_life / MIB, 8),
        bin(mam_id::LAST_LOAD_MB_WRITTEN, t.bytes_written_load / MIB, 8),
        bin(mam_id::LAST_LOAD_MB_READ, t.bytes_read_load / MIB, 8),
        ascii(mam_id::MEDIUM_MANUFACTURER, "SIMTAPE", 8),
        ascii(mam_id::MEDIUM_SERIAL, &t.serial, 32),
        bin(mam_id::MEDIUM_LENGTH, 960, 4),
        bin(mam_id::MEDIUM_WIDTH, 127, 4),
        bin(mam_id::MEDIUM_DENSITY_CODE, t.cartridge.density() as u64, 1),
        ascii(mam_id::MEDIUM_MANUFACTURE_DATE, &t.manufactured, 8),
        bin(
            mam_id::MEDIUM_TYPE,
            match t.kind {
                MediaKind::Data => 0,
                MediaKind::Cleaning => 1,
                MediaKind::Worm => 0x80,
            },
            1,
        ),
    ]);
    for (id, (fmt, value)) in &t.host_mam {
        let format = match fmt {
            0 => Format::Binary,
            1 => Format::Ascii,
            2 => Format::Text,
            _ => Format::Reserved,
        };
        out.push(Attribute { id: *id, read_only: false, format, value: value.clone() });
    }
    out.sort_by_key(|a| a.id);
    // Round-trip through the wire encoding so the simulator exercises the parser.
    mam::parse_attributes(&mam::encode_attributes(&out)).unwrap_or(out)
}

// ---------------------------------------------------------------------------------------
// Changer

pub struct SimChanger {
    world: SimWorld,
    info: ChangerInfo,
    serial: String,
}

impl Changer for SimChanger {
    fn info(&self) -> &ChangerInfo {
        &self.info
    }

    fn inventory(&mut self) -> Result<Inventory> {
        let serial = self.serial.clone();
        self.world.with_lazy(|st, _| {
            let lib = st.libraries.iter().find(|l| l.serial == serial).ok_or_else(|| sim_err("library gone"))?;
            let tag = |id: &Option<String>| id.as_ref().and_then(|i| st.tapes.get(i)).and_then(|t| t.barcode.clone());
            let src = |id: &Option<String>| id.as_ref().and_then(|i| lib.sources.get(i).copied());
            let mut els = vec![Element {
                kind: ElementType::Transport,
                address: ADDR_TRANSPORT,
                full: false,
                barcode: None,
                source: None,
                drive_serial: None,
                drive_product: None,
                exception: None,
            }];
            for (i, s) in lib.slots.iter().enumerate() {
                els.push(Element {
                    kind: ElementType::Storage,
                    address: ADDR_SLOT + i as u16,
                    full: s.is_some(),
                    barcode: tag(s),
                    source: src(s),
                    drive_serial: None,
                    drive_product: None,
                    exception: None,
                });
            }
            for (i, s) in lib.ie.iter().enumerate() {
                els.push(Element {
                    kind: ElementType::ImportExport,
                    address: ADDR_IE + i as u16,
                    full: s.is_some(),
                    barcode: tag(s),
                    source: src(s),
                    drive_serial: None,
                    drive_product: None,
                    exception: None,
                });
            }
            for (i, bay) in lib.bays.iter().enumerate() {
                let d = bay.as_ref().and_then(|ser| st.drives.iter().find(|d| &d.serial == ser));
                let loaded = d.and_then(|d| d.loaded.clone());
                els.push(Element {
                    kind: ElementType::DataTransfer,
                    address: ADDR_DRIVE + i as u16,
                    full: loaded.is_some(),
                    barcode: tag(&loaded),
                    source: src(&loaded),
                    drive_serial: bay.clone(),
                    drive_product: d.map(|d| format!("ULT3580-HH{}", digit(d.generation))),
                    exception: None,
                });
            }
            Ok(Inventory { elements: els })
        })
    }

    fn move_medium(&mut self, src: u16, dst: u16) -> Result<()> {
        let serial = self.serial.clone();
        self.world.with(|st, _| {
            let WorldState { libraries, drives, tapes, .. } = st;
            let lib = libraries.iter_mut().find(|l| l.serial == serial).ok_or_else(|| sim_err("library gone"))?;
            // Take the cartridge out of the source element.
            let id = match element_ref(lib, src)? {
                Elem::Slot(i) => lib.slots[i].take(),
                Elem::Ie(i) => lib.ie[i].take(),
                Elem::Bay(i) => {
                    let ser = lib.bays[i].clone().ok_or_else(|| sim_err("no drive in bay"))?;
                    let d = drives.iter_mut().find(|d| d.serial == ser).ok_or_else(|| sim_err("drive gone"))?;
                    if d.threaded {
                        return Err(sim_err("medium removal prevented: unload the drive first"));
                    }
                    d.loaded.take()
                }
            }
            .ok_or_else(|| sim_err(format!("source element {src:#06x} is empty")))?;
            let put_back = |lib: &mut LibState, id: String| match element_ref(lib, src) {
                Ok(Elem::Slot(i)) => lib.slots[i] = Some(id),
                Ok(Elem::Ie(i)) => lib.ie[i] = Some(id),
                _ => {}
            };
            match element_ref(lib, dst) {
                Ok(Elem::Slot(i)) if lib.slots[i].is_none() => lib.slots[i] = Some(id.clone()),
                Ok(Elem::Ie(i)) if lib.ie[i].is_none() => lib.ie[i] = Some(id.clone()),
                Ok(Elem::Bay(i)) => {
                    let ser = lib.bays[i].clone();
                    let d = ser.and_then(|ser| drives.iter_mut().find(|d| d.serial == ser));
                    match d {
                        Some(d) if d.loaded.is_none() => {
                            d.loaded = Some(id.clone());
                            // LTO drives load a cartridge as soon as it is inserted.
                            let t = tapes.get_mut(&id).ok_or_else(|| sim_err("cartridge state missing"))?;
                            thread(d, t);
                        }
                        _ => {
                            put_back(lib, id);
                            return Err(sim_err(format!("destination drive {dst:#06x} is not available")));
                        }
                    }
                }
                _ => {
                    put_back(lib, id);
                    return Err(sim_err(format!("destination element {dst:#06x} is full or invalid")));
                }
            }
            if matches!(element_ref(lib, src), Ok(Elem::Slot(_))) {
                lib.sources.insert(id, src);
            }
            Ok(())
        })
    }

    fn rescan(&mut self) -> Result<()> {
        Ok(())
    }
}

enum Elem {
    Slot(usize),
    Ie(usize),
    Bay(usize),
}

fn element_ref(lib: &LibState, addr: u16) -> Result<Elem> {
    let i = |base: u16, n: usize| (addr >= base && ((addr - base) as usize) < n).then(|| (addr - base) as usize);
    if let Some(k) = i(ADDR_SLOT, lib.slots.len()) {
        Ok(Elem::Slot(k))
    } else if let Some(k) = i(ADDR_IE, lib.ie.len()) {
        Ok(Elem::Ie(k))
    } else if let Some(k) = i(ADDR_DRIVE, lib.bays.len()) {
        Ok(Elem::Bay(k))
    } else {
        Err(sim_err(format!("invalid element address {addr:#06x}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world(dir: &Path) -> SimWorld {
        let spec = WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib1".into(),
                slots: 8,
                ie_slots: 1,
                drives: vec![
                    DriveSpec::new(Generation::Lto8).serial("D8"),
                    DriveSpec::new(Generation::Lto6).serial("D6").unattached(),
                ],
                tapes: vec![
                    TapeSpec {
                        barcode: Some("AA0001L8".into()),
                        cartridge: Generation::Lto8,
                        kind: MediaKind::Data,
                        capacity: Some(64 * MIB),
                        slot: None,
                    },
                    TapeSpec {
                        barcode: Some("AA0002L7".into()),
                        cartridge: Generation::Lto7,
                        kind: MediaKind::Data,
                        capacity: Some(60 * MIB),
                        slot: None,
                    },
                    TapeSpec {
                        barcode: Some("AA0003L5".into()),
                        cartridge: Generation::Lto5,
                        kind: MediaKind::Data,
                        capacity: Some(16 * MIB),
                        slot: None,
                    },
                ],
            }],
            standalone: vec![],
        };
        SimWorld::create(&dir.join("w"), &spec).unwrap()
    }

    fn load(w: &SimWorld, barcode: &str) -> SimDrive {
        let mut c = w.changers().unwrap().remove(0);
        let inv = c.inventory().unwrap();
        let src = inv.find_barcode(barcode).unwrap().address;
        c.move_medium(src, ADDR_DRIVE).unwrap();
        w.drive("D8").unwrap()
    }

    #[test]
    fn inventory_and_unattached_drive() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut c = w.changers().unwrap().remove(0);
        let inv = c.inventory().unwrap();
        assert_eq!(inv.of(ElementType::Storage).filter(|e| e.full).count(), 3);
        assert_eq!(inv.drive_bay("D6").unwrap().address, ADDR_DRIVE + 1);
        assert_eq!(w.drives().unwrap().len(), 1);
        assert!(w.drive("D6").is_err());
    }

    #[test]
    fn write_read_filemarks_and_append_only() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut d = load(&w, "AA0001L8");
        d.write_block(b"label").unwrap();
        d.write_filemarks(1).unwrap();
        d.write_block(b"one").unwrap();
        d.write_block(b"two").unwrap();
        d.write_filemarks(1).unwrap();
        assert_eq!(d.position().unwrap().pos.object, 5);
        assert_eq!(d.media_info().unwrap().generation, Some(Generation::Lto8));

        d.rewind().unwrap();
        d.space_filemarks(1).unwrap();
        let mut buf = vec![0u8; 64];
        assert_eq!(d.read_block(&mut buf).unwrap(), ReadStatus::Data(3));
        assert_eq!(&buf[..3], b"one");

        // Overwrite at object 3: "two" and the trailing filemark disappear.
        d.locate(Position { partition: 0, object: 3 }).unwrap();
        d.write_block(b"TWO!").unwrap();
        assert_eq!(d.locate_eod(0).unwrap().pos.object, 4);
        d.locate(Position { partition: 0, object: 3 }).unwrap();
        assert_eq!(d.read_block(&mut buf).unwrap(), ReadStatus::Data(4));
        assert_eq!(d.read_block(&mut buf).unwrap(), ReadStatus::EndOfData);
        assert!(matches!(d.locate(Position { partition: 0, object: 99 }), Err(Error::EndOfData)));
    }

    #[test]
    fn early_warning_then_end_of_medium() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut d = load(&w, "AA0001L8");
        let block = vec![7u8; MIB as usize];
        let mut ew_at = None;
        for i in 0..80 {
            match d.write_block(&block) {
                Ok(WriteStatus::Ok) => {}
                Ok(WriteStatus::EarlyWarning) => {
                    ew_at.get_or_insert(i);
                }
                Err(Error::EndOfMedium) => {
                    assert!(ew_at.is_some_and(|e| e < i));
                    return;
                }
                Err(e) => panic!("{e}"),
            }
        }
        panic!("never reached end of medium");
    }

    #[test]
    fn m8_conversion_via_preferred_density() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut d = load(&w, "AA0002L7");
        let rd = d.report_density(true).unwrap();
        assert_eq!(rd.len(), 2, "new LTO-7 in LTO-8 drive is Type M eligible");
        assert!(rd[0].default && rd[0].primary == dens::LTO7);
        d.set_preferred_density(medium_type::LTO7_DATA, dens::LTO7_M8).unwrap();
        d.format(PartitionLayout::Single).unwrap();
        let rd = d.report_density(true).unwrap();
        assert_eq!(rd.len(), 1);
        assert_eq!(rd[0].primary, dens::LTO7_M8);
        assert_eq!(d.media_info().unwrap().generation, Some(Generation::M8));
    }

    #[test]
    fn incompatible_media_and_mam() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut d = load(&w, "AA0003L5");
        assert!(matches!(d.write_block(b"x"), Err(Error::Incompatible { .. })));
        let attrs = d.read_attributes(0).unwrap();
        assert_eq!(attrs.iter().find(|a| a.id == mam_id::LOAD_COUNT).unwrap().as_u64(), Some(1));
        d.write_attributes(0, &[Attribute::ascii(mam_id::BARCODE, "AA0003L5", 32)]).unwrap();
        assert_eq!(d.read_attribute(0, mam_id::BARCODE).unwrap().unwrap().as_string(), "AA0003L5");
        assert!(d.write_attributes(0, &[Attribute::binary(mam_id::LOAD_COUNT, vec![0; 8])]).is_err());
    }

    #[test]
    fn unload_required_before_move_out() {
        let dir = tempfile::tempdir().unwrap();
        let w = world(dir.path());
        let mut d = load(&w, "AA0001L8");
        let mut c = w.changers().unwrap().remove(0);
        assert!(c.move_medium(ADDR_DRIVE, ADDR_SLOT + 7).is_err());
        d.unload().unwrap();
        c.move_medium(ADDR_DRIVE, ADDR_SLOT + 7).unwrap();
        assert_eq!(d.readiness().unwrap(), Readiness::NoMedium);
        let inv = c.inventory().unwrap();
        assert_eq!(inv.find_barcode("AA0001L8").unwrap().address, ADDR_SLOT + 7);
        assert_eq!(inv.find_barcode("AA0001L8").unwrap().source, Some(ADDR_SLOT));
    }

    #[test]
    fn state_survives_reopen_after_handles_drop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("w");
        {
            let w = world(dir.path());
            let mut d = load(&w, "AA0001L8");
            d.write_block(b"persist").unwrap();
        }
        // All handles dropped: state was written back and the lock released.
        let w = SimWorld::open(&root).unwrap();
        let mut d = w.drive("D8").unwrap();
        assert_eq!(d.position().unwrap().pos.object, 1);
    }

    #[test]
    fn motion_model_counts_pauses_for_a_slow_writer() {
        let dir = tempfile::tempdir().unwrap();
        let spec = WorldSpec {
            libraries: vec![LibrarySpec {
                name: "l".into(),
                slots: 2,
                ie_slots: 0,
                drives: vec![DriveSpec { native_mbps: Some(100.0), buffer_mib: Some(2), ..DriveSpec::new(Generation::Lto8).serial("D8") }],
                tapes: vec![TapeSpec {
                    barcode: Some("AA0001L8".into()),
                    cartridge: Generation::Lto8,
                    kind: MediaKind::Data,
                    capacity: Some(256 * MIB),
                    slot: None,
                }],
            }],
            standalone: vec![],
        };
        let w = SimWorld::create(&dir.path().join("w"), &spec).unwrap();
        let mut d = load(&w, "AA0001L8");
        let block = vec![1u8; 256 << 10];
        // Fast: 16 MiB back to back.
        for _ in 0..64 {
            d.write_block(&block).unwrap();
        }
        d.write_filemarks(1).unwrap();
        let (_, fast_pauses) = w.motion("D8").unwrap();
        // Slow: 256 KiB every 20 ms = 13 MB/s, well under the 33 MB/s floor.
        for _ in 0..40 {
            d.write_block(&block).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        let (_, slow_pauses) = w.motion("D8").unwrap();
        assert!(fast_pauses <= 1, "fast writer paused {fast_pauses} times");
        assert!(slow_pauses > fast_pauses + 2, "slow writer paused only {slow_pauses} times");
        let page = d.log_page(PAGE_PERFORMANCE, 0x60).unwrap();
        assert_eq!(page.u64(0x1001), Some(slow_pauses));
    }
}
