//! Online mount: the catalog's tree, with file contents read from tape on demand.
//!
//! FUSE threads ask for byte ranges; this process's main thread fetches them. It gathers the
//! requests that arrive close together, works out which chunks they need (and the rest of
//! each file being read, so a copy of a large file is one pass), groups them by cartridge
//! and position, and for each cartridge loads it (taking a drive lease, as any other job, at
//! the priority of an interactive read; a cartridge in no library is asked for when
//! --wait-tapes allows) and reads every piece in one go into a RAM cache. Readers are woken
//! as their chunks arrive.
//!
//! The cache is also the buffer between tape and reader: chunks not yet read are never
//! evicted, and when they fill it the tape side waits, so the drive is matched to the
//! reader's pace (or stops once) rather than running ahead and having data thrown away. A
//! reader that stops reading part-way lets the fetch go after a few seconds. A read that
//! cannot be served in time, or whose data fails its BLAKE3 check, gets EIO.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ltoarc_engine::restore::{self, Piece};

use crate::ctx::Ctx;
use crate::tape;

/// How long the tape side waits for readers to take buffered data before it gives up the
/// rest of a read-ahead (the reader went away).
const ABANDONED: Duration = Duration::from_secs(10);

/// A file's data: chunk spans (chunk id, file offset, length), sorted by offset, and where
/// each chunk is on tape.
struct FileMap {
    spans: Vec<(i64, u64, u64)>,
    pieces: HashMap<i64, (String, Piece)>,
}

#[derive(Default)]
struct State {
    /// Readers waiting now: token -> (object, offset). Only they are served: a request made
    /// while a long fetch ran may have been satisfied by it since.
    waiting: HashMap<u64, (i64, u64)>,
    next_token: u64,
    files: HashMap<i64, Arc<FileMap>>,
    failed: HashMap<i64, String>,
    /// Chunk id -> data, least recently used first.
    cache: HashMap<i64, Arc<Vec<u8>>>,
    lru: VecDeque<i64>,
    cached_bytes: u64,
    queued: HashSet<i64>,
    /// Cached chunks no reader has read to the end yet, and their bytes.
    unread: HashSet<i64>,
    unread_bytes: u64,
    /// When a reader last finished a chunk.
    last_taken: Option<Instant>,
    /// Reads served, bytes, the largest read, and time readers spent waiting for tape.
    reads: u64,
    read_bytes: u64,
    largest: u64,
    waited: Duration,
}

pub struct Fetcher {
    st: Mutex<State>,
    cv: Condvar,
    capacity: u64,
    timeout: Duration,
}

impl Fetcher {
    pub fn new(capacity: u64, timeout: Duration) -> Arc<Fetcher> {
        Arc::new(Fetcher { st: Mutex::new(State::default()), cv: Condvar::new(), capacity, timeout })
    }

    /// Add a chunk read from tape, waiting while unread data fills the cache. False when
    /// readers stopped taking data (the rest of the read-ahead is given up).
    fn insert(&self, chunk: i64, data: Vec<u8>) -> bool {
        let len = data.len() as u64;
        let mut st = self.st.lock().unwrap();
        let mut since = Instant::now();
        while st.unread_bytes > 0 && st.unread_bytes + len > self.capacity {
            if st.last_taken.is_some_and(|t| t > since) {
                since = st.last_taken.unwrap_or(since);
            }
            if since.elapsed() > ABANDONED {
                st.queued.remove(&chunk);
                return false;
            }
            st = self.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
        }
        st.queued.remove(&chunk);
        st.cached_bytes += len;
        if st.cache.insert(chunk, Arc::new(data)).is_none() {
            st.lru.push_back(chunk);
        }
        if st.unread.insert(chunk) {
            st.unread_bytes += len;
        }
        // Evict what has been read, oldest first.
        let mut keep = VecDeque::new();
        while st.cached_bytes > self.capacity {
            let Some(old) = st.lru.pop_front() else { break };
            if st.unread.contains(&old) {
                keep.push_back(old);
                continue;
            }
            if let Some(d) = st.cache.remove(&old) {
                st.cached_bytes -= d.len() as u64;
            }
        }
        while let Some(k) = keep.pop_back() {
            st.lru.push_front(k);
        }
        self.cv.notify_all();
        true
    }

    fn fail(&self, objects: &[i64], why: &str) {
        let mut st = self.st.lock().unwrap();
        for o in objects {
            st.failed.insert(*o, why.to_string());
        }
        self.cv.notify_all();
    }
}

