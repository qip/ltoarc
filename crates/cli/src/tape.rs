//! `ltoarc tape`: register cartridges, move them, inspect and initialize them.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Subcommand;
use ltoarc_catalog::{DriveRow, LibraryRow, VolumeRow};
use ltoarc_device::m8::{self, M8Status};
use ltoarc_engine::tapeio::{inspect, write_label, Content};
use ltoarc_device::{parse_barcode, ElementType, Error as DevError, Generation, Readiness, TapeDrive};
use ltoarc_format::VolumeLabel;
use ltoarc_scsi::density::LTO7;
use ltoarc_scsi::mam::{self, id, Attribute};

use crate::ctx::{opt, Ctx, READY_TIMEOUT};
use crate::table::{when, Table};

#[derive(Subcommand)]
pub enum TapeCmd {
    /// Read library inventories and record every barcoded cartridge. With --slot and --name,
    /// record a cartridge whose barcode the library cannot read under a name of your choice.
    Register {
        /// Only this library (name or serial).
        #[arg(long)]
        library: Option<String>,
        /// Storage slot (1-based) of a cartridge without a readable barcode.
        #[arg(long, requires = "name")]
        slot: Option<i64>,
        /// Name for that cartridge (used wherever a barcode would be; written to its cartridge memory).
        #[arg(long, requires = "slot")]
        name: Option<String>,
        /// Generation of that cartridge, if known (e.g. LTO-7); otherwise learned on load.
        #[arg(long)]
        generation: Option<String>,
    },
    /// Remove a cartridge from the catalog (registered by mistake, gone for good). One that
    /// holds sessions needs --force: the sessions held only on it are forgotten with it (asks
    /// first unless --yes). A barcoded cartridge still in a library is recorded again by the
    /// next `tape register`.
    Unregister {
        barcode: String,
        /// Also when it holds sessions: those held only on it are forgotten with it.
        #[arg(long)]
        force: bool,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
    /// List recorded cartridges.
    List,
    /// Move a cartridge into a drive.
    Load {
        barcode: String,
        /// Drive to use (default: a free one that can read its generation).
        #[arg(long)]
        drive: Option<String>,
    },
    /// Unload a drive and put its cartridge back in a slot.
    Unload {
        /// Drive name; may be omitted when only one drive holds a cartridge.
        #[arg(long)]
        drive: Option<String>,
        /// Destination slot number (default: where it came from, else the first free slot).
        #[arg(long)]
        slot: Option<i64>,
    },
    /// Move cartridges to other slots of their library, or to an I/E slot to take them out:
    /// `tape move DC8001L8 5`, or several at once, `tape move DC8001L8=5 DC8002L8=ie`. A place
    /// is a slot number, `ie` (a free I/E slot) or `ieN`. Cartridges that change places go
    /// through a free slot.
    Move {
        /// BARCODE PLACE, or BARCODE=PLACE ...
        #[arg(required = true, value_name = "MOVES")]
        moves: Vec<String>,
        /// Exchange with the cartridge in a place that is taken: it goes where the other came from.
        #[arg(long)]
        swap: bool,
        /// Only show the moves.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what is known about a cartridge; reads cartridge memory and label if it is loaded.
    Info {
        /// The cartridge (default: the one in --drive).
        barcode: Option<String>,
        /// Inspect whatever is in this drive.
        #[arg(long)]
        drive: Option<String>,
    },
    /// Write the ltoarc label and cartridge memory attributes (dry run without --yes).
    Init {
        barcode: String,
        /// Pool to record on the cartridge (a name for grouping cartridges, e.g. offsite).
        #[arg(long)]
        pool: Option<String>,
        /// Format a new LTO-7 cartridge as Type M (9 TB) first. Irreversible; LTO-8 drives only.
        #[arg(long)]
        m8: bool,
        /// Overwrite a cartridge that already holds data (an ltoarc label or anything else).
        #[arg(long)]
        force: bool,
        /// Drive to use (default: a free one that can write its generation).
        #[arg(long)]
        drive: Option<String>,
        /// Actually do it.
        #[arg(long)]
        yes: bool,
    },
    /// Rebuild catalog entries by reading the cartridge (no catalog needed).
    Scan {
        barcode: String,
        /// Re-read sessions already in the catalog and replace them.
        #[arg(long)]
        replace: bool,
        /// Drive to use (default: a free one that can read its generation).
        #[arg(long)]
        drive: Option<String>,
    },
    /// Close a session left unfinished on a cartridge (crash, power cut): catalog what reached
    /// the tape, append a fresh volume catalog, and mark the cartridge clean.
    Recover {
        barcode: String,
        /// Drive to use (default: a free one that can write its generation).
        #[arg(long)]
        drive: Option<String>,
    },
    /// Make the library re-read all barcodes (moves the robot).
    Rescan {
        /// Only this library (name or serial; default: all).
        #[arg(long)]
        library: Option<String>,
        /// Actually do it (otherwise only says what it would do).
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: TapeCmd) -> Result<()> {
    match cmd {
        TapeCmd::Register { library, slot: Some(slot), name: Some(name), generation } => {
            register_unlabelled(ctx, library.as_deref(), slot, &name, generation.as_deref())
        }
        TapeCmd::Register { library, .. } => register(ctx, library.as_deref()),
        TapeCmd::Unregister { barcode, force, yes } => unregister(ctx, &barcode, force, yes),
        TapeCmd::List => list(ctx),
        TapeCmd::Load { barcode, drive } => load(ctx, &barcode, drive.as_deref()).map(|_| ()),
        TapeCmd::Unload { drive, slot } => unload(ctx, drive.as_deref(), slot),
        TapeCmd::Move { moves, swap, dry_run } => crate::arrange::run(ctx, &moves, swap, dry_run),
        TapeCmd::Info { barcode, drive } => info(ctx, barcode.as_deref(), drive.as_deref()),
        TapeCmd::Init { barcode, pool, m8, force, drive, yes } => {
            init(ctx, &barcode, pool, m8, force, drive.as_deref(), yes)
        }
        TapeCmd::Rescan { library, yes } => rescan(ctx, library.as_deref(), yes),
        TapeCmd::Scan { barcode, replace, drive } => crate::archive::scan(ctx, &barcode, replace, drive.as_deref()),
        TapeCmd::Recover { barcode, drive } => recover(ctx, &barcode, drive.as_deref()),
    }
}

fn register(ctx: &Ctx, only: Option<&str>) -> Result<()> {
    let libs = match only {
        Some(l) => vec![ctx.library(l)?],
        None => ctx.cat.libraries()?,
    };
    if libs.is_empty() {
        bail!("no libraries recorded; run `ltoarc hw scan` first");
    }
    let mut t = Table::new(&["BARCODE", "GEN", "KIND", "STATE", "LOCATION", ""]);
    for lib in &libs {
        let inv = ctx.open_changer(lib)?.inventory()?;
        let mut present = Vec::new();
        let mut unlabelled = Vec::new();
        for e in inv.elements.iter().filter(|e| e.full) {
            let place = lib.element_name(e.address as i64);
            let Some(bc) = &e.barcode else {
                unlabelled.push(e.address as i64);
                if let Some(v) = ctx.cat.unlabelled_at(lib.id, e.address as i64)? {
                    t.row(vec![v.barcode.clone(), opt(&v.generation), v.kind.clone(), v.state.clone(), ctx.location(&v)?, "no label".into()]);
                    continue;
                }
                let why = e.exception.map_or(String::new(), |(a, q)| format!(" (exception {a:02X}/{q:02X})"));
                let hint = match lib.element_name(e.address as i64).strip_prefix("slot ") {
                    Some(n) => format!("; name it with `ltoarc tape register --slot {n} --name NAME`"),
                    None => String::new(),
                };
                crate::note!("warning: {} {place}: cartridge without a readable barcode{why}{hint}", lib.name);
                continue;
            };
            let b = parse_barcode(bc);
            let (v, new) =
                ctx.cat.volume_seen(bc, b.generation.map(|g| g.name()), b.kind.as_str(), lib.id, e.address as i64)?;
            if new {
                ctx.cat.log_event("tape", bc, &format!("registered in {} {place}", lib.name))?;
            }
            present.push(bc.clone());
            t.row(vec![
                bc.clone(),
                opt(&v.generation),
                v.kind.clone(),
                v.state.clone(),
                ctx.location(&v)?,
                if new { "new".into() } else { String::new() },
            ]);
            if b.generation.is_none() && b.kind != ltoarc_device::MediaKind::Cleaning && v.generation.is_none() {
                crate::note!(
                    "note: {bc} has no media identifier (e.g. \"L8\"); its generation is learned when it is loaded. \
                     Libraries can usually be set to report 8-character labels."
                );
            }
        }
        for bc in ctx.cat.volumes_missing_from_with(lib.id, &present, &unlabelled)? {
            crate::note!("note: {bc} is no longer in {}", lib.name);
            ctx.cat.log_event("tape", &bc, &format!("left {}", lib.name))?;
        }
    }
    t.print();
    Ok(())
}

fn unregister(ctx: &Ctx, barcode: &str, force: bool, yes: bool) -> Result<()> {
    let v = ctx.volume(barcode)?;
    let parts = ctx.cat.volume_sessions(v.id)?;
    if !parts.is_empty() {
        if !force {
            let mut sids: Vec<i64> = parts.iter().map(|p| p.session_id).collect();
            sids.sort();
            sids.dedup();
            bail!("{} holds {} session(s); add --force to forget them with it", v.barcode, sids.len());
        }
        crate::archive::confirm_loss(ctx, std::slice::from_ref(&v), yes, "forget")?;
    }
    let gone = ctx.cat.forget_volume(v.id)?;
    ctx.cat.delete_volume(v.id)?;
    ctx.cat.log_event("tape", &v.barcode, &format!("unregistered ({gone} session(s) forgotten)"))?;
    println!("{} removed from the catalog{}", v.barcode, if gone > 0 { format!("; forgot {gone} session(s) held only there") } else { String::new() });
    if v.labelled && v.library_id.is_some() && v.element.is_some() {
        println!("it is still in {}: the next `ltoarc tape register` records it again", ctx.location(&v)?);
    }
    Ok(())
}

fn register_unlabelled(ctx: &Ctx, library: Option<&str>, slot: i64, name: &str, generation: Option<&str>) -> Result<()> {
    let lib = match library {
        Some(l) => ctx.library(l)?,
        None => match ctx.cat.libraries()?.as_slice() {
            [one] => one.clone(),
            [] => bail!("no libraries recorded; run `ltoarc hw scan` first"),
            _ => bail!("several libraries; choose one with --library"),
        },
    };
    let addr = lib.slot_address(slot).ok_or_else(|| anyhow!("{} has no slot {slot}", lib.name))?;
    let inv = ctx.open_changer(&lib)?.inventory()?;
    let el = inv.element(addr as u16).ok_or_else(|| anyhow!("{} reports no element for slot {slot}", lib.name))?;
    if !el.full {
        bail!("{} slot {slot} is empty", lib.name);
    }
    if let Some(bc) = &el.barcode {
        bail!("{} slot {slot} holds {bc}, which has a readable barcode; use `ltoarc tape register`", lib.name);
    }
    let generation = generation.map(|g| g.parse::<Generation>().map_err(|e| anyhow!(e))).transpose()?;
    let v = ctx.cat.register_unlabelled(name, generation.map(|g| g.name()), lib.id, addr)?;
    ctx.cat.log_event("tape", name, &format!("registered without barcode in {} slot {slot}", lib.name))?;
    println!("{name} registered in {} slot {slot} (no barcode label; tracked by position)", lib.name);
    if v.generation.is_none() {
        println!("its generation is learned the first time it is loaded");
    }
    Ok(())
}

fn list(ctx: &Ctx) -> Result<()> {
    let mut t = Table::new(&["BARCODE", "GEN", "KIND", "STATE", "POOL", "LOCATION", "UPDATED"]);
    for v in ctx.cat.volumes()? {
        t.row(vec![
            v.barcode.clone(),
            opt(&v.generation),
            v.kind.clone(),
            v.state.clone(),
            v.pool.clone().unwrap_or_else(|| "-".into()),
            ctx.location(&v)?,
            when(v.updated_at),
        ]);
    }
    if t.is_empty() {
        println!("no cartridges recorded; run `ltoarc tape register`");
    } else {
        t.print();
    }
    Ok(())
}

/// Load a cartridge (if it is not already in a drive). Returns the drive row and handle.
/// Load a cartridge for reading (into a drive that can at least read it).
pub fn load(ctx: &Ctx, barcode: &str, named: Option<&str>) -> Result<(DriveRow, Box<dyn TapeDrive>)> {
    load_for(ctx, barcode, named, false)
}

/// Load a cartridge into a drive that can write it.
pub fn load_rw(ctx: &Ctx, barcode: &str, named: Option<&str>) -> Result<(DriveRow, Box<dyn TapeDrive>)> {
    load_for(ctx, barcode, named, true)
}

/// Where cartridge `barcode` is: its library, a changer for it, its inventory and element.
/// One that is in no library is asked for (operator request) when `--wait-tapes` allows,
/// and looked for in every library until it turns up.
type Located = (VolumeRow, LibraryRow, Box<dyn ltoarc_device::Changer>, ltoarc_device::Inventory, ltoarc_device::Element);

pub fn locate(ctx: &Ctx, barcode: &str) -> Result<Located> {
    let started = std::time::Instant::now();
    let mut asked = false;
    loop {
        let v = ctx.volume(barcode)?;
        if let Some(lib) = v.library_id.map(|id| ctx.cat.library_by_id(id)).transpose()?.flatten() {
            let mut changer = ctx.open_changer(&lib)?;
            let inv = changer.inventory()?;
            if let Some(el) = ctx.find_element(&v, &inv) {
                return Ok((v, lib, changer, inv, el));
            }
        }
        // Inserted somewhere else (or put back after being offsite)?
        for lib in ctx.cat.libraries()? {
            let Ok(mut changer) = ctx.open_changer(&lib) else { continue };
            let inv = changer.inventory()?;
            if let Some(el) = inv.elements.iter().find(|e| e.full && e.barcode.as_deref().is_some_and(|b| b.eq_ignore_ascii_case(barcode))).cloned() {
                let mut v = v.clone();
                v.library_id = Some(lib.id);
                v.element = Some(el.address as i64);
                ctx.cat.update_volume(&v)?;
                if asked {
                    crate::note!("{barcode} found in {} {}", lib.name, lib.element_name(el.address as i64));
                }
                return Ok((v, lib, changer, inv, el));
            }
        }
        let Some(limit) = ctx.wait_tapes else {
            bail!("{barcode} is not in any library; insert it and run `ltoarc tape register` (or pass --wait-tapes DURATION to wait for it)");
        };
        if started.elapsed() > limit {
            bail!("{barcode} was not inserted within {}s", limit.as_secs());
        }
        if !asked {
            crate::note!("OPERATOR: please insert {barcode} into a library (a free slot or the I/E slot); waiting up to {}s", limit.as_secs());
            ctx.cat.log_event("request", barcode, "insert cartridge")?;
            asked = true;
        }
        std::thread::sleep(Duration::from_secs(10));
    }
}

fn load_for(ctx: &Ctx, barcode: &str, named: Option<&str>, write: bool) -> Result<(DriveRow, Box<dyn TapeDrive>)> {
    let (v, lib, mut changer, inv, el) = locate(ctx, barcode)?;
    if el.kind == ElementType::DataTransfer {
        let d = ctx
            .cat
            .drives()?
            .into_iter()
            .find(|d| d.library_id == Some(lib.id) && d.element == Some(el.address as i64))
            .ok_or_else(|| anyhow!("{barcode} is in a drive bay this catalog does not know; run `ltoarc hw scan`"))?;
        if named.is_some_and(|n| n != d.name && n != d.serial) {
            bail!("{barcode} is already in drive {}", d.name);
        }
        if write && crate::ctx::access(&d, &v) != ltoarc_device::Access::ReadWrite {
            bail!("{barcode} is in drive {}, which cannot write {} media", d.name, opt(&v.generation));
        }
        let mut drive = ctx.open_drive(&d)?;
        if drive.readiness()? != Readiness::Ready {
            drive.load()?;
            drive.wait_ready(READY_TIMEOUT)?;
        }
        crate::note!("{barcode} is in drive {}", d.name);
        return Ok((d, drive));
    }
    let d = ctx.choose_drive(&lib, &inv, &v, named, write)?;
    let bay = d.element.ok_or_else(|| anyhow!("drive {} has no known bay", d.name))?;
    if let Some(held) = inv.element(bay as u16).filter(|e| e.full) {
        // The drive holds another cartridge: put it back and load this one.
        let holder = match &held.barcode {
            Some(b) => Some(b.clone()),
            None => ctx.cat.unlabelled_at(lib.id, bay)?.map(|v| v.barcode),
        };
        let holder = holder.ok_or_else(|| anyhow!("drive {} holds a cartridge this catalog does not know; unload it first", d.name))?;
        let mut drive = ctx.open_drive(&d)?;
        exchange(ctx, &d, &mut *drive, &holder, barcode)?;
        crate::note!("{barcode} loaded in drive {}", d.name);
        return Ok((d, drive));
    }
    crate::note!("moving {barcode} from {} {} to drive {} ...", lib.name, lib.element_name(el.address as i64), d.name);
    {
        let _arm = ctx.leases.changer(&lib)?;
        changer.move_medium(el.address, bay as u16)?;
    }
    let mut drive = ctx.open_drive(&d)?;
    match drive.wait_ready(READY_TIMEOUT) {
        Ok(()) => {}
        Err(DevError::NotReady(_)) => {
            drive.load()?;
            drive.wait_ready(READY_TIMEOUT)?;
        }
        Err(e) => return Err(e.into()),
    }
    let mut v = ctx.volume(barcode)?;
    v.element = Some(bay);
    learn_generation(&mut *drive, &mut v);
    ctx.cat.update_volume(&v)?;
    ctx.cat.log_event("tape", barcode, &format!("loaded into {}", d.name))?;
    ctx.snapshot(&d, &mut *drive, Some(barcode), "load");
    crate::note!("{barcode} loaded in drive {}", d.name);
    Ok((d, drive))
}

/// Fill in the generation from what the drive reports, when the barcode did not say.
fn learn_generation(drive: &mut dyn TapeDrive, v: &mut VolumeRow) {
    if let Ok(mi) = drive.media_info() {
        if let Some(g) = mi.generation {
            v.generation = Some(g.name().to_string());
            return;
        }
    }
    if v.generation.is_none() {
        let code = drive.read_attribute(0, id::MEDIUM_DENSITY_CODE).ok().flatten().and_then(|a| a.as_u64());
        if let Some(g) = code.and_then(|c| Generation::from_density(c as u8)) {
            v.generation = Some(g.name().to_string());
        }
    }
}

fn unload(ctx: &Ctx, named: Option<&str>, slot: Option<i64>) -> Result<()> {
    let drives: Vec<DriveRow> = match named {
        Some(n) => vec![ctx.drive(n)?],
        None => ctx.attached_drives()?,
    };
    let mut targets = Vec::new();
    for d in drives {
        let Some(lib) = d.library_id.and_then(|id| ctx.cat.library_by_id(id).ok().flatten()) else { continue };
        let inv = ctx.open_changer(&lib)?.inventory()?;
        if let Some(bay) = d.element.and_then(|e| inv.element(e as u16)).filter(|e| e.full) {
            targets.push((d, lib, inv.clone(), bay.clone()));
        }
    }
    let (d, lib, inv, bay) = match targets.len() {
        0 => bail!("no drive holds a cartridge"),
        1 => targets.remove(0),
        _ => bail!("several drives hold cartridges; choose one with --drive"),
    };
    // Unlabelled cartridges are known by the element the catalog last put them in.
    let barcode = match &bay.barcode {
        Some(b) => Some(b.clone()),
        None => ctx.cat.unlabelled_at(lib.id, bay.address as i64)?.map(|v| v.barcode),
    };
    let mut drive = ctx.open_drive(&d)?;
    if drive.readiness()? == Readiness::Ready {
        ctx.snapshot(&d, &mut *drive, barcode.as_deref(), "unload");
    }
    println!("unloading drive {} ...", d.name);
    match drive.unload() {
        Ok(()) | Err(DevError::NoMedium) => {}
        Err(e) => return Err(e.into()),
    }
    let dst = match slot {
        Some(n) => lib.slot_address(n).ok_or_else(|| anyhow!("{} has no slot {n}", lib.name))?,
        None => bay
            .source
            .filter(|s| inv.element(*s).is_some_and(|e| e.kind == ElementType::Storage && !e.full))
            .map(|s| s as i64)
            .or_else(|| inv.first_empty_slot().map(|e| e.address as i64))
            .ok_or_else(|| anyhow!("{} has no free slot", lib.name))?,
    };
    {
        let _arm = ctx.leases.changer(&lib)?;
        ctx.open_changer(&lib)?.move_medium(bay.address, dst as u16)?;
    }
    if let Some(bc) = &barcode {
        if let Ok(mut v) = ctx.volume(bc) {
            v.element = Some(dst);
            ctx.cat.update_volume(&v)?;
            ctx.cat.log_event("tape", bc, &format!("unloaded from {} to {}", d.name, lib.element_name(dst)))?;
        }
    }
    println!("{} returned to {} {}", barcode.as_deref().unwrap_or("cartridge"), lib.name, lib.element_name(dst));
    Ok(())
}

/// Display form of a cartridge memory attribute.
fn mam_value(a: &Attribute) -> String {
    let hex = || a.value.iter().map(|b| format!("{b:02x}")).collect::<String>();
    match (a.id, a.format) {
        (id::FORMAT_DENSITY_CODE | id::MEDIUM_DENSITY_CODE, _) => a.as_u64().map_or_else(hex, |c| {
            let name = Generation::from_density(c as u8).map_or("", |g| g.name());
            format!("{c:02X}h {name}").trim_end().to_string()
        }),
        (id::TEXT_LOCALIZATION_ID | id::MEDIUM_TYPE, _) => a.as_u64().map_or_else(hex, |c| format!("{c:02X}h")),
        (_, mam::Format::Binary) if a.value.len() <= 8 => a.as_u64().map(|n| n.to_string()).unwrap_or_default(),
        (_, mam::Format::Binary) if a.value.iter().all(|b| (0x20..0x7f).contains(b)) => a.as_string(),
        (_, mam::Format::Binary) => hex(),
        _ => a.as_string(),
    }
}


fn info(ctx: &Ctx, barcode: Option<&str>, named: Option<&str>) -> Result<()> {
    if let Some(bc) = barcode {
        let v = ctx.volume(bc)?;
        println!("barcode      {}", v.barcode);
        println!("generation   {}", opt(&v.generation));
        println!("kind         {}", v.kind);
        println!("state        {}", v.state);
        println!("pool         {}", v.pool.as_deref().unwrap_or("-"));
        println!("uuid         {}", v.uuid.as_deref().unwrap_or("-"));
        println!("location     {}", ctx.location(&v)?);
        println!("registered   {}", when(v.registered_at));
        if v.generation.as_deref() == Some("LTO-7 M8") {
            println!("note         M8 cartridges can only be read by LTO-8 drives (not LTO-7 or LTO-9)");
        }
    }
    let target = match (named, barcode) {
        (Some(n), _) => Some(ctx.drive(n)?),
        (None, Some(bc)) => ctx.drive_holding(bc)?.map(|(d, _)| d),
        (None, None) => bail!("give a barcode or --drive"),
    };
    let Some(d) = target else { return Ok(()) };
    let mut drive = ctx.open_drive(&d)?;
    println!("\ndrive        {} ({} {}, serial {})", d.name, d.vendor, d.product, d.serial);
    match drive.readiness()? {
        Readiness::Ready => {}
        other => {
            println!("drive state  {other:?}");
            return Ok(());
        }
    }
    let mi = drive.media_info()?;
    println!(
        "media        density {:#04x} ({}), medium type {:#04x}{}",
        mi.density,
        mi.generation.map_or("blank/unknown", |g| g.name()),
        mi.medium_type,
        if mi.write_protected { ", WRITE PROTECTED" } else { "" }
    );
    for dd in drive.report_density(true)? {
        println!(
            "density      {:#04x} {:<8} {:>6} GB{}{}",
            dd.primary,
            dd.name,
            dd.capacity_mb / 1000,
            if dd.write_ok { " writable" } else { " read-only" },
            if dd.default { " (default)" } else { "" }
        );
    }
    if d.generation.as_deref() == Some("LTO-8") && drive.report_density(true)?.iter().any(|x| x.primary == LTO7) {
        match m8::m8_status(&mut *drive)? {
            M8Status::Eligible => println!("M8           eligible (new LTO-7 cartridge): `ltoarc tape init --m8` can make it 9 TB"),
            M8Status::AlreadyM8 => println!("M8           yes"),
            M8Status::NotEligible(_) => println!("M8           not eligible (cartridge has been written)"),
        }
    }
    println!("\ncartridge memory:");
    let mut t = Table::new(&["ID", "ATTRIBUTE", "VALUE"]);
    for a in drive.read_attributes(0)? {
        let value = mam_value(&a);
        if value.is_empty() {
            continue;
        }
        t.row(vec![format!("{:04X}h", a.id), mam::attr_name(a.id).unwrap_or("").into(), value]);
    }
    t.print();
    println!();
    match inspect(&mut *drive)? {
        Content::Blank => println!("content      blank"),
        Content::Foreign(why) => println!("content      not ltoarc ({why})"),
        Content::Ltoarc(l) => {
            println!("content      ltoarc volume {} (format {}), label barcode {}", l.volume_uuid, l.format_version, l.barcode);
            println!("             pool {}, created {} by {}", l.pool.as_deref().unwrap_or("-"), l.created, l.created_by);
        }
    }
    Ok(())
}

fn init(
    ctx: &Ctx,
    barcode: &str,
    pool: Option<String>,
    want_m8: bool,
    force: bool,
    named: Option<&str>,
    yes: bool,
) -> Result<()> {
    let v = ctx.volume(barcode)?;
    if v.kind == "cleaning" {
        bail!("{barcode} is a cleaning cartridge");
    }
    let holding = ctx.drive_holding(barcode)?;
    if holding.is_none() && !yes {
        println!("dry run: {barcode} would be loaded, inspected, and labelled{}.", if want_m8 { " after M8 formatting" } else { "" });
        println!("re-run with --yes to do it.");
        return Ok(());
    }
    let (d, mut drive) = load_rw(ctx, barcode, named)?;
    let mut actions = Vec::new();
    match inspect(&mut *drive)? {
        Content::Blank => {}
        Content::Ltoarc(l) if force => {
            let n = ctx.cat.volume_sessions(v.id)?.len();
            actions.push(format!(
                "DESTROY existing ltoarc volume {} (pool {}), and forget the {n} session part(s) the catalog has on it",
                l.volume_uuid,
                l.pool.unwrap_or_default()
            ));
        }
        Content::Ltoarc(l) => bail!(
            "{barcode} already holds ltoarc volume {} (pool {}); use --force to erase it",
            l.volume_uuid,
            l.pool.unwrap_or_default()
        ),
        Content::Foreign(why) if force => actions.push(format!("DESTROY existing non-ltoarc data ({why})")),
        Content::Foreign(why) => bail!("{barcode} holds data that is not ltoarc ({why}); use --force to overwrite it"),
    }
    let mut m8_needed = false;
    if want_m8 {
        match m8::m8_status(&mut *drive)? {
            M8Status::Eligible => {
                m8_needed = true;
                actions.push("format as M8 (LTO-7 Type M, 9 TB). IRREVERSIBLE; afterwards only LTO-8 drives can use it".into());
            }
            M8Status::AlreadyM8 => println!("{barcode} is already M8"),
            M8Status::NotEligible(why) => bail!("cannot make {barcode} an M8: {why}"),
        }
    }
    actions.push(format!("write ltoarc label (pool {}) and cartridge memory attributes", pool.as_deref().unwrap_or("-")));
    println!("{barcode} in drive {}:", d.name);
    for a in &actions {
        println!("  - {a}");
    }
    if !yes {
        println!("dry run; re-run with --yes to do it.");
        return Ok(());
    }
    if m8_needed {
        println!("formatting {barcode} as M8 ...");
        m8::format_m8(&mut *drive).context("M8 format")?;
        ctx.cat.log_event("tape", barcode, "formatted as M8")?;
    }
    let forgotten = ctx.cat.forget_volume(v.id)?;
    if forgotten > 0 {
        println!("removed {forgotten} session(s) held only on {barcode} from the catalog");
    }
    let (_, generation) = label_cartridge(ctx, &d, &mut *drive, barcode, pool)?;
    if generation == Generation::M8 && !barcode.to_ascii_uppercase().ends_with("M8") {
        println!("relabel the cartridge with an \"M8\" media identifier so the library reports it correctly");
    }
    Ok(())
}

/// Write the ltoarc label to the cartridge loaded in `drive` and record it in the catalog.
fn label_cartridge(
    ctx: &Ctx,
    d: &DriveRow,
    drive: &mut dyn TapeDrive,
    barcode: &str,
    pool: Option<String>,
) -> Result<(VolumeRow, Generation)> {
    // Formatted density, else what loading learned from cartridge memory, else the drive's
    // default density for this cartridge.
    let mi = drive.media_info()?;
    let learned = ctx.volume(barcode)?.generation.and_then(|g| g.parse::<Generation>().ok());
    let default = drive.report_density(true)?.into_iter().find(|d| d.default).and_then(|d| Generation::from_density(d.primary));
    let generation = mi
        .generation
        .or(learned)
        .or(default)
        .ok_or_else(|| anyhow!("cannot tell the media generation of {barcode}"))?;
    let created = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut label = VolumeLabel::new(barcode, generation.name(), pool.clone(), created);
    let max_block = drive.max_block_size()? as u32;
    if max_block < label.block_size {
        // Largest power of two the host can transfer; at least 64 KiB.
        label.block_size = (1u32 << (31 - max_block.leading_zeros())).max(64 << 10);
        crate::note!("note: this host transfers at most {max_block} bytes per command; using {} byte blocks", label.block_size);
    }
    for w in write_label(drive, &label)? {
        crate::note!("warning: {w}");
    }
    // The cartridge's own figure (MAM 0001h, maximum capacity in partition, MiB), else the
    // drive's nominal one for the density.
    let capacity = match drive.read_attribute(0, 0x0001).ok().flatten().and_then(|a| a.as_u64()) {
        Some(mib) => Some((mib * 1_048_576 / 1_000_000) as i64),
        None => drive.report_density(true)?.into_iter().find(|x| x.default).map(|x| x.capacity_mb as i64),
    };
    let mut v = ctx.volume(barcode)?;
    v.generation = Some(generation.name().to_string());
    v.state = "scratch".into();
    v.uuid = Some(label.volume_uuid.to_string());
    v.pool = pool;
    v.capacity_mb = capacity;
    v.block_size = Some(label.block_size as i64);
    ctx.cat.update_volume(&v)?;
    ctx.cat.log_event("tape", barcode, &format!("initialized as {} volume {}", generation, label.volume_uuid))?;
    ctx.snapshot(d, drive, Some(barcode), "init");
    println!("{barcode} initialized: {} volume {}", generation, label.volume_uuid);
    Ok((v, generation))
}

/// Cartridges named on the command line, in order: barcodes, comma-separated, each possibly
/// a range such as DC8001L8..DC8020L8. A name without a media identifier (DC8001, or a range
/// DC8001..DC8020) matches the registered cartridge of any generation, so a set may mix
/// generations and libraries; numbers of a range that are not registered are skipped. Where
/// a number is registered in more than one generation, a range that starts with a media
/// identifier (DC8001L8..DC8020) takes that one; otherwise the user is asked which, or, when
/// not at a terminal, the command stops.
pub fn tape_set(ctx: &Ctx, spec: &str) -> Result<Vec<VolumeRow>> {
    let all = ctx.cat.volumes()?;
    resolve_set(&all, spec, &mut |names, kinds| {
        use std::io::{BufRead, IsTerminal, Write};
        let listed = names.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
        let more = if names.len() > 8 { format!(" and {} more", names.len() - 8) } else { String::new() };
        if !std::io::stdin().is_terminal() {
            bail!(
                "{listed}{more} match cartridges of more than one generation ({}); start the range with the media type to use, e.g. {}{}..",
                kinds.join(", "),
                names[0],
                kinds[0]
            );
        }
        loop {
            print!("{listed}{more} match cartridges of more than one generation. Which media type? [{}] ", kinds.join("/"));
            std::io::stdout().flush()?;
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line)? == 0 {
                bail!("no media type chosen");
            }
            let pick = line.trim().to_ascii_uppercase();
            if kinds.contains(&pick) {
                return Ok(pick);
            }
        }
    })
}

/// Picks the media type for ambiguous numbers: (numbers, media types they come in).
type ChooseType<'a> = dyn FnMut(&[String], &[String]) -> Result<String> + 'a;

/// A label's media identifier (L8, M8, LY, ...), if it ends with one.
fn media_suffix(name: &str) -> Option<String> {
    ltoarc_device::parse_barcode(name).generation.map(|_| name.trim()[name.trim().len() - 2..].to_ascii_uppercase())
}

/// The cartridges `spec` names among `all` (see [`tape_set`]). `choose` picks the media type
/// for numbers registered in several generations when the spec does not say which.
fn resolve_set(all: &[VolumeRow], spec: &str, choose: &mut ChooseType) -> Result<Vec<VolumeRow>> {
    enum Slot<'a> {
        One(&'a VolumeRow),
        Several(String, Vec<&'a VolumeRow>),
    }
    let mut slots: Vec<Slot> = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // (names to look up, media type they must have, media type preferred)
        let (names, exact, prefer) = match part.split_once("..") {
            Some((a, b)) => {
                let a = a.trim();
                // "DC5001..": up to the last registered number of that series.
                let open;
                let b = match b.trim() {
                    "" => {
                        open = open_end(all, a)?;
                        open.as_str()
                    }
                    b => b,
                };
                match (media_suffix(a), media_suffix(b)) {
                    (Some(x), Some(y)) if x == y => (expand(a, b)?, Some(x), None),
                    (Some(x), Some(y)) => bail!("{part}: the range ends have different media types ({x}, {y})"),
                    (Some(x), None) => (expand(&a[..a.len() - 2], b)?, None, Some(x)),
                    (None, Some(y)) => bail!("{part}: put the media type on the first label ({a}{y}..{b}, or {a}{y}..{} to prefer {y} but allow others)", &b[..b.len() - 2]),
                    (None, None) => (expand(a, b)?, None, None),
                }
            }
            None => match media_suffix(part) {
                Some(x) => (vec![part.to_string()], Some(x), None),
                None => (vec![part.to_string()], None, None),
            },
        };
        let range = names.len() > 1 || part.contains("..");
        let mut found = 0;
        for name in &names {
            let up = name.to_ascii_uppercase();
            let hits: Vec<&VolumeRow> = all
                .iter()
                .filter(|v| {
                    let b = v.barcode.to_ascii_uppercase();
                    if exact.is_some() {
                        b == up
                    } else {
                        b == up || (b.len() == up.len() + 2 && b.starts_with(&up) && media_suffix(&b).is_some())
                    }
                })
                .collect();
            if hits.is_empty() && !range {
                bail!("{name} is not a registered cartridge (see `ltoarc tape list`)");
            }
            found += hits.len().min(1);
            let preferred: Vec<&VolumeRow> = match &prefer {
                Some(p) => hits.iter().copied().filter(|v| media_suffix(&v.barcode).as_ref() == Some(p)).collect(),
                None => Vec::new(),
            };
            match (hits.len(), preferred.len()) {
                (0, _) => {}
                (1, _) => slots.push(Slot::One(hits[0])),
                (_, 1) => slots.push(Slot::One(preferred[0])),
                _ => slots.push(Slot::Several(up, hits)),
            }
        }
        if found == 0 {
            bail!("no registered cartridge in {part}");
        }
    }
    let ambiguous: Vec<String> = slots.iter().filter_map(|s| if let Slot::Several(n, _) = s { Some(n.clone()) } else { None }).collect();
    let pick = if ambiguous.is_empty() {
        None
    } else {
        let mut kinds: Vec<String> = slots
            .iter()
            .flat_map(|s| if let Slot::Several(_, v) = s { v.clone() } else { Vec::new() })
            .filter_map(|v| media_suffix(&v.barcode))
            .collect();
        kinds.sort();
        kinds.dedup();
        Some(choose(&ambiguous, &kinds)?)
    };
    let mut out: Vec<VolumeRow> = Vec::new();
    for s in slots {
        let v = match s {
            Slot::One(v) => v,
            Slot::Several(name, hits) => {
                let p = pick.as_deref().expect("chosen");
                *hits.iter().find(|v| media_suffix(&v.barcode).as_deref() == Some(p)).ok_or_else(|| anyhow!("{name} has no {p} cartridge"))?
            }
        };
        if !out.iter().any(|o| o.id == v.id) {
            out.push(v.clone());
        }
    }
    Ok(out)
}

/// The end of an open range starting at `a` (DC5001 or DC5001L5): the highest registered
/// number of the same series (same prefix and number width), without media identifier.
fn open_end(all: &[VolumeRow], a: &str) -> Result<String> {
    let base = match media_suffix(a) {
        Some(_) => &a[..a.len() - 2],
        None => a,
    };
    let digits = base.chars().rev().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        bail!("{a}..: the label has no number to count from");
    }
    let (prefix, num) = base.split_at(base.len() - digits);
    let start: u64 = num.parse()?;
    let last = all
        .iter()
        .filter_map(|v| {
            let b = v.barcode.to_ascii_uppercase();
            let b = match media_suffix(&b) {
                Some(_) => b[..b.len() - 2].to_string(),
                None => b,
            };
            let n = b.strip_prefix(&prefix.to_ascii_uppercase())?;
            (n.len() == digits && n.chars().all(|c| c.is_ascii_digit())).then(|| n.parse::<u64>().ok()).flatten()
        })
        .filter(|&n| n >= start)
        .max()
        .ok_or_else(|| anyhow!("no registered cartridge from {a} on"))?;
    Ok(format!("{prefix}{last:0digits$}"))
}

fn expand(a: &str, b: &str) -> Result<Vec<String>> {
    ltoarc_device::expand_barcode_range(&format!("{a}..{b}")).map_err(|e| anyhow!(e))
}

/// Make the cartridge loaded in `drive` ready to append a session to: label it if it is
/// blank, and recover it if a session was left open on it.
/// With `overwrite` the cartridge is started afresh whatever it holds: relabelled, and what
/// the catalog had on it forgotten (the caller has had that confirmed).
pub fn ready_for_append(ctx: &Ctx, d: &DriveRow, drive: &mut dyn TapeDrive, barcode: &str, pool: Option<String>, overwrite: bool) -> Result<VolumeRow> {
    let v = ctx.volume(barcode)?;
    if v.kind == "cleaning" {
        bail!("{barcode} is a cleaning cartridge");
    }
    if v.uuid.is_none() {
        return match inspect(drive)? {
            Content::Blank => {
                crate::note!("{barcode} is blank; labelling it");
                Ok(label_cartridge(ctx, d, drive, barcode, pool)?.0)
            }
            Content::Ltoarc(_) | Content::Foreign(_) if overwrite => {
                crate::note!("{barcode}: overwriting what it holds (not in this catalog)");
                Ok(label_cartridge(ctx, d, drive, barcode, pool)?.0)
            }
            Content::Ltoarc(l) => bail!("{barcode} holds ltoarc volume {} this catalog does not know; run `ltoarc tape scan {barcode}`", l.volume_uuid),
            Content::Foreign(why) => bail!("{barcode} holds data that is not ltoarc ({why}); `ltoarc tape init --force` would erase it"),
        };
    }
    if overwrite {
        let gone = ctx.cat.forget_volume(v.id)?;
        crate::note!("{barcode}: rewriting it from the start (forgot {gone} session(s) held only there)");
        ctx.cat.log_event("overwrite", barcode, &format!("{gone} session(s) forgotten"))?;
        return Ok(label_cartridge(ctx, d, drive, barcode, pool)?.0);
    }
    if v.state == "expired" {
        // Retention left nothing on it that is still needed: start it afresh.
        let gone = ctx.cat.forget_volume(v.id)?;
        crate::note!("{barcode} holds only expired sessions; relabelling it (forgot {gone} session(s) from the catalog)");
        return Ok(label_cartridge(ctx, d, drive, barcode, pool)?.0);
    }
    recover_if_needed(ctx, drive, &v)?;
    ctx.volume(barcode)
}

/// Recover a cartridge whose cartridge memory shows an unfinished session.
fn recover_if_needed(ctx: &Ctx, drive: &mut dyn TapeDrive, v: &VolumeRow) -> Result<()> {
    if !matches!(ltoarc_engine::vci::state(drive)?, ltoarc_engine::vci::State::Open(_)) {
        return Ok(());
    }
    crate::note!("{} has an unfinished session; recovering it first", v.barcode);
    let r = ltoarc_engine::recover::recover_volume(&ctx.cat, drive, v, &ctx.keys)?;
    print_recovery(&v.barcode, &r);
    ctx.cat.log_event("recover", &v.barcode, &r.state)?;
    Ok(())
}

fn print_recovery(barcode: &str, r: &ltoarc_engine::recover::Recovery) {
    match r.state.as_str() {
        "recovered" => crate::note!(
            "{barcode}: recovered the session from block {}: {} entries, {} chunks{}; new catalog at block {}",
            r.session_start.unwrap_or(0),
            r.scan.files,
            r.scan.chunks,
            if r.already_known { " (already in the catalog)" } else { "" },
            r.catalog_object.unwrap_or(0)
        ),
        "nothing written" => crate::note!("{barcode}: the open session wrote nothing; marked clean"),
        "clean" => crate::note!("{barcode}: clean, nothing to recover"),
        "unknown" => crate::note!("{barcode}: no ltoarc coherency record (written before ltoarc recorded one); nothing to recover"),
        "stale" => crate::note!("{barcode}: something other than ltoarc wrote to it after its last catalog; check it with `ltoarc tape scan`"),
        other => crate::note!("{barcode}: {other}"),
    }
    for e in r.scan.errors.iter().take(5) {
        crate::note!("  note: {e}");
    }
}

fn recover(ctx: &Ctx, barcode: &str, named: Option<&str>) -> Result<()> {
    let v = ctx.volume(barcode)?;
    let (_, mut drive) = load_rw(ctx, barcode, named)?;
    let r = ltoarc_engine::recover::recover_volume(&ctx.cat, &mut *drive, &v, &ctx.keys)?;
    print_recovery(barcode, &r);
    ctx.cat.log_event("recover", barcode, &r.state)?;
    Ok(())
}

/// Unload drive `d` (open as `drive`) and put its cartridge `from` back in its slot (or the
/// first free one).
pub fn put_away(ctx: &Ctx, d: &DriveRow, drive: &mut dyn TapeDrive, from: &str) -> Result<()> {
    let lib_id = d.library_id.ok_or_else(|| anyhow!("drive {} is not in a library", d.name))?;
    let lib = ctx.cat.library_by_id(lib_id)?.ok_or_else(|| anyhow!("library {lib_id} missing from catalog"))?;
    let bay = d.element.ok_or_else(|| anyhow!("drive {} has no known bay", d.name))? as u16;
    ctx.snapshot(d, drive, Some(from), "unload");
    crate::note!("unloading {from} from drive {} ...", d.name);
    match drive.unload() {
        Ok(()) | Err(DevError::NoMedium) => {}
        Err(e) => return Err(e.into()),
    }
    let _arm = ctx.leases.changer(&lib)?;
    let mut changer = ctx.open_changer(&lib)?;
    let inv = changer.inventory()?;
    let held = inv.element(bay).cloned().ok_or_else(|| anyhow!("{} has no element {bay}", lib.name))?;
    let dst = held
        .source
        .filter(|s| inv.element(*s).is_some_and(|e| e.kind == ElementType::Storage && !e.full))
        .or_else(|| inv.first_empty_slot().map(|e| e.address))
        .ok_or_else(|| anyhow!("{} has no free slot for {from}", lib.name))?;
    changer.move_medium(bay, dst)?;
    if let Ok(mut v) = ctx.volume(from) {
        v.element = Some(dst as i64);
        ctx.cat.update_volume(&v)?;
    }
    ctx.cat.log_event("tape", from, &format!("unloaded from {} to {}", d.name, lib.element_name(dst as i64)))?;
    Ok(())
}

/// Swap the cartridge in drive `d` (open as `drive`) for `to`, which must be in the same
/// library: the old one goes back to its slot (or the first free one).
pub fn exchange(ctx: &Ctx, d: &DriveRow, drive: &mut dyn TapeDrive, from: &str, to: &str) -> Result<()> {
    let lib_id = d.library_id.ok_or_else(|| anyhow!("drive {} is not in a library, so it cannot change cartridges", d.name))?;
    let lib = ctx.cat.library_by_id(lib_id)?.ok_or_else(|| anyhow!("library {lib_id} missing from catalog"))?;
    let bay = d.element.ok_or_else(|| anyhow!("drive {} has no known bay", d.name))? as u16;
    let next = ctx.volume(to)?;
    if next.library_id != Some(lib_id) {
        bail!("{to} is not in {} with drive {}; move it there (cross-library spanning comes with the scheduler)", lib.name, d.name);
    }
    ctx.snapshot(d, drive, Some(from), "unload");
    crate::note!("unloading {from} from drive {} ...", d.name);
    match drive.unload() {
        Ok(()) | Err(DevError::NoMedium) => {}
        Err(e) => return Err(e.into()),
    }
    let arm = ctx.leases.changer(&lib)?;
    let mut changer = ctx.open_changer(&lib)?;
    let inv = changer.inventory()?;
    let held = inv.element(bay).cloned().ok_or_else(|| anyhow!("{} has no element {bay}", lib.name))?;
    let dst = held
        .source
        .filter(|s| inv.element(*s).is_some_and(|e| e.kind == ElementType::Storage && !e.full))
        .or_else(|| inv.first_empty_slot().map(|e| e.address))
        .ok_or_else(|| anyhow!("{} has no free slot for {from}", lib.name))?;
    let el = ctx.find_element(&next, &inv).ok_or_else(|| anyhow!("{to} is not in {} any more; run `ltoarc tape register`", lib.name))?;
    changer.move_medium(bay, dst)?;
    if let Ok(mut v) = ctx.volume(from) {
        v.element = Some(dst as i64);
        ctx.cat.update_volume(&v)?;
    }
    ctx.cat.log_event("tape", from, &format!("unloaded from {} to {}", d.name, lib.element_name(dst as i64)))?;
    crate::note!("moving {to} from {} {} to drive {} ...", lib.name, lib.element_name(el.address as i64), d.name);
    changer.move_medium(el.address, bay)?;
    drop(arm);
    match drive.wait_ready(READY_TIMEOUT) {
        Ok(()) => {}
        Err(DevError::NotReady(_)) => {
            drive.load()?;
            drive.wait_ready(READY_TIMEOUT)?;
        }
        Err(e) => return Err(e.into()),
    }
    let mut v = ctx.volume(to)?;
    v.element = Some(bay as i64);
    learn_generation(drive, &mut v);
    ctx.cat.update_volume(&v)?;
    ctx.cat.log_event("tape", to, &format!("loaded into {}", d.name))?;
    ctx.snapshot(d, drive, Some(to), "load");
    Ok(())
}

/// Pacing for a drive writing a media generation (None: full speed), from the configured
/// spec-sheet rates, and those rates.
pub fn pacing(
    ctx: &Ctx,
    d: &DriveRow,
    generation: &str,
    mode: ltoarc_engine::plan::PaceMode,
    source_rate: Option<f64>,
) -> Result<(ltoarc_engine::plan::Decision, ltoarc_engine::plan::DriveRates, &'static str)> {
    let (rates, from) = match ctx.drive_rates(d, generation)? {
        Some(r) => (r, "config"),
        None => (ltoarc_engine::plan::DriveRates::default_for(generation), "default"),
    };
    Ok((ltoarc_engine::plan::decide(mode, rates, source_rate), rates, from))
}

/// An attached drive that can write cartridge `bc`, in its library, and is empty or already
/// holds it.
pub fn free_writer(ctx: &Ctx, bc: &str) -> Result<Option<DriveRow>> {
    let v = ctx.volume(bc)?;
    // (Callers keep full cartridges out unless they overwrite.)
    if !crate::ctx::usable(&v, true) {
        return Ok(None);
    }
    let Some(lib) = v.library_id else { return Ok(None) };
    let writers = ctx.writers(lib, &v)?;
    if writers.is_empty() {
        return Ok(None);
    }
    let lib_row = ctx.cat.library_by_id(lib)?.ok_or_else(|| anyhow!("library {lib} missing"))?;
    let inv = ctx.open_changer(&lib_row)?.inventory()?;
    // Empty, or holding this very cartridge (an unlabelled one is known by its element).
    let at = ctx.find_element(&v, &inv).map(|e| e.address);
    let holds = |e: &ltoarc_device::Element| !e.full || e.barcode.as_deref() == Some(bc) || at == Some(e.address);
    for d in writers {
        if d.element.and_then(|e| inv.element(e as u16)).is_some_and(holds) && ctx.leases.busy(&d)?.is_none() {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

/// The rest of a tape set, handed out one cartridge at a time as a session's drives (lanes)
/// fill them. A lane's next cartridge goes into its own drive when that drive can write one
/// (the first such in set order), else into a free drive that can; cartridges no attached
/// drive can write are skipped.
pub struct TapeSet<'c> {
    pub ctx: &'c Ctx,
    /// The drive each lane is writing with.
    pub drives: Vec<DriveRow>,
    pub rest: std::collections::VecDeque<String>,
    pub pool: Option<String>,
    pub mode: ltoarc_engine::plan::PaceMode,
    pub source_rate: Option<f64>,
    /// Start each cartridge afresh instead of appending.
    pub overwrite: bool,
}

impl TapeSet<'_> {
    fn usable(&self, bc: &str) -> Option<VolumeRow> {
        let v = self.ctx.volume(bc).ok()?;
        crate::ctx::usable(&v, self.overwrite).then_some(v)
    }

    /// Where lane `lane` continues: (index in `rest`, a free drive to move to if not its own).
    fn choose(&self, lane: usize) -> Result<Option<(usize, Option<DriveRow>)>> {
        let own = &self.drives[lane];
        for (i, bc) in self.rest.iter().enumerate() {
            let Some(v) = self.usable(bc) else { continue };
            if v.library_id.is_some() && v.library_id == own.library_id && crate::ctx::access(own, &v) == ltoarc_device::Access::ReadWrite {
                return Ok(Some((i, None)));
            }
        }
        for (i, bc) in self.rest.iter().enumerate() {
            if let Some(d) = free_writer(self.ctx, bc)? {
                return Ok(Some((i, Some(d))));
            }
        }
        Ok(None)
    }
}

impl ltoarc_engine::backup::NextVolume for TapeSet<'_> {
    fn available(&mut self, lane: usize) -> bool {
        matches!(self.choose(lane), Ok(Some(_)))
    }

    fn next(&mut self, lane: usize, drive: &mut dyn TapeDrive, full: &VolumeRow) -> Result<ltoarc_engine::backup::Next> {
        let (i, move_to) = self.choose(lane)?.ok_or_else(|| anyhow!("the tape set has no further cartridge"))?;
        let to = self.rest.remove(i).expect("chosen index");
        crate::note!("{} is full; continuing on {to}", full.barcode);
        let new_drive = match move_to {
            None => {
                exchange(self.ctx, &self.drives[lane], drive, &full.barcode, &to)?;
                None
            }
            Some(d2) => {
                put_away(self.ctx, &self.drives[lane], drive, &full.barcode)?;
                self.ctx.leases.release(&self.drives[lane]);
                let (d2, mut dr) = load_rw(self.ctx, &to, Some(&d2.name))?;
                crate::note!("continuing in drive {}", d2.name);
                ready_for_append(self.ctx, &d2, &mut *dr, &to, self.pool.clone(), self.overwrite)?;
                self.drives[lane] = d2;
                Some(dr)
            }
        };
        let volume = match &new_drive {
            Some(_) => self.ctx.volume(&to)?,
            None => ready_for_append(self.ctx, &self.drives[lane], drive, &to, self.pool.clone(), self.overwrite)?,
        };
        let generation = volume.generation.clone().unwrap_or_default();
        let share = self.source_rate.map(|r| r / self.drives.len() as f64);
        let (decision, _, _) = pacing(self.ctx, &self.drives[lane], &generation, self.mode, share)?;
        Ok(ltoarc_engine::backup::Next { volume, drive: new_drive, pace: decision.pace })
    }
}

fn rescan(ctx: &Ctx, library: Option<&str>, yes: bool) -> Result<()> {
    let libs = match library {
        Some(l) => vec![ctx.library(l)?],
        None => ctx.cat.libraries()?,
    };
    if !yes {
        println!("this makes {} re-scan every slot with the robot; re-run with --yes", libs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "));
        return Ok(());
    }
    for l in libs {
        println!("re-scanning {} ...", l.name);
        ctx.open_changer(&l)?.rescan()?;
    }
    register(ctx, library)
}

/// Clean drive `d` with a cleaning cartridge of its library (`with`, or the first one found):
/// put away what it holds, load the cleaning cartridge, wait for the drive to finish and
/// unthread it, and put it back where it came from.
pub fn clean_drive(ctx: &Ctx, d: &DriveRow, with: Option<&str>) -> Result<()> {
    let lib_id = d.library_id.ok_or_else(|| anyhow!("drive {} is not in a library; clean it by hand", d.name))?;
    let lib = ctx.cat.library_by_id(lib_id)?.ok_or_else(|| anyhow!("library {lib_id} missing from catalog"))?;
    let bay = d.element.ok_or_else(|| anyhow!("drive {} has no known bay", d.name))? as u16;
    let mut drive = ctx.open_drive(d)?;
    let inv = ctx.open_changer(&lib)?.inventory()?;
    if let Some(held) = inv.element(bay).filter(|e| e.full) {
        let holder = match &held.barcode {
            Some(b) => Some(b.clone()),
            None => ctx.cat.unlabelled_at(lib.id, bay as i64)?.map(|v| v.barcode),
        };
        let holder = holder.ok_or_else(|| anyhow!("drive {} holds a cartridge this catalog does not know; unload it first", d.name))?;
        put_away(ctx, d, &mut *drive, &holder)?;
    }
    let inv = ctx.open_changer(&lib)?.inventory()?;
    let cleaners: Vec<VolumeRow> = ctx.cat.volumes()?.into_iter().filter(|v| v.kind == "cleaning" && v.library_id == Some(lib.id)).collect();
    let pick = cleaners
        .iter()
        .filter(|v| with.is_none_or(|w| v.barcode.eq_ignore_ascii_case(w)))
        .find_map(|v| ctx.find_element(v, &inv).filter(|e| e.kind != ElementType::DataTransfer).map(|e| (v.clone(), e)));
    let (cv, from) = pick.ok_or_else(|| match with {
        Some(w) => anyhow!("cleaning cartridge {w} is not in a slot of {}", lib.name),
        None => anyhow!("{} has no cleaning cartridge in a slot", lib.name),
    })?;
    crate::note!("cleaning drive {} with {} from {} ...", d.name, cv.barcode, lib.element_name(from.address as i64));
    {
        let _arm = ctx.leases.changer(&lib)?;
        ctx.open_changer(&lib)?.move_medium(from.address, bay)?;
    }
    // The drive cleans on its own and unthreads the cartridge when done.
    let started = std::time::Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        match drive.readiness() {
            Ok(Readiness::Ready) | Ok(Readiness::Becoming) if started.elapsed() < Duration::from_secs(600) => continue,
            Ok(Readiness::Ready) | Ok(Readiness::Becoming) => bail!("drive {} still busy with the cleaning cartridge after 10 minutes", d.name),
            _ => break,
        }
    }
    let _ = drive.unload();
    {
        let _arm = ctx.leases.changer(&lib)?;
        ctx.open_changer(&lib)?.move_medium(bay, from.address)?;
    }
    let mut cv = cv;
    cv.element = Some(from.address as i64);
    ctx.cat.update_volume(&cv)?;
    ctx.cat.log_event("clean", &d.name, &format!("cleaned with {} in {:.0} s", cv.barcode, started.elapsed().as_secs_f64()))?;
    ctx.snapshot(d, &mut *drive, None, "clean");
    crate::note!("drive {} cleaned; {} back in {}", d.name, cv.barcode, lib.element_name(from.address as i64));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vols(names: &[&str]) -> Vec<VolumeRow> {
        names
            .iter()
            .enumerate()
            .map(|(i, b)| VolumeRow {
                id: i as i64 + 1,
                barcode: b.to_string(),
                generation: None,
                kind: "data".into(),
                state: "scratch".into(),
                uuid: None,
                pool: None,
                library_id: None,
                element: None,
                capacity_mb: None,
                registered_at: 0,
                updated_at: 0,
                notes: None,
                labelled: true,
                block_size: None,
            })
            .collect()
    }

    fn names(all: &[VolumeRow], spec: &str) -> Result<Vec<String>> {
        let mut never = |_: &[String], _: &[String]| -> Result<String> { bail!("asked") };
        Ok(resolve_set(all, spec, &mut never)?.into_iter().map(|v| v.barcode).collect())
    }

    #[test]
    fn ranges_with_and_without_media_type() {
        let all = vols(&["DC0001L8", "DC0001L6", "DC0002L8", "DC0003L6", "DC0004L5"]);
        // Typed at both ends: that type only.
        assert_eq!(names(&all, "DC0001L6..DC0004L6").unwrap(), ["DC0001L6", "DC0003L6"]);
        // Typed start: preferred where a number has several, others allowed.
        assert_eq!(names(&all, "DC0001L8..DC0004").unwrap(), ["DC0001L8", "DC0002L8", "DC0003L6", "DC0004L5"]);
        assert_eq!(names(&all, "DC0001L6..DC0003").unwrap(), ["DC0001L6", "DC0002L8", "DC0003L6"]);
        // Untyped: ask when a number is registered in several generations.
        assert!(names(&all, "DC0001..DC0004").unwrap_err().to_string().contains("asked"));
        assert_eq!(names(&all, "DC0002..DC0004").unwrap(), ["DC0002L8", "DC0003L6", "DC0004L5"]);
        let mut pick = |n: &[String], k: &[String]| -> Result<String> {
            assert_eq!((n, k), (&["DC0001".to_string()][..], &["L6".to_string(), "L8".to_string()][..]));
            Ok("L6".into())
        };
        let got: Vec<String> = resolve_set(&all, "DC0001..DC0003", &mut pick).unwrap().into_iter().map(|v| v.barcode).collect();
        assert_eq!(got, ["DC0001L6", "DC0002L8", "DC0003L6"]);
        // Single names.
        assert_eq!(names(&all, "DC0004, DC0002L8").unwrap(), ["DC0004L5", "DC0002L8"]);
        assert!(names(&all, "DC0001").is_err());
        assert!(names(&all, "DC0002L6").unwrap_err().to_string().contains("not a registered"));
        assert!(names(&all, "DC0001..DC0004L8").unwrap_err().to_string().contains("first label"));
        assert!(names(&all, "DC0001L8..DC0004L6").unwrap_err().to_string().contains("different media types"));
        // Open-ended: to the last registered number of the series.
        assert_eq!(names(&all, "DC0002..").unwrap(), ["DC0002L8", "DC0003L6", "DC0004L5"]);
        assert_eq!(names(&all, "DC0001L6..").unwrap(), ["DC0001L6", "DC0002L8", "DC0003L6", "DC0004L5"]);
        assert!(names(&all, "DC0009..").unwrap_err().to_string().contains("no registered cartridge from DC0009 on"));
    }
}
