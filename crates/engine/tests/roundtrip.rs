//! Backup -> verify -> restore -> scan round trips on a simulated library.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ltoarc_catalog::{Catalog, LibraryRow, VolumeRow, View};
use ltoarc_device::sim::{DriveSpec, LibrarySpec, SimWorld, TapeSpec, WorldSpec, ADDR_DRIVE, ADDR_SLOT};
use ltoarc_device::{Changer, Generation, MediaKind, TapeDrive};
use ltoarc_engine::backup::{self, NoProgress, Options};
use ltoarc_engine::hash::{self, Mode, Policy};
use ltoarc_engine::meta;
use ltoarc_engine::restore::{self, Restorer};
use ltoarc_engine::{scan, verify};
use ltoarc_format::VolumeLabel;

/// Tests that measure simulated tape motion against wall-clock time run alone; the others
/// share the CPUs freely.
static TIMING: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn alone() -> std::sync::RwLockWriteGuard<'static, ()> {
    TIMING.write().unwrap_or_else(|e| e.into_inner())
}

fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
    TIMING.read().unwrap_or_else(|e| e.into_inner())
}

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    world: SimWorld,
}

impl Rig {
    fn new(capacity: u64) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let world = SimWorld::create(
            &root.join("world"),
            &WorldSpec {
                libraries: vec![LibrarySpec {
                    name: "lib".into(),
                    slots: 4,
                    ie_slots: 0,
                    drives: vec![DriveSpec::new(Generation::Lto8).serial("D")],
                    tapes: vec![TapeSpec {
                        barcode: Some("AA0001L8".into()),
                        cartridge: Generation::Lto8,
                        kind: MediaKind::Data,
                        capacity: Some(capacity),
                        slot: None,
                    }],
                }],
                standalone: vec![],
            },
        )
        .unwrap();
        let mut c = world.changers().unwrap().remove(0);
        c.move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
        Rig { _tmp: tmp, root, world }
    }

    fn drive(&self) -> Box<dyn TapeDrive> {
        Box::new(self.world.drive("D").unwrap())
    }

    /// Catalog with the cartridge registered and labelled (64 KiB blocks to exercise spanning).
    fn catalog(&self, name: &str, write_label: bool) -> (Catalog, VolumeRow) {
        let cat = Catalog::open(&self.root.join(name)).unwrap();
        let lib = cat
            .upsert_library(
                &LibraryRow {
                    id: 0,
                    name: String::new(),
                    serial: "SIMLIB0001".into(),
                    backend: "sim".into(),
                    location: self.root.join("world").display().to_string(),
                    vendor: "IBM".into(),
                    product: "3573-TL".into(),
                    revision: "x".into(),
                    slots: 4,
                    ie_slots: 0,
                    bays: 1,
                    slot_base: Some(ADDR_SLOT as i64),
                    ie_base: None,
                    bay_base: Some(ADDR_DRIVE as i64),
                    last_seen: 0,
                },
                "lib1",
            )
            .unwrap();
        let (mut v, _) = cat.volume_seen("AA0001L8", Some("LTO-8"), "data", lib.id, ADDR_DRIVE as i64).unwrap();
        if write_label {
            let mut label = VolumeLabel::new("AA0001L8", "LTO-8", Some("test".into()), "now".into());
            label.block_size = 64 << 10;
            let mut d = self.drive();
            d.rewind().unwrap();
            d.write_block(&label.to_block()).unwrap();
            d.write_filemarks(1).unwrap();
            v.uuid = Some(label.volume_uuid.to_string());
            v.state = "scratch".into();
            cat.update_volume(&v).unwrap();
        }
        (cat, v)
    }
}

fn write(p: &Path, data: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

/// Source tree with the interesting cases. Returns its root.
fn source_tree(root: &Path) -> PathBuf {
    let src = root.join("src");
    write(&src.join("docs/readme.txt"), b"hello tape\n");
    write(&src.join("docs/deep/nested/file.bin"), &(0..200_000u32).flat_map(|i| i.to_le_bytes()).collect::<Vec<_>>());
    // Several chunks at the 256 KiB chunk size used below, and compressible.
    write(&src.join("big/log.txt"), "the quick brown fox jumps over the lazy dog\n".repeat(30_000).as_bytes());
    write(&src.join("empty"), b"");
    // Sparse: data, 8 MiB hole, data, trailing hole.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = fs::File::create(src.join("sparse.img")).unwrap();
        f.write_all(b"start").unwrap();
        f.seek(SeekFrom::Start(8 << 20)).unwrap();
        f.write_all(b"middle").unwrap();
        f.set_len(16 << 20).unwrap();
    }
    std::os::unix::fs::symlink("docs/readme.txt", src.join("link")).unwrap();
    fs::hard_link(src.join("docs/readme.txt"), src.join("hard")).unwrap();
    write(&src.join("non-utf8-\u{fffd}"), b"x");
    let mut raw = std::os::unix::ffi::OsStrExt::as_bytes(src.as_os_str()).to_vec();
    raw.extend_from_slice(b"/raw-\xff\xfe");
    fs::write(PathBuf::from(std::ffi::OsString::from_vec(raw)), b"y").unwrap();
    let _ = meta::set_xattr(&src.join("docs/readme.txt"), b"user.tag", b"important");
    unsafe {
        let c = std::ffi::CString::new(src.join("fifo").into_os_string().into_vec()).unwrap();
        libc::mkfifo(c.as_ptr(), 0o640);
    }
    fs::set_permissions(src.join("docs/readme.txt"), fs::Permissions::from_mode(0o640)).unwrap();
    meta::set_times(&src.join("big/log.txt"), 1_000_000_000_123_456_789, 1_600_000_000_987_654_321).unwrap();
    src
}

use std::os::unix::ffi::OsStringExt;

/// Everything comparable about a tree: relative path -> description.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut out = BTreeMap::new();
    fn rec(base: &Path, p: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let md = fs::symlink_metadata(p).unwrap();
        let rel = p.strip_prefix(base).unwrap().to_path_buf();
        let xattrs: Vec<_> = meta::list_xattrs(p).unwrap_or_default().into_iter().filter(|(n, _)| n.starts_with(b"user.")).collect();
        let kind = meta::kind_of(&md);
        let content = match kind.as_str() {
            "file" => blake3::hash(&fs::read(p).unwrap()).to_hex().to_string(),
            "symlink" => fs::read_link(p).unwrap().display().to_string(),
            _ => String::new(),
        };
        let mtime = if kind.as_str() == "symlink" || kind.as_str() == "dir" { 0 } else { meta::mtime_ns(&md) };
        out.insert(rel, format!("{} {:o} {} {} {} {:?}", kind.as_str(), md.mode() & 0o7777, md.len(), mtime, content, xattrs));
        if md.is_dir() {
            let mut kids: Vec<_> = fs::read_dir(p).unwrap().map(|e| e.unwrap().path()).collect();
            kids.sort();
            for k in kids {
                rec(base, &k, out);
            }
        }
    }
    rec(root, root, &mut out);
    out
}

fn opts(src: &Path) -> Options {
    let mut o = Options::new(vec![src.to_path_buf()]);
    o.chunk_size = 256 << 10;
    o.ring = ltoarc_engine::ringbuf::Config::new(4 << 20);
    o.pack_max = 64 << 10;
    o.pack_target = 256 << 10;
    o.hash = Policy {
        profiles: hash::resolve_profiles(&["blake3".into(), "sha256".into(), "md5".into()], &Default::default()).unwrap(),
        mode: Mode::UseExisting,
        write_back: false,
        extra_xattrs: vec![],
    };
    o
}

fn restore_all(cat: &Catalog, drive: &mut dyn TapeDrive, src: &Path, dest: &Path, view: View) -> restore::Report {
    let o = restore::Options { dest: dest.to_path_buf(), strip_prefix: Some(src.to_path_buf()), view, ..Default::default() };
    let plan = restore::plan(cat, &[src.to_path_buf()], &o).unwrap();
    let barcodes = plan.barcodes();
    let mut r = Restorer::new(plan, o).unwrap();
    for b in barcodes {
        r.volume(&b, drive).unwrap();
    }
    r.finish().unwrap()
}

#[test]
fn backup_verify_restore_scan() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = source_tree(&rig.root);
    let before = snapshot(&src);
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();

    let rep = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete", "{:?}", rep.warnings);
    assert!(rep.volumes[0].catalog_object.is_some(), "{:?}", rep.warnings);
    assert_eq!(rep.volumes[0].start_object, 2);
    assert_eq!((rep.files, rep.others), (7, 3), "7 files; symlink, hard link, fifo");

    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
    assert_eq!(v.missing, 0);
    assert_eq!(v.catalogs, 1);
    assert!(v.file_hashes >= 3 * 7);

    let dest = rig.root.join("restored");
    let rr = restore_all(&cat, &mut *drive, &src, &dest, View::default());
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert!(rr.verified_hashes > 0);
    assert_eq!(snapshot(&dest), before);
    let a = fs::metadata(dest.join("docs/readme.txt")).unwrap();
    let b = fs::metadata(dest.join("hard")).unwrap();
    assert_eq!(a.ino(), b.ino(), "hard link preserved");
    let sparse = fs::metadata(dest.join("sparse.img")).unwrap();
    assert!(sparse.blocks() * 512 < sparse.len(), "holes preserved");

    // Second session with a modified file, compressed; restore the first via as_of.
    let first_started = cat.sessions().unwrap()[0].started;
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&src.join("docs/readme.txt"), b"changed!\n");
    let mut o2 = opts(&src);
    o2.compression = ltoarc_engine::compress::Policy::new(Some(3), &[], &[]).unwrap();
    let rep2 = backup::backup(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &o2, &mut NoProgress).unwrap();
    assert!(rep2.stored < rep2.bytes, "compression used");
    assert_eq!(rep2.volumes[0].start_object, rep.volumes[0].catalog_object.unwrap() + 2);
    let v2 = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v2.errors.is_empty(), "{:?}", v2.errors);
    assert_eq!((v2.sessions, v2.catalogs), (2, 2));

    let latest = rig.root.join("latest");
    restore_all(&cat, &mut *drive, &src, &latest, View::default());
    assert_eq!(fs::read(latest.join("docs/readme.txt")).unwrap(), b"changed!\n");
    let old = rig.root.join("old");
    restore_all(&cat, &mut *drive, &src, &old, View { as_of: Some(first_started), session: None });
    assert_eq!(fs::read(old.join("docs/readme.txt")).unwrap(), b"hello tape\n");

    // Rebuild a catalog from the tape alone and restore from it.
    let (cat2, vol2) = rig.catalog("rebuilt.db", false);
    let s = scan::scan_volume(&cat2, &mut *drive, &vol2, false, &[]).unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!((s.added.len(), s.catalogs), (2, 2));
    assert_eq!(cat2.volume_sessions(vol2.id).unwrap().iter().filter(|sv| sv.catalog_object.is_some()).count(), 2);
    let again = scan::scan_volume(&cat2, &mut *drive, &cat2.volume("AA0001L8").unwrap().unwrap(), false, &[]).unwrap();
    assert_eq!(again.skipped.len(), 2);
    let from_scan = rig.root.join("from-scan");
    let rr = restore_all(&cat2, &mut *drive, &src, &from_scan, View::default());
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&from_scan), snapshot(&latest));
    let v3 = verify::verify_volume(&cat2, &mut *drive, &cat2.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v3.errors.is_empty(), "{:?}", v3.errors);
}

