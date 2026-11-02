//! REPORT DENSITY SUPPORT (44h) and LTO density codes.

use crate::{ascii_trim, be16, be24, be32, need, timeouts, DataDir, Result, Transport};

pub const LTO3: u8 = 0x44;
pub const LTO4: u8 = 0x46;
pub const LTO5: u8 = 0x58;
pub const LTO6: u8 = 0x5a;
pub const LTO7: u8 = 0x5c;
/// LTO-7 cartridge initialized as Type M ("M8", 9 TB) in an LTO-8 drive.
pub const LTO7_M8: u8 = 0x5d;
pub const LTO8: u8 = 0x5e;
pub const LTO9: u8 = 0x60;

/// MEDIUM TYPE values reported in the mode parameter header by IBM LTO drives.
pub mod medium_type {
    pub const LTO5_DATA: u8 = 0x58;
    pub const LTO5_WORM: u8 = 0x5c;
    pub const LTO6_DATA: u8 = 0x68;
    pub const LTO6_WORM: u8 = 0x6c;
    pub const LTO7_DATA: u8 = 0x78;
    pub const LTO7_WORM: u8 = 0x7c;
    pub const LTO8_DATA: u8 = 0x88;
    pub const LTO8_WORM: u8 = 0x8c;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DensityDescriptor {
    pub primary: u8,
    pub secondary: u8,
    pub write_ok: bool,
    pub duplicate: bool,
    pub default: bool,
    pub bits_per_mm: u32,
    pub media_width: u16,
    pub tracks: u16,
    /// Capacity in megabytes (10^6 bytes).
    pub capacity_mb: u32,
    pub assigning_org: String,
    pub name: String,
    pub description: String,
}

/// With `media = true` the drive reports densities usable with the mounted cartridge;
/// otherwise all densities the drive supports.
pub fn report_density_support(t: &dyn Transport, media: bool) -> Result<Vec<DensityDescriptor>> {
    let mut buf = vec![0u8; 4 + 52 * 16];
    let len = buf.len() as u16;
    let mut cdb = [0u8; 10];
    cdb[0] = 0x44;
    cdb[1] = media as u8;
    cdb[7..9].copy_from_slice(&len.to_be_bytes());
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_density_support(&buf[..c.transferred])
}

pub fn parse_density_support(buf: &[u8]) -> Result<Vec<DensityDescriptor>> {
    need(buf, 4, "REPORT DENSITY SUPPORT")?;
    let avail = be16(&buf[0..2]) as usize + 2;
    let end = avail.min(buf.len());
    let mut out = Vec::new();
    let mut off = 4;
    while off + 52 <= end {
        let d = &buf[off..off + 52];
        out.push(DensityDescriptor {
            primary: d[0],
            secondary: d[1],
            write_ok: d[2] & 0x80 != 0,
            duplicate: d[2] & 0x40 != 0,
            default: d[2] & 0x20 != 0,
            bits_per_mm: be24(&d[5..8]),
            media_width: be16(&d[8..10]),
            tracks: be16(&d[10..12]),
            capacity_mb: be32(&d[12..16]),
            assigning_org: ascii_trim(&d[16..24]),
            name: ascii_trim(&d[24..32]),
            description: ascii_trim(&d[32..52]),
        });
        off += 52;
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn encode_descriptor(code: u8, write_ok: bool, default: bool, cap_mb: u32, name: &str) -> Vec<u8> {
    let mut d = vec![0u8; 52];
    d[0] = code;
    d[1] = code;
    d[2] = (write_ok as u8) << 7 | (default as u8) << 5;
    d[12..16].copy_from_slice(&cap_mb.to_be_bytes());
    d[16..24].copy_from_slice(b"LTO-CVE ");
    let mut n = [b' '; 8];
    n[..name.len()].copy_from_slice(name.as_bytes());
    d[24..32].copy_from_slice(&n);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m8_eligible_two_descriptors() {
        let mut b = vec![0u8; 4];
        b.extend(encode_descriptor(LTO7, true, true, 6_000_000, "U-732"));
        b.extend(encode_descriptor(LTO7_M8, true, false, 9_000_000, "U-832M"));
        let len = (b.len() - 2) as u16;
        b[0..2].copy_from_slice(&len.to_be_bytes());
        let d = parse_density_support(&b).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].primary, LTO7);
        assert!(d[0].default && d[0].write_ok);
        assert_eq!(d[1].primary, LTO7_M8);
        assert!(!d[1].default);
        assert_eq!(d[1].capacity_mb, 9_000_000);
        assert_eq!(d[1].name, "U-832M");
    }
}
