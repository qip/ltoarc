//! Sense data (fixed and descriptor formats) and ASC/ASCQ descriptions.

use std::fmt;

use crate::{be16, be32, be64};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenseKey {
    NoSense,
    RecoveredError,
    NotReady,
    MediumError,
    HardwareError,
    IllegalRequest,
    UnitAttention,
    DataProtect,
    BlankCheck,
    VendorSpecific,
    CopyAborted,
    AbortedCommand,
    VolumeOverflow,
    Miscompare,
    Completed,
    Reserved(u8),
}

impl From<u8> for SenseKey {
    fn from(v: u8) -> Self {
        match v & 0x0f {
            0x0 => SenseKey::NoSense,
            0x1 => SenseKey::RecoveredError,
            0x2 => SenseKey::NotReady,
            0x3 => SenseKey::MediumError,
            0x4 => SenseKey::HardwareError,
            0x5 => SenseKey::IllegalRequest,
            0x6 => SenseKey::UnitAttention,
            0x7 => SenseKey::DataProtect,
            0x8 => SenseKey::BlankCheck,
            0x9 => SenseKey::VendorSpecific,
            0xa => SenseKey::CopyAborted,
            0xb => SenseKey::AbortedCommand,
            0xd => SenseKey::VolumeOverflow,
            0xe => SenseKey::Miscompare,
            0xf => SenseKey::Completed,
            other => SenseKey::Reserved(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sense {
    pub key: SenseKey,
    pub asc: u8,
    pub ascq: u8,
    pub filemark: bool,
    pub eom: bool,
    pub ili: bool,
    /// INFORMATION field when valid (for READ: residue; for WRITE at EOM: unwritten count).
    pub information: Option<u64>,
    /// Progress indication (0..=65535) during long operations, when reported.
    pub progress: Option<u16>,
}

impl Sense {
    /// Parse fixed (70h/71h) or descriptor (72h/73h) format sense data.
    pub fn parse(buf: &[u8]) -> Option<Sense> {
        if buf.is_empty() {
            return None;
        }
        match buf[0] & 0x7f {
            0x70 | 0x71 => {
                if buf.len() < 3 {
                    return None;
                }
                let valid = buf[0] & 0x80 != 0;
                let key = SenseKey::from(buf[2]);
                let information = (valid && buf.len() >= 7).then(|| be32(&buf[3..7]) as u64);
                let asc = *buf.get(12).unwrap_or(&0);
                let ascq = *buf.get(13).unwrap_or(&0);
                let progress = (buf.len() >= 18
                    && buf[15] & 0x80 != 0
                    && matches!(key, SenseKey::NoSense | SenseKey::NotReady))
                .then(|| be16(&buf[16..18]));
                Some(Sense {
                    key,
                    asc,
                    ascq,
                    filemark: buf[2] & 0x80 != 0,
                    eom: buf[2] & 0x40 != 0,
                    ili: buf[2] & 0x20 != 0,
                    information,
                    progress,
                })
            }
            0x72 | 0x73 => {
                if buf.len() < 4 {
                    return None;
                }
                let mut s = Sense {
                    key: SenseKey::from(buf[1]),
                    asc: buf[2],
                    ascq: buf[3],
                    filemark: false,
                    eom: false,
                    ili: false,
                    information: None,
                    progress: None,
                };
                let total = (8 + *buf.get(7).unwrap_or(&0) as usize).min(buf.len());
                let mut off = 8;
                while off + 2 <= total {
                    let dtype = buf[off];
                    let dlen = buf[off + 1] as usize;
                    let d = &buf[off..(off + 2 + dlen).min(total)];
                    match dtype {
                        0x00 if d.len() >= 12 && d[2] & 0x80 != 0 => {
                            s.information = Some(be64(&d[4..12]))
                        }
                        0x02 if d.len() >= 7 && d[4] & 0x80 != 0 => {
                            s.progress = Some(be16(&d[5..7]))
                        }
                        0x04 if d.len() >= 4 => {
                            s.filemark = d[3] & 0x80 != 0;
                            s.eom = d[3] & 0x40 != 0;
                            s.ili = d[3] & 0x20 != 0;
                        }
                        _ => {}
                    }
                    off += 2 + dlen;
                }
                Some(s)
            }
            _ => None,
        }
    }

    pub fn code(&self) -> u16 {
        ((self.asc as u16) << 8) | self.ascq as u16
    }

    pub fn is_medium_not_present(&self) -> bool {
        self.key == SenseKey::NotReady && self.asc == 0x3a
    }

    pub fn is_becoming_ready(&self) -> bool {
        self.key == SenseKey::NotReady && self.asc == 0x04 && matches!(self.ascq, 0x01 | 0x07)
    }

    pub fn is_unit_attention(&self) -> bool {
        self.key == SenseKey::UnitAttention
    }

    pub fn is_end_of_data(&self) -> bool {
        self.key == SenseKey::BlankCheck
    }

    /// Early warning / end of partition reached (data was written when key is NO SENSE).
    pub fn is_early_warning(&self) -> bool {
        self.eom && self.key == SenseKey::NoSense
    }

    pub fn is_volume_overflow(&self) -> bool {
        self.key == SenseKey::VolumeOverflow
    }

    pub fn is_write_protected(&self) -> bool {
        self.key == SenseKey::DataProtect && self.asc == 0x27
    }

    pub fn description(&self) -> &'static str {
        describe(self.asc, self.ascq)
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:02X}/{:02X} ({})", self.key, self.asc, self.ascq, self.description())?;
        if self.filemark {
            f.write_str(" [FM]")?;
        }
        if self.eom {
            f.write_str(" [EOM]")?;
        }
        if self.ili {
            f.write_str(" [ILI]")?;
        }
        Ok(())
    }
}

/// Human-readable description of common ASC/ASCQ codes for tape drives and changers.
pub fn describe(asc: u8, ascq: u8) -> &'static str {
    match (asc, ascq) {
        (0x00, 0x00) => "no additional sense information",
        (0x00, 0x01) => "filemark detected",
        (0x00, 0x02) => "end-of-partition/medium detected",
        (0x00, 0x04) => "beginning-of-partition/medium detected",
        (0x00, 0x05) => "end-of-data detected",
        (0x00, 0x16) => "operation in progress",
        (0x00, 0x17) => "cleaning requested",
        (0x04, 0x00) => "not ready, cause not reportable",
        (0x04, 0x01) => "not ready, becoming ready",
        (0x04, 0x02) => "not ready, initializing command required",
        (0x04, 0x03) => "not ready, manual intervention required",
        (0x04, 0x04) => "not ready, format in progress",
        (0x04, 0x07) => "not ready, operation in progress",
        (0x04, 0x12) => "not ready, offline",
        (0x0c, 0x00) => "write error",
        (0x11, 0x00) => "unrecovered read error",
        (0x14, 0x00) => "recorded entity not found",
        (0x14, 0x01) => "record not found",
        (0x14, 0x03) => "end-of-data not found",
        (0x1a, 0x00) => "parameter list length error",
        (0x20, 0x00) => "invalid command operation code",
        (0x21, 0x01) => "invalid element address",
        (0x24, 0x00) => "invalid field in CDB",
        (0x25, 0x00) => "logical unit not supported",
        (0x26, 0x00) => "invalid field in parameter list",
        (0x27, 0x00) => "write protected",
        (0x28, 0x00) => "not ready to ready change, medium may have changed",
        (0x28, 0x01) => "import or export element accessed",
        (0x29, _) => "power on, reset, or bus device reset occurred",
        (0x2a, 0x01) => "mode parameters changed",
        (0x30, 0x00) => "incompatible medium installed",
        (0x30, 0x01) => "cannot read medium, unknown format",
        (0x30, 0x02) => "cannot read medium, incompatible format",
        (0x30, 0x03) => "cleaning cartridge installed",
        (0x30, 0x05) => "cannot write medium, incompatible format",
        (0x30, 0x07) => "cleaning failure",
        (0x30, 0x0c) => "WORM medium, overwrite attempted",
        (0x31, 0x00) => "medium format corrupted",
        (0x33, 0x00) => "tape length error",
        (0x3a, _) => "medium not present",
        (0x3b, 0x00) => "sequential positioning error",
        (0x3b, 0x0d) => "medium destination element full",
        (0x3b, 0x0e) => "medium source element empty",
        (0x3b, 0x11) => "medium magazine not accessible",
        (0x3b, 0x12) => "medium magazine removed",
        (0x3f, 0x01) => "microcode has been changed",
        (0x44, 0x00) => "internal target failure",
        (0x50, 0x00) => "write append error",
        (0x51, 0x00) => "erase failure",
        (0x52, 0x00) => "cartridge fault",
        (0x53, 0x00) => "media load or eject failed",
        (0x53, 0x02) => "medium removal prevented",
        (0x55, 0x06) => "auxiliary memory out of space",
        (0x5d, _) => "failure prediction threshold exceeded",
        (0x82, 0x82) => "drive requires cleaning",
        (0x82, 0x83) => "bad microcode detected",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_filemark() {
        let mut b = [0u8; 18];
        b[0] = 0xf0;
        b[2] = 0x80; // FILEMARK, NO SENSE
        b[3..7].copy_from_slice(&1048576u32.to_be_bytes());
        b[7] = 10;
        b[13] = 0x01;
        let s = Sense::parse(&b).unwrap();
        assert_eq!(s.key, SenseKey::NoSense);
        assert!(s.filemark);
        assert_eq!(s.information, Some(1048576));
        assert_eq!(s.code(), 0x0001);
    }

    #[test]
    fn fixed_not_present() {
        let mut b = [0u8; 18];
        b[0] = 0x70;
        b[2] = 0x02;
        b[7] = 10;
        b[12] = 0x3a;
        let s = Sense::parse(&b).unwrap();
        assert!(s.is_medium_not_present());
        assert_eq!(s.information, None);
    }

    #[test]
    fn fixed_progress() {
        let mut b = [0u8; 18];
        b[0] = 0x70;
        b[2] = 0x02;
        b[7] = 10;
        b[12] = 0x04;
        b[13] = 0x04;
        b[15] = 0x80;
        b[16..18].copy_from_slice(&0x8000u16.to_be_bytes());
        let s = Sense::parse(&b).unwrap();
        assert_eq!(s.progress, Some(0x8000));
    }

    #[test]
    fn descriptor_eom() {
        let b = [
            0x72, 0x00, 0x00, 0x02, 0, 0, 0, 16, // header: NO SENSE 00/02, 16 bytes of descriptors
            0x00, 0x0a, 0x80, 0x00, 0, 0, 0, 0, 0, 0, 0x10, 0x00, // information = 0x1000
            0x04, 0x02, 0x00, 0x40, // stream commands: EOM
        ];
        let s = Sense::parse(&b).unwrap();
        assert!(s.is_early_warning());
        assert_eq!(s.information, Some(0x1000));
    }
}