#[test]
fn early_warning_ends_session_cleanly() {
    let _shared = shared();
    let rig = Rig::new(24 << 20); // early warning at 20 MiB (4 MiB reserve)
    let src = rig.root.join("src");
    for i in 0..12 {
        let data: Vec<u8> = (0..(3 << 20)).map(|j: u32| (j.wrapping_mul(2654435761).wrapping_add(i) >> 13) as u8).collect();
        write(&src.join(format!("f{i:02}.bin")), &data);
    }
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let rep = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap();
    assert_eq!(rep.status, "partial");
    assert!(rep.early_warning);
    assert!(rep.files < 12);
    assert_eq!(cat.volume("AA0001L8").unwrap().unwrap().state, "full");
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
}

/// A slow source: without buffering the drive keeps running dry (shoe-shining); with the
/// hysteresis ring buffer it writes long bursts and stops once per burst.
#[test]
fn ring_buffer_prevents_shoe_shining() {
    let _alone = alone();
    use ltoarc_device::sim::{DriveSpec, LibrarySpec, TapeSpec, WorldSpec};
    let run = |ring: u64| -> (u64, u64) {
        let tmp = tempfile::tempdir().unwrap();
        let world = SimWorld::create(
            &tmp.path().join("world"),
            &WorldSpec {
                libraries: vec![LibrarySpec {
                    name: "lib".into(),
                    slots: 2,
                    ie_slots: 0,
                    drives: vec![DriveSpec { native_mbps: Some(90.0), buffer_mib: Some(1), ..DriveSpec::new(Generation::Lto8).serial("D") }],
                    tapes: vec![TapeSpec {
                        barcode: Some("AA0001L8".into()),
                        cartridge: Generation::Lto8,
                        kind: MediaKind::Data,
                        capacity: Some(1 << 30),
                        slot: None,
                    }],
                }],
                standalone: vec![],
            },
        )
        .unwrap();
        world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
        let rig = Rig { _tmp: tmp, root: PathBuf::new(), world };
        let root = rig._tmp.path().to_path_buf();
        let rig = Rig { root, ..rig };
        let src = rig.root.join("src");
        for i in 0..6u8 {
            write(&src.join(format!("f{i}")), &vec![i; 4 << 20]);
        }
        let (cat, vol) = rig.catalog("cat.db", true);
        let mut drive = rig.drive();
        let mut o = opts(&src);
        o.ring = ltoarc_engine::ringbuf::Config::new(ring);
        o.read_limit = Some(12_000_000); // 12 MB/s against a 30 MB/s speed-matching floor
        let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
        assert_eq!(rep.status, "complete");
        // Each write cycle is the tape starting after a stop (what IBM LP 37h counts).
        (rep.drive_write_cycles.unwrap(), rep.writer.bursts)
    };
    let (pauses_unbuffered, _) = run(128 << 10);
    let (pauses_buffered, bursts) = run(16 << 20);
    eprintln!("tape start/stop cycles: {pauses_unbuffered} unbuffered; {pauses_buffered} with a 16 MiB ring ({bursts} bursts)");
    assert!(pauses_unbuffered >= 8, "expected shoe-shining without a buffer, got {pauses_unbuffered} pauses");
    assert!(pauses_buffered <= bursts + 1 && pauses_buffered * 4 <= pauses_unbuffered,
        "buffered run paused {pauses_buffered} times in {bursts} bursts vs {pauses_unbuffered} unbuffered");
}

/// Simulated LTO-8 rig with a given native rate and an 8 MiB drive buffer (enough to ride
/// out a speed change, as a real drive's gigabyte is).
fn speed_rig(native_mbps: f64) -> Rig {
    use ltoarc_device::sim::{DriveSpec, LibrarySpec, TapeSpec, WorldSpec};
    let tmp = tempfile::tempdir().unwrap();
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 2,
                ie_slots: 0,
                drives: vec![DriveSpec { native_mbps: Some(native_mbps), buffer_mib: Some(8), ..DriveSpec::new(Generation::Lto8).serial("D") }],
                tapes: vec![TapeSpec {
                    barcode: Some("AA0001L8".into()),
                    cartridge: Generation::Lto8,
                    kind: MediaKind::Data,
                    capacity: Some(1 << 30),
                    slot: None,
                }],
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let root = tmp.path().to_path_buf();
    Rig { _tmp: tmp, root, world }
}

/// A source slower than the drive but above its floor: bursts stop the tape after each one;
/// pacing lets the drive slow down and keep moving.
#[test]
fn pacing_matches_speed_instead_of_stopping() {
    let _alone = alone();
    let run = |pace: Option<ltoarc_engine::ringbuf::Pace>| -> u64 {
        let rig = speed_rig(60.0); // floor 20 MB/s in the simulator
        let src = rig.root.join("src");
        for i in 0..6u8 {
            write(&src.join(format!("f{i}")), &vec![i; 6 << 20]);
        }
        let (cat, vol) = rig.catalog("cat.db", true);
        let mut drive = rig.drive();
        let mut o = opts(&src);
        o.ring = ltoarc_engine::ringbuf::Config { pace, ..ltoarc_engine::ringbuf::Config::new(8 << 20) };
        o.read_limit = Some(35_000_000);
        let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
        assert_eq!(rep.status, "complete");
        rep.drive_write_cycles.unwrap()
    };
    let bursts = run(None);
    let paced = run(Some(ltoarc_engine::ringbuf::Pace { floor: 20e6, max: 60e6, gap: None }));
    eprintln!("tape start/stop cycles at 35 MB/s: {bursts} in burst mode, {paced} paced");
    assert!(paced <= 1, "paced run stopped {paced} times");
    assert!(bursts >= paced + 3, "burst mode stopped only {bursts} times");
}

/// Large files go to tape first and whole; small files are stacked and written after them.
#[test]
fn large_files_first_small_files_stacked() {
    let _shared = shared();
    let rig = speed_rig(300.0);
    let src = rig.root.join("src");
    for i in 0..200u32 {
        write(&src.join(format!("a-small/{i:03}.txt")), format!("small file {i}\n").repeat(40).as_bytes());
    }
    for i in 0..3u8 {
        write(&src.join(format!("z-large/{i}.bin")), &vec![i; 3 << 20]);
    }
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let mut o = opts(&src);
    o.startup_window = Duration::from_secs(5);
    let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    assert!(rep.packs >= 1);
    let placements = cat.volume_placements(vol.id).unwrap();
    let last_large = placements.iter().filter(|(_, c, _)| c.pack_offset.is_none()).map(|(p, _, _)| (p.block, p.offset)).max().unwrap();
    let first_pack = placements.iter().filter(|(_, c, _)| c.pack_offset.is_some()).map(|(p, _, _)| (p.block, p.offset)).min().unwrap();
    assert!(last_large < first_pack, "large data ends at {last_large:?}, packs start at {first_pack:?}");
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
}

// ---------------------------------------------------------------------------------------
// Spanning and crash recovery

/// Library with `n` small cartridges AA0001L8.. ; the first is loaded.
fn set_rig(capacity: u64, n: usize) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let tapes = (1..=n)
        .map(|i| TapeSpec {
            barcode: Some(format!("AA{i:04}L8")),
            cartridge: Generation::Lto8,
            kind: MediaKind::Data,
            capacity: Some(capacity),
            slot: None,
        })
        .collect();
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 4,
                ie_slots: 0,
                drives: vec![DriveSpec::new(Generation::Lto8).serial("D")],
                tapes,
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let root = tmp.path().to_path_buf();
    Rig { _tmp: tmp, root, world }
}

/// Put `barcode` into the drive (returning whatever is there to its slot).
fn load_tape(rig: &Rig, drive: &mut dyn TapeDrive, barcode: &str) {
    load_tape_at(rig, drive, barcode, ADDR_DRIVE)
}

/// Put `barcode` into the drive in bay `bay`.
fn load_tape_at(rig: &Rig, drive: &mut dyn TapeDrive, barcode: &str, bay: u16) {
    let mut c = rig.world.changers().unwrap().remove(0);
    let inv = c.inventory().unwrap();
    let held = inv.element(bay).unwrap().clone();
    if held.barcode.as_deref() == Some(barcode) {
        return;
    }
    if held.full {
        drive.unload().unwrap();
        let home = held.source.filter(|s| inv.element(*s).is_some_and(|e| !e.full)).or_else(|| inv.first_empty_slot().map(|e| e.address));
        c.move_medium(bay, home.unwrap()).unwrap();
    }
    let el = c.inventory().unwrap().find_barcode(barcode).unwrap().address;
    c.move_medium(el, bay).unwrap();
    drive.wait_ready(Duration::from_secs(5)).unwrap();
}

/// The rest of a simulated tape set; blank cartridges are labelled as they come up.
struct SimSet<'a> {
    rig: &'a Rig,
    cat: &'a Catalog,
    rest: std::collections::VecDeque<String>,
    bay: u16,
}

impl backup::NextVolume for SimSet<'_> {
    fn available(&mut self, _lane: usize) -> bool {
        !self.rest.is_empty()
    }

    fn next(&mut self, _lane: usize, drive: &mut dyn TapeDrive, _full: &VolumeRow) -> anyhow::Result<backup::Next> {
        let bc = self.rest.pop_front().unwrap();
        load_tape_at(self.rig, drive, &bc, self.bay);
        let generation = ltoarc_device::parse_barcode(&bc).generation.unwrap().name();
        let mut label = VolumeLabel::new(&bc, generation, Some("test".into()), "now".into());
        label.block_size = 64 << 10;
        ltoarc_engine::tapeio::write_label(drive, &label)?;
        let lib = self.cat.libraries()?.remove(0);
        let (mut v, _) = self.cat.volume_seen(&bc, Some(generation), "data", lib.id, self.bay as i64)?;
        v.uuid = Some(label.volume_uuid.to_string());
        v.state = "scratch".into();
        self.cat.update_volume(&v)?;
        Ok(backup::Next { volume: v, drive: None, pace: None })
    }
}

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn restore_set(rig: &Rig, cat: &Catalog, drive: &mut dyn TapeDrive, src: &Path, dest: &Path) -> restore::Report {
    let o = restore::Options { dest: dest.to_path_buf(), strip_prefix: Some(src.to_path_buf()), ..Default::default() };
    let plan = restore::plan(cat, &[src.to_path_buf()], &o).unwrap();
    let barcodes = plan.barcodes();
    let mut r = Restorer::new(plan, o).unwrap();
    for b in barcodes {
        load_tape(rig, drive, &b);
        r.volume(&b, drive).unwrap();
    }
    r.finish().unwrap()
}

