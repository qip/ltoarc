//! Adapters between tape drives and block streams.

use anyhow::{bail, Context, Result};
use ltoarc_device::{Error as DevError, Position, ReadStatus, TapeDrive};
use ltoarc_format::stream::BlockSource;
use ltoarc_format::VolumeLabel;

/// Reads the blocks of one tape file (up to a filemark or end of data).
pub struct TapeBlocks<'a> {
    pub drive: &'a mut dyn TapeDrive,
    buf: Vec<u8>,
    pending: Option<Vec<u8>>,
    pub blocks: u64,
    pub hit_filemark: bool,
    pub hit_eod: bool,
}

impl<'a> TapeBlocks<'a> {
    pub fn new(drive: &'a mut dyn TapeDrive, block_size: usize) -> Self {
        TapeBlocks { drive, buf: vec![0u8; block_size], pending: None, blocks: 0, hit_filemark: false, hit_eod: false }
    }

    /// Hand back a block that was already read, to be returned first.
    pub fn with_pending(mut self, block: Vec<u8>) -> Self {
        self.pending = Some(block);
        self
    }

    pub fn locate(&mut self, pos: Position) -> Result<()> {
        self.hit_filemark = false;
        self.hit_eod = false;
        self.pending = None;
        self.drive.locate(pos).with_context(|| format!("locating to block {}", pos.object))
    }
}

impl BlockSource for TapeBlocks<'_> {
    fn next_block(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if let Some(b) = self.pending.take() {
            self.blocks += 1;
            return Ok(Some(b));
        }
        match self.drive.read_block(&mut self.buf) {
            Ok(ReadStatus::Data(n)) => {
                self.blocks += 1;
                Ok(Some(self.buf[..n].to_vec()))
            }
            Ok(ReadStatus::Filemark) => {
                self.hit_filemark = true;
                Ok(None)
            }
            Ok(ReadStatus::EndOfData) | Err(DevError::EndOfData) => {
                self.hit_eod = true;
                Ok(None)
            }
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    }
}

/// Read the ltoarc label at the beginning of partition 0.
pub fn read_label(drive: &mut dyn TapeDrive) -> Result<VolumeLabel> {
    drive.locate(Position { partition: 0, object: 0 }).context("rewinding to the label")?;
    let mut buf = vec![0u8; drive.max_block_size()?.min(8 << 20)];
    match drive.read_block(&mut buf) {
        Ok(ReadStatus::Data(n)) => VolumeLabel::from_block(&buf[..n]).context("reading the volume label"),
        Ok(ReadStatus::EndOfData) | Err(DevError::EndOfData) => bail!("cartridge is blank (run `ltoarc tape init`)"),
        Ok(ReadStatus::Filemark) => bail!("cartridge does not start with an ltoarc label"),
        Err(e) => Err(e).context("reading the volume label"),
    }
}

/// The volume UUID of the loaded cartridge read from the last block before its end of data
/// (the end of a volume catalog, so one of ltoarc's), and that end-of-data position, without
/// travelling back to the label: appending to a cartridge positioned anywhere but at its
/// start (left loaded, or just recovered) then costs no rewind. None when that block is not
/// ltoarc's (a cartridge with no session yet ends with its bootstrap file); the label
/// decides then. Leaves the tape at the end of data.
pub fn identify_at_end(drive: &mut dyn TapeDrive) -> Result<Option<(uuid::Uuid, u64)>> {
    let eod = drive.locate_eod(0).context("locating end of data")?.pos.object;
    if eod < 4 {
        return Ok(None);
    }
    // [... volume catalog][filemark] end of data
    drive.locate(Position { partition: 0, object: eod - 2 }).context("locating the last volume catalog block")?;
    let mut buf = vec![0u8; drive.max_block_size()?.min(8 << 20)];
    let found = match drive.read_block(&mut buf) {
        Ok(ReadStatus::Data(n)) => ltoarc_format::BlockHeader::decode(&buf[..n]).ok().filter(|h| h.kind == ltoarc_format::BlockKind::Catalog),
        _ => None,
    };
    let end = drive.locate_eod(0).context("locating end of data")?.pos.object;
    if end != eod {
        bail!("end of data moved from block {eod} to {end} while reading the last block");
    }
    Ok(found.map(|h| (h.volume_uuid, eod)))
}

