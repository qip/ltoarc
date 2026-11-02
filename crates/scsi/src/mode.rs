//! MODE SENSE / MODE SELECT (6 and 10 byte forms).

use crate::{be16, be24, need, timeouts, DataDir, Result, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockDescriptor {
    pub density: u8,
    pub blocks: u32,
    /// 0 means variable-block mode.
    pub block_length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModeData {
    pub medium_type: u8,
    pub device_specific: u8,
    pub blocks: Vec<BlockDescriptor>,
    /// Raw mode pages following the block descriptors.
    pub pages: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModePage {
    pub code: u8,
    pub subpage: u8,
    /// Whole page including its header.
    pub data: Vec<u8>,
}

impl ModePage {
    /// Offset of the page body (after the 2- or 4-byte page header).
    pub fn body_offset(&self) -> usize {
        if self.data[0] & 0x40 != 0 { 4 } else { 2 }
    }

    pub fn body(&self) -> &[u8] {
        &self.data[self.body_offset()..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        let off = self.body_offset();
        &mut self.data[off..]
    }
}

impl ModeData {
    /// WP bit of the device-specific parameter (sequential-access devices).
    pub fn write_protected(&self) -> bool {
        self.device_specific & 0x80 != 0
    }

    pub fn parsed_pages(&self) -> Vec<ModePage> {
        let mut out = Vec::new();
        let p = &self.pages;
        let mut off = 0;
        while off + 2 <= p.len() {
            let spf = p[off] & 0x40 != 0;
            let code = p[off] & 0x3f;
            let (subpage, total) = if spf {
                if off + 4 > p.len() {
                    break;
                }
                (p[off + 1], 4 + be16(&p[off + 2..off + 4]) as usize)
            } else {
                (0, 2 + p[off + 1] as usize)
            };
            let end = (off + total).min(p.len());
            out.push(ModePage { code, subpage, data: p[off..end].to_vec() });
            off += total;
        }
        out
    }

    pub fn page(&self, code: u8, subpage: u8) -> Option<ModePage> {
        self.parsed_pages().into_iter().find(|p| p.code == code && p.subpage == subpage)
    }
}

pub fn mode_sense10(t: &dyn Transport, page: u8, subpage: u8, dbd: bool) -> Result<ModeData> {
    let mut buf = vec![0u8; 0x2000];
    let mut cdb = [0u8; 10];
    cdb[0] = 0x5a;
    cdb[1] = (dbd as u8) << 3;
    cdb[2] = page & 0x3f;
    cdb[3] = subpage;
    cdb[7..9].copy_from_slice(&(buf.len() as u16).to_be_bytes());
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_mode10(&buf[..c.transferred])
}

pub fn mode_sense6(t: &dyn Transport, page: u8, subpage: u8, dbd: bool) -> Result<ModeData> {
    let mut buf = vec![0u8; 255];
    let cdb = [0x1a, (dbd as u8) << 3, page & 0x3f, subpage, 255, 0];
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_mode6(&buf[..c.transferred])
}

/// MODE SELECT(10) with PF=1. `pages` must have the PS bit cleared.
pub fn mode_select10(t: &dyn Transport, data: &ModeData, save: bool) -> Result<()> {
    let buf = encode_mode10(data);
    let mut cdb = [0u8; 10];
    cdb[0] = 0x55;
    cdb[1] = 0x10 | save as u8;
    cdb[7..9].copy_from_slice(&(buf.len() as u16).to_be_bytes());
    t.exec(&cdb, DataDir::Out(&buf), timeouts::SHORT)?;
    Ok(())
}

pub fn parse_mode10(buf: &[u8]) -> Result<ModeData> {
    need(buf, 8, "MODE SENSE(10)")?;
    let total = (be16(&buf[0..2]) as usize + 2).min(buf.len());
    let bd_len = be16(&buf[6..8]) as usize;
    parse_common(buf, total, buf[2], buf[3], 8, bd_len)
}

pub fn parse_mode6(buf: &[u8]) -> Result<ModeData> {
    need(buf, 4, "MODE SENSE(6)")?;
    let total = (buf[0] as usize + 1).min(buf.len());
    parse_common(buf, total, buf[1], buf[2], 4, buf[3] as usize)
}

fn parse_common(buf: &[u8], total: usize, mt: u8, ds: u8, hdr: usize, bd_len: usize) -> Result<ModeData> {
    let bd_end = (hdr + bd_len).min(total);
    let blocks = buf[hdr..bd_end]
        .chunks_exact(8)
        .map(|d| BlockDescriptor { density: d[0], blocks: be24(&d[1..4]), block_length: be24(&d[5..8]) })
        .collect();
    Ok(ModeData { medium_type: mt, device_specific: ds, blocks, pages: buf[bd_end..total].to_vec() })
}

pub fn encode_mode10(data: &ModeData) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    // Mode data length and medium type are reserved on MODE SELECT. For tape drives the
    // device-specific byte is not: bits 6-4 are BUFFERED MODE (0 = unbuffered, which makes
    // every WRITE wait for the tape), bits 3-0 SPEED. The WP bit is ignored.
    out[3] = data.device_specific & 0x7f;
    out[6..8].copy_from_slice(&((data.blocks.len() * 8) as u16).to_be_bytes());
    for b in &data.blocks {
        out.push(b.density);
        out.extend_from_slice(&b.blocks.to_be_bytes()[1..]);
        out.push(0);
        out.extend_from_slice(&b.block_length.to_be_bytes()[1..]);
    }
    out.extend_from_slice(&data.pages);
    out
}

/// Device-specific byte for MODE SELECT: buffered mode 1, default speed.
pub const BUFFERED: u8 = 0x10;

/// Put the drive in variable-block mode, keeping the current density.
pub fn set_variable_block(t: &dyn Transport) -> Result<()> {
    let cur = mode_sense10(t, 0x10, 0, false)?;
    let density = cur.blocks.first().map(|b| b.density).unwrap_or(0);
    if cur.blocks.first().is_some_and(|b| b.block_length == 0) {
        return Ok(());
    }
    let sel = ModeData {
        device_specific: BUFFERED,
        blocks: vec![BlockDescriptor { density, blocks: 0, block_length: 0 }],
        ..Default::default()
    };
    mode_select10(t, &sel, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_block_descriptor_and_subpage() {
        let mut b = vec![0u8; 8];
        b[2] = 0x88; // LTO-8 data
        b[3] = 0x10; // buffered mode 1
        b[7] = 8;
        b.extend_from_slice(&[0x5e, 0, 0, 0, 0, 0, 0, 0]);
        b.extend_from_slice(&[0x70, 0x44, 0x00, 0x04, 0x78, 0x5d, 0, 0]); // MP 30h[44h], PS set
        let len = (b.len() - 2) as u16;
        b[0..2].copy_from_slice(&len.to_be_bytes());
        let m = parse_mode10(&b).unwrap();
        assert_eq!(m.medium_type, 0x88);
        assert!(!m.write_protected());
        assert_eq!(m.blocks[0].density, 0x5e);
        assert_eq!(m.blocks[0].block_length, 0);
        let p = m.page(0x30, 0x44).unwrap();
        assert_eq!(p.body(), &[0x78, 0x5d, 0, 0]);
    }

    #[test]
    fn encode_select() {
        let d = ModeData {
            device_specific: BUFFERED | 0x80,
            blocks: vec![BlockDescriptor { density: 0x5c, blocks: 0, block_length: 0 }],
            pages: vec![0x30 | 0x40, 0x44, 0x00, 0x04, 0x78, 0x5d, 0, 0],
            ..Default::default()
        };
        let enc = encode_mode10(&d);
        assert_eq!(enc[3], BUFFERED, "buffered mode kept, WP dropped");
        assert_eq!(&enc[6..8], &[0, 8]);
        assert_eq!(enc[8], 0x5c);
        let back = parse_mode10(&{
            let mut e = enc.clone();
            let len = (e.len() - 2) as u16;
            e[0..2].copy_from_slice(&len.to_be_bytes());
            e
        })
        .unwrap();
        assert_eq!(back.blocks, d.blocks);
        assert_eq!(back.pages, d.pages);
    }
}
