//! Packing records into fixed-size tape blocks and reading them back.

use std::collections::VecDeque;

use uuid::Uuid;

use crate::record::{RecordHeader, RECORD_HEADER_LEN};
use crate::{build_block, parse_block, BlockHeader, BlockKind, Error, Result, HEADER_LEN, NO_RECORD};

/// Where a record starts: block sequence number within the stream and payload offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordPos {
    pub block_seq: u64,
    pub offset: u32,
}

/// Packs records into blocks. Completed blocks queue up in `take_block`.
pub struct StreamWriter {
    block_size: usize,
    volume: Uuid,
    session_id: u64,
    kind: BlockKind,
    block_seq: u64,
    payload: Vec<u8>,
    first_record: Option<u32>,
    record_seq: u32,
    out: VecDeque<Vec<u8>>,
    payload_bytes: u64,
}

impl StreamWriter {
    pub fn new(block_size: u32, volume: Uuid, session_id: u64, kind: BlockKind) -> Self {
        let cap = block_size as usize - HEADER_LEN;
        StreamWriter {
            block_size: block_size as usize,
            volume,
            session_id,
            kind,
            block_seq: 0,
            payload: Vec::with_capacity(cap),
            first_record: None,
            record_seq: 0,
            out: VecDeque::new(),
            payload_bytes: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.block_size - HEADER_LEN
    }

    /// Append one record whose body is the concatenation of `body`. Returns its position.
    pub fn append(&mut self, rtype: u16, flags: u16, body: &[&[u8]]) -> RecordPos {
        let body_len: u64 = body.iter().map(|b| b.len() as u64).sum();
        let header = RecordHeader { rtype, flags, body_len, seq: self.record_seq }.encode();
        self.record_seq = self.record_seq.wrapping_add(1);
        if self.payload.len() == self.capacity() {
            self.flush_full();
        }
        let pos = RecordPos { block_seq: self.block_seq, offset: self.payload.len() as u32 };
        self.first_record.get_or_insert(pos.offset);
        self.put(&header);
        for part in body {
            self.put(part);
        }
        pos
    }

    fn put(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            if self.payload.len() == self.capacity() {
                self.flush_full();
            }
            let n = (self.capacity() - self.payload.len()).min(data.len());
            self.payload.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
    }

    fn header(&self) -> BlockHeader {
        BlockHeader {
            block_size: 0,
            payload_len: 0,
            volume_uuid: self.volume,
            session_id: self.session_id,
            block_seq: self.block_seq,
            first_record_offset: self.first_record.unwrap_or(NO_RECORD),
            kind: self.kind,
            flags: 0,
            payload_crc: 0,
        }
    }

    fn flush_full(&mut self) {
        let block = build_block(self.header(), &self.payload, Some(self.block_size as u32));
        self.emit(block);
    }

    fn emit(&mut self, block: Vec<u8>) {
        self.payload_bytes += self.payload.len() as u64;
        self.out.push_back(block);
        self.payload.clear();
        self.first_record = None;
        self.block_seq += 1;
    }

    /// Next completed block, if any.
    pub fn take_block(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// Close the stream: the partly filled last block is emitted short.
    pub fn finish(&mut self) {
        if !self.payload.is_empty() {
            let block = build_block(self.header(), &self.payload, None);
            self.emit(block);
        }
    }

    /// Blocks produced so far (taken or not).
    pub fn blocks(&self) -> u64 {
        self.block_seq
    }

    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes + self.payload.len() as u64
    }
}

/// Supplies blocks of one stream; returns `Ok(None)` at a filemark or end of data.
pub trait BlockSource {
    fn next_block(&mut self) -> std::io::Result<Option<Vec<u8>>>;
}

#[derive(Debug)]
pub struct Record {
    pub header: RecordHeader,
    pub body: Vec<u8>,
    pub pos: RecordPos,
}

/// Reads records from a block stream, starting at any block.
pub struct StreamReader<S: BlockSource> {
    src: S,
    block: Option<(BlockHeader, Vec<u8>)>,
    pos: usize,
    ended: bool,
}

impl<S: BlockSource> StreamReader<S> {
    /// Start at the current block of `src`, at the first record that begins in it.
    pub fn new(src: S) -> Result<Self> {
        let mut r = StreamReader { src, block: None, pos: 0, ended: false };
        if r.load_next()? {
            let first = r.block.as_ref().map(|(h, _)| h.first_record_offset).unwrap_or(NO_RECORD);
            match first {
                NO_RECORD => {
                    // Whole block continues a record; resync at the next block that starts one.
                    r.resync()?;
                }
                off => r.pos = off as usize,
            }
        }
        Ok(r)
    }