#[test]
fn session_spans_tape_set() {
    let _shared = shared();
    let rig = set_rig(40 << 20, 4); // about 35 MiB of session data per cartridge
    let src = rig.root.join("src");
    write(&src.join("a/first.bin"), &noise(1, 30 << 20));
    write(&src.join("a/second.bin"), &noise(2, 45 << 20)); // cut across a cartridge boundary
    for i in 0..300u64 {
        write(&src.join(format!("small/{:03}/f{i}.dat", i % 7)), &noise(100 + i, 1000 + (i as usize * 37) % 30_000));
    }
    let before = snapshot(&src);
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let mut set = SimSet { rig: &rig, cat: &cat, rest: ["AA0002L8", "AA0003L8", "AA0004L8"].map(String::from).into(), bay: ADDR_DRIVE };
    let rep = backup::backup_spanning(&cat, &mut *drive, &vol, &opts(&src), &mut set, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete", "{:?}", rep.warnings);
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let used: Vec<&str> = rep.volumes.iter().map(|v| v.barcode.as_str()).collect();
    assert_eq!(used, ["AA0001L8", "AA0002L8", "AA0003L8"]);
    assert!(rep.volumes.iter().all(|v| v.catalog_object.is_some()));
    assert_eq!(set.rest.len(), 1, "fourth cartridge untouched");
    let svs = cat.session_volumes(rep.session_id).unwrap();
    assert_eq!(svs.iter().map(|s| s.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(cat.volume("AA0001L8").unwrap().unwrap().state, "full");
    assert_eq!(cat.volume("AA0003L8").unwrap().unwrap().state, "filling");
    // Some file has chunks on two cartridges.
    let root_id = cat.lookup_path(src.as_os_str().as_encoded_bytes()).unwrap().unwrap();
    let mut split = 0;
    for (_, o) in cat.resolve(root_id, View::default()).unwrap() {
        let mut vols: Vec<i64> = cat.chunk_placements(o.id).unwrap().iter().map(|c| c.placement.volume_id).collect();
        vols.sort();
        vols.dedup();
        if vols.len() > 1 {
            split += 1;
        }
    }
    assert!(split >= 1, "a file is cut across cartridges");

    // Every cartridge verifies on its own, with a coherent cartridge memory record.
    for bc in used.iter() {
        load_tape(&rig, &mut *drive, bc);
        let v = verify::verify_volume(&cat, &mut *drive, &cat.volume(bc).unwrap().unwrap(), &[]).unwrap();
        assert!(v.errors.is_empty(), "{bc}: {:?}", v.errors);
        assert_eq!(v.missing, 0, "{bc}");
        assert!(matches!(ltoarc_engine::vci::state(&mut *drive).unwrap(), ltoarc_engine::vci::State::Clean(_)), "{bc}");
    }
    let dest = rig.root.join("restored");
    let rr = restore_set(&rig, &cat, &mut *drive, &src, &dest);
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&dest), before);

    // A catalog rebuilt from the cartridges alone, read in reverse order.
    let cat2 = Catalog::open(&rig.root.join("rebuilt.db")).unwrap();
    let lib = cat.libraries().unwrap().remove(0);
    let lib2 = cat2.upsert_library(&lib, "lib1").unwrap();
    for bc in used.iter().rev() {
        let (v2, _) = cat2.volume_seen(bc, Some("LTO-8"), "data", lib2.id, ADDR_DRIVE as i64).unwrap();
        load_tape(&rig, &mut *drive, bc);
        let s = scan::scan_volume(&cat2, &mut *drive, &v2, false, &[]).unwrap();
        assert!(s.errors.is_empty(), "{bc}: {:?}", s.errors);
        assert_eq!(s.added.len(), 1, "{bc}");
    }
    let sessions = cat2.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].status, "complete");
    assert_eq!(sessions[0].files, rep.files as i64);
    let from_scan = rig.root.join("from-scan");
    let rr = restore_set(&rig, &cat2, &mut *drive, &src, &from_scan);
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&from_scan), before);
}

#[test]
fn crash_mid_session_is_recovered() {
    let _shared = shared();
    use ltoarc_engine::recover::recover_volume;
    use ltoarc_engine::vci::{state, State};
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    for i in 0..6u64 {
        write(&src.join(format!("f{i}.bin")), &noise(i, 2 << 20));
    }
    for i in 0..40u64 {
        write(&src.join(format!("small/s{i}")), &noise(50 + i, 4000));
    }
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap();
    assert!(matches!(state(&mut *drive).unwrap(), State::Clean(_)));

    // The drive dies before the first block: nothing to recover but the open mark.
    rig.world.fail_writes_after("D", Some(0)).unwrap();
    assert!(backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).is_err());
    rig.world.fail_writes_after("D", None).unwrap();
    assert!(matches!(state(&mut *drive).unwrap(), State::Open(_)));
    assert_eq!(recover_volume(&cat, &mut *drive, &vol, &[]).unwrap().state, "nothing written");
    assert!(matches!(state(&mut *drive).unwrap(), State::Clean(_)));

    // The drive dies 100 blocks (6.25 MiB) into a session.
    rig.world.fail_writes_after("D", Some(100)).unwrap();
    let err = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap_err();
    assert!(format!("{err:#}").contains("simulated failure"), "{err:#}");
    rig.world.fail_writes_after("D", None).unwrap();
    assert_eq!(cat.sessions().unwrap().len(), 1, "the failed session was rolled back");
    let refused = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap_err();
    assert!(format!("{refused:#}").contains("unfinished session"), "{refused:#}");

    let r = recover_volume(&cat, &mut *drive, &vol, &[]).unwrap();
    assert_eq!(r.state, "recovered");
    assert!(!r.already_known);
    let sessions = cat.sessions().unwrap();
    assert_eq!(sessions.len(), 2);
    let recovered = &sessions[1];
    assert_eq!(recovered.status, "partial");
    // What reached the tape is there: some complete files, the one being written truncated.
    let root_id = cat.lookup_path(src.as_os_str().as_encoded_bytes()).unwrap().unwrap();
    let objs: Vec<_> = cat.resolve(root_id, View { as_of: None, session: Some(recovered.id) }).unwrap();
    assert!(objs.iter().any(|(_, o)| o.kind == "file" && o.status == "ok"));
    assert!(objs.iter().any(|(_, o)| o.status == "truncated"), "{:?}", objs.iter().map(|(_, o)| &o.status).collect::<Vec<_>>());
    assert!(matches!(state(&mut *drive).unwrap(), State::Clean(_)));
    // The latest view keeps the complete earlier version of the file that was cut.
    let cut = objs.iter().find(|(_, o)| o.status == "truncated").unwrap().0.clone();
    let shown = cat.latest(cat.lookup_path(&cut).unwrap().unwrap(), View::default()).unwrap().unwrap();
    assert_eq!((shown.session_id, shown.status.as_str()), (sessions[0].id, "ok"));

    // The cartridge is usable again and verifies, recovered session included.
    let rep = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    assert_eq!(rep.volumes[0].start_object, r.catalog_object.unwrap() + 2);
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
    assert_eq!((v.sessions, v.catalogs), (3, 3));
    let dest = rig.root.join("restored");
    let rr = restore_all(&cat, &mut *drive, &src, &dest, View::default());
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&dest), snapshot(&src));
}

#[test]
fn encrypted_session_hides_names_and_data() {
    let _shared = shared();
    use ltoarc_engine::crypt::Key;
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("secret-plans/needle-name.txt"), b"needle-content needle-content");
    write(&src.join("big.bin"), &"needle-content ".repeat(100_000).into_bytes());
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let key = Key::generate();
    let mut o = opts(&src);
    o.encrypt = Some(key.clone());
    let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    assert!(cat.sessions().unwrap()[0].encryption.is_some());
    // Nothing readable on tape: no names, no data, not even in the volume catalog.
    rig.world.flush().unwrap();
    let mut raw = Vec::new();
    for e in walkdir(&rig.root.join("world")) {
        raw.extend(fs::read(e).unwrap());
    }
    let find = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
    assert!(!find(b"needle-name"), "file name on tape");
    assert!(!find(b"needle-content"), "file data on tape");
    assert!(!find(b"secret-plans"));

    // The key is needed to read it back, and enough.
    assert!(verify::verify_volume(&cat, &mut *drive, &vol, &[]).is_err());
    let v = verify::verify_volume(&cat, &mut *drive, &vol, std::slice::from_ref(&key)).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
    assert_eq!((v.catalogs, v.missing), (1, 0));
    let dest = rig.root.join("restored");
    let ro = |keys: Vec<Key>| restore::Options { dest: dest.clone(), strip_prefix: Some(src.clone()), keys, ..Default::default() };
    let plan = restore::plan(&cat, std::slice::from_ref(&src), &ro(vec![])).unwrap();
    assert!(Restorer::new(plan, ro(vec![Key::generate()])).is_err(), "wrong key refused before reading");
    let plan = restore::plan(&cat, std::slice::from_ref(&src), &ro(vec![key.clone()])).unwrap();
    let mut r = Restorer::new(plan, ro(vec![key.clone()])).unwrap();
    r.volume("AA0001L8", &mut *drive).unwrap();
    let rr = r.finish().unwrap();
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&dest), snapshot(&src));

    // A plaintext session cannot join the encrypted cartridge.
    let e = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap_err();
    assert!(format!("{e:#}").contains("encrypted (key"), "{e:#}");

    // Scan needs the key, then rebuilds the session with its wrapped key.
    let (cat2, vol2) = rig.catalog("rebuilt.db", false);
    let s = scan::scan_volume(&cat2, &mut *drive, &vol2, false, &[]).unwrap();
    assert!(s.added.is_empty() && !s.errors.is_empty());
    let vol2 = cat2.volume("AA0001L8").unwrap().unwrap();
    let s = scan::scan_volume(&cat2, &mut *drive, &vol2, false, std::slice::from_ref(&key)).unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.added.len(), 1);
    assert!(cat2.sessions().unwrap()[0].encryption.is_some());
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn written_xattrs_reach_source_and_archive() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("doc.txt"), b"hello xattrs");
    write(&src.join("big.bin"), &noise(9, 3 << 20));
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let mut o = opts(&src);
    o.hash.profiles = hash::resolve_profiles(&["blake3".into()], &Default::default()).unwrap();
    o.hash.write_back = true;
    o.hash.extra_xattrs = vec![("user.backup.tapes".into(), "{barcodes}".into()), ("user.backup.b3".into(), "{blake3}".into())];
    let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    let b3 = blake3::hash(b"hello xattrs").to_hex().to_string();
    let get = |p: &Path, n: &str| meta::list_xattrs(p).unwrap().into_iter().find(|(k, _)| k == n.as_bytes()).map(|(_, v)| String::from_utf8(v).unwrap());
    let profile = &o.hash.profiles[0];
    let hash_x = profile.xattr.clone().expect("blake3 profile has an xattr");
    // On the source ...
    assert_eq!(get(&src.join("doc.txt"), &hash_x).as_deref(), Some(b3.as_str()));
    assert_eq!(get(&src.join("doc.txt"), "user.backup.tapes").as_deref(), Some("AA0001L8"));
    assert_eq!(get(&src.join("doc.txt"), "user.backup.b3").as_deref(), Some(b3.as_str()));
    // ... and on what a restore brings back (from the catalog, and from a catalog rebuilt from tape).
    let dest = rig.root.join("restored");
    let rr = restore_all(&cat, &mut *drive, &src, &dest, View::default());
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(get(&dest.join("doc.txt"), &hash_x).as_deref(), Some(b3.as_str()));
    assert_eq!(get(&dest.join("big.bin"), "user.backup.tapes").as_deref(), Some("AA0001L8"));
    let (cat2, vol2) = rig.catalog("rebuilt.db", false);
    scan::scan_volume(&cat2, &mut *drive, &vol2, false, &[]).unwrap();
    let dest2 = rig.root.join("from-scan");
    restore_all(&cat2, &mut *drive, &src, &dest2, View::default());
    assert_eq!(get(&dest2.join("doc.txt"), "user.backup.b3").as_deref(), Some(b3.as_str()));
}

