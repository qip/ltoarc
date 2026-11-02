//! Records carried in a block stream (FORMAT.md section 6).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use uuid::Uuid;

use crate::{Error, Result};

pub const RECORD_MAGIC: &[u8; 4] = b"LREC";
pub const RECORD_HEADER_LEN: usize = 24;

pub mod rtype {
    pub const SESSION_BEGIN: u16 = 1;
    pub const FILE: u16 = 2;
    pub const CHUNK: u16 = 3;
    pub const PACK: u16 = 4;
    pub const STREAM: u16 = 5;
    pub const FILE_END: u16 = 6;
    pub const WHITEOUT: u16 = 7;
    pub const REF: u16 = 8;
    /// The session continues on the next volume of the set.
    pub const VOLUME_END: u16 = 14;
    pub const SESSION_END: u16 = 15;
    pub const PAD: u16 = 16;
    pub const CATALOG: u16 = 32;
}

pub mod flags {
    pub const COMPRESSED: u16 = 1;
    pub const ENCRYPTED: u16 = 2;
    /// FILE record repeated at the start of a continuation volume for the file whose data
    /// carries on there (its earlier chunks are on the previous volume).
    pub const CONTINUED: u16 = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub rtype: u16,
    pub flags: u16,
    pub body_len: u64,
    pub seq: u32,
}

impl RecordHeader {
    pub fn encode(&self) -> [u8; RECORD_HEADER_LEN] {
        let mut b = [0u8; RECORD_HEADER_LEN];
        b[0..4].copy_from_slice(RECORD_MAGIC);
        b[4..6].copy_from_slice(&self.rtype.to_le_bytes());
        b[6..8].copy_from_slice(&self.flags.to_le_bytes());
        b[8..16].copy_from_slice(&self.body_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.seq.to_le_bytes());
        let crc = crc32c::crc32c(&b[0..20]);
        b[20..24].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < RECORD_HEADER_LEN {
            return Err(Error::Truncated("record header".into()));
        }
        if &b[0..4] != RECORD_MAGIC {
            return Err(Error::Record("bad record magic".into()));
        }
        if crc32c::crc32c(&b[0..20]) != u32::from_le_bytes(b[20..24].try_into().unwrap()) {
            return Err(Error::Record("record header checksum mismatch".into()));
        }
        Ok(RecordHeader {
            rtype: u16::from_le_bytes([b[4], b[5]]),
            flags: u16::from_le_bytes([b[6], b[7]]),
            body_len: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            seq: u32::from_le_bytes(b[16..20].try_into().unwrap()),
        })
    }
}

pub fn to_cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).expect("CBOR encoding into memory cannot fail");
    out
}

pub fn from_cbor<T: for<'de> Deserialize<'de>>(b: &[u8]) -> Result<T> {
    ciborium::from_reader(b).map_err(|e| Error::Record(format!("CBOR: {e}")))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBegin {
    pub session: Uuid,
    pub volume: Uuid,
    pub job: Option<String>,
    pub level: String,
    pub started: i64,
    pub host: String,
    pub tool: String,
    pub sources: Vec<ByteBuf>,
    pub copy: u8,
    /// Position of this volume within its copy of the session (1 for the copy's first tape).
    pub volume_seq: u32,
    pub parent: Option<Uuid>,
    /// The session's data key, wrapped with the user's key, when its records are encrypted.
    #[serde(default)]
    pub encryption: Option<WrappedKey>,
}

/// A data key encrypted with a user's key (FORMAT.md §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    pub cipher: String,
    /// Public id of the user's key that wraps it.
    pub key_id: String,
    /// nonce, encrypted data key, tag.
    pub wrapped: ByteBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Hardlink,
    Fifo,
    Char,
    Block,
    Socket,
    /// The path was deleted since the session this one builds on (incremental and
    /// differential sessions); views from this session on no longer show it.
    Whiteout,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Dir => "dir",
            FileKind::Symlink => "symlink",
            FileKind::Hardlink => "hardlink",
            FileKind::Fifo => "fifo",
            FileKind::Char => "char",
            FileKind::Block => "block",
            FileKind::Socket => "socket",
            FileKind::Whiteout => "whiteout",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "file" => FileKind::File,
            "dir" => FileKind::Dir,
            "symlink" => FileKind::Symlink,
            "hardlink" => FileKind::Hardlink,
            "fifo" => FileKind::Fifo,
            "char" => FileKind::Char,
            "block" => FileKind::Block,
            "socket" => FileKind::Socket,
            "whiteout" => FileKind::Whiteout,
            _ => return None,
        })
    }
}