    /// A reader that has not read anything yet; position it with `restart_at`.
    pub fn idle(src: S) -> Self {
        StreamReader { src, block: None, pos: 0, ended: true }
    }

    /// Drop the current block, read the next block from the source (after the caller has
    /// repositioned it) and continue at `offset` within it.
    pub fn restart_at(&mut self, offset: u32) -> Result<()> {
        self.ended = false;
        if self.load_next()? {
            self.pos = offset as usize;
            Ok(())
        } else {
            Err(Error::Truncated("no block at the requested position".into()))
        }
    }

    pub fn source_mut(&mut self) -> &mut S {
        &mut self.src
    }

    /// Start at a known record position in the first block of `src`.
    pub fn at(src: S, offset: u32) -> Result<Self> {
        let mut r = StreamReader { src, block: None, pos: 0, ended: false };
        if r.load_next()? {
            r.pos = offset as usize;
        }
        Ok(r)
    }

    fn load_next(&mut self) -> Result<bool> {
        match self.src.next_block().map_err(|e| Error::Io(e.to_string()))? {
            None => {
                self.ended = true;
                self.block = None;
                Ok(false)
            }
            Some(raw) => {
                let (h, payload) = parse_block(&raw)?;
                let payload = payload.to_vec();
                self.block = Some((h, payload));
                self.pos = 0;
                Ok(true)
            }
        }
    }

    /// Skip ahead to the next block where a record starts (after damage or mid-record start).
    pub fn resync(&mut self) -> Result<()> {
        loop {
            if !self.load_next()? {
                return Ok(());
            }
            if let Some((h, _)) = &self.block {
                if h.first_record_offset != NO_RECORD {
                    self.pos = h.first_record_offset as usize;
                    return Ok(());
                }
            }
        }
    }

    pub fn current_block(&self) -> Option<&BlockHeader> {
        self.block.as_ref().map(|(h, _)| h)
    }

    /// Position of the next unread byte.
    pub fn position(&self) -> Option<RecordPos> {
        self.block.as_ref().map(|(h, _)| RecordPos { block_seq: h.block_seq, offset: self.pos as u32 })
    }

    fn read_exact(&mut self, mut out: &mut [u8], what: &str) -> Result<()> {
        while !out.is_empty() {
            let Some((_, payload)) = &self.block else {
                return Err(Error::Truncated(format!("stream ended inside {what}")));
            };
            if self.pos >= payload.len() {
                if !self.load_next()? {
                    return Err(Error::Truncated(format!("stream ended inside {what}")));
                }
                continue;
            }
            let n = (payload.len() - self.pos).min(out.len());
            out[..n].copy_from_slice(&payload[self.pos..self.pos + n]);
            self.pos += n;
            out = &mut out[n..];
        }
        Ok(())
    }

    /// Next record, or None at the end of the stream.
    pub fn next_record(&mut self) -> Result<Option<Record>> {
        loop {
            match &self.block {
                None => return Ok(None),
                Some((_, payload)) if self.pos >= payload.len() => {
                    if !self.load_next()? {
                        return Ok(None);
                    }
                }
                Some(_) => break,
            }
        }
        let pos = self.position().expect("block loaded");
        let mut hb = [0u8; RECORD_HEADER_LEN];
        self.read_exact(&mut hb, "record header")?;
        let header = RecordHeader::decode(&hb)?;
        let mut body = vec![0u8; header.body_len as usize];
        self.read_exact(&mut body, "record body")?;
        Ok(Some(Record { header, body, pos }))
    }

