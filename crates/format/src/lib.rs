//! On-tape structures. `docs/FORMAT.md` is the normative description.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod record;
pub mod stream;

pub const MAGIC: &[u8; 6] = b"LTOARC";
pub const HEADER_VERSION: u16 = 1;
pub const HEADER_LEN: usize = 64;
pub const FORMAT_VERSION: u32 = 1;
pub const DEFAULT_BLOCK_SIZE: u32 = 1 << 20;
pub const NO_RECORD: u32 = u32::MAX;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("not an ltoarc block (bad magic)")]
    BadMagic,
    #[error("unsupported block header version {0}")]
    HeaderVersion(u16),
    #[error("block header checksum mismatch")]
    HeaderCrc,
    #[error("block payload checksum mismatch")]
    PayloadCrc,
    #[error("block truncated: {0}")]
    Truncated(String),
    #[error("unknown block kind {0}")]
    Kind(u16),
    #[error("label: {0}")]
    Label(String),
    #[error("record: {0}")]
    Record(String),
    #[error("I/O: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u16)]
pub enum BlockKind {
    Label = 1,
    Data = 2,
    Catalog = 3,
}

impl BlockKind {
    fn from_u16(v: u16) -> Result<Self> {
        match v {
            1 => Ok(BlockKind::Label),
            2 => Ok(BlockKind::Data),
            3 => Ok(BlockKind::Catalog),
            other => Err(Error::Kind(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// Bytes written to tape for this block (header + payload + padding).
    pub block_size: u32,
    pub payload_len: u32,
    pub volume_uuid: Uuid,
    pub session_id: u64,
    pub block_seq: u64,
    pub first_record_offset: u32,
    pub kind: BlockKind,
    pub flags: u16,
    pub payload_crc: u32,
}

impl BlockHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..6].copy_from_slice(MAGIC);
        b[6..8].copy_from_slice(&HEADER_VERSION.to_le_bytes());
        b[8..12].copy_from_slice(&self.block_size.to_le_bytes());
        b[12..16].copy_from_slice(&self.payload_len.to_le_bytes());
        b[16..32].copy_from_slice(self.volume_uuid.as_bytes());
        b[32..40].copy_from_slice(&self.session_id.to_le_bytes());
        b[40..48].copy_from_slice(&self.block_seq.to_le_bytes());
        b[48..52].copy_from_slice(&self.first_record_offset.to_le_bytes());
        b[52..54].copy_from_slice(&(self.kind as u16).to_le_bytes());
        b[54..56].copy_from_slice(&self.flags.to_le_bytes());
        b[56..60].copy_from_slice(&self.payload_crc.to_le_bytes());
        let crc = crc32c::crc32c(&b[0..60]);
        b[60..64].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < HEADER_LEN {
            return Err(Error::Truncated(format!("{} bytes, header needs {HEADER_LEN}", b.len())));
        }
        if &b[0..6] != MAGIC {
            return Err(Error::BadMagic);
        }
        let le16 = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let le32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let le64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let version = le16(6);
        if version != HEADER_VERSION {
            return Err(Error::HeaderVersion(version));
        }
        if crc32c::crc32c(&b[0..60]) != le32(60) {
            return Err(Error::HeaderCrc);
        }
        Ok(BlockHeader {
            block_size: le32(8),
            payload_len: le32(12),
            volume_uuid: Uuid::from_bytes(b[16..32].try_into().unwrap()),
            session_id: le64(32),
            block_seq: le64(40),
            first_record_offset: le32(48),
            kind: BlockKind::from_u16(le16(52))?,
            flags: le16(54),
            payload_crc: le32(56),
        })
    }
}

/// Assemble a block. With `pad_to = Some(size)` the block is zero-padded to `size` bytes;
/// otherwise it is written short (the last block of a stream).
pub fn build_block(mut header: BlockHeader, payload: &[u8], pad_to: Option<u32>) -> Vec<u8> {
    header.payload_len = payload.len() as u32;
    header.payload_crc = crc32c::crc32c(payload);
    let len = HEADER_LEN + payload.len();
    header.block_size = pad_to.map_or(len as u32, |p| p.max(len as u32));
    let mut out = Vec::with_capacity(header.block_size as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out.resize(header.block_size as usize, 0);
    out
}

/// Split a block read from tape into its header and verified payload.
pub fn parse_block(b: &[u8]) -> Result<(BlockHeader, &[u8])> {
    let h = BlockHeader::decode(b)?;
    let end = HEADER_LEN + h.payload_len as usize;
    if b.len() < end {
        return Err(Error::Truncated(format!("payload needs {end} bytes, block has {}", b.len())));
    }
    let payload = &b[HEADER_LEN..end];
    if crc32c::crc32c(payload) != h.payload_crc {
        return Err(Error::PayloadCrc);
    }
    Ok((h, payload))
}

/// First block of every partition ltoarc writes (FORMAT.md section 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeLabel {
    pub format: String,
    pub format_version: u32,
    pub volume_uuid: Uuid,
    pub barcode: String,
    pub generation: String,
    pub block_size: u32,
    pub partitions: u8,
    #[serde(default)]
    pub pool: Option<String>,
    pub created: String,
    pub created_by: String,
    #[serde(default)]
    pub encryption: Option<serde_json::Value>,
    /// Fields written by newer versions are preserved and ignored.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl VolumeLabel {
    pub fn new(barcode: &str, generation: &str, pool: Option<String>, created: String) -> Self {
        VolumeLabel {
            format: "ltoarc".into(),
            format_version: FORMAT_VERSION,
            volume_uuid: Uuid::new_v4(),
            barcode: barcode.into(),
            generation: generation.into(),
            block_size: DEFAULT_BLOCK_SIZE,
            partitions: 1,
            pool,
            created,
            created_by: concat!("ltoarc ", env!("CARGO_PKG_VERSION")).into(),
            encryption: None,
            extra: BTreeMap::new(),
        }
    }

    pub fn to_block(&self) -> Vec<u8> {
        let payload = serde_json::to_vec_pretty(self).expect("label serializes");
        let header = BlockHeader {
            block_size: 0,
            payload_len: 0,
            volume_uuid: self.volume_uuid,
            session_id: 0,
            block_seq: 0,
            first_record_offset: NO_RECORD,
            kind: BlockKind::Label,
            flags: 0,
            payload_crc: 0,
        };
        build_block(header, &payload, None)
    }

    pub fn from_block(b: &[u8]) -> Result<Self> {
        let (h, payload) = parse_block(b)?;
        if h.kind != BlockKind::Label {
            return Err(Error::Label(format!("block kind is {:?}, not a label", h.kind)));
        }
        let label: VolumeLabel = serde_json::from_slice(payload).map_err(|e| Error::Label(e.to_string()))?;
        if label.format != "ltoarc" {
            return Err(Error::Label(format!("format is '{}'", label.format)));
        }
        if label.format_version > FORMAT_VERSION {
            return Err(Error::Label(format!(
                "format version {} is newer than this ltoarc ({FORMAT_VERSION})",
                label.format_version
            )));
        }
        if label.volume_uuid != h.volume_uuid {
            return Err(Error::Label("volume UUID in label and block header differ".into()));
        }
        Ok(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_roundtrip() {
        let l = VolumeLabel::new("DC8001L8", "LTO-8", Some("dashcam".into()), "2026-10-30T13:05:00Z".into());
        let block = l.to_block();
        assert_eq!(&block[0..6], MAGIC);
        assert!(block.len() < 4096, "label block is written short");
        assert_eq!(VolumeLabel::from_block(&block).unwrap(), l);
    }

    #[test]
    fn corruption_detected() {
        let l = VolumeLabel::new("X", "LTO-8", None, "t".into());
        let mut block = l.to_block();
        let n = block.len();
        block[n - 2] ^= 1;
        assert_eq!(VolumeLabel::from_block(&block), Err(Error::PayloadCrc));
        block[n - 2] ^= 1;
        block[20] ^= 1;
        assert_eq!(VolumeLabel::from_block(&block), Err(Error::HeaderCrc));
        assert_eq!(VolumeLabel::from_block(b"tar archive......................................................."), Err(Error::BadMagic));
    }

    #[test]
    fn padded_block() {
        let h = BlockHeader {
            block_size: 0,
            payload_len: 0,
            volume_uuid: Uuid::nil(),
            session_id: 7,
            block_seq: 3,
            first_record_offset: 0,
            kind: BlockKind::Data,
            flags: 0,
            payload_crc: 0,
        };
        let b = build_block(h, b"hello", Some(4096));
        assert_eq!(b.len(), 4096);
        let (h2, p) = parse_block(&b).unwrap();
        assert_eq!((h2.session_id, h2.block_seq, h2.block_size), (7, 3, 4096));
        assert_eq!(p, b"hello");
    }

    #[test]
    fn unknown_label_fields_preserved() {
        let l = VolumeLabel::new("X", "LTO-8", None, "t".into());
        let mut v: serde_json::Value = serde_json::to_value(&l).unwrap();
        v["future_field"] = serde_json::json!(42);
        let back: VolumeLabel = serde_json::from_value(v).unwrap();
        assert_eq!(back.extra["future_field"], serde_json::json!(42));
    }
}
