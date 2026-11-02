//! Will it fit? Room on the cartridges of a tape set, and which of them a session of a known
//! size would write to (in the order the session takes them).

use anyhow::Result;
use ltoarc_catalog::{Catalog, VolumeRow};

use crate::archive::human;
use crate::ctx::{media_generation, Ctx};

/// Room on a cartridge for a new session, and how it was worked out.
pub struct Room {
    pub bytes: u64,
    pub how: &'static str,
}

/// Kept free at the end of every cartridge (the session's trailer and volume catalog; the
/// writer stops at least 1% short of the end).
fn reserve(capacity: u64) -> u64 {
    capacity / 100 + (8 << 20)
}

/// The cartridge's full native capacity: as cartridge memory reported it, else nominal.
pub fn capacity(v: &VolumeRow) -> Option<u64> {
    v.capacity_mb.map(|mb| mb as u64 * 1_000_000).or_else(|| media_generation(v).map(|g| g.native_capacity()))
}

/// Room for new data on `v` (all of it when the session rewrites it from the start).
pub fn room(cat: &Catalog, v: &VolumeRow, overwrite: bool) -> Result<Option<Room>> {
    let Some(cap) = capacity(v) else { return Ok(None) };
    let usable = cap.saturating_sub(reserve(cap));
    if overwrite && v.uuid.is_some() {
        return Ok(Some(Room { bytes: usable, how: "rewritten from the start" }));
    }
    if v.state == "expired" {
        return Ok(Some(Room { bytes: usable, how: "expired, relabelled" }));
    }
    let parts = cat.volume_sessions(v.id)?;
    if v.uuid.is_none() || parts.is_empty() {
        return Ok(Some(Room { bytes: usable, how: "empty" }));
    }
    // What cartridge memory said after the last session written to it, if it said so since.
    let sessions = cat.sessions()?;
    let last_write = parts.iter().filter_map(|p| sessions.iter().find(|s| s.id == p.session_id).and_then(|s| s.ended)).max().unwrap_or(0);
    if let Some(r) = cat.volume_stats(v.id, 1)?.into_iter().next() {
        if r.taken_at >= last_write {
            if let Some(mib) = r.data.get("remaining_mib").and_then(|x| x.as_u64()) {
                return Ok(Some(Room { bytes: (mib << 20).saturating_sub(reserve(cap)), how: "per cartridge memory" }));
            }
        }
    }
    // Else from what the catalog has on it (1 MiB blocks).
    let used_blocks = parts.iter().map(|p| p.catalog_object.or(p.end_object).unwrap_or(p.start_object) + 2).max().unwrap_or(0) as u64;
    Ok(Some(Room { bytes: usable.saturating_sub(used_blocks << 20), how: "per catalog" }))
}

/// Show which cartridges of `set` (in order) a session of `need` bytes would write to and
/// whether they suffice; with the size unknown, the room on each. Returns how many
/// cartridges it would use (all of them when the size is unknown or they do not suffice).
pub fn show(ctx: &Ctx, set: &[VolumeRow], need: Option<u64>, overwrite: bool, label: &str) -> Result<usize> {
    let Some(need) = need else {
        let mut total = 0u64;
        println!("{label:<9} size not known beforehand (--plan scan or --plan list:FILE works it out); room:");
        for (i, v) in set.iter().enumerate() {
            let r = room(&ctx.cat, v, overwrite)?;
            total += r.as_ref().map_or(0, |r| r.bytes);
            if i < 4 {
                match r {
                    Some(r) => println!("          {:<10} {:>9} ({})", v.barcode, human(r.bytes), r.how),
                    None => println!("          {:<10} size unknown (generation not known)", v.barcode),
                }
            } else if i == 4 {
                println!("          ... and {} more", set.len() - 4);
            }
        }
        println!("          {} in {} cartridge(s)", human(total), set.len());
        return Ok(set.len());
    };
    let mut left = need;
    let mut used = 0usize;
    println!("{label:<9} about {} to write", human(need));
    for (i, v) in set.iter().enumerate() {
        if left == 0 {
            let rest: Vec<&str> = set[i..].iter().map(|v| v.barcode.as_str()).collect();
            let shown = if rest.len() > 4 { format!("{}..{} ({} cartridges)", rest[0], rest[rest.len() - 1], rest.len()) } else { rest.join(", ") };
            println!("          not needed: {shown}");
            break;
        }
        match room(&ctx.cat, v, overwrite)? {
            Some(r) if r.bytes > 0 => {
                let take = left.min(r.bytes);
                left -= take;
                used += 1;
                println!("          {:<10} {:>9} of {:>9} room ({})", v.barcode, human(take), human(r.bytes), r.how);
            }
            Some(_) => println!("          {:<10} full", v.barcode),
            None => {
                used += 1;
                println!("          {:<10} size unknown (generation not known)", v.barcode);
            }
        }
    }
    if left == 0 {
        println!("          {used} cartridge(s) of {}", set.len());
        return Ok(used);
    }
    let each = set.last().and_then(capacity).unwrap_or(0);
    let more = if each > reserve(each) {
        format!("; about {} more cartridge(s) like {}", left.div_ceil(each - reserve(each)), set.last().map_or("", |v| v.barcode.as_str()))
    } else {
        String::new()
    };
    println!("          SHORT by {}{more}: the session would end partial", human(left));
    Ok(set.len())
}
