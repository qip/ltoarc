//! `ltoarc stats`: wear statistics of drives and cartridges.

use anyhow::Result;
use clap::Subcommand;
use ltoarc_catalog::StatsRow;
use ltoarc_device::{stats, Readiness};
use serde_json::Value;

use crate::ctx::Ctx;
use crate::table::{when, Table};

#[derive(Subcommand)]
pub enum StatsCmd {
    /// Drive lifetime counters (head hours, loads, cleanings, errors, TapeAlerts).
    Drive {
        /// Drive name (default: all attached drives).
        name: Option<String>,
        /// Show the last N recorded snapshots.
        #[arg(long)]
        history: Option<usize>,
        /// Do not query the drive; show recorded data only.
        #[arg(long)]
        offline: bool,
    },
    /// Cartridge wear (loads, passes, data written, errors, remaining life).
    Tape {
        barcode: String,
        /// Show the last N recorded snapshots.
        #[arg(long)]
        history: Option<usize>,
    },
    /// Check wear against the [wear] thresholds: drives that need cleaning, cartridges to
    /// retire, and whether errors follow a cartridge (across drives) or a drive (across
    /// cartridges).
    Check {
        /// Mark the cartridges to retire as "retiring": still read, no longer written.
        #[arg(long)]
        retire: bool,
        /// Clean the drives that need it (each with a cleaning cartridge of its library).
        #[arg(long)]
        clean: bool,
    },
}

const DRIVE_KEYS: &[(&str, &str)] = &[
    ("lifetime_volume_loads", "loads"),
    ("lifetime_cleanings", "cleanings"),
    ("lifetime_power_on_hours", "power-on h"),
    ("lifetime_head_hours", "head h"),
    ("head_hours_since_cleaning", "head h since clean"),
    ("lifetime_meters", "meters"),
    ("hard_write_errors", "hard write err"),
    ("hard_read_errors", "hard read err"),
];

const TAPE_KEYS: &[(&str, &str)] = &[
    ("load_count", "loads"),
    ("mib_written_life", "MiB written"),
    ("mib_read_life", "MiB read"),
    ("bom_passes", "BOM passes"),
    ("mot_passes", "MOT passes"),
    ("recovered_write_errors", "rec write err"),
    ("unrecovered_write_errors", "unrec write err"),
    ("recovered_read_errors", "rec read err"),
    ("unrecovered_read_errors", "unrec read err"),
    ("volume_lifetime_remaining", "life left %"),
    ("remaining_mib", "free MiB"),
];

pub fn run(ctx: &Ctx, cmd: StatsCmd) -> Result<()> {
    match cmd {
        StatsCmd::Check { retire, clean } => check(ctx, retire, clean),
        StatsCmd::Drive { name, history, offline } => {
            let drives = match &name {
                Some(n) => vec![ctx.drive(n)?],
                None => ctx.attached_drives()?,
            };
            for d in drives {
                if !offline && d.state == "attached" {
                    match ctx.open_drive_shared(&d) {
                        Ok(mut drive) => match stats::drive_stats(&mut *drive) {
                            Ok(s) => ctx.cat.add_drive_stats(d.id, "manual", &s)?,
                            Err(e) => eprintln!("warning: {}: {e}", d.name),
                        },
                        Err(e) => eprintln!("warning: {}: {e}", d.name),
                    }
                }
                let rows = ctx.cat.drive_stats(d.id, history.unwrap_or(1))?;
                println!("drive {} ({} {}, serial {})", d.name, d.vendor, d.product, d.serial);
                show(&rows, DRIVE_KEYS, history.is_some());
                if let Some(Value::Array(alerts)) = rows.first().and_then(|r| r.data.get("tapealerts")) {
                    for a in alerts {
                        println!("  TapeAlert {}: {}", a["flag"], a["name"].as_str().unwrap_or(""));
                    }
                }
                println!();
            }
            Ok(())
        }
        StatsCmd::Tape { barcode, history } => {
            let v = ctx.volume(&barcode)?;
            if let Some((d, _)) = ctx.drive_holding(&barcode)? {
                let mut drive = ctx.open_drive_shared(&d)?;
                if drive.readiness()? == Readiness::Ready {
                    let s = stats::volume_stats(&mut *drive)?;
                    ctx.cat.add_volume_stats(v.id, Some(d.id), "manual", &s)?;
                }
            }
            let rows = ctx.cat.volume_stats(v.id, history.unwrap_or(1))?;
            println!("cartridge {} ({})", v.barcode, v.generation.as_deref().unwrap_or("?"));
            show(&rows, TAPE_KEYS, history.is_some());
            if let Some(Value::Array(ds)) = rows.first().and_then(|r| r.data.get("last_drives")) {
                let ds: Vec<&str> = ds.iter().filter_map(Value::as_str).collect();
                println!("  last loaded in: {}", ds.join(", "));
            }
            Ok(())
        }
    }
}

