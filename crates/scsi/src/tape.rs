//! Sequential-access (SSC) commands: readiness, load/unload, positioning, data transfer.

use crate::{be32, be64, need, timeouts, DataDir, Error, Result, Sense, Transport};

pub fn test_unit_ready(t: &dyn Transport) -> Result<()> {
    t.exec(&[0x00, 0, 0, 0, 0, 0], DataDir::None, timeouts::SHORT)?;
    Ok(())
}

pub fn request_sense(t: &dyn Transport) -> Result<Option<Sense>> {
    let mut buf = [0u8; 96];
    let c = t.exec(&[0x03, 0, 0, 0, buf.len() as u8, 0], DataDir::In(&mut buf), timeouts::SHORT)?;
    Ok(Sense::parse(&buf[..c.transferred]))
}

/// LOAD UNLOAD (1Bh). `hold` loads to the point where MAM is accessible without threading.
pub fn load_unload(t: &dyn Transport, load: bool, hold: bool, immed: bool) -> Result<()> {
    let cdb = [0x1b, immed as u8, 0, 0, (hold as u8) << 3 | load as u8, 0];
    t.exec(&cdb, DataDir::None, timeouts::LOAD)?;
    Ok(())
}

pub fn rewind(t: &dyn Transport) -> Result<()> {
    t.exec(&[0x01, 0, 0, 0, 0, 0], DataDir::None, timeouts::POSITIONING)?;
    Ok(())
}

/// PREVENT ALLOW MEDIUM REMOVAL (1Eh).
pub fn prevent_removal(t: &dyn Transport, prevent: bool) -> Result<()> {
    t.exec(&[0x1e, 0, 0, 0, prevent as u8, 0], DataDir::None, timeouts::SHORT)?;
    Ok(())
}

/// FORMAT MEDIUM (04h). `format` 0 = default format, 1 = partition per mode page 11h.
pub fn format_medium(t: &dyn Transport, format: u8) -> Result<()> {
    let cdb = [0x04, 0, format & 0x0f, 0, 0, 0];
    t.exec(&cdb, DataDir::None, timeouts::FORMAT)?;
    Ok(())
}