impl ltoarc_fs::Data for Fetcher {
    fn read(&self, object_id: i64, len: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        let end = len.min(offset + size as u64);
        if offset >= end {
            return Ok(Vec::new());
        }
        let begun = Instant::now();
        let deadline = begun + self.timeout;
        let mut st = self.st.lock().unwrap();
        let token = st.next_token;
        st.next_token += 1;
        let result = loop {
            if let Some(why) = st.failed.get(&object_id) {
                eprintln!("read of object {object_id} failed: {why}");
                break Err(libc::EIO);
            }
            if let Some(map) = st.files.get(&object_id).cloned() {
                let want: Vec<&(i64, u64, u64)> = map.spans.iter().filter(|(_, o, l)| o + l > offset && *o < end).collect();
                if want.iter().all(|(c, _, _)| st.cache.contains_key(c)) {
                    // Holes of sparse files read as zeros.
                    let mut out = vec![0u8; (end - offset) as usize];
                    let mut done = Vec::new();
                    for (c, o, l) in want {
                        let data = &st.cache[c];
                        let from = offset.max(*o);
                        let to = end.min(o + data.len() as u64);
                        if to > from {
                            out[(from - offset) as usize..(to - offset) as usize].copy_from_slice(&data[(from - o) as usize..(to - o) as usize]);
                        }
                        if end >= o + l {
                            done.push((*c, data.len() as u64));
                        }
                    }
                    // Chunks read to their end may make room for the tape side.
                    for (c, len) in done {
                        if st.unread.remove(&c) {
                            st.unread_bytes -= len;
                            st.last_taken = Some(Instant::now());
                            self.cv.notify_all();
                        }
                    }
                    break Ok(out);
                }
            }
            if let std::collections::hash_map::Entry::Vacant(e) = st.waiting.entry(token) {
                e.insert((object_id, offset));
                self.cv.notify_all();
            }
            let now = Instant::now();
            if now >= deadline {
                eprintln!("read of object {object_id} timed out after {}s", self.timeout.as_secs());
                break Err(libc::EIO);
            }
            st = self.cv.wait_timeout(st, deadline - now).unwrap().0;
        };
        st.waiting.remove(&token);
        st.reads += 1;
        st.read_bytes += end - offset;
        st.largest = st.largest.max(end - offset);
        st.waited += begun.elapsed();
        result
    }
}

/// Serve the fetcher's requests until the filesystem is unmounted.
pub fn serve(ctx: &Ctx, f: &Fetcher, read_buffer: u64, unmounted: &dyn Fn() -> bool) -> Result<()> {
    let mut stream_starts = HashMap::new();
    loop {
        let batch: Vec<(i64, u64)> = {
            let mut st = f.st.lock().unwrap();
            while st.waiting.is_empty() {
                if unmounted() {
                    eprintln!(
                        "served {} read(s), {} MB (largest {} KiB), {:.1} s in reads",
                        st.reads,
                        st.read_bytes / 1_000_000,
                        st.largest >> 10,
                        st.waited.as_secs_f64()
                    );
                    return Ok(());
                }
                st = f.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
            }
            drop(st);
            // Let the reads that come together arrive, then serve every reader still
            // waiting (from the earliest offset each file is wanted at).
            std::thread::sleep(Duration::from_millis(50));
            let st = f.st.lock().unwrap();
            let mut first: HashMap<i64, u64> = HashMap::new();
            for (o, off) in st.waiting.values() {
                let e = first.entry(*o).or_insert(*off);
                *e = (*e).min(*off);
            }
            first.into_iter().collect()
        };
        // What the requests need: their chunks, then the rest of each file up to the
        // read-ahead limit; minus what is cached or already on its way.
        let mut by_tape: HashMap<String, Vec<(i64, Piece)>> = HashMap::new();
        for (object, offset) in batch {
            // (The lookup's guard must not live into the match: the arms lock again.)
            let known = f.st.lock().unwrap().files.get(&object).cloned();
            let map = match known {
                Some(m) => m,
                None => match file_map(ctx, object, &mut stream_starts) {
                    Ok(m) => {
                        let m = Arc::new(m);
                        f.st.lock().unwrap().files.insert(object, m.clone());
                        // Its chunks may all be cached already (a pack read for another file).
                        f.cv.notify_all();
                        m
                    }
                    Err(e) => {
                        f.fail(&[object], &format!("{e:#}"));
                        continue;
                    }
                },
            };
            let mut st = f.st.lock().unwrap();
            // The chunks asked for and the rest of the file (read on, so a file read from
            // start to end is one pass).
            for (chunk, o, l) in &map.spans {
                if o + l <= offset {
                    continue;
                }
                if st.cache.contains_key(chunk) || !st.queued.insert(*chunk) {
                    continue;
                }
                let (barcode, piece) = map.pieces[chunk].clone();
                by_tape.entry(barcode).or_default().push((object, piece));
            }
        }
        for (barcode, mut wanted) in by_tape {
            wanted.sort_by_key(|(_, p)| (p.placement.partition, p.placement.block, p.placement.offset));
            let objects: Vec<i64> = wanted.iter().map(|(o, _)| *o).collect();
            if let Err(e) = fetch(ctx, f, &barcode, &wanted, read_buffer) {
                let mut st = f.st.lock().unwrap();
                for (_, p) in &wanted {
                    st.queued.remove(&p.chunk.id);
                }
                drop(st);
                f.fail(&objects, &format!("{barcode}: {e:#}"));
            }
        }
        f.cv.notify_all();
    }
}