/// FILE record body: everything needed to recreate an entry except its data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    /// Object number within the session; CHUNK records refer to it.
    pub id: u64,
    pub path: ByteBuf,
    pub kind: FileKind,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub user: Option<String>,
    pub group: Option<String>,
    pub size: u64,
    pub mtime_ns: i64,
    pub atime_ns: i64,
    pub ctime_ns: i64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub rdev: u64,
    /// Symlink target, or for a hard link the path of the entry it links to.
    pub target: Option<ByteBuf>,
    pub xattrs: Vec<(ByteBuf, ByteBuf)>,
    /// Data extents (offset, length) of a sparse file; None means one extent [0, size).
    pub extents: Option<Vec<(u64, u64)>>,
    /// Fingerprints known before the data was read (profile name -> hex digest).
    #[serde(default)]
    pub fingerprints: BTreeMap<String, String>,
    /// The file's data is not repeated: it is that of a file archived earlier (a moved or
    /// duplicate file, found by its content hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<SameAs>,
}

/// Where the data of a file stored by reference lies (FORMAT.md §6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SameAs {
    /// Session that stored the data, and the file's id within it.
    pub session: Uuid,
    pub id: u64,
    /// Content digest both files share: (algorithm, hex digest).
    pub algo: String,
    pub digest: String,
}

pub const CHUNK_HEADER_LEN: usize = 72;

pub mod codec {
    pub const STORED: u8 = 0;
    pub const ZSTD: u8 = 1;
}

/// Fixed binary header at the start of a CHUNK (or STREAM) record body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub object: u64,
    pub seq: u32,
    pub codec: u8,
    pub flags: u8,
    /// Offset of this chunk's data within the file.
    pub offset: u64,
    pub raw_len: u64,
    pub stored_len: u64,
    /// BLAKE3 of the raw (uncompressed, unencrypted) data.
    pub blake3: [u8; 32],
}

impl ChunkHeader {
    pub fn encode(&self) -> [u8; CHUNK_HEADER_LEN] {
        let mut b = [0u8; CHUNK_HEADER_LEN];
        b[0..8].copy_from_slice(&self.object.to_le_bytes());
        b[8..12].copy_from_slice(&self.seq.to_le_bytes());
        b[12] = self.codec;
        b[13] = self.flags;
        b[16..24].copy_from_slice(&self.offset.to_le_bytes());
        b[24..32].copy_from_slice(&self.raw_len.to_le_bytes());
        b[32..40].copy_from_slice(&self.stored_len.to_le_bytes());
        b[40..72].copy_from_slice(&self.blake3);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() < CHUNK_HEADER_LEN {
            return Err(Error::Truncated("chunk header".into()));
        }
        let le64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        Ok(ChunkHeader {
            object: le64(0),
            seq: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            codec: b[12],
            flags: b[13],
            offset: le64(16),
            raw_len: le64(24),
            stored_len: le64(32),
            blake3: b[40..72].try_into().unwrap(),
        })
    }
}

/// One file inside a PACK record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackMember {
    /// FILE record id.
    pub object: u64,
    /// Offset of the file's bytes within the raw (decoded) pack.
    pub offset: u64,
    pub len: u64,
    #[serde(with = "serde_bytes")]
    pub blake3: Vec<u8>,
}

/// PACK body: 72-byte header (object 0, raw/stored lengths, BLAKE3 of the raw pack),
/// u32 LE length of the CBOR member table, the table, then the stored bytes.
pub fn encode_pack(header: &ChunkHeader, members: &[PackMember], stored: &[u8]) -> Vec<u8> {
    let table = to_cbor(&members);
    let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + 4 + table.len() + stored.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(stored);
    out
}