/// Two drives write one session at once; files are spread by size and every cartridge
/// carries the directory tree.
#[test]
fn parallel_drives_split_by_size() {
    let _shared = shared();
    let tmp = tempfile::tempdir().unwrap();
    let tapes = (1..=2)
        .map(|i| TapeSpec { barcode: Some(format!("AA{i:04}L8")), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(256 << 20), slot: None })
        .collect();
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 4,
                ie_slots: 0,
                drives: vec![DriveSpec::new(Generation::Lto8).serial("D"), DriveSpec::new(Generation::Lto8).serial("E")],
                tapes,
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    let mut c = world.changers().unwrap().remove(0);
    c.move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    c.move_medium(ADDR_SLOT + 1, ADDR_DRIVE + 1).unwrap();
    let rig = Rig { root: tmp.path().to_path_buf(), _tmp: tmp, world };
    let src = rig.root.join("src");
    for (i, mb) in [9usize, 7, 6, 5, 4, 3, 2, 2].iter().enumerate() {
        write(&src.join(format!("large/f{i}.bin")), &noise(i as u64, mb << 20));
    }
    for i in 0..200u64 {
        write(&src.join(format!("small/d{}/s{i}", i % 5)), &noise(1000 + i, 3000 + (i as usize * 13) % 20_000));
    }
    let before = snapshot(&src);
    let (cat, v1) = rig.catalog("cat.db", true);
    let lib = cat.libraries().unwrap().remove(0);
    let mut d1: Box<dyn TapeDrive> = Box::new(rig.world.drive("D").unwrap());
    let mut d2: Box<dyn TapeDrive> = Box::new(rig.world.drive("E").unwrap());
    let mut label = VolumeLabel::new("AA0002L8", "LTO-8", None, "now".into());
    label.block_size = 64 << 10;
    ltoarc_engine::tapeio::write_label(&mut *d2, &label).unwrap();
    let (mut v2, _) = cat.volume_seen("AA0002L8", Some("LTO-8"), "data", lib.id, ADDR_DRIVE as i64 + 1).unwrap();
    v2.uuid = Some(label.volume_uuid.to_string());
    cat.update_volume(&v2).unwrap();
    let lanes = vec![
        backup::LaneStart { drive: &mut *d1, volume: v1.clone(), pace: None },
        backup::LaneStart { drive: &mut *d2, volume: v2.clone(), pace: None },
    ];
    let rep = backup::backup_parallel(&cat, lanes, &opts(&src), &mut backup::SingleVolume, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete", "{:?}", rep.warnings);
    assert_eq!(rep.writers.len(), 2);
    let (a, b) = (rep.writers[0].bytes as f64, rep.writers[1].bytes as f64);
    assert!(a.min(b) / (a + b) > 0.3, "lanes balanced by size: {a} vs {b}");

    for (bc, drive) in [("AA0001L8", &mut d1), ("AA0002L8", &mut d2)] {
        let v = verify::verify_volume(&cat, &mut **drive, &cat.volume(bc).unwrap().unwrap(), &[]).unwrap();
        assert!(v.errors.is_empty(), "{bc}: {:?}", v.errors);
        assert_eq!(v.missing, 0);
    }
    let dest = rig.root.join("restored");
    let o = restore::Options { dest: dest.clone(), strip_prefix: Some(src.clone()), ..Default::default() };
    let plan = restore::plan(&cat, std::slice::from_ref(&src), &o).unwrap();
    assert_eq!(plan.barcodes(), ["AA0001L8", "AA0002L8"]);
    let mut r = Restorer::new(plan, o).unwrap();
    r.volume("AA0001L8", &mut *d1).unwrap();
    r.volume("AA0002L8", &mut *d2).unwrap();
    let rr = r.finish().unwrap();
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&dest), before);

    // Each cartridge alone knows the whole directory tree.
    let (cat2, _) = rig.catalog("only-second.db", false);
    let (v2b, _) = cat2.volume_seen("AA0002L8", Some("LTO-8"), "data", cat2.libraries().unwrap()[0].id, ADDR_DRIVE as i64 + 1).unwrap();
    let s = scan::scan_volume(&cat2, &mut *d2, &v2b, false, &[]).unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let dirs: Vec<String> = cat2
        .resolve(cat2.lookup_path(src.as_os_str().as_encoded_bytes()).unwrap().unwrap(), View::default())
        .unwrap()
        .into_iter()
        .filter(|(_, o)| o.kind == "dir")
        .map(|(p, _)| String::from_utf8_lossy(&p).into_owned())
        .collect();
    assert!(dirs.iter().any(|d| d.ends_with("small/d3")) && dirs.iter().any(|d| d.ends_with("/large")), "{dirs:?}");
}

/// The source copy's cartridges, loaded one after another into one drive.
struct SimSource<'a> {
    rig: &'a Rig,
    drive: Box<dyn TapeDrive>,
}

impl backup::copy::SourceVolumes for SimSource<'_> {
    fn load(&mut self, volume: &VolumeRow) -> anyhow::Result<&mut dyn TapeDrive> {
        load_tape_at(self.rig, &mut *self.drive, &volume.barcode, ADDR_DRIVE);
        Ok(&mut *self.drive)
    }
}

/// A session spanning two LTO-8 cartridges is copied, in an LTO-6 drive, onto LTO-5
/// cartridges of a third the size; the copy alone restores everything.
#[test]
fn tape_to_tape_copy_spans_both_sides() {
    let _shared = shared();
    let tmp = tempfile::tempdir().unwrap();
    let mut tapes: Vec<TapeSpec> = (1..=2)
        .map(|i| TapeSpec { barcode: Some(format!("AA{i:04}L8")), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(40 << 20), slot: None })
        .collect();
    tapes.extend((1..=4).map(|i| TapeSpec {
        barcode: Some(format!("BB{i:04}L5")),
        cartridge: Generation::Lto5,
        kind: MediaKind::Data,
        capacity: Some(24 << 20),
        slot: None,
    }));
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 8,
                ie_slots: 0,
                drives: vec![DriveSpec::new(Generation::Lto8).serial("D"), DriveSpec::new(Generation::Lto6).serial("E")],
                tapes,
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let rig = Rig { root: tmp.path().to_path_buf(), _tmp: tmp, world };
    let src = rig.root.join("src");
    write(&src.join("a.bin"), &noise(1, 20 << 20));
    write(&src.join("b.bin"), &noise(2, 25 << 20));
    for i in 0..80u64 {
        write(&src.join(format!("small/{i}")), &noise(10 + i, 5000));
    }
    let before = snapshot(&src);
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let mut set = SimSet { rig: &rig, cat: &cat, rest: ["AA0002L8"].map(String::from).into(), bay: ADDR_DRIVE };
    let rep = backup::backup_spanning(&cat, &mut *drive, &vol, &opts(&src), &mut set, &mut NoProgress).unwrap();
    assert_eq!((rep.status.as_str(), rep.volumes.len()), ("complete", 2), "{:?}", rep.warnings);

    // Destination: the first LTO-5 cartridge in the LTO-6 drive, the rest as they fill.
    let mut dest: Box<dyn TapeDrive> = Box::new(rig.world.drive("E").unwrap());
    let mut dset = SimSet { rig: &rig, cat: &cat, rest: ["BB0001L5", "BB0002L5", "BB0003L5", "BB0004L5"].map(String::from).into(), bay: ADDR_DRIVE + 1 };
    let first = backup::NextVolume::next(&mut dset, 0, &mut *dest, &vol).unwrap().volume;
    let mut source = SimSource { rig: &rig, drive };
    let copts = backup::copy::CopyOptions { ring: ltoarc_engine::ringbuf::Config::new(4 << 20), read_buffer: 8 << 20, keys: vec![], source_pace: None, match_rate: None };
    let c = backup::copy::copy_session(
        &cat,
        rep.session_id,
        1,
        2,
        &mut source,
        backup::LaneStart { drive: &mut *dest, volume: first, pace: None },
        &copts,
        &mut dset,
        &mut NoProgress,
    )
    .unwrap();
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    assert!(c.volumes.len() >= 3, "spans the small cartridges: {:?}", c.volumes.iter().map(|v| &v.barcode).collect::<Vec<_>>());
    assert!(c.verified > 0 && c.verified == c.chunks);
    let copy2: Vec<_> = cat.session_volumes(rep.session_id).unwrap().into_iter().filter(|sv| sv.copy == 2).collect();
    assert_eq!(copy2.len(), c.volumes.len());
    assert!(copy2.iter().all(|sv| sv.catalog_object.is_some()));

    // Every new cartridge verifies on its own.
    for v in &c.volumes {
        load_tape_at(&rig, &mut *dest, &v.barcode, ADDR_DRIVE + 1);
        let r = verify::verify_volume(&cat, &mut *dest, &cat.volume(&v.barcode).unwrap().unwrap(), &[]).unwrap();
        assert!(r.errors.is_empty(), "{}: {:?}", v.barcode, r.errors);
        assert_eq!(r.missing, 0);
    }
    // Copy 2 alone restores everything, reading only the LTO-5 cartridges.
    let dest_dir = rig.root.join("restored");
    let o = restore::Options { dest: dest_dir.clone(), strip_prefix: Some(src.clone()), copy: Some(2), ..Default::default() };
    let plan = restore::plan(&cat, std::slice::from_ref(&src), &o).unwrap();
    assert!(plan.barcodes().iter().all(|b| b.starts_with("BB")), "{:?}", plan.barcodes());
    let barcodes = plan.barcodes();
    let mut r = Restorer::new(plan, o).unwrap();
    for b in barcodes {
        load_tape_at(&rig, &mut *dest, &b, ADDR_DRIVE + 1);
        r.volume(&b, &mut *dest).unwrap();
    }
    let rr = r.finish().unwrap();
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&dest_dir), before);
}

