//! Creating LTO-7 Type M ("M8", 9 TB) cartridges in IBM LTO-8 drives.
//!
//! Per the IBM LTO SCSI Reference (GA32-0928-06, Annex E.2 and MP 30h[44h]): a new, never
//! written LTO-7 cartridge becomes M8 on its first write or FORMAT MEDIUM when the drive's
//! preferred density for Ultrium 7 media is 5Dh. While the cartridge is still eligible,
//! REPORT DENSITY SUPPORT with MEDIA=1 lists both 5Ch (LTO-7) and 5Dh (M8). Conversion cannot be
//! undone, and M8 cartridges are readable only by LTO-8 drives.

use ltoarc_scsi::density::{medium_type, LTO7, LTO7_M8};
use ltoarc_scsi::mam;

use crate::{Error, Generation, PartitionLayout, Result, TapeDrive};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M8Status {
    Eligible,
    AlreadyM8,
    NotEligible(String),
}

pub fn m8_status(drive: &mut dyn TapeDrive) -> Result<M8Status> {
    if drive.info().generation != Some(Generation::Lto8) {
        return Ok(M8Status::NotEligible("M8 cartridges can only be created by an LTO-8 drive".into()));
    }
    let densities = drive.report_density(true)?;
    let codes: Vec<u8> = densities.iter().map(|d| d.primary).collect();
    let writable = |c: u8| densities.iter().any(|d| d.primary == c && d.write_ok);
    Ok(if codes == [LTO7_M8] {
        M8Status::AlreadyM8
    } else if writable(LTO7) && writable(LTO7_M8) {
        M8Status::Eligible
    } else {
        M8Status::NotEligible(format!(
            "cartridge reports densities {codes:02x?}; only new, never-written LTO-7 cartridges can become M8"
        ))
    })
}

/// Convert the loaded cartridge to M8. Destroys nothing (eligible cartridges are blank) but
/// is irreversible. The drive's preferred-density setting is restored afterwards.
pub fn format_m8(drive: &mut dyn TapeDrive) -> Result<()> {
    match m8_status(drive)? {
        M8Status::Eligible => {}
        M8Status::AlreadyM8 => return Ok(()),
        M8Status::NotEligible(why) => return Err(Error::Invalid(why)),
    }
    if let Some(mib) = drive.read_attribute(0, mam::id::TOTAL_MB_WRITTEN)?.and_then(|a| a.as_u64()) {
        if mib > 0 {
            return Err(Error::Invalid(format!("cartridge memory shows {mib} MiB written in its life; refusing M8")));
        }
    }
    let previous = drive.preferred_density(medium_type::LTO7_DATA)?;
    drive.set_preferred_density(medium_type::LTO7_DATA, LTO7_M8)?;
    let formatted = drive.format(PartitionLayout::Single);
    let restored = drive.set_preferred_density(medium_type::LTO7_DATA, previous);
    formatted?;
    restored?;
    match m8_status(drive)? {
        M8Status::AlreadyM8 => Ok(()),
        other => Err(Error::Invalid(format!(
            "cartridge did not become M8 ({other:?}); the library may be overriding the preferred cartridge type"
        ))),
    }
}
