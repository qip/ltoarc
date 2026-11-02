//! Drive and cartridge wear statistics, collected from log pages and cartridge memory into
//! flat key/value maps (stored as time series in the catalog).

use std::collections::BTreeMap;

use ltoarc_scsi::log::{self, LogPage};
use ltoarc_scsi::mam::{self, id};
use serde_json::{json, Value};

use crate::{Result, TapeDrive};

pub type StatMap = BTreeMap<String, Value>;

fn add_page(map: &mut StatMap, page: &LogPage) {
    for p in &page.params {
        let Some(name) = log::param_name(page.page, p.code) else { continue };
        let v = if p.is_ascii() || p.value.len() > 8 {
            json!(p.as_ascii())
        } else {
            match p.as_u64() {
                Some(n) => json!(n),
                None => continue,
            }
        };
        map.insert(name.key.to_string(), v);
    }
}

fn read_pages(drive: &mut dyn TapeDrive, pages: &[u8], map: &mut StatMap) -> Result<()> {
    let supported = drive.supported_log_pages().unwrap_or_default();
    for &pg in pages {
        if !supported.is_empty() && !supported.contains(&pg) {
            continue;
        }
        match drive.log_page(pg, 0) {
            Ok(page) => add_page(map, &page),
            Err(e) => {
                map.insert(format!("error_page_{pg:02x}"), json!(e.to_string()));
            }
        }
    }
    Ok(())
}

/// Lifetime counters of the drive itself (head hours, loads, cleanings, errors, alerts).
pub fn drive_stats(drive: &mut dyn TapeDrive) -> Result<StatMap> {
    let mut map = StatMap::new();
    map.insert("firmware".into(), json!(drive.info().revision));
    read_pages(drive, &[log::PAGE_SEQUENTIAL_ACCESS, log::PAGE_DEVICE_STATISTICS], &mut map)?;
    if let Ok(ta) = drive.log_page(log::PAGE_TAPEALERT, 0) {
        let flags: Vec<Value> = log::tapealert_flags(&ta)
            .into_iter()
            .map(|f| json!({"flag": f, "name": log::tapealert_name(f)}))
            .collect();
        map.insert("tapealerts".into(), Value::Array(flags));
    }
    Ok(map)
}

/// Statistics of the loaded cartridge: cartridge memory plus volume statistics pages.
pub fn volume_stats(drive: &mut dyn TapeDrive) -> Result<StatMap> {
    let mut map = StatMap::new();
    let attrs = drive.read_attributes(0)?;
    let get = |i: u16| attrs.iter().find(|a| a.id == i);
    for (key, i) in [
        ("load_count", id::LOAD_COUNT),
        ("initialization_count", id::INITIALIZATION_COUNT),
        ("mib_written_life", id::TOTAL_MB_WRITTEN),
        ("mib_read_life", id::TOTAL_MB_READ),
        ("mib_written_last_load", id::LAST_LOAD_MB_WRITTEN),
        ("mib_read_last_load", id::LAST_LOAD_MB_READ),
        ("remaining_mib", id::REMAINING_CAPACITY),
        ("maximum_mib", id::MAXIMUM_CAPACITY),
        ("medium_length_m", id::MEDIUM_LENGTH),
        ("formatted_density", id::FORMAT_DENSITY_CODE),
        ("medium_density", id::MEDIUM_DENSITY_CODE),
    ] {
        if let Some(v) = get(i).and_then(|a| a.as_u64()) {
            map.insert(key.into(), json!(v));
        }
    }
    for (key, i) in [
        ("medium_manufacturer", id::MEDIUM_MANUFACTURER),
        ("medium_serial", id::MEDIUM_SERIAL),
        ("manufacture_date", id::MEDIUM_MANUFACTURE_DATE),
        ("barcode_mam", id::BARCODE),
    ] {
        if let Some(a) = get(i) {
            map.insert(key.into(), json!(a.as_string()));
        }
    }
    let last_drives: Vec<Value> = (0..4)
        .filter_map(|k| get(id::DEVICE_AT_LAST_LOAD + k))
        .filter_map(|a| mam::parse_device_at_load(&a.value))
        .map(|(vendor, serial)| json!(format!("{vendor} {serial}")))
        .collect();
    map.insert("last_drives".into(), Value::Array(last_drives));
    read_pages(drive, &[log::PAGE_VOLUME_STATISTICS, log::PAGE_TAPE_CAPACITY], &mut map)?;
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{DriveSpec, LibrarySpec, SimWorld, TapeSpec, WorldSpec, ADDR_DRIVE};
    use crate::{Changer, Generation, MediaKind};

    #[test]
    fn collects_from_simulator() {
        let dir = tempfile::tempdir().unwrap();
        let w = SimWorld::create(
            &dir.path().join("w"),
            &WorldSpec {
                libraries: vec![LibrarySpec {
                    name: "l".into(),
                    slots: 2,
                    ie_slots: 0,
                    drives: vec![DriveSpec::new(Generation::Lto8).serial("D")],
                    tapes: vec![TapeSpec {
                        barcode: Some("X00001L8".into()),
                        cartridge: Generation::Lto8,
                        kind: MediaKind::Data,
                        capacity: Some(32 << 20),
                        slot: None,
                    }],
                }],
                standalone: vec![],
            },
        )
        .unwrap();
        let mut c = w.changers().unwrap().remove(0);
        c.move_medium(crate::sim::ADDR_SLOT, ADDR_DRIVE).unwrap();
        let mut d = w.drive("D").unwrap();
        d.write_block(&vec![1u8; 2 << 20]).unwrap();
        let ds = drive_stats(&mut d).unwrap();
        assert_eq!(ds["lifetime_volume_loads"], json!(1));
        assert_eq!(ds["drive_serial"], json!("D"));
        let vs = volume_stats(&mut d).unwrap();
        assert_eq!(vs["load_count"], json!(1));
        assert_eq!(vs["mib_written_life"], json!(2));
        assert_eq!(vs["last_drives"], json!(["IBM D"]));
        assert!(vs.contains_key("p0_remaining_mb"));
    }
}