/// What a cartridge holds, judged from its first block.
pub enum Content {
    Blank,
    Ltoarc(Box<VolumeLabel>),
    Foreign(String),
}

pub fn inspect(drive: &mut dyn TapeDrive) -> Result<Content> {
    drive.locate(Position { partition: 0, object: 0 })?;
    let mut buf = vec![0u8; drive.max_block_size()?.min(8 << 20)];
    Ok(match drive.read_block(&mut buf) {
        Ok(ReadStatus::EndOfData) | Err(DevError::EndOfData) => Content::Blank,
        Ok(ReadStatus::Data(n)) => match VolumeLabel::from_block(&buf[..n]) {
            Ok(l) => Content::Ltoarc(Box::new(l)),
            Err(_) => Content::Foreign(format!("first block of {n} bytes is not an ltoarc label")),
        },
        Ok(ReadStatus::Filemark) => Content::Foreign("starts with a filemark".into()),
        Err(e) => Content::Foreign(format!("first block unreadable: {e}")),
    })
}

/// Write `label` and the bootstrap file at the beginning of the tape (erasing everything after
/// them), plus the cartridge memory attributes and a clean coherency record (FORMAT.md §4, §5,
/// §8). Returns warnings for what could not be written.
pub fn write_label(drive: &mut dyn TapeDrive, label: &VolumeLabel) -> Result<Vec<String>> {
    use ltoarc_device::Attribute;
    use ltoarc_scsi::mam::id;
    drive.rewind()?;
    drive.write_block(&label.to_block())?;
    drive.write_filemarks(1)?;
    let mut warnings = Vec::new();
    if !crate::bootstrap::write(drive, label.block_size)? {
        warnings.push("the bootstrap file holds FORMAT.md and recovery notes but no ltoarc binary (not a release build)".into());
    }
    let pool = label.pool.as_deref().unwrap_or("");
    let attrs = vec![
        Attribute::ascii(id::APPLICATION_VENDOR, "LTOARC", 8),
        Attribute::ascii(id::APPLICATION_NAME, "ltoarc", 32),
        Attribute::ascii(id::APPLICATION_VERSION, env!("CARGO_PKG_VERSION"), 8),
        Attribute::text(id::USER_MEDIUM_TEXT_LABEL, &format!("{} {pool}", label.barcode), 160),
        Attribute::ascii(id::DATE_TIME_LAST_WRITTEN, &chrono::Utc::now().format("%Y%m%d%H%M").to_string(), 12),
        Attribute::binary(id::TEXT_LOCALIZATION_ID, vec![0x81]),
        Attribute::ascii(id::BARCODE, &label.barcode, 32),
        Attribute::text(id::MEDIA_POOL, pool, 160),
        Attribute::ascii(id::APPLICATION_FORMAT_VERSION, "ltoarc-1", 16),
        Attribute::binary(id::MEDIUM_GUID, label.volume_uuid.to_string().into_bytes()),
    ];
    if let Err(e) = drive.write_attributes(0, &attrs) {
        warnings.push(format!("could not write cartridge memory attributes: {e}"));
    }
    let vci = crate::vci::Vci {
        change_ref: crate::vci::change_ref(drive).unwrap_or_default(),
        count: 0,
        set_id: 0,
        volume: label.volume_uuid,
        open: None,
    };
    if let Err(e) = crate::vci::write(drive, &vci) {
        warnings.push(format!("could not write the volume coherency record: {e:#}"));
    }
    Ok(warnings)
}