/// Where object `object`'s data is (the file holding it, for one stored by reference).
fn file_map(ctx: &Ctx, object: i64, stream_starts: &mut HashMap<(i64, i64), i64>) -> Result<FileMap> {
    let o = ctx.cat.object(object)?.ok_or_else(|| anyhow!("object {object} is not in the catalog"))?;
    let pieces = restore::object_pieces(&ctx.cat, &o, 0, None, stream_starts)?;
    let mut spans: Vec<(i64, u64, u64)> = pieces.iter().map(|(_, p)| (p.chunk.id, p.chunk.offset as u64, p.chunk.raw_len as u64)).collect();
    spans.sort_by_key(|s| s.1);
    Ok(FileMap { spans, pieces: pieces.into_iter().map(|(b, p)| (p.chunk.id, (b, p))).collect() })
}

/// Load `barcode` and read the wanted pieces into the cache.
fn fetch(ctx: &Ctx, f: &Fetcher, barcode: &str, wanted: &[(i64, Piece)], read_buffer: u64) -> Result<()> {
    let sessions: HashSet<i64> = wanted.iter().map(|(_, p)| p.session_id).collect();
    let deks = restore::session_keys(&ctx.cat, &sessions, &ctx.keys)?;
    let (d, mut drive) = tape::load(ctx, barcode, None)?;
    let mut pieces: Vec<Piece> = wanted.iter().enumerate().map(|(i, (_, p))| Piece { item: i, ..p.clone() }).collect();
    // A pack is decoded whole anyway: keep every member (the next small files read are
    // usually its neighbours).
    let mut extra = Vec::new();
    {
        let st = f.st.lock().unwrap();
        let mut seen: HashSet<i64> = pieces.iter().map(|p| p.chunk.id).collect();
        for p in pieces.iter().filter(|p| p.chunk.pack_offset.is_some()) {
            let pl = &p.placement;
            for (chunk, obj_seq, session_id) in ctx.cat.record_chunks(pl.volume_id, pl.copy, pl.block, pl.offset)? {
                if !st.cache.contains_key(&chunk.id) && seen.insert(chunk.id) {
                    extra.push(Piece { item: usize::MAX, chunk, obj_seq, session_id, ..p.clone() });
                }
            }
        }
    }
    pieces.extend(extra);
    pieces.sort_by_key(|p| (p.placement.partition, p.placement.block, p.placement.offset, p.chunk.pack_offset));
    let bytes: u64 = pieces.iter().map(|p| p.chunk.raw_len as u64).sum();
    eprintln!("reading {} chunk(s), {} MB, from {barcode} in drive {}", pieces.len(), bytes / 1_000_000, d.name);
    let mut errors = Vec::new();
    // Reads follow the reader's pace within the drive's speed-matching range.
    let v = ctx.volume(barcode)?;
    let pace = crate::archive::read_pace(ctx, &d, &v, false)?;
    let mut given_up = false;
    let r = restore::read_volume(&mut *drive, barcode, &pieces, read_buffer, pace, &deks, &mut errors, &mut |p, _, data| {
        if !f.insert(p.chunk.id, data.to_vec()) {
            given_up = true;
            anyhow::bail!("readers stopped");
        }
        Ok(())
    });
    if given_up {
        // Leave the rest for when it is asked for again; what was read stays cached but
        // may be evicted.
        eprintln!("readers stopped taking data from {barcode}; leaving the rest for later");
        let mut st = f.st.lock().unwrap();
        for p in &pieces {
            st.queued.remove(&p.chunk.id);
        }
        st.unread.clear();
        st.unread_bytes = 0;
    }
    // Others may use the drive between reads; the cartridge stays in it.
    ctx.leases.release(&d);
    let bad: Vec<i64> = errors.iter().filter(|(i, _)| *i != usize::MAX).map(|(i, _)| wanted[*i].0).collect();
    if !bad.is_empty() {
        f.fail(&bad, &errors.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>().join("; "));
    }
    if given_up {
        return Ok(());
    }
    let stats = r?;
    eprintln!("read {barcode}: {}", stats.describe());
    Ok(())
}
