//! Shared command context: catalog, configuration, and opening devices recorded in it.

use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use ltoarc_catalog::{Catalog, DriveRow, LibraryRow, VolumeRow};
use ltoarc_device::real::{RealChanger, RealDrive};
use ltoarc_device::sim::SimWorld;
use ltoarc_device::stats;
use ltoarc_device::{drive_access, Access, Changer, Element, ElementType, Generation, Inventory, TapeDrive};
use ltoarc_engine::hash::Profile;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Config {
    catalog: Option<PathBuf>,
    #[serde(default)]
    sim: Vec<PathBuf>,
    /// Hash profiles: [hash.<name>] tables (see ltoarc_engine::hash::Profile).
    #[serde(default)]
    hash: BTreeMap<String, Profile>,
    /// Data rates from drive spec sheets: [rates."<drive name, serial or product id>"] with
    /// one "SLOWEST-RATED" MB/s range per media generation, e.g. LTO-8 = "112-300", or two
    /// with the gap the drive cannot match between them, e.g. LTO-7 = "66-155, 280-300".
    #[serde(default)]
    rates: BTreeMap<String, BTreeMap<String, String>>,
    /// Key files for encrypted sessions.
    #[serde(default)]
    keys: Vec<PathBuf>,
    /// Lock files shared by every ltoarc process using the host's drives (default
    /// /run/lock/ltoarc).
    lock_dir: Option<PathBuf>,
    #[serde(default)]
    wear: Wear,
}

/// Wear thresholds (`[wear]` in the config), checked by `ltoarc stats check`.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Wear {
    /// Retire a cartridge when its drive reports this little of its life left (%; the drive
    /// works it out from passes and loads; default 10).
    pub tape_min_life_percent: Option<u64>,
    /// ... or after this many loads (the media maker's rating; not set by default).
    pub tape_max_loads: Option<u64>,
    /// ... or with more unrecovered read and write errors than this (not set by default).
    pub tape_max_unrecovered_errors: Option<u64>,
    /// Clean a drive after this many head hours since its last cleaning (default: only when
    /// the drive asks for it, TapeAlert 20 or 21).
    pub drive_clean_after_head_hours: Option<u64>,
}

pub struct Ctx {
    pub cat: Catalog,
    pub catalog_path: PathBuf,
    pub sims: Vec<PathBuf>,
    /// Skip discovery of real SCSI devices.
    pub sim_only: bool,
    /// Hash profiles from the config file.
    pub profiles: BTreeMap<String, Profile>,
    rates: BTreeMap<String, BTreeMap<String, String>>,
    /// Keys for encrypted sessions (--key and the config file).
    pub keys: Vec<ltoarc_engine::crypt::Key>,
    /// Drive leases this process holds, shared with other ltoarc processes.
    pub leases: crate::lease::Leases,
    /// Wait this long for a cartridge that is not in any library to be inserted.
    pub wait_tapes: Option<Duration>,
    pub wear: Wear,
    /// Global options to pass on to child ltoarc processes (jobs run their steps that way).
    pub child_args: Vec<std::ffi::OsString>,
}