/// Copying from a fast drive to a slow one: paced, the source drive slows to the
/// destination's speed and streams; reading at full speed instead, it fills the buffer and
/// has to stop again and again.
#[test]
fn copy_matches_the_source_to_the_destination() {
    let _alone = alone();
    let tmp = tempfile::tempdir().unwrap();
    let mut tapes: Vec<TapeSpec> = vec![TapeSpec { barcode: Some("AA0001L8".into()), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(400 << 20), slot: None }];
    tapes.extend((1..=2).map(|i| TapeSpec { barcode: Some(format!("BB{i:04}L5")), cartridge: Generation::Lto5, kind: MediaKind::Data, capacity: Some(400 << 20), slot: None }));
    // The source reads up to 90 MB/s and can slow down to 30 MB/s; its buffer is small.
    let fast = DriveSpec { native_mbps: Some(90.0), buffer_mib: Some(4), ..DriveSpec::new(Generation::Lto8).serial("D") };
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec { name: "lib".into(), slots: 8, ie_slots: 0, drives: vec![fast, DriveSpec::new(Generation::Lto6).serial("E")], tapes }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let rig = Rig { root: tmp.path().to_path_buf(), _tmp: tmp, world };
    let src = rig.root.join("src");
    for i in 0..4u64 {
        write(&src.join(format!("f{i}.bin")), &noise(i, 12 << 20));
    }
    let (cat, vol) = rig.catalog("cat.db", true);
    let drive = rig.drive();
    let mut o = opts(&src);
    o.ring = ltoarc_engine::ringbuf::Config::new(16 << 20);
    let mut drive = drive;
    let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    let mut dest: Box<dyn TapeDrive> = Box::new(rig.world.drive("E").unwrap());
    let mut source = SimSource { rig: &rig, drive };
    // The destination writes at most 40 MB/s.
    let slow = ltoarc_engine::ringbuf::Pace { floor: 13e6, max: 40e6, gap: None };
    let mut run = |copy: i64, bc: &str, source_pace: Option<ltoarc_engine::ringbuf::Pace>| {
        let mut dset = SimSet { rig: &rig, cat: &cat, rest: [bc].map(String::from).into(), bay: ADDR_DRIVE + 1 };
        let first = backup::NextVolume::next(&mut dset, 0, &mut *dest, &vol).unwrap().volume;
        let copts = backup::copy::CopyOptions {
            ring: ltoarc_engine::ringbuf::Config::new(4 << 20),
            read_buffer: 16 << 20,
            keys: vec![],
            source_pace,
            match_rate: Some(36e6),
        };
        let c = backup::copy::copy_session(&cat, rep.session_id, 1, copy, &mut source, backup::LaneStart { drive: &mut *dest, volume: first, pace: Some(slow) }, &copts, &mut dset, &mut NoProgress).unwrap();
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        c
    };
    let paced = run(2, "BB0001L5", Some(ltoarc_engine::ringbuf::Pace { floor: 30e6, max: 90e6, gap: None }));
    let burst = run(3, "BB0002L5", None);
    let pauses = |c: &backup::copy::CopyReport| c.read.drive.expect("the sim reports motion").pauses;
    let matched = paced.read.matched.average().expect("the source was paced");
    assert!((30e6..=60e6).contains(&matched), "source matched at {:.1} MB/s", matched / 1e6);
    assert!(burst.read.matched.average().is_none());
    assert_eq!(pauses(&paced), 0, "paced: {}", paced.read.describe());
    assert!(pauses(&burst) > 0, "bursts: {}", burst.read.describe());
    eprintln!("paced:  {}\nbursts: {}", paced.read.describe(), burst.read.describe());
}

// ---------------------------------------------------------------------------------------
// Phase 5: incremental sessions, deletions, references, append-only

use ltoarc_engine::incr::{Dedup, Level};

/// One session of job `job` at `level` onto the cartridge in the drive.
fn session(cat: &Catalog, drive: &mut dyn TapeDrive, src: &Path, job: &str, level: Level, tweak: impl FnOnce(&mut Options)) -> backup::Report {
    let mut o = opts(src);
    o.job = Some(job.into());
    o.level = level;
    tweak(&mut o);
    let vol = cat.volume("AA0001L8").unwrap().unwrap();
    let r = backup::backup(cat, drive, &vol, &o, &mut NoProgress).unwrap();
    assert_eq!(r.status, "complete", "{:?}", r.warnings);
    r
}

fn started(cat: &Catalog, r: &backup::Report) -> i64 {
    cat.session(&r.session_uuid.to_string()).unwrap().unwrap().started
}

/// Sessions start on whole seconds; views by time need them apart.
fn next_second() {
    std::thread::sleep(Duration::from_millis(1100));
}

/// A full session, then incremental and differential ones: unchanged files are neither read
/// nor written again, deletions are recorded, and every earlier state restores exactly.
#[test]
fn incremental_chain_restores_any_point() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("keep.txt"), b"unchanged");
    write(&src.join("edit.txt"), b"version 1");
    write(&src.join("gone/a.bin"), &noise(1, 100_000));
    write(&src.join("big.bin"), &noise(2, 2 << 20));
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let full = session(&cat, &mut *drive, &src, "j", Level::Full, |_| {});
    assert_eq!((full.level.as_str(), full.files), ("full", 4));
    let t_full = snapshot(&src);
    next_second();

    write(&src.join("edit.txt"), b"version 2, longer");
    fs::remove_dir_all(src.join("gone")).unwrap();
    write(&src.join("new.txt"), b"added");
    let inc = session(&cat, &mut *drive, &src, "j", Level::Incremental, |_| {});
    assert_eq!(inc.parent, Some(full.session_uuid));
    assert_eq!(inc.files, 2, "edit.txt and new.txt");
    assert_eq!(inc.whiteouts, 2, "gone/ and gone/a.bin");
    assert!(inc.unchanged >= 2, "keep.txt and big.bin are not read: {}", inc.unchanged);
    assert!(inc.stored < 64 << 10, "only the small changes are written: {}", inc.stored);
    let t_inc = snapshot(&src);
    next_second();

    write(&src.join("edit.txt"), b"version 3");
    let diff = session(&cat, &mut *drive, &src, "j", Level::Differential, |_| {});
    assert_eq!(diff.parent, Some(full.session_uuid), "a differential session builds on the full one");
    assert_eq!(diff.files, 2, "everything changed since the full session");
    assert_eq!(diff.whiteouts, 2);
    let t_diff = snapshot(&src);

    let check = |name: &str, view: View, want: &BTreeMap<PathBuf, String>| {
        let dest = rig.root.join(name);
        let mut d = rig.drive();
        let r = restore_all(&cat, &mut *d, &src, &dest, view);
        assert!(r.errors.is_empty(), "{name}: {:?}", r.errors);
        assert_eq!(&snapshot(&dest), want, "{name}");
    };
    check("latest", View::default(), &t_diff);
    check("as-of-inc", View { as_of: Some(started(&cat, &inc)), session: None }, &t_inc);
    check("as-of-full", View { as_of: Some(started(&cat, &full)), session: None }, &t_full);
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
}

/// A directory moved between sessions: the incremental session stores its files by
/// reference to the data already on tape, writes no data, and restores them, also from a
/// catalog rebuilt from the tape alone.
#[test]
fn moved_files_are_stored_by_reference() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("photos/2024/big.raw"), &noise(3, 3 << 20));
    write(&src.join("photos/2024/a.jpg"), &noise(4, 20_000));
    write(&src.join("photos/2024/b.jpg"), &noise(5, 30_000));
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    session(&cat, &mut *drive, &src, "j", Level::Full, |_| {});
    next_second();
    fs::create_dir_all(src.join("archive")).unwrap();
    fs::rename(src.join("photos/2024"), src.join("archive/2024")).unwrap();
    let inc = session(&cat, &mut *drive, &src, "j", Level::Incremental, |_| {});
    assert_eq!(inc.referenced, 3, "all three moved files");
    assert_eq!(inc.referenced_bytes, (3 << 20) + 50_000);
    assert!(inc.stored < 64 << 10, "no data written again: {}", inc.stored);
    assert_eq!(inc.whiteouts, 4, "photos/2024 and its three files");
    let want = snapshot(&src);
    let dest = rig.root.join("restored");
    let r = restore_all(&cat, &mut *drive, &src, &dest, View::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(snapshot(&dest), want);
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
    // From the tape alone: the reference names the earlier session and file.
    let (cat2, vol2) = rig.catalog("rebuilt.db", false);
    let s = scan::scan_volume(&cat2, &mut *drive, &vol2, false, &[]).unwrap();
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    let dest2 = rig.root.join("from-scan");
    let r2 = restore_all(&cat2, &mut *drive, &src, &dest2, View::default());
    assert!(r2.errors.is_empty(), "{:?}", r2.errors);
    assert_eq!(snapshot(&dest2), want);
    // Self-contained sessions write the data instead.
    next_second();
    fs::rename(src.join("archive/2024"), src.join("photos/2024")).unwrap();
    let sc = session(&cat, &mut *drive, &src, "j", Level::Incremental, |o| o.self_contained = true);
    assert_eq!(sc.referenced, 0);
    assert!(sc.stored > 3 << 20);
}

