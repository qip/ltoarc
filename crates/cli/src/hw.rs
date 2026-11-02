//! `ltoarc hw`: discover libraries and drives, record them in the catalog.

use std::path::Path;

use anyhow::Result;
use clap::Subcommand;
use ltoarc_catalog::{now, DriveRow, LibraryRow};
use ltoarc_device::real::{self, RealChanger, RealDrive};
use ltoarc_device::sim::SimWorld;
use ltoarc_device::{serial_matches, Changer, ChangerInfo, DriveInfo, ElementType, Generation, Inventory, TapeDrive};
use ltoarc_scsi::inquiry::{PDT_CHANGER, PDT_SEQUENTIAL};

use crate::ctx::Ctx;
use crate::table::{opt, when, Table};

#[derive(Subcommand)]
pub enum HwCmd {
    /// Discover drives and libraries (SCSI generic devices and --sim worlds).
    Scan,
    /// Show recorded libraries and drives.
    List,
    /// Rename a drive or library.
    Rename {
        /// "drive" or "library".
        kind: String,
        old: String,
        new: String,
    },
    /// Clean a drive with a cleaning cartridge from its library.
    Clean {
        drive: String,
        /// The cleaning cartridge to use (default: the first one in a slot).
        #[arg(long)]
        with: Option<String>,
    },
}

pub fn run(ctx: &Ctx, cmd: HwCmd) -> Result<()> {
    match cmd {
        HwCmd::Scan => scan(ctx),
        HwCmd::List => list(ctx),
        HwCmd::Rename { kind, old, new } => {
            ctx.cat.rename(&kind, &old, &new)?;
            println!("renamed {kind} {old} -> {new}");
            Ok(())
        }
        HwCmd::Clean { drive, with } => crate::tape::clean_drive(ctx, &ctx.drive(&drive)?, with.as_deref()),
    }
}

struct Found {
    libraries: Vec<(ChangerInfo, Inventory)>,
    drives: Vec<DriveInfo>,
}

fn discover(ctx: &Ctx) -> Found {
    let mut f = Found { libraries: Vec::new(), drives: Vec::new() };
    for d in if ctx.sim_only { Vec::new() } else { real::discover() } {
        match d.peripheral_type {
            PDT_CHANGER => match RealChanger::open(&d.sg_path).and_then(|mut c| Ok((c.info().clone(), c.inventory()?))) {
                Ok(x) => f.libraries.push(x),
                Err(e) => eprintln!("warning: library at {}: {e}", d.sg_path),
            },
            PDT_SEQUENTIAL => match RealDrive::open(&d.sg_path, d.st_path.clone()) {
                Ok(drive) => f.drives.push(drive.info().clone()),
                Err(e) => eprintln!("warning: drive at {}: {e}", d.sg_path),
            },
            _ => {}
        }
    }
    for dir in &ctx.sims {
        let world = match SimWorld::open(dir) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("warning: {e}");
                continue;
            }
        };
        for mut c in world.changers().unwrap_or_default() {
            match c.inventory() {
                Ok(inv) => f.libraries.push((c.info().clone(), inv)),
                Err(e) => eprintln!("warning: simulated library {}: {e}", c.info().serial),
            }
        }
        for d in world.drives().unwrap_or_default() {
            f.drives.push(d.info().clone());
        }
    }
    f
}

fn drive_name(ctx: &Ctx, generation: Option<Generation>) -> Result<String> {
    let prefix = generation.map_or("drive".to_string(), |g| match g {
        Generation::M8 => "lto8".into(),
        g => format!("lto{}", g.suffix().trim_start_matches('L')),
    });
    let existing = ctx.cat.drives()?;
    let n = (1..).find(|n| !existing.iter().any(|d| d.name == format!("{prefix}-{n}"))).unwrap_or(1);
    Ok(format!("{prefix}-{n}"))
}