/// READ BLOCK LIMITS (05h): (maximum, minimum) block length in bytes.
pub fn read_block_limits(t: &dyn Transport) -> Result<(u32, u16)> {
    let mut buf = [0u8; 6];
    let c = t.exec(&[0x05, 0, 0, 0, 0, 0], DataDir::In(&mut buf), timeouts::SHORT)?;
    need(&buf[..c.transferred], 6, "READ BLOCK LIMITS")?;
    Ok((crate::be24(&buf[1..4]), crate::be16(&buf[4..6])))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionLong {
    pub bop: bool,
    pub eop: bool,
    /// Between programmable early warning and end of partition.
    pub bpew: bool,
    pub partition: u32,
    /// Logical object number (blocks and filemarks both count).
    pub object: u64,
    /// Logical file identifier (number of filemarks before this position).
    pub file: u64,
}

/// READ POSITION (34h), long form (service action 06h).
pub fn read_position(t: &dyn Transport) -> Result<PositionLong> {
    let mut buf = [0u8; 32];
    let mut cdb = [0u8; 10];
    cdb[0] = 0x34;
    cdb[1] = 0x06;
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_position_long(&buf[..c.transferred])
}

pub fn parse_position_long(b: &[u8]) -> Result<PositionLong> {
    need(b, 32, "READ POSITION long form")?;
    Ok(PositionLong {
        bop: b[0] & 0x80 != 0,
        eop: b[0] & 0x40 != 0,
        bpew: b[0] & 0x01 != 0,
        partition: be32(&b[4..8]),
        object: be64(&b[8..16]),
        file: be64(&b[16..24]),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocateTarget {
    Object(u64),
    File(u64),
    EndOfData,
}

/// LOCATE(16) (92h). `partition` changes partition when `Some`.
pub fn locate16(t: &dyn Transport, partition: Option<u8>, target: LocateTarget) -> Result<()> {
    let mut cdb = [0u8; 16];
    cdb[0] = 0x92;
    let (dest, id) = match target {
        LocateTarget::Object(n) => (0u8, n),
        LocateTarget::File(n) => (1, n),
        LocateTarget::EndOfData => (3, 0),
    };
    cdb[1] = dest << 3 | if partition.is_some() { 0x02 } else { 0 };
    cdb[3] = partition.unwrap_or(0);
    cdb[4..12].copy_from_slice(&id.to_be_bytes());
    t.exec(&cdb, DataDir::None, timeouts::POSITIONING)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceCode {
    Blocks = 0,
    Filemarks = 1,
    EndOfData = 3,
}

/// SPACE(6) (11h); negative counts move backwards.
pub fn space6(t: &dyn Transport, code: SpaceCode, count: i32) -> Result<()> {
    let c = (count as u32).to_be_bytes();
    let cdb = [0x11, code as u8, c[1], c[2], c[3], 0];
    t.exec(&cdb, DataDir::None, timeouts::POSITIONING)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    Ok,
    /// Written, but the early-warning point has been passed.
    EarlyWarning,
}

/// WRITE FILEMARKS(6) (10h).
pub fn write_filemarks(t: &dyn Transport, count: u32) -> Result<WriteStatus> {
    let c = count.to_be_bytes();
    let cdb = [0x10, 0, c[1], c[2], c[3], 0];
    let done = t.exec(&cdb, DataDir::None, timeouts::IO)?;
    Ok(write_status(done.sense.as_ref()))
}

/// WRITE(6) (0Ah) of one variable-length block.
pub fn write6(t: &dyn Transport, data: &[u8]) -> Result<WriteStatus> {
    if data.len() > 0xff_ffff {
        return Err(Error::Parse(format!("block of {} bytes is too large", data.len())));
    }
    let n = (data.len() as u32).to_be_bytes();
    let cdb = [0x0a, 0, n[1], n[2], n[3], 0];
    let done = t.exec(&cdb, DataDir::Out(data), timeouts::IO)?;
    Ok(write_status(done.sense.as_ref()))
}

fn write_status(sense: Option<&Sense>) -> WriteStatus {
    if sense.is_some_and(|s| s.eom) { WriteStatus::EarlyWarning } else { WriteStatus::Ok }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStatus {
    Data(usize),
    Filemark,
    EndOfData,
}

/// READ(6) (08h) of one variable-length block with SILI set (short blocks are not errors).
/// A block longer than `buf` is an error, reported with the drive's sense data.
pub fn read6(t: &dyn Transport, buf: &mut [u8]) -> Result<ReadStatus> {
    let n = (buf.len().min(0xff_ffff) as u32).to_be_bytes();
    let cdb = [0x08, 0x02, n[1], n[2], n[3], 0];
    match t.exec(&cdb, DataDir::In(buf), timeouts::IO) {
        Ok(c) => match c.sense {
            Some(s) if s.filemark => Ok(ReadStatus::Filemark),
            Some(s) if s.ili && s.information.is_some_and(|i| (i as i32) < 0) => Err(Error::Check(s)),
            _ => Ok(ReadStatus::Data(c.transferred)),
        },
        Err(Error::Check(s)) if s.is_end_of_data() => Ok(ReadStatus::EndOfData),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_long() {
        let mut b = [0u8; 32];
        b[0] = 0x80;
        b[4..8].copy_from_slice(&1u32.to_be_bytes());
        b[8..16].copy_from_slice(&123456u64.to_be_bytes());
        b[16..24].copy_from_slice(&3u64.to_be_bytes());
        let p = parse_position_long(&b).unwrap();
        assert!(p.bop && !p.eop);
        assert_eq!((p.partition, p.object, p.file), (1, 123456, 3));
    }
}