/// Hash digests written back to the sources change their ctime; the next incremental
/// session still sees the files as unchanged.
#[test]
fn write_back_does_not_trigger_a_new_copy() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("a.txt"), b"tagged");
    write(&src.join("b.bin"), &noise(6, 1 << 20));
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    session(&cat, &mut *drive, &src, "j", Level::Full, |o| o.hash.write_back = true);
    next_second();
    let inc = session(&cat, &mut *drive, &src, "j", Level::Incremental, |o| o.hash.write_back = true);
    assert_eq!((inc.files, inc.stored), (0, 0), "nothing changed but the xattrs ltoarc wrote");
    assert_eq!(inc.unchanged, 3);
}

/// Append-only: new files only; a file still being written waits; a changed archived file
/// is reported, not saved; deletions are not recorded.
#[test]
fn append_only_takes_new_settled_files() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    let old = (std::time::SystemTime::now() - Duration::from_secs(7200)).duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
    let aged = |p: &Path, data: &[u8]| {
        write(p, data);
        meta::set_times(p, old, old).unwrap();
    };
    aged(&src.join("cam/0001.mp4"), &noise(7, 300_000));
    aged(&src.join("cam/0002.mp4"), &noise(8, 300_000));
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let first = session(&cat, &mut *drive, &src, "cam", Level::Append, |o| o.settle = Some(Duration::from_secs(600)));
    assert_eq!((first.level.as_str(), first.files), ("append", 2));
    next_second();
    aged(&src.join("cam/0003.mp4"), &noise(9, 300_000));
    write(&src.join("cam/0004.mp4"), &noise(10, 300_000)); // being written right now
    aged(&src.join("cam/0001.mp4"), &noise(11, 300_000)); // changed after archiving
    fs::remove_file(src.join("cam/0002.mp4")).unwrap(); // pruned by the user
    let second = session(&cat, &mut *drive, &src, "cam", Level::Append, |o| o.settle = Some(Duration::from_secs(600)));
    assert_eq!(second.files, 1, "0003 only");
    assert_eq!(second.settling, 1, "0004 waits for the next session");
    assert_eq!(second.whiteouts, 0);
    assert_eq!(second.changed_archived.len(), 1);
    assert!(second.changed_archived[0].ends_with("cam/0001.mp4"));
    // The archive keeps everything ever saved.
    let cam = cat.lookup_path(src.join("cam").as_os_str().as_encoded_bytes()).unwrap().unwrap();
    let names: Vec<String> = cat.children(cam, View::default()).unwrap().into_iter().map(|(_, n, _)| String::from_utf8(n).unwrap()).collect();
    assert_eq!(names, ["0001.mp4", "0002.mp4", "0003.mp4"]);
}

/// With --dedup all, a file whose fresh sha256 xattr matches an archived file is stored by
/// reference without being read: the stored digest is believed (the restore brings back
/// the archived data, not this file's own bytes, which here differ on purpose).
#[test]
fn dedup_by_xattr_digest_without_reading() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    let data = noise(12, 200_000);
    write(&src.join("one/x.bin"), &data);
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let sha = |o: &mut Options| {
        o.hash.profiles = hash::resolve_profiles(&["sha256".into()], &Default::default()).unwrap();
        o.sources = vec![o.sources[0].join("one")];
    };
    session(&cat, &mut *drive, &src, "a", Level::Full, |o| {
        sha(o);
        o.hash.write_back = true;
    });
    let profile = hash::resolve_profiles(&["sha256".into()], &Default::default()).unwrap().remove(0);
    let digest = meta::get_xattr(&src.join("one/x.bin"), profile.xattr.as_ref().unwrap().as_bytes()).unwrap().unwrap();
    // Same size, other bytes, and xattrs saying it is the archived content.
    let copy = src.join("two/x-copy.bin");
    write(&copy, &noise(13, 200_000));
    meta::set_xattr(&copy, profile.xattr.as_ref().unwrap().as_bytes(), &digest).unwrap();
    let md = fs::metadata(&copy).unwrap();
    meta::set_xattr(&copy, profile.meta_xattr.as_ref().unwrap().as_bytes(), hash::meta_value("sha256", meta::mtime_ns(&md), md.len()).as_bytes()).unwrap();
    next_second();
    let r = session(&cat, &mut *drive, &src, "b", Level::Full, |o| {
        sha(o);
        o.sources = vec![src.join("two")];
        o.dedup = Dedup::All;
    });
    assert_eq!(r.referenced, 1);
    let dest = rig.root.join("restored");
    let rr = restore_all(&cat, &mut *drive, &src.join("two"), &dest, View::default());
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(fs::read(dest.join("x-copy.bin")).unwrap(), data);
}

// ---------------------------------------------------------------------------------------
// Phase 6: two copies written at once

/// Tape sets per lane: lane 0 writes copy 1, lane 1 copy 2.
struct PerLane<'a>(Vec<SimSet<'a>>);

impl backup::NextVolume for PerLane<'_> {
    fn available(&mut self, lane: usize) -> bool {
        self.0[lane].available(lane)
    }

    fn next(&mut self, lane: usize, drive: &mut dyn TapeDrive, full: &VolumeRow) -> anyhow::Result<backup::Next> {
        self.0[lane].next(lane, drive, full)
    }
}

/// One read of the source, two copies: copy 1 on LTO-8 cartridges in one drive, copy 2 on
/// smaller LTO-5 cartridges in an LTO-6 drive, each spanning its own tape set. Either copy
/// alone restores everything and verifies.
#[test]
fn tee_writes_two_copies_from_one_read() {
    let _shared = shared();
    let tmp = tempfile::tempdir().unwrap();
    let mut tapes: Vec<TapeSpec> = (1..=2)
        .map(|i| TapeSpec { barcode: Some(format!("AA{i:04}L8")), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(48 << 20), slot: None })
        .collect();
    tapes.extend((1..=3).map(|i| TapeSpec { barcode: Some(format!("BB{i:04}L5")), cartridge: Generation::Lto5, kind: MediaKind::Data, capacity: Some(28 << 20), slot: None }));
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 8,
                ie_slots: 0,
                drives: vec![DriveSpec::new(Generation::Lto8).serial("D"), DriveSpec::new(Generation::Lto6).serial("E")],
                tapes,
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let rig = Rig { root: tmp.path().to_path_buf(), _tmp: tmp, world };
    let src = rig.root.join("src");
    write(&src.join("a.bin"), &noise(1, 22 << 20));
    write(&src.join("b.bin"), &noise(2, 15 << 20));
    for i in 0..60u64 {
        write(&src.join(format!("small/{i}")), &noise(10 + i, 6000));
    }
    let before = snapshot(&src);
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut d1 = rig.drive();
    let mut d2: Box<dyn TapeDrive> = Box::new(rig.world.drive("E").unwrap());
    let mut sets = PerLane(vec![
        SimSet { rig: &rig, cat: &cat, rest: ["AA0002L8"].map(String::from).into(), bay: ADDR_DRIVE },
        SimSet { rig: &rig, cat: &cat, rest: ["BB0002L5", "BB0003L5"].map(String::from).into(), bay: ADDR_DRIVE + 1 },
    ]);
    // Copy 2 starts on BB0001L5 in drive E.
    let mut first2 = SimSet { rig: &rig, cat: &cat, rest: ["BB0001L5"].map(String::from).into(), bay: ADDR_DRIVE + 1 };
    let v2 = backup::NextVolume::next(&mut first2, 0, &mut *d2, &vol).unwrap().volume;
    let mut o = opts(&src);
    o.copies = 2;
    let rep = backup::backup_copies(
        &cat,
        vec![
            vec![backup::LaneStart { drive: &mut *d1, volume: vol.clone(), pace: None }],
            vec![backup::LaneStart { drive: &mut *d2, volume: v2, pace: None }],
        ],
        &o,
        &mut sets,
        &mut NoProgress,
    )
    .unwrap();
    assert_eq!(rep.status, "complete", "{:?}", rep.warnings);
    let by_copy = |c: u8| rep.volumes.iter().filter(|v| v.copy == c).map(|v| v.barcode.clone()).collect::<Vec<_>>();
    assert_eq!(by_copy(1), ["AA0001L8"], "copy 1 fits one LTO-8 cartridge");
    assert!(by_copy(2).len() >= 2, "copy 2 spans the small cartridges: {:?}", by_copy(2));
    let data = (22u64 << 20) + (15 << 20) + 60 * 6000;
    assert!(rep.writers.iter().all(|w| w.bytes > data), "both drives wrote everything");
    // Each copy alone: verify every cartridge, restore everything.
    for v in &rep.volumes {
        let mut d: Box<dyn TapeDrive> = if v.copy == 1 { rig.drive() } else { Box::new(rig.world.drive("E").unwrap()) };
        load_tape_at(&rig, &mut *d, &v.barcode, if v.copy == 1 { ADDR_DRIVE } else { ADDR_DRIVE + 1 });
        let r = verify::verify_volume(&cat, &mut *d, &cat.volume(&v.barcode).unwrap().unwrap(), &[]).unwrap();
        assert!(r.errors.is_empty() && r.missing == 0, "{}: {:?}", v.barcode, r.errors);
    }
    for copy in [1i64, 2] {
        let dest = rig.root.join(format!("restored-{copy}"));
        let ro = restore::Options { dest: dest.clone(), strip_prefix: Some(src.clone()), copy: Some(copy), ..Default::default() };
        let plan = restore::plan(&cat, std::slice::from_ref(&src), &ro).unwrap();
        let barcodes = plan.barcodes();
        assert!(barcodes.iter().all(|b| b.starts_with(if copy == 1 { "AA" } else { "BB" })), "{barcodes:?}");
        let mut r = Restorer::new(plan, ro).unwrap();
        let (mut d, bay): (Box<dyn TapeDrive>, u16) = if copy == 1 { (rig.drive(), ADDR_DRIVE) } else { (Box::new(rig.world.drive("E").unwrap()), ADDR_DRIVE + 1) };
        for b in barcodes {
            load_tape_at(&rig, &mut *d, &b, bay);
            r.volume(&b, &mut *d).unwrap();
        }
        let rr = r.finish().unwrap();
        assert!(rr.errors.is_empty(), "copy {copy}: {:?}", rr.errors);
        assert_eq!(snapshot(&dest), before, "copy {copy}");
    }
}