/// The home directory: $HOME, else the user's passwd entry (systemd sets no $HOME for a system
/// service without `User=`), else "/".
pub(crate) fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    let mut buf = vec![0u8; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pw` and `buf`, which outlive the call.
    let rc = unsafe { libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result) };
    if rc != 0 || result.is_null() || pw.pw_dir.is_null() {
        return PathBuf::from("/");
    }
    // SAFETY: pw_dir points into `buf` and is NUL-terminated.
    let dir = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) };
    PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()))
}

impl Ctx {
    pub fn open(catalog: Option<PathBuf>, config: Option<PathBuf>, mut sims: Vec<PathBuf>, sim_only: bool, keys: &[PathBuf]) -> Result<Ctx> {
        let cfg_path = config.clone().unwrap_or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("ltoarc/config.toml")
        });
        let cfg: Config = match std::fs::read_to_string(&cfg_path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", cfg_path.display()))?,
            Err(_) if config.is_none() => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", cfg_path.display())),
        };
        let catalog = catalog
            .or_else(|| std::env::var_os("LTOARC_CATALOG").map(PathBuf::from))
            .or(cfg.catalog)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home().join(".local/share"))
                    .join("ltoarc/catalog.db")
            });
        sims.extend(cfg.sim);
        let cat = Catalog::open(&catalog).with_context(|| format!("opening catalog {}", catalog.display()))?;
        let keys = keys.iter().chain(&cfg.keys).map(|p| ltoarc_engine::crypt::Key::load(p)).collect::<Result<_>>()?;
        let lock_dir = cfg.lock_dir.unwrap_or_else(|| PathBuf::from("/run/lock/ltoarc"));
        let leases = crate::lease::Leases::new(lock_dir, crate::lease::Priority::Interactive, "ltoarc");
        Ok(Ctx { cat, catalog_path: catalog, sims, sim_only, profiles: cfg.hash, rates: cfg.rates, keys, leases, wait_tapes: None, wear: cfg.wear, child_args: Vec::new() })
    }

    /// The configured spec-sheet rates for a drive writing media of `generation`: one range
    /// "SLOWEST-RATED" in MB/s, or two with a gap between that the drive cannot match.
    pub fn drive_rates(&self, d: &DriveRow, generation: &str) -> Result<Option<ltoarc_engine::plan::DriveRates>> {
        let keys = [d.name.as_str(), d.serial.as_str(), d.product.trim()];
        let Some((key, table)) = keys.iter().find_map(|k| self.rates.get_key_value(*k)) else { return Ok(None) };
        let Some(range) = table.iter().find(|(g, _)| g.eq_ignore_ascii_case(generation)).map(|(_, r)| r) else {
            return Ok(None);
        };
        let r = ltoarc_engine::plan::parse_ranges(range).ok_or_else(|| {
            anyhow!("config [rates.\"{key}\"] {generation} = \"{range}\": expected SLOWEST-RATED in MB/s, e.g. \"112-300\", or two ranges with the gap the drive cannot match, e.g. \"66-155, 280-300\"")
        })?;
        Ok(Some(ltoarc_engine::plan::DriveRates::from_ranges(&r)))
    }

    /// Open a drive, holding its lease (waiting while another ltoarc process uses it).
    pub fn open_drive(&self, row: &DriveRow) -> Result<Box<dyn TapeDrive>> {
        self.lease(row)?;
        self.open_drive_shared(row)
    }

    /// Hold drive `row`'s lease, waiting for it if another process has it.
    pub fn lease(&self, row: &DriveRow) -> Result<()> {
        if !self.leases.try_take(row)? {
            self.leases.take_any(std::slice::from_ref(row), &mut |who| eprintln!("waiting for {who}"))?;
        }
        Ok(())
    }

    /// Open a drive without its lease, for reading statistics only.
    pub fn open_drive_shared(&self, row: &DriveRow) -> Result<Box<dyn TapeDrive>> {
        let loc = row.location.as_deref().ok_or_else(|| anyhow!("drive {} is not attached to this host", row.name))?;
        Ok(match row.backend.as_str() {
            "real" => Box::new(RealDrive::open(loc, row.st_path.clone()).with_context(|| format!("opening {loc}"))?),
            "sim" => Box::new(SimWorld::open(Path::new(loc))?.drive(&row.serial)?),
            other => bail!("unknown backend {other}"),
        })
    }

    pub fn open_changer(&self, row: &LibraryRow) -> Result<Box<dyn Changer>> {
        Ok(match row.backend.as_str() {
            "real" => Box::new(RealChanger::open(&row.location).with_context(|| format!("opening {}", row.location))?),
            "sim" => Box::new(SimWorld::open(Path::new(&row.location))?.changer(&row.serial)?),
            other => bail!("unknown backend {other}"),
        })
    }

    pub fn library(&self, key: &str) -> Result<LibraryRow> {
        self.cat.library(key)?.ok_or_else(|| anyhow!("no library '{key}' (run `ltoarc hw scan`)"))
    }

    pub fn drive(&self, key: &str) -> Result<DriveRow> {
        self.cat.drive(key)?.ok_or_else(|| anyhow!("no drive '{key}' (run `ltoarc hw scan`)"))
    }

    pub fn volume(&self, barcode: &str) -> Result<VolumeRow> {
        self.cat.volume(barcode)?.ok_or_else(|| anyhow!("unknown cartridge {barcode} (run `ltoarc tape register`)"))
    }

    pub fn attached_drives(&self) -> Result<Vec<DriveRow>> {
        Ok(self.cat.drives()?.into_iter().filter(|d| d.state == "attached").collect())
    }

    /// Human-readable location of a volume.
    pub fn location(&self, v: &VolumeRow) -> Result<String> {
        let (Some(lib_id), Some(el)) = (v.library_id, v.element) else { return Ok("not in a library".into()) };
        let Some(lib) = self.cat.library_by_id(lib_id)? else { return Ok("unknown".into()) };
        let mut name = format!("{} {}", lib.name, lib.element_name(el));
        if let Some(d) = self.cat.drives()?.into_iter().find(|d| d.library_id == Some(lib_id) && d.element == Some(el)) {
            name = format!("{} (drive {})", lib.name, d.name);
        }
        Ok(name)
    }

    /// Where a cartridge is in an inventory: by barcode, or for cartridges without a readable
    /// label, at the element recorded in the catalog (which must hold an unlabelled cartridge).
    pub fn find_element(&self, v: &VolumeRow, inv: &Inventory) -> Option<Element> {
        if v.labelled {
            return inv.find_barcode(&v.barcode).cloned();
        }
        let el = inv.element(v.element? as u16)?;
        (el.full && el.barcode.is_none()).then(|| el.clone())
    }

    /// Find which drive (if any) holds a cartridge, according to the library inventory.
    pub fn drive_holding(&self, barcode: &str) -> Result<Option<(DriveRow, LibraryRow)>> {
        let v = self.volume(barcode)?;
        let Some(lib) = v.library_id.and_then(|id| self.cat.library_by_id(id).ok().flatten()) else { return Ok(None) };
        let inv = self.open_changer(&lib)?.inventory()?;
        let Some(el) = self.find_element(&v, &inv) else { return Ok(None) };
        if el.kind != ElementType::DataTransfer {
            return Ok(None);
        }
        let drive = self.cat.drives()?.into_iter().find(|d| d.library_id == Some(lib.id) && d.element == Some(el.address as i64));
        Ok(drive.map(|d| (d, lib)))
    }

    /// Attached drives in library `lib_id` that can write `v`.
    pub fn writers(&self, lib_id: i64, v: &VolumeRow) -> Result<Vec<DriveRow>> {
        Ok(self
            .attached_drives()?
            .into_iter()
            .filter(|d| d.library_id == Some(lib_id) && access(d, v) == Access::ReadWrite)
            .collect())
    }

    /// Pick a drive for a cartridge: the named one, or an attached drive in the same library
    /// that can write (else, unless `write`, read) its generation, preferring a free one.
    pub fn choose_drive(&self, lib: &LibraryRow, inv: &Inventory, v: &VolumeRow, named: Option<&str>, write: bool) -> Result<DriveRow> {
        let access = |d: &DriveRow| -> Access {
            match access(d, v) {
                Access::ReadOnly if write => Access::None,
                a => a,
            }
        };
        if let Some(name) = named {
            let d = self.drive(name)?;
            if d.library_id != Some(lib.id) {
                bail!("drive {} is not in library {}", d.name, lib.name);
            }
            if d.state != "attached" {
                bail!("drive {} is {}", d.name, d.state);
            }
            if access(&d) == Access::None {
                bail!("drive {} ({}) cannot {} {} media", d.name, opt(&d.generation), if write { "write" } else { "use" }, opt(&v.generation));
            }
            return Ok(d);
        }
        let free = |d: &DriveRow| d.element.and_then(|e| inv.element(e as u16)).is_some_and(|e| !e.full);
        let mut cands: Vec<DriveRow> = self
            .attached_drives()?
            .into_iter()
            .filter(|d| d.library_id == Some(lib.id) && access(d) != Access::None)
            .collect();
        if cands.is_empty() {
            let elsewhere: Vec<String> = self
                .cat
                .drives()?
                .into_iter()
                .filter(|d| d.library_id == Some(lib.id) && d.state != "attached" && access(d) != Access::None)
                .map(|d| format!("{} ({})", d.name, d.state))
                .collect();
            let hint = if elsewhere.is_empty() { String::new() } else { format!("; {} could, but", elsewhere.join(", ")) };
            let tail = if elsewhere.is_empty() { "" } else { " is not reachable from this host" };
            bail!("no attached drive in {} can use {} media{hint}{tail}", lib.name, opt(&v.generation));
        }
        cands.sort_by_key(|d| (access(d) != Access::ReadWrite, d.name.clone()));
        // An empty drive no other process uses, else one holding a cartridge that goes back to
        // a slot, else wait for one to come free.
        for d in cands.iter().filter(|d| free(d)).chain(cands.iter().filter(|d| !free(d))) {
            if self.leases.try_take(d)? {
                return Ok(d.clone());
            }
        }
        self.leases.take_any(&cands, &mut |who| eprintln!("all drives that can take {} are in use ({who}); waiting", v.barcode))
    }

    /// Record drive and (when a cartridge is loaded) volume statistics.
    pub fn snapshot(&self, drive_row: &DriveRow, drive: &mut dyn TapeDrive, barcode: Option<&str>, event: &str) {
        match stats::drive_stats(drive) {
            Ok(s) => {
                let _ = self.cat.add_drive_stats(drive_row.id, event, &s);
            }
            Err(e) => eprintln!("warning: drive statistics of {}: {e}", drive_row.name),
        }
        if let Some(bc) = barcode {
            if let Ok(Some(v)) = self.cat.volume(bc) {
                match stats::volume_stats(drive) {
                    Ok(s) => {
                        let _ = self.cat.add_volume_stats(v.id, Some(drive_row.id), event, &s);
                    }
                    Err(e) => eprintln!("warning: cartridge statistics of {bc}: {e}"),
                }
            }
        }
    }
}

/// Whether new sessions may be written to a cartridge (not full, not retiring, not a
/// cleaning cartridge).
pub fn writable(v: &VolumeRow) -> bool {
    v.state != "full" && v.state != "retiring" && v.kind != "cleaning"
}

/// Whether a session may write to a cartridge, appending or (`overwrite`) starting it
/// afresh: then a full one will do too.
pub fn usable(v: &VolumeRow, overwrite: bool) -> bool {
    if overwrite {
        v.state != "retiring" && v.kind != "cleaning"
    } else {
        writable(v)
    }
}

/// Media generation of a cartridge: as recorded, else from its barcode's media identifier.
pub fn media_generation(v: &VolumeRow) -> Option<Generation> {
    v.generation.as_deref().and_then(|g| g.parse().ok()).or_else(|| ltoarc_device::parse_barcode(&v.barcode).generation)
}

/// What drive `d` can do with cartridge `v` (unknown generations are assumed to work).
pub fn access(d: &DriveRow, v: &VolumeRow) -> Access {
    match (d.generation.as_deref().and_then(|g| g.parse::<Generation>().ok()), media_generation(v)) {
        (Some(dg), Some(mg)) => drive_access(dg, mg),
        _ => Access::ReadWrite,
    }
}

pub fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "?".into())
}

pub const READY_TIMEOUT: Duration = Duration::from_secs(15 * 60);
