//! SCSI command layer for LTO tape drives (SSC) and media changers (SMC).
//!
//! Commands are encoded as CDBs and sent through a [`Transport`]. The real transport is the
//! Linux `SG_IO` ioctl ([`sg::SgDevice`]). Response parsers are plain functions over byte
//! slices so they can be tested against fixtures without hardware.

use std::time::Duration;

pub mod density;
pub mod inquiry;
pub mod log;
pub mod mam;
pub mod mode;
pub mod sense;
pub mod sg;
pub mod smc;
pub mod tape;

pub use sense::{Sense, SenseKey};

/// Direction and buffer of the data phase of a command.
pub enum DataDir<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

/// Result of a command that completed (GOOD status, or CHECK CONDITION with a sense key of
/// NO SENSE or RECOVERED ERROR). `sense` carries filemark / EOM / ILI indications.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Bytes actually transferred (requested length minus residual).
    pub transferred: usize,
    pub sense: Option<Sense>,
}

/// Anything that can execute a SCSI CDB.
pub trait Transport {
    fn exec(&self, cdb: &[u8], data: DataDir<'_>, timeout: Duration) -> Result<Completion>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("check condition: {0}")]
    Check(Sense),
    #[error("SCSI status {0:#04x}")]
    Status(u8),
    #[error("transport error (host status {host:#06x}, driver status {driver:#06x})")]
    Transport { host: u16, driver: u16 },
    #[error("malformed response: {0}")]
    Parse(String),
}

impl Error {
    pub fn sense(&self) -> Option<&Sense> {
        match self {
            Error::Check(s) => Some(s),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Command timeouts. LTO loads can include calibration (LTO-9 media optimization takes up to
/// 2 h); locates and rewinds cross the full tape length.
pub mod timeouts {
    use std::time::Duration;
    pub const SHORT: Duration = Duration::from_secs(60);
    pub const IO: Duration = Duration::from_secs(15 * 60);
    pub const POSITIONING: Duration = Duration::from_secs(30 * 60);
    pub const LOAD: Duration = Duration::from_secs(3 * 60 * 60);
    pub const FORMAT: Duration = Duration::from_secs(4 * 60 * 60);
    pub const MOVE: Duration = Duration::from_secs(15 * 60);
    pub const INVENTORY: Duration = Duration::from_secs(2 * 60 * 60);
}

pub(crate) fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

pub(crate) fn be24(b: &[u8]) -> u32 {
    u32::from_be_bytes([0, b[0], b[1], b[2]])
}

pub(crate) fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Big-endian unsigned integer of 1..=8 bytes.
pub fn be_uint(b: &[u8]) -> Option<u64> {
    if b.is_empty() || b.len() > 8 {
        return None;
    }
    Some(b.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64))
}

/// ASCII field with trailing spaces and NULs removed; non-printable bytes become '?'.
pub fn ascii_trim(b: &[u8]) -> String {
    let s: String = b
        .iter()
        .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else if c == 0 { '\0' } else { '?' })
        .collect();
    s.trim_end_matches(['\0', ' ']).trim_start().to_string()
}

pub(crate) fn need(buf: &[u8], n: usize, what: &str) -> Result<()> {
    if buf.len() < n {
        Err(Error::Parse(format!("{what}: need {n} bytes, got {}", buf.len())))
    } else {
        Ok(())
    }
}
