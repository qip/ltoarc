//! Tape drive and media changer abstraction: real hardware over SCSI generic, and a
//! file-backed simulator with the same behaviour for testing.

use std::time::{Duration, Instant};

pub mod generation;
pub mod m8;
pub mod real;
pub mod sim;
pub mod stats;

pub use generation::{drive_access, expand_barcode_range, parse_barcode, Access, BarcodeInfo, Generation, MediaKind};
pub use ltoarc_scsi::density::DensityDescriptor;
pub use ltoarc_scsi::log::{LogPage, LogParam};
pub use ltoarc_scsi::mam::Attribute;
pub use ltoarc_scsi::smc::ElementType;
pub use ltoarc_scsi::tape::{ReadStatus, WriteStatus};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Scsi(#[from] ltoarc_scsi::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("no medium in drive")]
    NoMedium,
    #[error("drive not ready: {0}")]
    NotReady(String),
    #[error("medium is write protected")]
    WriteProtected,
    #[error("end of medium reached; block not written")]
    EndOfMedium,
    #[error("end of data reached")]
    EndOfData,
    #[error("{drive} cannot {op} {media} media")]
    Incompatible { drive: Generation, media: Generation, op: &'static str },
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Real,
    Sim,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: String,
    pub generation: Option<Generation>,
    pub backend: Backend,
    /// How to reach the drive again: "/dev/sg1" for real drives, "<world dir>" for simulated.
    pub location: String,
    pub st_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangerInfo {
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub serial: String,
    pub backend: Backend,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    NoMedium,
    Becoming,
    NotReady(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaInfo {
    /// Density code the medium is formatted with; 0 when the drive reports none (blank media).
    pub density: u8,
    pub generation: Option<Generation>,
    pub medium_type: u8,
    pub write_protected: bool,
    /// 0 in variable-block mode.
    pub block_length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Position {
    pub partition: u8,
    pub object: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PositionInfo {
    pub pos: Position,
    /// Filemarks between the beginning of the partition and the position.
    pub file: u64,
    pub bop: bool,
    pub eop: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionLayout {
    Single,
    /// Index partition 0 of `p0_mb` megabytes, the rest in partition 1.
    Two { p0_mb: u32 },
}

pub trait TapeDrive: Send {
    fn info(&self) -> &DriveInfo;
    fn readiness(&mut self) -> Result<Readiness>;
    fn load(&mut self) -> Result<()>;
    /// Rewind and unthread so a changer can take the cartridge.
    fn unload(&mut self) -> Result<()>;
    fn media_info(&mut self) -> Result<MediaInfo>;
    /// Largest block this drive can read or write through this host (drive and HBA limits).
    fn max_block_size(&mut self) -> Result<usize>;
    fn report_density(&mut self, media: bool) -> Result<Vec<DensityDescriptor>>;

    fn position(&mut self) -> Result<PositionInfo>;
    fn locate(&mut self, pos: Position) -> Result<()>;
    fn locate_eod(&mut self, partition: u8) -> Result<PositionInfo>;
    fn rewind(&mut self) -> Result<()>;
    fn space_filemarks(&mut self, count: i32) -> Result<()>;

    fn write_block(&mut self, data: &[u8]) -> Result<WriteStatus>;
    fn write_filemarks(&mut self, count: u32) -> Result<WriteStatus>;
    /// Reads one block into `buf` (which must be at least as large as the block).
    fn read_block(&mut self, buf: &mut [u8]) -> Result<ReadStatus>;

    fn read_attributes(&mut self, partition: u8) -> Result<Vec<Attribute>>;
    fn write_attributes(&mut self, partition: u8, attrs: &[Attribute]) -> Result<()>;
    fn supported_log_pages(&mut self) -> Result<Vec<u8>>;
    fn log_page(&mut self, page: u8, subpage: u8) -> Result<LogPage>;

    /// Whether the drive compresses data (hardware compression).
    fn compression(&mut self) -> Result<bool>;
    fn set_compression(&mut self, on: bool) -> Result<()>;

    /// Destroys all data on the cartridge.
    fn format(&mut self, layout: PartitionLayout) -> Result<()>;
    /// Preferred density for Type M eligible cartridges of `medium_type` (IBM MP 30h[44h]).
    fn preferred_density(&mut self, medium_type: u8) -> Result<u8>;
    fn set_preferred_density(&mut self, medium_type: u8, density: u8) -> Result<()>;

    /// Poll until the drive is ready, the medium is missing, or `timeout` passes.
    fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            match self.readiness()? {
                Readiness::Ready => return Ok(()),
                Readiness::NoMedium => return Err(Error::NoMedium),
                Readiness::Becoming if start.elapsed() < timeout => {}
                Readiness::NotReady(_) if start.elapsed() < timeout => {}
                Readiness::Becoming => return Err(Error::NotReady("timed out becoming ready".into())),
                Readiness::NotReady(why) => return Err(Error::NotReady(why)),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn read_attribute(&mut self, partition: u8, id: u16) -> Result<Option<Attribute>> {
        Ok(self.read_attributes(partition)?.into_iter().find(|a| a.id == id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Element {
    pub kind: ElementType,
    pub address: u16,
    pub full: bool,
    pub barcode: Option<String>,
    pub source: Option<u16>,
    /// For drive bays: serial of the drive installed there, when the changer reports it.
    pub drive_serial: Option<String>,
    /// For drive bays: product id of the installed drive (e.g. "ULT3580-HH6").
    pub drive_product: Option<String>,
    /// Exception condition (ASC, ASCQ) reported for the element.
    pub exception: Option<(u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Inventory {
    pub elements: Vec<Element>,
}

impl Inventory {
    pub fn of(&self, kind: ElementType) -> impl Iterator<Item = &Element> {
        self.elements.iter().filter(move |e| e.kind == kind)
    }

    pub fn find_barcode(&self, barcode: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.full && e.barcode.as_deref() == Some(barcode))
    }

    pub fn drive_bay(&self, serial: &str) -> Option<&Element> {
        self.of(ElementType::DataTransfer).find(|e| e.drive_serial.as_deref().is_some_and(|s| serial_matches(s, serial)))
    }

    pub fn first_empty_slot(&self) -> Option<&Element> {
        self.of(ElementType::Storage).find(|e| !e.full && e.exception.is_none())
    }

    pub fn element(&self, address: u16) -> Option<&Element> {
        self.elements.iter().find(|e| e.address == address)
    }
}

/// Serial numbers from different sources (INQUIRY, DVCID, MAM) may differ in padding/prefix.
pub fn serial_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && !b.is_empty() && (a == b || a.ends_with(b) || b.ends_with(a))
}

pub trait Changer: Send {
    fn info(&self) -> &ChangerInfo;
    fn inventory(&mut self) -> Result<Inventory>;
    fn move_medium(&mut self, src: u16, dst: u16) -> Result<()>;
    /// Re-scan all elements (barcode inventory). Moves the robot.
    fn rescan(&mut self) -> Result<()>;
}