fn scan(ctx: &Ctx) -> Result<()> {
    let started = now();
    let found = discover(ctx);
    // Drive bay -> (library id, element address), keyed by drive serial.
    let mut bays: Vec<(String, Option<String>, i64, i64)> = Vec::new();
    for (info, inv) in &found.libraries {
        let base = |k: ElementType| inv.of(k).map(|e| e.address as i64).min();
        let row = LibraryRow {
            id: 0,
            name: String::new(),
            serial: info.serial.clone(),
            backend: format!("{:?}", info.backend).to_lowercase(),
            location: info.location.clone(),
            vendor: info.vendor.clone(),
            product: info.product.clone(),
            revision: info.revision.clone(),
            slots: inv.of(ElementType::Storage).count() as i64,
            ie_slots: inv.of(ElementType::ImportExport).count() as i64,
            bays: inv.of(ElementType::DataTransfer).count() as i64,
            slot_base: base(ElementType::Storage),
            ie_base: base(ElementType::ImportExport),
            bay_base: base(ElementType::DataTransfer),
            last_seen: now(),
        };
        let existing_libs = ctx.cat.libraries()?;
        let n = (1..).find(|n| !existing_libs.iter().any(|l| l.name == format!("lib{n}") && l.serial != info.serial));
        let lib = ctx.cat.upsert_library(&row, &format!("lib{}", n.unwrap_or(1)))?;
        for bay in inv.of(ElementType::DataTransfer) {
            if let Some(serial) = &bay.drive_serial {
                bays.push((serial.clone(), bay.drive_product.clone(), lib.id, bay.address as i64));
            }
        }
    }
    let mut seen = Vec::new();
    for d in &found.drives {
        let bay = bays.iter().find(|(s, _, _, _)| serial_matches(s, &d.serial));
        let row = DriveRow {
            id: 0,
            name: String::new(),
            serial: d.serial.clone(),
            backend: format!("{:?}", d.backend).to_lowercase(),
            location: Some(d.location.clone()),
            st_path: d.st_path.clone(),
            vendor: d.vendor.clone(),
            product: d.product.clone(),
            revision: d.revision.clone(),
            generation: d.generation.map(|g| g.name().to_string()),
            library_id: bay.map(|b| b.2),
            element: bay.map(|b| b.3),
            state: "attached".into(),
            last_seen: now(),
        };
        let name = drive_name(ctx, d.generation)?;
        ctx.cat.upsert_drive(&row, &name)?;
        seen.push(d.serial.clone());
    }
    // Drives the libraries report in their bays but this host cannot reach.
    for (serial, product, lib_id, element) in &bays {
        if seen.iter().any(|s| serial_matches(s, serial)) {
            continue;
        }
        let generation = product.as_deref().and_then(Generation::from_drive_product);
        let row = DriveRow {
            id: 0,
            name: String::new(),
            serial: serial.clone(),
            backend: ctx.cat.library_by_id(*lib_id)?.map(|l| l.backend).unwrap_or_else(|| "real".into()),
            location: None,
            st_path: None,
            vendor: "IBM".into(),
            product: product.clone().unwrap_or_default(),
            revision: String::new(),
            generation: generation.map(|g| g.name().to_string()),
            library_id: Some(*lib_id),
            element: Some(*element),
            state: "unattached".into(),
            last_seen: now(),
        };
        let name = drive_name(ctx, generation)?;
        ctx.cat.upsert_drive(&row, &name)?;
    }
    for name in ctx.cat.mark_unseen_drives_offline(started)? {
        eprintln!("note: drive {name} was not found; marked offline");
    }
    ctx.cat.log_event("hw", "scan", &format!("{} libraries, {} drives", found.libraries.len(), found.drives.len()))?;
    list(ctx)
}

fn list(ctx: &Ctx) -> Result<()> {
    let libs = ctx.cat.libraries()?;
    let mut t = Table::new(&["LIBRARY", "PRODUCT", "SERIAL", "SLOTS", "I/E", "BAYS", "BACKEND", "DEVICE", "SEEN"]);
    for l in &libs {
        t.row(vec![
            l.name.clone(),
            format!("{} {}", l.vendor, l.product),
            l.serial.clone(),
            l.slots.to_string(),
            l.ie_slots.to_string(),
            l.bays.to_string(),
            l.backend.clone(),
            short(&l.location),
            when(l.last_seen),
        ]);
    }
    if !t.is_empty() {
        t.print();
        println!();
    }
    let mut t = Table::new(&["DRIVE", "GEN", "PRODUCT", "SERIAL", "LIBRARY", "BAY", "STATE", "DEVICE"]);
    for d in ctx.cat.drives()? {
        let lib = d.library_id.and_then(|id| libs.iter().find(|l| l.id == id));
        t.row(vec![
            d.name.clone(),
            opt(&d.generation),
            format!("{} {}", d.vendor, d.product),
            d.serial.clone(),
            lib.map_or("-".into(), |l| l.name.clone()),
            match (lib, d.element) {
                (Some(l), Some(e)) => l.element_name(e).trim_start_matches("drive bay ").to_string(),
                _ => "-".into(),
            },
            d.state.clone(),
            match (&d.location, &d.st_path) {
                (Some(sg), Some(st)) => format!("{sg} {st}"),
                (Some(loc), None) => short(loc),
                _ => "-".into(),
            },
        ]);
    }
    if t.is_empty() {
        println!("no drives recorded; run `ltoarc hw scan`");
    } else {
        t.print();
    }
    Ok(())
}

fn short(loc: &str) -> String {
    if loc.starts_with("/dev/") {
        return loc.to_string();
    }
    let p = Path::new(loc);
    format!("sim:{}", p.file_name().map_or(loc.into(), |f| f.to_string_lossy().to_string()))
}