/// Split a PACK body into header, members and stored bytes.
pub fn decode_pack(body: &[u8]) -> Result<(ChunkHeader, Vec<PackMember>, &[u8])> {
    let h = ChunkHeader::decode(body)?;
    let rest = &body[CHUNK_HEADER_LEN..];
    if rest.len() < 4 {
        return Err(Error::Truncated("pack member table length".into()));
    }
    let tlen = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
    if rest.len() < 4 + tlen {
        return Err(Error::Truncated("pack member table".into()));
    }
    let members: Vec<PackMember> = from_cbor(&rest[4..4 + tlen])?;
    Ok((h, members, &rest[4 + tlen..]))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEnd {
    pub id: u64,
    pub size: u64,
    /// "ok", "changed" (modified while being read), "unreadable", "truncated" (tape full).
    pub status: String,
    /// Content hashes: (profile, algorithm, hex digest).
    pub hashes: Vec<(String, String, String)>,
    /// Extended attributes written to the source after archiving (hash write-back,
    /// --set-xattr); they belong to the archived copy too, over those in FILE.
    #[serde(default)]
    pub xattrs: Vec<(ByteBuf, ByteBuf)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnd {
    pub session: Uuid,
    pub files: u64,
    pub bytes: u64,
    pub stored: u64,
    pub ended: i64,
    /// "complete" or "partial".
    pub status: String,
}

/// Last record of a session stream on a volume when the session continues on another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeEnd {
    pub session: Uuid,
    /// Position of this volume within the session (1 for the first).
    pub volume_seq: u32,
    /// Running totals for the session so far.
    pub files: u64,
    pub bytes: u64,
    pub stored: u64,
    pub ended: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_crc() {
        let h = RecordHeader { rtype: rtype::CHUNK, flags: flags::COMPRESSED, body_len: 1 << 40, seq: 9 };
        let mut b = h.encode();
        assert_eq!(RecordHeader::decode(&b).unwrap(), h);
        b[9] ^= 1;
        assert!(RecordHeader::decode(&b).is_err());
    }

    #[test]
    fn chunk_header_roundtrip() {
        let c = ChunkHeader {
            object: 5,
            seq: 2,
            codec: codec::ZSTD,
            flags: 0,
            offset: 32 << 20,
            raw_len: 16 << 20,
            stored_len: 1234,
            blake3: [7; 32],
        };
        assert_eq!(ChunkHeader::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn pack_roundtrip() {
        let h = ChunkHeader { object: 0, seq: 3, codec: codec::STORED, flags: 0, offset: 0, raw_len: 7, stored_len: 7, blake3: [1; 32] };
        let members = vec![
            PackMember { object: 4, offset: 0, len: 3, blake3: vec![2; 32] },
            PackMember { object: 9, offset: 3, len: 4, blake3: vec![3; 32] },
        ];
        let body = encode_pack(&h, &members, b"abcdefg");
        let (h2, m2, stored) = decode_pack(&body).unwrap();
        assert_eq!((h2, m2, stored), (h, members, &b"abcdefg"[..]));
    }

    #[test]
    fn file_meta_cbor_with_binary_path() {
        let m = FileMeta {
            id: 1,
            path: ByteBuf::from(b"/nebula/\xff\xfe-not-utf8".to_vec()),
            kind: FileKind::File,
            mode: 0o100644,
            uid: 1000,
            gid: 1000,
            user: Some("peng".into()),
            group: None,
            size: 10,
            mtime_ns: 1_700_000_000_123_456_789,
            atime_ns: 0,
            ctime_ns: 0,
            dev: 1,
            ino: 2,
            nlink: 1,
            rdev: 0,
            target: None,
            xattrs: vec![(ByteBuf::from(b"user.zfssum.checksum".to_vec()), ByteBuf::from(b"abc".to_vec()))],
            extents: Some(vec![(0, 4), (8, 2)]),
            fingerprints: [("zfssum".to_string(), "abc".to_string())].into(),
            same_as: None,
        };
        let back: FileMeta = from_cbor(&to_cbor(&m)).unwrap();
        assert_eq!(back, m);
    }
}