fn show(rows: &[StatsRow], keys: &[(&str, &str)], as_history: bool) {
    if rows.is_empty() {
        println!("  no statistics recorded yet");
        return;
    }
    if as_history {
        let mut header = vec!["TIME", "EVENT"];
        header.extend(keys.iter().map(|(_, h)| *h));
        let mut t = Table::new(&header);
        for r in rows.iter().rev() {
            let mut cells = vec![when(r.taken_at), r.event.clone()];
            cells.extend(keys.iter().map(|(k, _)| r.data.get(*k).map_or("-".into(), fmt)));
            t.row(cells);
        }
        t.print();
    } else {
        let r = &rows[0];
        println!("  as of {} ({})", when(r.taken_at), r.event);
        for (k, label) in keys {
            if let Some(v) = r.data.get(*k) {
                println!("  {label:<20} {}", fmt(v));
            }
        }
    }
}

fn fmt(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn num(r: &StatsRow, key: &str) -> Option<u64> {
    r.data.get(key).and_then(Value::as_u64)
}

/// Read and write errors a cartridge has had (recovered and not).
fn errors(r: &StatsRow) -> u64 {
    ["recovered_write_errors", "unrecovered_write_errors", "recovered_read_errors", "unrecovered_read_errors"]
        .iter()
        .filter_map(|k| num(r, k))
        .sum()
}

fn check(ctx: &Ctx, retire: bool, clean: bool) -> Result<()> {
    use std::collections::{BTreeMap, BTreeSet};
    let w = &ctx.wear;
    let drives = ctx.attached_drives()?;
    let mut to_clean = Vec::new();
    for d in &drives {
        // A fresh reading when no other process is using the drive.
        if ctx.leases.busy(d)?.is_none() {
            if let Ok(mut dr) = ctx.open_drive_shared(d) {
                if let Ok(s) = stats::drive_stats(&mut *dr) {
                    ctx.cat.add_drive_stats(d.id, "check", &s)?;
                }
            }
        }
        let Some(r) = ctx.cat.drive_stats(d.id, 1)?.into_iter().next() else {
            println!("drive {}: no statistics recorded yet", d.name);
            continue;
        };
        let alerts: Vec<u64> = match r.data.get("tapealerts") {
            Some(Value::Array(a)) => a.iter().filter_map(|x| x["flag"].as_u64()).collect(),
            _ => Vec::new(),
        };
        let hours = num(&r, "head_hours_since_cleaning");
        let mut why = Vec::new();
        if alerts.contains(&20) {
            why.push("the drive asks for cleaning now (TapeAlert 20)".to_string());
        } else if alerts.contains(&21) {
            why.push("periodic cleaning due (TapeAlert 21)".to_string());
        }
        if let (Some(limit), Some(h)) = (w.drive_clean_after_head_hours, hours) {
            if h >= limit {
                why.push(format!("{h} head hours since the last cleaning (limit {limit})"));
            }
        }
        let hard = num(&r, "hard_write_errors").unwrap_or(0) + num(&r, "hard_read_errors").unwrap_or(0);
        let others: Vec<String> = alerts.iter().filter(|f| ![20, 21].contains(*f)).map(|f| format!("{f} {}", ltoarc_scsi::log::tapealert_name(*f as u8))).collect();
        if why.is_empty() {
            println!("drive {}: OK ({} head hours since cleaning, {hard} hard error(s))", d.name, hours.map_or("?".into(), |h| h.to_string()));
        } else {
            println!("drive {}: NEEDS CLEANING: {}", d.name, why.join("; "));
            to_clean.push(d.clone());
        }
        for o in others {
            println!("drive {}: TapeAlert {o}", d.name);
        }
    }
    // Cartridges, by their latest statistics.
    let min_life = w.tape_min_life_percent.unwrap_or(10);
    let mut retired = 0;
    for v in ctx.cat.volumes()?.into_iter().filter(|v| v.kind == "data") {
        let Some(r) = ctx.cat.volume_stats(v.id, 1)?.into_iter().next() else { continue };
        let mut why = Vec::new();
        if let Some(life) = num(&r, "volume_lifetime_remaining") {
            if life < min_life {
                why.push(format!("{life}% of its life left (minimum {min_life}%)"));
            }
        }
        if let (Some(limit), Some(loads)) = (w.tape_max_loads, num(&r, "load_count")) {
            if loads >= limit {
                why.push(format!("{loads} loads (limit {limit})"));
            }
        }
        let unrec = num(&r, "unrecovered_read_errors").unwrap_or(0) + num(&r, "unrecovered_write_errors").unwrap_or(0);
        if let Some(limit) = w.tape_max_unrecovered_errors {
            if unrec > limit {
                why.push(format!("{unrec} unrecovered error(s) (limit {limit})"));
            }
        }
        if why.is_empty() {
            continue;
        }
        let state = if v.state == "retiring" { " (already retiring)" } else { "" };
        println!("{}: RETIRE{state}: {}", v.barcode, why.join("; "));
        if retire && v.state != "retiring" {
            let mut v = v.clone();
            v.state = "retiring".into();
            ctx.cat.update_volume(&v)?;
            ctx.cat.log_event("retire", &v.barcode, &why.join("; "))?;
            retired += 1;
        }
    }
    if retired > 0 {
        println!("{retired} cartridge(s) marked retiring: they are read as before but not written again");
    }
    // Do errors follow cartridges or drives? Each rise in a cartridge's error counts between
    // two snapshots is put down to the drive of the later one.
    let mut tape_drives: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut drive_tapes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let names: BTreeMap<i64, String> = ctx.cat.drives()?.into_iter().map(|d| (d.id, d.name)).collect();
    for v in ctx.cat.volumes()? {
        let mut rows = ctx.cat.volume_stats(v.id, 10_000)?;
        rows.reverse();
        for pair in rows.windows(2) {
            if errors(&pair[1]) > errors(&pair[0]) {
                let dn = pair[1].drive_id.and_then(|i| names.get(&i).cloned()).unwrap_or_else(|| "?".into());
                tape_drives.entry(v.barcode.clone()).or_default().insert(dn.clone());
                drive_tapes.entry(dn).or_default().insert(v.barcode.clone());
            }
        }
    }
    for (t, ds) in &tape_drives {
        if ds.len() >= 2 {
            println!("{t}: errors in {} drives ({}): suspect the cartridge", ds.len(), ds.iter().cloned().collect::<Vec<_>>().join(", "));
        }
    }
    for (d, ts) in &drive_tapes {
        if ts.len() >= 2 {
            println!("drive {d}: errors on {} cartridges ({}): suspect the drive (or its head needs cleaning)", ts.len(), ts.iter().cloned().collect::<Vec<_>>().join(", "));
        }
    }
    if clean {
        for d in to_clean {
            if let Some(who) = ctx.leases.busy(&d)? {
                println!("drive {}: in use ({who}); clean it later", d.name);
                continue;
            }
            crate::tape::clean_drive(ctx, &d, None)?;
        }
    }
    Ok(())
}
