//! Volume Coherency Information (MAM 080Ch, FORMAT.md §8.2): where the newest volume catalog
//! is, and whether a session was left open on the cartridge.
//!
//! A session marks the volume open before writing its first block and clean after its volume
//! catalog, so a crash in between leaves the open mark (and the session's start) behind. The
//! drive changes the VOLUME CHANGE REFERENCE (MAM 0009h) whenever the tape is written, so a
//! clean mark whose reference no longer matches means something wrote after the catalog.

use anyhow::{bail, Result};
use ltoarc_device::{Attribute, TapeDrive};
use ltoarc_scsi::mam::id;
use uuid::Uuid;

const APP_MAGIC: &[u8; 6] = b"LTOARC";
const APP_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSession {
    /// Low 64 bits of the session UUID (as in block headers).
    pub session_id: u64,
    /// Logical object where the session stream starts.
    pub start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vci {
    /// VOLUME CHANGE REFERENCE when this was written.
    pub change_ref: Vec<u8>,
    /// Catalog generation (number of volume catalogs written).
    pub count: u64,
    /// Logical object of the first block of the newest catalog stream (0: none yet, the label).
    pub set_id: u64,
    pub volume: Uuid,
    pub open: Option<OpenSession>,
}

impl Vci {
    pub fn encode(&self) -> Vec<u8> {
        let mut app = Vec::with_capacity(40);
        app.extend_from_slice(APP_MAGIC);
        app.push(APP_VERSION);
        app.extend_from_slice(self.volume.as_bytes());
        let (state, sid, start) = match &self.open {
            Some(o) => (1u8, o.session_id, o.start),
            None => (0, 0, 0),
        };
        app.push(state);
        app.extend_from_slice(&sid.to_be_bytes());
        app.extend_from_slice(&start.to_be_bytes());
        let mut v = Vec::with_capacity(1 + self.change_ref.len() + 18 + app.len());
        v.push(self.change_ref.len() as u8);
        v.extend_from_slice(&self.change_ref);
        v.extend_from_slice(&self.count.to_be_bytes());
        v.extend_from_slice(&self.set_id.to_be_bytes());
        v.extend_from_slice(&(app.len() as u16).to_be_bytes());
        v.extend_from_slice(&app);
        v
    }

    pub fn decode(b: &[u8]) -> Result<Vci> {
        let n = *b.first().unwrap_or(&0) as usize;
        if b.len() < 1 + n + 18 {
            bail!("volume coherency information is {} bytes, too short", b.len());
        }
        let change_ref = b[1..1 + n].to_vec();
        let at = 1 + n;
        let count = u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
        let set_id = u64::from_be_bytes(b[at + 8..at + 16].try_into().unwrap());
        let app_len = u16::from_be_bytes([b[at + 16], b[at + 17]]) as usize;
        let app = b.get(at + 18..at + 18 + app_len).unwrap_or(&[]);
        if app.len() < 40 || &app[..6] != APP_MAGIC {
            bail!("volume coherency information was not written by ltoarc");
        }
        if app[6] != APP_VERSION {
            bail!("volume coherency information version {} is newer than this ltoarc", app[6]);
        }
        let volume = Uuid::from_bytes(app[7..23].try_into().unwrap());
        let open = (app[23] == 1).then(|| OpenSession {
            session_id: u64::from_be_bytes(app[24..32].try_into().unwrap()),
            start: u64::from_be_bytes(app[32..40].try_into().unwrap()),
        });
        Ok(Vci { change_ref, count, set_id, volume, open })
    }
}

/// The cartridge's VOLUME CHANGE REFERENCE, if the drive reports one.
pub fn change_ref(drive: &mut dyn TapeDrive) -> Option<Vec<u8>> {
    drive.read_attribute(0, id::VOLUME_CHANGE_REFERENCE).ok().flatten().map(|a| a.value)
}

/// The ltoarc coherency record, if any (None for cartridges written before it existed, or
/// drives without cartridge memory access). Partition 0 is used for single-partition volumes.
pub fn read(drive: &mut dyn TapeDrive) -> Result<Option<Vci>> {
    match drive.read_attribute(0, id::VOLUME_COHERENCY_INFO) {
        Ok(Some(a)) if !a.value.is_empty() => Vci::decode(&a.value).map(Some),
        _ => Ok(None),
    }
}

pub fn write(drive: &mut dyn TapeDrive, vci: &Vci) -> Result<()> {
    drive.write_attributes(0, &[Attribute::binary(id::VOLUME_COHERENCY_INFO, vci.encode())])?;
    Ok(())
}

/// What the coherency record says about a cartridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// No ltoarc record (older cartridge): trust the local catalog.
    Unknown,
    /// The newest catalog is current.
    Clean(Vci),
    /// A session was opened and never closed (crash, power loss, cable pulled).
    Open(Vci),
    /// Something wrote to the tape after the newest catalog without ltoarc knowing.
    Stale(Vci),
}

pub fn state(drive: &mut dyn TapeDrive) -> Result<State> {
    let Some(v) = read(drive)? else { return Ok(State::Unknown) };
    if v.open.is_some() {
        return Ok(State::Open(v));
    }
    match change_ref(drive) {
        Some(cur) if cur != v.change_ref => Ok(State::Stale(v)),
        _ => Ok(State::Clean(v)),
    }
}

/// Mark a session open before its first block is written.
pub fn mark_open(drive: &mut dyn TapeDrive, volume: Uuid, session_id: u64, start: u64) -> Result<()> {
    let prev = read(drive).ok().flatten();
    let vci = Vci {
        change_ref: change_ref(drive).unwrap_or_default(),
        count: prev.as_ref().map_or(0, |p| p.count),
        set_id: prev.as_ref().map_or(0, |p| p.set_id),
        volume,
        open: Some(OpenSession { session_id, start }),
    };
    write(drive, &vci)
}

/// Mark the volume clean once a catalog has been written at `catalog_object` (all writes
/// done and synchronised, so the change reference is final).
pub fn mark_clean(drive: &mut dyn TapeDrive, volume: Uuid, catalog_object: u64) -> Result<()> {
    let prev = read(drive).ok().flatten();
    let vci = Vci {
        change_ref: change_ref(drive).unwrap_or_default(),
        count: prev.map_or(0, |p| p.count) + 1,
        set_id: catalog_object,
        volume,
        open: None,
    };
    write(drive, &vci)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let v = Vci {
            change_ref: vec![0, 0, 1, 2],
            count: 7,
            set_id: 12345,
            volume: Uuid::new_v4(),
            open: Some(OpenSession { session_id: 99, start: 777 }),
        };
        assert_eq!(Vci::decode(&v.encode()).unwrap(), v);
        let c = Vci { open: None, ..v };
        assert_eq!(Vci::decode(&c.encode()).unwrap(), c);
        // Layout per SSC: length, reference, count, set id, application info length.
        let e = c.encode();
        assert_eq!(e[0], 4);
        assert_eq!(&e[5..13], &7u64.to_be_bytes());
        assert_eq!(u16::from_be_bytes([e[21], e[22]]), 40);
        assert_eq!(&e[23..29], b"LTOARC");
    }
}
