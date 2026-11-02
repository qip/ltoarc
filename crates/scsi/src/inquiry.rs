//! INQUIRY: standard data and the unit serial number VPD page.

use crate::{ascii_trim, need, timeouts, DataDir, Result, Transport};

pub const PDT_SEQUENTIAL: u8 = 0x01;
pub const PDT_CHANGER: u8 = 0x08;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inquiry {
    pub peripheral_type: u8,
    pub removable: bool,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

pub fn inquiry(t: &dyn Transport) -> Result<Inquiry> {
    let mut buf = [0u8; 96];
    let cdb = [0x12, 0, 0, 0, buf.len() as u8, 0];
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_inquiry(&buf[..c.transferred])
}

pub fn parse_inquiry(buf: &[u8]) -> Result<Inquiry> {
    need(buf, 36, "INQUIRY")?;
    Ok(Inquiry {
        peripheral_type: buf[0] & 0x1f,
        removable: buf[1] & 0x80 != 0,
        vendor: ascii_trim(&buf[8..16]),
        product: ascii_trim(&buf[16..32]),
        revision: ascii_trim(&buf[32..36]),
    })
}

/// Unit serial number (VPD page 80h).
pub fn unit_serial(t: &dyn Transport) -> Result<String> {
    let mut buf = [0u8; 255];
    let cdb = [0x12, 0x01, 0x80, 0, buf.len() as u8, 0];
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_unit_serial(&buf[..c.transferred])
}

pub fn parse_unit_serial(buf: &[u8]) -> Result<String> {
    need(buf, 4, "VPD 80h")?;
    let len = (buf[3] as usize).min(buf.len() - 4);
    Ok(ascii_trim(&buf[4..4 + len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ibm_drive() {
        let mut b = vec![0u8; 36];
        b[0] = 0x01;
        b[1] = 0x80;
        b[8..16].copy_from_slice(b"IBM     ");
        b[16..32].copy_from_slice(b"ULT3580-HH8     ");
        b[32..36].copy_from_slice(b"J4D1");
        let i = parse_inquiry(&b).unwrap();
        assert_eq!(i.peripheral_type, PDT_SEQUENTIAL);
        assert_eq!(i.vendor, "IBM");
        assert_eq!(i.product, "ULT3580-HH8");
        assert_eq!(i.revision, "J4D1");
    }

    #[test]
    fn parse_serial() {
        let mut b = vec![0x01, 0x80, 0x00, 10];
        b.extend_from_slice(b"90WT018174");
        assert_eq!(parse_unit_serial(&b).unwrap(), "90WT018174");
    }
}
