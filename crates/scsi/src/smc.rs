//! SCSI media changer (SMC) commands: element addressing, inventory, moves.

use crate::{ascii_trim, be16, be24, mode, need, timeouts, DataDir, Error, Result, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementType {
    Transport = 1,
    Storage = 2,
    ImportExport = 3,
    DataTransfer = 4,
}

impl ElementType {
    fn from_code(c: u8) -> Option<Self> {
        Some(match c & 0x0f {
            1 => ElementType::Transport,
            2 => ElementType::Storage,
            3 => ElementType::ImportExport,
            4 => ElementType::DataTransfer,
            _ => return None,
        })
    }
}

/// Element Address Assignment mode page (1Dh): (first address, count) per element type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ElementAddresses {
    pub transport: (u16, u16),
    pub storage: (u16, u16),
    pub import_export: (u16, u16),
    pub data_transfer: (u16, u16),
}

pub fn element_addresses(t: &dyn Transport) -> Result<ElementAddresses> {
    let m = mode::mode_sense6(t, 0x1d, 0, true)?;
    let page = m.page(0x1d, 0).ok_or_else(|| Error::Parse("no element address page".into()))?;
    parse_element_addresses(page.body())
}

pub fn parse_element_addresses(b: &[u8]) -> Result<ElementAddresses> {
    need(b, 16, "element address assignment page")?;
    let pair = |o: usize| (be16(&b[o..o + 2]), be16(&b[o + 2..o + 4]));
    Ok(ElementAddresses { transport: pair(0), storage: pair(4), import_export: pair(8), data_transfer: pair(12) })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementStatus {
    pub kind: ElementType,
    pub address: u16,
    pub full: bool,
    pub except: bool,
    pub access: bool,
    pub import_export: bool,
    pub asc: u8,
    pub ascq: u8,
    /// Element the medium came from, when the changer knows it.
    pub source: Option<u16>,
    pub primary_tag: Option<String>,
    pub alternate_tag: Option<String>,
    /// Device identifier (DVCID) of a data transfer element, as reported.
    pub device_id: Option<String>,
}

impl ElementStatus {
    /// Best-effort drive serial number from the DVCID identifier. IBM libraries report a T10
    /// vendor identifier: 8-byte vendor, 16-byte product, then the serial.
    pub fn drive_serial(&self) -> Option<String> {
        let id = self.device_id.as_deref()?;
        if id.len() > 24 && id.is_char_boundary(24) {
            let tail = id[24..].trim();
            if !tail.is_empty() {
                return Some(tail.to_string());
            }
        }
        id.split_whitespace().last().map(str::to_string)
    }

    /// Product identification from a T10 vendor DVCID identifier (bytes 8..24).
    pub fn drive_product(&self) -> Option<String> {
        let id = self.device_id.as_deref()?;
        id.get(8..24).map(|p| p.trim().to_string()).filter(|p| !p.is_empty())
    }
}

pub fn read_element_status(
    t: &dyn Transport,
    kind: ElementType,
    start: u16,
    count: u16,
    dvcid: bool,
) -> Result<Vec<ElementStatus>> {
    let alloc = (16 + count as usize * 160).min(0xff_ffff);
    let mut buf = vec![0u8; alloc];
    let mut cdb = [0u8; 12];
    cdb[0] = 0xb8;
    cdb[1] = 0x10 | kind as u8; // VOLTAG
    cdb[2..4].copy_from_slice(&start.to_be_bytes());
    cdb[4..6].copy_from_slice(&count.to_be_bytes());
    cdb[6] = dvcid as u8;
    cdb[7..10].copy_from_slice(&(alloc as u32).to_be_bytes()[1..]);
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::INVENTORY)?;
    parse_element_status(&buf[..c.transferred])
}

pub fn parse_element_status(buf: &[u8]) -> Result<Vec<ElementStatus>> {
    need(buf, 8, "element status header")?;
    let end = (8 + be24(&buf[5..8]) as usize).min(buf.len());
    let mut out = Vec::new();
    let mut off = 8;
    while off + 8 <= end {
        let kind = ElementType::from_code(buf[off]);
        let pvoltag = buf[off + 1] & 0x80 != 0;
        let avoltag = buf[off + 1] & 0x40 != 0;
        let dlen = be16(&buf[off + 2..off + 4]) as usize;
        let page_bytes = be24(&buf[off + 5..off + 8]) as usize;
        let pstart = off + 8;
        let pend = (pstart + page_bytes).min(end);
        if let (Some(kind), true) = (kind, dlen >= 12) {
            let mut d = pstart;
            while d + dlen <= pend {
                out.push(parse_descriptor(kind, &buf[d..d + dlen], pvoltag, avoltag));
                d += dlen;
            }
        }
        off = pend;
    }
    Ok(out)
}

fn parse_descriptor(kind: ElementType, d: &[u8], pvoltag: bool, avoltag: bool) -> ElementStatus {
    let flags = d[2];
    let svalid = d[9] & 0x80 != 0;
    let tag = |o: usize| -> Option<String> {
        d.get(o..o + 32).map(ascii_trim).filter(|s| !s.is_empty())
    };
    let mut o = 12;
    let primary_tag = if pvoltag { let t = tag(o); o += 36; t } else { None };
    let alternate_tag = if avoltag { let t = tag(o); o += 36; t } else { None };
    let device_id = (d.len() >= o + 4)
        .then(|| {
            let len = d[o + 3] as usize;
            d.get(o + 4..o + 4 + len).map(ascii_trim).filter(|s| !s.is_empty())
        })
        .flatten();
    ElementStatus {
        kind,
        address: be16(&d[0..2]),
        full: flags & 0x01 != 0,
        import_export: flags & 0x02 != 0,
        except: flags & 0x04 != 0,
        access: flags & 0x08 != 0,
        asc: d[4],
        ascq: d[5],
        source: svalid.then(|| be16(&d[10..12])),
        primary_tag,
        alternate_tag,
        device_id,
    }
}