/// A second copy made later from the source disks: every chunk is checked against the
/// session's BLAKE3 as it is read; a file changed since is left out of the copy (and
/// reported), everything else restores from the new copy alone.
#[test]
fn deferred_copy_from_source() {
    let _shared = shared();
    let tmp = tempfile::tempdir().unwrap();
    let mut tapes = vec![TapeSpec { barcode: Some("AA0001L8".into()), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(64 << 20), slot: None }];
    tapes.extend((1..=2).map(|i| TapeSpec { barcode: Some(format!("BB{i:04}L5")), cartridge: Generation::Lto5, kind: MediaKind::Data, capacity: Some(20 << 20), slot: None }));
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec { name: "lib".into(), slots: 8, ie_slots: 0, drives: vec![DriveSpec::new(Generation::Lto8).serial("D"), DriveSpec::new(Generation::Lto6).serial("E")], tapes }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let rig = Rig { root: tmp.path().to_path_buf(), _tmp: tmp, world };
    let src = rig.root.join("src");
    write(&src.join("keep/big.bin"), &noise(1, 6 << 20));
    write(&src.join("keep/empty"), b"");
    for i in 0..30u64 {
        write(&src.join(format!("keep/small/{i}")), &noise(10 + i, 3000));
    }
    std::os::unix::fs::symlink("big.bin", src.join("keep/link")).unwrap();
    write(&src.join("later.bin"), &noise(2, 3 << 20));
    let before = snapshot(&src.join("keep"));
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let rep = backup::backup(&cat, &mut *drive, &vol, &opts(&src), &mut NoProgress).unwrap();
    assert_eq!(rep.status, "complete");
    // Changed after the session: same size, other bytes (noise seeds 2 and 3 are alike).
    write(&src.join("later.bin"), &noise(4, 3 << 20));

    let mut dest: Box<dyn TapeDrive> = Box::new(rig.world.drive("E").unwrap());
    let mut dset = SimSet { rig: &rig, cat: &cat, rest: ["BB0001L5", "BB0002L5"].map(String::from).into(), bay: ADDR_DRIVE + 1 };
    let first = backup::NextVolume::next(&mut dset, 0, &mut *dest, &vol).unwrap().volume;
    let fo = backup::from_source::FromSourceOptions { ring: ltoarc_engine::ringbuf::Config::new(4 << 20), keys: vec![], zstd_level: None };
    let c = backup::from_source::copy_from_source(&cat, rep.session_id, 2, backup::LaneStart { drive: &mut *dest, volume: first, pace: None }, &fo, &mut dset, &mut NoProgress).unwrap();
    assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
    assert!(c.errors[0].contains("later.bin"), "{:?}", c.errors);
    assert_eq!(c.verified, 24 + 30, "chunks of big.bin and the packed small files");
    for v in &c.volumes {
        load_tape_at(&rig, &mut *dest, &v.barcode, ADDR_DRIVE + 1);
        let r = verify::verify_volume(&cat, &mut *dest, &cat.volume(&v.barcode).unwrap().unwrap(), &[]).unwrap();
        assert!(r.errors.is_empty() && r.missing == 0, "{}: {:?}", v.barcode, r.errors);
    }
    let restore_from = |what: &Path, copy: Option<i64>, dir: &str, d: &mut dyn TapeDrive, bay: u16| -> anyhow::Result<restore::Report> {
        let strip = if what.is_dir() { what } else { what.parent().unwrap() };
        let o = restore::Options { dest: rig.root.join(dir), strip_prefix: Some(strip.to_path_buf()), copy, ..Default::default() };
        let plan = restore::plan(&cat, &[what.to_path_buf()], &o)?;
        let barcodes = plan.barcodes();
        let mut r = Restorer::new(plan, o)?;
        for b in barcodes {
            load_tape_at(&rig, d, &b, bay);
            r.volume(&b, d)?;
        }
        r.finish()
    };
    let rr = restore_from(&src.join("keep"), Some(2), "from-copy2", &mut *dest, ADDR_DRIVE + 1).unwrap();
    assert!(rr.errors.is_empty(), "{:?}", rr.errors);
    assert_eq!(snapshot(&rig.root.join("from-copy2")), before);
    // The changed file has no copy 2; copy 1 still restores it as it was.
    assert!(restore_from(&src.join("later.bin"), Some(2), "later2", &mut *dest, ADDR_DRIVE + 1).is_err());
    let mut d1 = rig.drive();
    restore_from(&src.join("later.bin"), Some(1), "later1", &mut *d1, ADDR_DRIVE).unwrap();
    assert_eq!(fs::read(rig.root.join("later1/later.bin")).unwrap(), noise(2, 3 << 20));
}

// ---------------------------------------------------------------------------------------
// Phase 7: snapshot sources under their live names, stream sources

/// Each run backs up a differently named snapshot directory under the same live name: the
/// archive shows one tree, and an incremental run sees unchanged files as unchanged.
#[test]
fn snapshot_dirs_archived_under_the_live_name() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let snap1 = rig.root.join("snaps/s1");
    write(&snap1.join("keep.bin"), &noise(1, 1 << 20));
    write(&snap1.join("docs/a.txt"), b"one");
    let (cat, _) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let live = PathBuf::from("/data");
    let real1 = fs::canonicalize(&snap1).unwrap();
    session(&cat, &mut *drive, &snap1, "j", Level::Full, |o| o.map = vec![(real1.clone(), live.clone())]);
    next_second();
    // The next snapshot: same files (same inodes, as with ZFS), one changed.
    let snap2 = rig.root.join("snaps/s2");
    fs::rename(&snap1, &snap2).unwrap();
    write(&snap2.join("docs/a.txt"), b"two!");
    let real2 = fs::canonicalize(&snap2).unwrap();
    let inc = session(&cat, &mut *drive, &snap2, "j", Level::Incremental, |o| o.map = vec![(real2.clone(), live.clone())]);
    assert_eq!(inc.files, 1, "only docs/a.txt changed");
    assert!(inc.unchanged >= 2, "keep.bin and the root are unchanged: {}", inc.unchanged);
    assert_eq!(inc.whiteouts, 0);
    // One tree in the archive, under the live name.
    assert!(cat.lookup_path(b"/data/keep.bin").unwrap().is_some());
    assert!(cat.lookup_path(real2.join("keep.bin").as_os_str().as_encoded_bytes()).unwrap().is_none());
    let dest = rig.root.join("restored");
    let r = restore_all(&cat, &mut *drive, &live, &dest, View::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(snapshot(&dest), snapshot(&snap2));
}

/// A command's output archived as a file: restored byte for byte, also from a catalog
/// rebuilt from tape; a failing command leaves the file unreadable and says so.
#[test]
fn stream_source_archives_a_command() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("plain.txt"), b"alongside");
    let dump = rig.root.join("dump.bin");
    fs::write(&dump, noise(5, 3 << 20)).unwrap();
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    let mut o = opts(&src);
    o.streams = vec![
        backup::StreamSource { path: "/.ltoarc/streams/db/dump".into(), command: format!("cat {}", dump.display()) },
        backup::StreamSource { path: "/.ltoarc/streams/db/broken".into(), command: "echo partial; echo oops >&2; exit 3".into() },
    ];
    let rep = backup::backup(&cat, &mut *drive, &vol, &o, &mut NoProgress).unwrap();
    assert!(rep.warnings.iter().any(|w| w.contains("broken") && w.contains("oops")), "{:?}", rep.warnings);
    let id = cat.lookup_path(b"/.ltoarc/streams/db/dump").unwrap().unwrap();
    let obj = cat.latest(id, View::default()).unwrap().unwrap();
    assert_eq!((obj.size, obj.status.as_str()), (3 << 20, "ok"));
    let broken = cat.latest(cat.lookup_path(b"/.ltoarc/streams/db/broken").unwrap().unwrap(), View::default()).unwrap().unwrap();
    assert_eq!(broken.status, "unreadable");
    let dest = rig.root.join("restored");
    let ro = restore::Options { dest: dest.clone(), strip_prefix: Some("/.ltoarc/streams/db".into()), ..Default::default() };
    let plan = restore::plan(&cat, &[PathBuf::from("/.ltoarc/streams/db/dump")], &ro).unwrap();
    let mut rr = Restorer::new(plan, ro).unwrap();
    rr.volume("AA0001L8", &mut *drive).unwrap();
    let r = rr.finish().unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let back = fs::read(dest.join("dump")).unwrap_or_default();
    assert!(back == noise(5, 3 << 20), "restored stream differs ({} bytes)", back.len());
    let v = verify::verify_volume(&cat, &mut *drive, &cat.volume("AA0001L8").unwrap().unwrap(), &[]).unwrap();
    assert!(v.errors.is_empty(), "{:?}", v.errors);
    let (cat2, vol2) = rig.catalog("rebuilt.db", false);
    scan::scan_volume(&cat2, &mut *drive, &vol2, false, &[]).unwrap();
    let o2 = cat2.latest(cat2.lookup_path(b"/.ltoarc/streams/db/dump").unwrap().unwrap(), View::default()).unwrap().unwrap();
    assert_eq!(o2.size, 3 << 20);
}

/// Appending finds which cartridge is loaded, and its block size, from the end of its last
/// volume catalog, so a cartridge left at its end of data is not rewound to the label first.
#[test]
fn append_identifies_the_cartridge_at_its_end() {
    let _shared = shared();
    let rig = Rig::new(256 << 20);
    let src = rig.root.join("src");
    write(&src.join("a.bin"), &noise(7, 300_000));
    let (cat, vol) = rig.catalog("cat.db", true);
    let mut drive = rig.drive();
    // Only a label and its filemark: nothing of ltoarc's at the end yet.
    assert!(ltoarc_engine::tapeio::identify_at_end(&mut *drive).unwrap().is_none());
    session(&cat, &mut *drive, &src, "j", Level::Full, |_| {});
    let (uuid, eod) = ltoarc_engine::tapeio::identify_at_end(&mut *drive).unwrap().expect("a volume catalog ends the cartridge");
    let v = cat.volume("AA0001L8").unwrap().unwrap();
    assert_eq!(Some(uuid.to_string()), v.uuid);
    // Learned from the label on the first append (this catalog did not label it).
    assert_eq!(v.block_size, Some(64 << 10));
    assert_eq!(drive.position().unwrap().pos.object, eod, "left at the end of data");
    // A second session appends from there and both restore.
    write(&src.join("b.bin"), &noise(8, 200_000));
    next_second();
    session(&cat, &mut *drive, &src, "j", Level::Incremental, |_| {});
    let dest = rig.root.join("dest");
    let r = restore_all(&cat, &mut *drive, &src, &dest, View::default());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(snapshot(&src), snapshot(&dest));
    let _ = vol;
}

