//! Bringing a cartridge left with an open session (crash, power cut, cable pulled) back to a
//! consistent state (FORMAT.md §9): catalog whatever reached the tape, end the session's file
//! with a filemark, append a fresh volume catalog, and mark the cartridge clean.

use anyhow::Result;
use ltoarc_catalog::{Catalog, VolumeRow};
use ltoarc_device::{Position, ReadStatus, TapeDrive};

use crate::backup::{check_label, write_volume_catalog};
use crate::crypt::{self, Key};
use crate::scan;
use crate::tapeio::read_label;
use crate::vci::{self, Vci};

#[derive(Debug, Default)]
pub struct Recovery {
    /// "clean", "unknown" (no coherency record), "stale" (written by something else after the
    /// last catalog), "nothing written" (open session without data) or "recovered".
    pub state: String,
    /// Session that was open, and where its stream starts.
    pub session_start: Option<u64>,
    pub scan: scan::Report,
    /// The session part was already in the local catalog (the crash came after its commit).
    pub already_known: bool,
    pub catalog_object: Option<u64>,
}

/// Check the cartridge in `drive` and recover it if a session was left open on it. An
/// encrypted cartridge needs its key among `keys` (its new catalog is encrypted too).
pub fn recover_volume(cat: &Catalog, drive: &mut dyn TapeDrive, volume: &VolumeRow, keys: &[Key]) -> Result<Recovery> {
    let label = read_label(drive)?;
    check_label(&label, volume)?;
    let v = match vci::state(drive)? {
        vci::State::Open(v) => v,
        vci::State::Clean(_) => return Ok(Recovery { state: "clean".into(), ..Default::default() }),
        vci::State::Unknown => return Ok(Recovery { state: "unknown".into(), ..Default::default() }),
        vci::State::Stale(_) => return Ok(Recovery { state: "stale".into(), ..Default::default() }),
    };
    let open = v.open.clone().expect("open state has a session");
    let mut rec = Recovery { session_start: Some(open.start), ..Default::default() };
    let eod = drive.locate_eod(0)?.pos.object;
    if eod <= open.start {
        // Nothing reached the tape: the previous catalog is still the newest.
        let change_ref = vci::change_ref(drive).unwrap_or_default();
        vci::write(drive, &Vci { change_ref, open: None, ..v })?;
        rec.state = "nothing written".into();
        return Ok(rec);
    }
    let found = scan::scan_stream_at(cat, drive, volume, open.start, keys, &mut rec.scan)?;
    // The new catalog is encrypted like the cartridge's sessions.
    let key_id = cat
        .volume_encryption(volume.id)?
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .next()
        .map(|j| crypt::from_json(&j).map(|w| w.key_id))
        .transpose()?;
    let kek = match &key_id {
        Some(id) => Some(keys.iter().find(|k| &k.id() == id).ok_or_else(|| anyhow::anyhow!("{} is encrypted with key {id}, which was not given (--key FILE)", volume.barcode))?),
        None => None,
    };
    rec.already_known = found.is_none();
    // End the session's file, unless the crash came after its filemark (e.g. while the
    // catalog was being written; a cut catalog is simply superseded).
    let mut buf = vec![0u8; label.block_size as usize];
    drive.locate(Position { partition: 0, object: eod - 1 })?;
    let filemark_last = matches!(drive.read_block(&mut buf), Ok(ReadStatus::Filemark));
    let mut at = drive.locate_eod(0)?.pos.object;
    if !filemark_last {
        drive.write_filemarks(1)?;
        at += 1;
    }
    write_volume_catalog(cat, drive, volume, &label, open.session_id, at, kek)?;
    let part = match found {
        Some(sv) => Some(sv),
        None => cat.volume_sessions(volume.id)?.into_iter().find(|sv| sv.start_object == open.start as i64),
    };
    if let Some(mut sv) = part {
        sv.catalog_object = Some(at as i64);
        cat.add_session_volume(&sv)?;
    }
    vci::mark_clean(drive, label.volume_uuid, at)?;
    let mut vol = cat.volume(&volume.barcode)?.unwrap_or_else(|| volume.clone());
    if vol.state == "scratch" || vol.state == "new" {
        vol.state = "filling".into();
        cat.update_volume(&vol)?;
    }
    rec.catalog_object = Some(at);
    rec.state = "recovered".into();
    Ok(rec)
}