/// MOVE MEDIUM (A5h).
pub fn move_medium(t: &dyn Transport, transport: u16, src: u16, dst: u16) -> Result<()> {
    let mut cdb = [0u8; 12];
    cdb[0] = 0xa5;
    cdb[2..4].copy_from_slice(&transport.to_be_bytes());
    cdb[4..6].copy_from_slice(&src.to_be_bytes());
    cdb[6..8].copy_from_slice(&dst.to_be_bytes());
    t.exec(&cdb, DataDir::None, timeouts::MOVE)?;
    Ok(())
}

/// INITIALIZE ELEMENT STATUS (07h): re-scan the library inventory (barcodes).
pub fn initialize_element_status(t: &dyn Transport) -> Result<()> {
    t.exec(&[0x07, 0, 0, 0, 0, 0], DataDir::None, timeouts::INVENTORY)?;
    Ok(())
}

/// Encode an element status page (used by the simulator and tests).
pub fn encode_element_status(elements: &[ElementStatus]) -> Vec<u8> {
    let mut pages = Vec::new();
    let mut kinds: Vec<ElementType> = elements.iter().map(|e| e.kind).collect();
    kinds.dedup();
    for kind in kinds {
        let dvcid = kind == ElementType::DataTransfer;
        let dlen = 12 + 36 + if dvcid { 4 + 32 } else { 0 };
        let mut descs = Vec::new();
        for e in elements.iter().filter(|e| e.kind == kind) {
            let mut d = vec![0u8; dlen];
            d[0..2].copy_from_slice(&e.address.to_be_bytes());
            d[2] = e.full as u8 | (e.import_export as u8) << 1 | (e.except as u8) << 2 | (e.access as u8) << 3;
            d[4] = e.asc;
            d[5] = e.ascq;
            if let Some(s) = e.source {
                d[9] = 0x80;
                d[10..12].copy_from_slice(&s.to_be_bytes());
            }
            let mut tag = [b' '; 32];
            if let Some(t) = &e.primary_tag {
                let n = t.len().min(32);
                tag[..n].copy_from_slice(&t.as_bytes()[..n]);
            }
            d[12..44].copy_from_slice(&tag);
            if dvcid {
                d[48] = 0x02; // ASCII
                d[49] = 0x00;
                d[51] = 32;
                let mut id = [b' '; 32];
                if let Some(s) = &e.device_id {
                    let n = s.len().min(32);
                    id[..n].copy_from_slice(&s.as_bytes()[..n]);
                }
                d[52..84].copy_from_slice(&id);
            }
            descs.extend(d);
        }
        pages.push(kind as u8);
        pages.push(0x80); // PVOLTAG
        pages.extend_from_slice(&(dlen as u16).to_be_bytes());
        pages.push(0);
        pages.extend_from_slice(&(descs.len() as u32).to_be_bytes()[1..]);
        pages.extend(descs);
    }
    let mut out = vec![0u8; 8];
    out[0..2].copy_from_slice(&elements.first().map_or(0, |e| e.address).to_be_bytes());
    out[2..4].copy_from_slice(&(elements.len() as u16).to_be_bytes());
    out[5..8].copy_from_slice(&(pages.len() as u32).to_be_bytes()[1..]);
    out.extend(pages);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(kind: ElementType, address: u16, tag: Option<&str>, dev: Option<&str>) -> ElementStatus {
        ElementStatus {
            kind,
            address,
            full: tag.is_some(),
            except: false,
            access: true,
            import_export: false,
            asc: 0,
            ascq: 0,
            source: None,
            primary_tag: tag.map(str::to_string),
            alternate_tag: None,
            device_id: dev.map(str::to_string),
        }
    }

    #[test]
    fn roundtrip_inventory() {
        let els = vec![
            el(ElementType::Storage, 0x1000, Some("T0481SL8"), None),
            el(ElementType::Storage, 0x1001, None, None),
            el(ElementType::DataTransfer, 0x0100, None, Some("IBM     ULT3580-HH8     90WT0181")),
        ];
        let parsed = parse_element_status(&encode_element_status(&els)).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].primary_tag.as_deref(), Some("T0481SL8"));
        assert!(parsed[0].full && !parsed[1].full);
        assert_eq!(parsed[1].primary_tag, None);
        assert_eq!(parsed[2].drive_serial().as_deref(), Some("90WT0181"));
        assert_eq!(parsed[2].drive_product().as_deref(), Some("ULT3580-HH8"));
    }

    #[test]
    fn addresses() {
        let b = [0, 1, 0, 1, 0x10, 0, 0, 23, 0, 0x10, 0, 1, 1, 0, 0, 2];
        let a = parse_element_addresses(&b).unwrap();
        assert_eq!(a.storage, (0x1000, 23));
        assert_eq!(a.data_transfer, (0x0100, 2));
    }
}