/// Simulated drive with a given native rate (floor a third of it), drive buffer and capacity.
fn motion_rig(native_mbps: f64, buffer_mib: u64, capacity: u64) -> Rig {
    motion_rig_gap(native_mbps, buffer_mib, capacity, None)
}

/// The same, with a gap in the speeds it can match (MB/s).
fn motion_rig_gap(native_mbps: f64, buffer_mib: u64, capacity: u64, gap: Option<(f64, f64)>) -> Rig {
    use ltoarc_device::sim::{DriveSpec, LibrarySpec, TapeSpec, WorldSpec};
    let tmp = tempfile::tempdir().unwrap();
    let world = SimWorld::create(
        &tmp.path().join("world"),
        &WorldSpec {
            libraries: vec![LibrarySpec {
                name: "lib".into(),
                slots: 2,
                ie_slots: 0,
                drives: vec![DriveSpec {
                    native_mbps: Some(native_mbps),
                    buffer_mib: Some(buffer_mib),
                    speed_gap_mbps: gap,
                    ..DriveSpec::new(Generation::Lto8).serial("D")
                }],
                tapes: vec![TapeSpec { barcode: Some("AA0001L8".into()), cartridge: Generation::Lto8, kind: MediaKind::Data, capacity: Some(capacity), slot: None }],
            }],
            standalone: vec![],
        },
    )
    .unwrap();
    world.changers().unwrap().remove(0).move_medium(ADDR_SLOT, ADDR_DRIVE).unwrap();
    let root = tmp.path().to_path_buf();
    Rig { _tmp: tmp, root, world }
}

/// Write `secs` seconds of a source whose rate (bytes/s) at time t is `rate(t)` through a
/// paced ring buffer to a simulated drive; the drive's own counters for the run:
/// (cycles, pauses, speed changes, writer stats).
fn paced_run(rate: &(dyn Fn(f64) -> f64 + Sync), secs: f64, ring_mib: u64) -> (u64, u64, u64, ltoarc_engine::ringbuf::WriterStats) {
    let pace = ltoarc_engine::ringbuf::Pace { floor: 10e6, max: 30e6, gap: None };
    paced_run_on(motion_rig(30.0, 64, 8 << 30), pace, rate, secs, ring_mib)
}

fn paced_run_on(
    rig: Rig,
    pace: ltoarc_engine::ringbuf::Pace,
    rate: &(dyn Fn(f64) -> f64 + Sync),
    secs: f64,
    ring_mib: u64,
) -> (u64, u64, u64, ltoarc_engine::ringbuf::WriterStats) {
    use ltoarc_engine::ringbuf::{Config, DriveSlot, Ring};
    let mut drive = rig.drive();
    let before = ltoarc_engine::backup::drive_perf(&mut *drive).unwrap().write;
    let ring = Ring::new(Config { capacity: ring_mib << 20, high: ring_mib << 19, pace: Some(pace) });
    let shared = std::sync::Mutex::new(DriveSlot::new(&mut *drive));
    let stats = std::thread::scope(|s| {
        let w = s.spawn(|| ring.write_to(&shared));
        let start = std::time::Instant::now();
        let mut sent = 0f64;
        let mut t = 0f64;
        while t < secs {
            // Keep the cumulative bytes on the scripted curve.
            sent += rate(t) * 0.01;
            while (sent as u64) >= 1 << 20 {
                ring.push(vec![0u8; 1 << 20]).unwrap();
                sent -= (1 << 20) as f64;
            }
            t += 0.01;
            if let Some(d) = Duration::from_secs_f64(t).checked_sub(start.elapsed()) {
                std::thread::sleep(d);
            }
        }
        ring.close();
        w.join().unwrap().unwrap()
    });
    drop(shared);
    let after = ltoarc_engine::backup::drive_perf(&mut *drive).unwrap().write;
    (after.cycles - before.cycles, after.pauses - before.pauses, after.speed_changes - before.speed_changes, stats)
}

/// A rate (bytes/s) by time (s) for the pacing experiments.
type Schedule = Box<dyn Fn(f64) -> f64 + Sync>;

/// Pacing against sources that vary (run with --ignored --nocapture; about 5 minutes).
#[test]
#[ignore]
fn pacing_experiment() {
    let _alone = alone();
    let cases: [(&str, Schedule); 5] = [
        ("alternating 45/18 MB/s every 4 s", Box::new(|t: f64| if (t / 4.0) as u64 % 2 == 0 { 45e6 } else { 18e6 })),
        ("alternating 30/12 MB/s every 4 s", Box::new(|t: f64| if (t / 4.0) as u64 % 2 == 0 { 30e6 } else { 12e6 })),
        ("25 MB/s, then 15 MB/s after 30 s", Box::new(|t: f64| if t < 30.0 { 25e6 } else { 15e6 })),
        ("steady 25 MB/s", Box::new(|_| 25e6)),
        ("8 MB/s, below the 10 MB/s floor", Box::new(|_| 8e6)),
    ];
    for (i, (name, f)) in cases.iter().enumerate() {
        // Below the floor: a smaller buffer and a longer run, so it runs dry now and then.
        let (secs, ring) = if i == 4 { (120.0, 128) } else { (60.0, 512) };
        let (cycles, pauses, changes, w) = paced_run(&**f, secs, ring);
        eprintln!(
            "{name}: drive {cycles} cycle(s), {pauses} pause(s), {changes} speed change(s); buffer ran dry {} time(s); {}",
            w.underruns,
            w.matched.describe().unwrap_or_default()
        );
    }
}

/// Read `secs` seconds of tape for a consumer whose rate (bytes/s) at time t is `rate(t)`
/// through a paced read-ahead buffer: the drive's counters for the reads (stops beyond the
/// one locate, speed changes) and the read stats.
fn paced_read_run(rate: &(dyn Fn(f64) -> f64 + Sync), secs: f64, buffer_mib: u64) -> (u64, u64, ltoarc_engine::readahead::ReadStats) {
    use ltoarc_engine::readahead::{Blocks, Extent, ReadAhead};
    use ltoarc_engine::ringbuf::Pace;
    use ltoarc_format::stream::BlockSource;
    let rig = motion_rig(30.0, 64, 8 << 30);
    let mut drive = rig.drive();
    // Enough data for the run at the consumer's fastest.
    let blocks = (secs * 32e6 / (1 << 20) as f64) as u64;
    for _ in 0..blocks {
        drive.write_block(&vec![7u8; 1 << 20]).unwrap();
    }
    drive.write_filemarks(1).unwrap();
    drive.rewind().unwrap();
    let before = ltoarc_engine::backup::drive_perf(&mut *drive).unwrap().read;
    // Starting at the consumer's first rate (the simulator reads at once at "full speed").
    let ra = ReadAhead::paced(buffer_mib << 20, Some(Pace { floor: 10e6, max: 30e6, gap: None }), Some(rate(0.0)));
    let stats = std::thread::scope(|s| {
        let r = s.spawn(|| ra.read(&mut *drive, 1 << 20, &[Extent { partition: 0, start: 0, end: Some(blocks - 1) }]));
        let mut b = Blocks::new(&ra);
        let start = std::time::Instant::now();
        let mut owed = 0f64;
        let mut t = 0f64;
        while t < secs {
            owed += rate(t) * 0.01;
            while owed >= (1 << 20) as f64 {
                if b.next_block().unwrap().is_none() {
                    break;
                }
                owed -= (1 << 20) as f64;
            }
            t += 0.01;
            if let Some(d) = Duration::from_secs_f64(t).checked_sub(start.elapsed()) {
                std::thread::sleep(d);
            }
        }
        ra.cancel();
        r.join().unwrap()
    });
    let after = ltoarc_engine::backup::drive_perf(&mut *drive).unwrap().read;
    ((after.cycles - before.cycles).saturating_sub(stats.locates), after.speed_changes - before.speed_changes, stats)
}

/// Read pacing against consumers that vary (run with --ignored --nocapture; about 4 minutes).
#[test]
#[ignore]
fn read_pacing_experiment() {
    let _alone = alone();
    let cases: [(&str, Schedule); 4] = [
        ("consumer alternating 30/12 MB/s every 4 s", Box::new(|t: f64| if (t / 4.0) as u64 % 2 == 0 { 30e6 } else { 12e6 })),
        ("consumer 25 MB/s, then 15 MB/s after 30 s", Box::new(|t: f64| if t < 30.0 { 25e6 } else { 15e6 })),
        ("consumer steady 20 MB/s", Box::new(|_| 20e6)),
        ("consumer 8 MB/s, below the 10 MB/s floor", Box::new(|_| 8e6)),
    ];
    for (i, (name, f)) in cases.iter().enumerate() {
        let (secs, buf) = if i == 3 { (90.0, 128) } else { (60.0, 512) };
        let (stops, changes, st) = paced_read_run(&**f, secs, buf);
        eprintln!("{name}: drive {stops} stop(s), {changes} speed change(s); read-ahead full {} time(s); {}", st.pauses, st.matched.describe().unwrap_or_else(|| "full speed".into()));
    }
}

/// A drive that cannot match the speeds in a gap (like the LTO-8 with LTO-7 media between 155
/// and 280 MB/s): fed at a rate in there it stops again and again; with the gap configured,
/// a source in it is written at the gap's lower edge and nothing stops.
#[test]
fn pacing_keeps_out_of_the_drive_gap() {
    let _alone = alone();
    use ltoarc_engine::ringbuf::Pace;
    // The drive cannot match 16.5-28 MB/s; the config gives the last rate measured clean
    // below that (as `calibrate --sweep` does).
    let (drive_gap, configured_gap) = ((16.5, 28.0), (15.5e6, 28e6));
    let source = |_: f64| 20e6;
    let run = |configured: bool| {
        let pace = Pace { floor: 10e6, max: 30e6, gap: configured.then_some(configured_gap) };
        paced_run_on(motion_rig_gap(30.0, 16, 8 << 30, Some(drive_gap)), pace, &source, 20.0, 64)
    };
    let (_, unaware, _, w1) = run(false);
    let (_, aware, _, w2) = run(true);
    eprintln!(
        "source at 20 MB/s, drive gap 16.5-28: {unaware} pause(s) paced at {}; {aware} pause(s) paced at {}",
        w1.matched.describe().unwrap_or_default(),
        w2.matched.describe().unwrap_or_default()
    );
    assert!(aware <= 1, "paced around the gap, the drive paused {aware} times");
    assert!(unaware >= aware + 3, "paced in the gap, the drive paused only {unaware} times");
    assert!((w2.matched.average().unwrap() - 15.5e6).abs() < 0.5e6);
}