    /// Advance to `target` (which must be at or after the current position in this stream).
    pub fn seek_forward(&mut self, target: RecordPos) -> Result<()> {
        loop {
            let Some((h, _)) = &self.block else {
                return Err(Error::Truncated("stream ended before seek target".into()));
            };
            if h.block_seq == target.block_seq {
                if (target.offset as usize) < self.pos {
                    return Err(Error::Record("seek target is behind the reader".into()));
                }
                self.pos = target.offset as usize;
                return Ok(());
            }
            if h.block_seq > target.block_seq {
                return Err(Error::Record("seek target is behind the reader".into()));
            }
            if !self.load_next()? {
                return Err(Error::Truncated("stream ended before seek target".into()));
            }
        }
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn into_source(self) -> S {
        self.src
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::rtype;

    struct Vecs(VecDeque<Vec<u8>>);

    impl BlockSource for Vecs {
        fn next_block(&mut self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(self.0.pop_front())
        }
    }

    fn write(records: &[Vec<u8>], bs: u32) -> (Vec<Vec<u8>>, Vec<RecordPos>) {
        let mut w = StreamWriter::new(bs, Uuid::nil(), 1, BlockKind::Data);
        let mut pos = Vec::new();
        for (i, r) in records.iter().enumerate() {
            pos.push(w.append(rtype::CHUNK, i as u16, &[&r[..r.len() / 2], &r[r.len() / 2..]]));
        }
        w.finish();
        let mut blocks = Vec::new();
        while let Some(b) = w.take_block() {
            blocks.push(b);
        }
        (blocks, pos)
    }

    #[test]
    fn records_span_blocks_and_read_back() {
        let bs = 256u32;
        let records: Vec<Vec<u8>> = (0..20).map(|i| vec![i as u8; (i * 37) % 700]).collect();
        let (blocks, pos) = write(&records, bs);
        assert!(blocks[..blocks.len() - 1].iter().all(|b| b.len() == bs as usize));
        assert!(blocks.last().unwrap().len() <= bs as usize);
        let mut r = StreamReader::new(Vecs(blocks.clone().into())).unwrap();
        for (i, expect) in records.iter().enumerate() {
            let rec = r.next_record().unwrap().unwrap();
            assert_eq!(rec.header.flags, i as u16);
            assert_eq!(&rec.body, expect);
            assert_eq!(rec.pos, pos[i]);
        }
        assert!(r.next_record().unwrap().is_none());

        // Start in the middle: at the block holding record 12, at its offset.
        let p = pos[12];
        let rest: VecDeque<Vec<u8>> = blocks[p.block_seq as usize..].to_vec().into();
        let mut r = StreamReader::at(Vecs(rest), p.offset).unwrap();
        assert_eq!(r.next_record().unwrap().unwrap().body, records[12]);
        r.seek_forward(pos[15]).unwrap();
        assert_eq!(r.next_record().unwrap().unwrap().body, records[15]);
    }

    #[test]
    fn resync_after_mid_record_start() {
        let bs = 128u32;
        let records: Vec<Vec<u8>> = vec![vec![1; 500], vec![2; 10], vec![3; 10]];
        let (blocks, pos) = write(&records, bs);
        // Begin at block 1, which lies wholly inside record 0.
        let rest: VecDeque<Vec<u8>> = blocks[1..].to_vec().into();
        let mut r = StreamReader::new(Vecs(rest)).unwrap();
        let rec = r.next_record().unwrap().unwrap();
        assert_eq!(rec.pos, pos[1]);
        assert_eq!(rec.body, records[1]);
    }

    #[test]
    fn corrupt_block_detected() {
        let (mut blocks, _) = write(&[vec![9; 300]], 128);
        blocks[1][100] ^= 0xff;
        let mut r = StreamReader::new(Vecs(blocks.into())).unwrap();
        assert_eq!(r.next_record().unwrap_err(), Error::PayloadCrc);
    }
}
