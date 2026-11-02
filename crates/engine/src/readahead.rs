//! Reading tape ahead of its consumer, so the drive keeps streaming on read-back (restore,
//! verify, tape-to-tape copy).
//!
//! A reader thread owns the drive and reads planned extents (runs of blocks) into a memory
//! buffer with hysteresis: it reads while there is room and, once the buffer is full, waits
//! until it has drained to half before moving the tape again. A consumer slower than the
//! drive then costs one stop per half buffer instead of a stop every few blocks. Between
//! extents the reader locates on its own, while the consumer works through what is buffered.
//!
//! With pacing the reader matches the drive to the consumer instead, the mirror of a paced
//! backup (`ringbuf::Stepper` with `Side::Read`): the read rate is held steady while the
//! buffer stays within its band, and stepped toward the consumer's recent rate when the
//! buffer leaves it and keeps going, within the drive's speed-matching range, so the drive
//! slows down rather than stopping and changes speed seldom. A tape-to-tape copy paces the
//! faster drive to the slower one this way. Only a consumer slower than the drive's
//! slowest speed fills the buffer; then the drive stops, and reads again once the buffer is
//! nearly empty, so each burst is as long as the buffer allows.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use ltoarc_device::{Error as DevError, Position, ReadStatus, TapeDrive};
use ltoarc_format::stream::BlockSource;

use crate::ringbuf::{Matched, Pace, Side, Stepper, READ_RESTART};

/// Blocks `start..=end` of a partition (`end` None: up to the next filemark).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub partition: u8,
    pub start: u64,
    pub end: Option<u64>,
}

enum Item {
    Block(u64, Vec<u8>),
    Filemark(u64),
    /// The reader moved on to the next extent.
    Jump,
    Error(String),
}

struct State {
    q: VecDeque<Item>,
    bytes: u64,
    done: bool,
    /// The consumer has gone; the reader stops.
    cancelled: bool,
    /// (time, total bytes taken by the consumer) over the last few seconds.
    samples: VecDeque<(Instant, u64)>,
    taken: u64,
}

impl State {
    fn consumer_rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let span = t1.duration_since(t0).as_secs_f64();
        (span >= 0.5).then(|| (b1 - b0) as f64 / span)
    }
}

const RATE_WINDOW: Duration = Duration::from_secs(8);

/// The buffer between the reader thread and the consumer.
pub struct ReadAhead {
    st: Mutex<State>,
    cv: Condvar,
    capacity: u64,
    pace: Option<Pace>,
    /// Rate to start at before the consumer's is known (default: the drive's maximum).
    start_rate: Option<f64>,
}

#[derive(Debug, Default, Clone)]
pub struct ReadStats {
    pub blocks: u64,
    pub bytes: u64,
    pub locates: u64,
    /// Times the buffer filled up and the drive was stopped until it had drained to half.
    pub pauses: u64,
    /// Rates the reads were paced at.
    pub matched: Matched,
    /// Time spent reading (from the first read to the last).
    pub secs: f64,
    /// What the drive reported about its tape motion while reading.
    pub drive: Option<crate::backup::Motion>,
}

impl ReadStats {
    pub fn merge(&mut self, o: &ReadStats) {
        self.blocks += o.blocks;
        self.bytes += o.bytes;
        self.locates += o.locates;
        self.pauses += o.pauses;
        self.matched.merge(&o.matched);
        self.secs += o.secs;
        if let Some(m) = &o.drive {
            self.drive.get_or_insert_with(Default::default).merge(m);
        }
    }

    /// For reports: "301.2 MB/s reading, full speed, 1 locate(s), read-ahead buffer full 0
    /// time(s); drive: tape moving at ...".
    pub fn describe(&self) -> String {
        format!(
            "{:.1} MB/s reading, {}, {} locate(s), read-ahead buffer full {} time(s); drive: {}",
            self.bytes as f64 / self.secs.max(1e-3) / 1e6,
            self.matched.describe().unwrap_or_else(|| "full speed".into()),
            self.locates,
            self.pauses,
            self.drive.map_or_else(|| "no motion counters".into(), |m| m.describe_read(self.bytes, self.locates))
        )
    }
}

impl ReadAhead {
    pub fn new(capacity: u64) -> ReadAhead {
        ReadAhead::paced(capacity, None, None)
    }

    /// Read at the consumer's rate within `pace` (the drive's speed-matching range),
    /// starting at `start_rate`.
    pub fn paced(capacity: u64, pace: Option<Pace>, start_rate: Option<f64>) -> ReadAhead {
        ReadAhead {
            st: Mutex::new(State { q: VecDeque::new(), bytes: 0, done: false, cancelled: false, samples: VecDeque::new(), taken: 0 }),
            cv: Condvar::new(),
            capacity: capacity.max(4 << 20),
            pace,
            start_rate,
        }
    }

    /// The consumer's recent rate, and how full the buffer is.
    fn consumer(&self) -> (Option<f64>, f64) {
        let st = self.st.lock().unwrap();
        (st.consumer_rate(), st.bytes as f64 / self.capacity as f64)
    }

    fn push(&self, item: Item, len: u64, stats: &mut ReadStats) -> bool {
        let mut st = self.st.lock().unwrap();
        if st.bytes + len > self.capacity && !st.cancelled {
            // Full: let the drive stop, and start again once the buffer is nearly empty when
            // paced (the consumer is slower than the drive's slowest speed: long bursts),
            // half empty otherwise.
            stats.pauses += 1;
            let resume = if self.pace.is_some() { (self.capacity as f64 * READ_RESTART) as u64 } else { self.capacity / 2 };
            while st.bytes > resume && !st.cancelled {
                st = self.cv.wait(st).unwrap();
            }
        }
        if st.cancelled {
            return false;
        }
        st.bytes += len;
        st.q.push_back(item);
        self.cv.notify_all();
        true
    }

    fn pop(&self) -> Option<Item> {
        let mut st = self.st.lock().unwrap();
        loop {
            if let Some(item) = st.q.pop_front() {
                if let Item::Block(_, d) = &item {
                    st.bytes -= d.len() as u64;
                    st.taken += d.len() as u64;
                    let (now, taken) = (Instant::now(), st.taken);
                    if st.samples.back().is_none_or(|(t, _)| now.duration_since(*t) >= Duration::from_millis(50)) {
                        st.samples.push_back((now, taken));
                        while st.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW) {
                            st.samples.pop_front();
                        }
                    }
                }
                self.cv.notify_all();
                return Some(item);
            }
            if st.done {
                return None;
            }
            st = self.cv.wait(st).unwrap();
        }
    }

    fn peek_pos(&self) -> Option<Option<u64>> {
        let mut st = self.st.lock().unwrap();
        loop {
            match st.q.front() {
                Some(Item::Block(p, _)) | Some(Item::Filemark(p)) => return Some(Some(*p)),
                Some(_) => return Some(None),
                None if st.done => return None,
                None => st = self.cv.wait(st).unwrap(),
            }
        }
    }

    /// Stop the reader (the consumer is done or failed).
    pub fn cancel(&self) {
        let mut st = self.st.lock().unwrap();
        st.cancelled = true;
        st.q.clear();
        st.bytes = 0;
        self.cv.notify_all();
    }

    /// Reader thread body: read `extents` in order into the buffer.
    pub fn read(&self, drive: &mut dyn TapeDrive, block_size: usize, extents: &[Extent]) -> ReadStats {
        let mut stats = ReadStats::default();
        let perf0 = crate::backup::drive_perf(drive);
        let mut buf = vec![0u8; block_size];
        let mut at: Option<(u8, u64)> = None;
        let mut first: Option<Instant> = None;
        // Its rate changes are counted in when it is replaced and at the end.
        let mut step = Stepper::new(Side::Read);
        'extents: for (i, e) in extents.iter().enumerate() {
            if i > 0 && !self.push(Item::Jump, 0, &mut stats) {
                break;
            }
            if at != Some((e.partition, e.start)) {
                if let Err(err) = drive.locate(Position { partition: e.partition, object: e.start }) {
                    self.push(Item::Error(format!("locating block {}: {err}", e.start)), 0, &mut stats);
                    break;
                }
                stats.locates += 1;
            }
            let mut pos = e.start;
            let mut next_due = Instant::now();
            stats.matched.changes += step.changes;
            step = Stepper::new(Side::Read);
            loop {
                if e.end.is_some_and(|end| pos > end) {
                    break;
                }
                if self.pace.is_some() {
                    let now = Instant::now();
                    if let Some(wait) = next_due.checked_duration_since(now) {
                        std::thread::sleep(wait);
                    }
                }
                match drive.read_block(&mut buf) {
                    Ok(ReadStatus::Data(n)) => {
                        first.get_or_insert_with(Instant::now);
                        stats.blocks += 1;
                        stats.bytes += n as u64;
                        let pauses = stats.pauses;
                        if !self.push(Item::Block(pos, buf[..n].to_vec()), n as u64, &mut stats) {
                            break 'extents;
                        }
                        if let Some(p) = self.pace {
                            let now = Instant::now();
                            if stats.pauses > pauses {
                                // The drive stopped while the buffer drained; start afresh.
                                next_due = now;
                                stats.matched.changes += step.changes;
                                step = Stepper::new(Side::Read);
                            } else {
                                let (taking, fill) = self.consumer();
                                let rate = step.next(fill, self.capacity, taking, self.start_rate.unwrap_or(p.max), p);
                                if rate >= p.max {
                                    // The consumer keeps up: the drive runs at its own speed.
                                    next_due = now;
                                } else {
                                    stats.matched.add(n as u64, rate);
                                    next_due = next_due.max(now - Duration::from_millis(200)) + Duration::from_secs_f64(n as f64 / rate);
                                }
                            }
                        }
                    }
                    Ok(ReadStatus::Filemark) => {
                        if !self.push(Item::Filemark(pos), 0, &mut stats) {
                            break 'extents;
                        }
                        if e.end.is_none() {
                            pos += 1;
                            break;
                        }
                    }
                    Ok(ReadStatus::EndOfData) | Err(DevError::EndOfData) => break,
                    Err(err) => {
                        self.push(Item::Error(format!("reading block {pos}: {err}")), 0, &mut stats);
                        break 'extents;
                    }
                }
                pos += 1;
            }
            at = Some((e.partition, pos));
        }
        stats.matched.changes += step.changes;
        stats.secs = first.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if let (Some(a), Some(b)) = (perf0, crate::backup::drive_perf(drive)) {
            stats.drive = Some(crate::backup::Motion::between(&a.read, &b.read));
        }
        let mut st = self.st.lock().unwrap();
        st.done = true;
        self.cv.notify_all();
        stats
    }
}

/// The consumer's view: tape blocks in order, as a block source for record streams. A
/// stream ends at a filemark or where the reader moved to the next extent; `skip_to` then
/// positions on the next block wanted.
pub struct Blocks<'a> {
    ra: &'a ReadAhead,
    /// Position of the last block handed out.
    pub last: Option<u64>,
    pub error: Option<String>,
}

impl<'a> Blocks<'a> {
    pub fn new(ra: &'a ReadAhead) -> Self {
        Blocks { ra, last: None, error: None }
    }

    /// Drop what comes before block `pos`, so the next block handed out is `pos`. False if
    /// the reader never reaches it (not in the extents, or it stopped).
    pub fn skip_to(&mut self, pos: u64) -> bool {
        loop {
            match self.ra.peek_pos() {
                None => return false,
                Some(Some(p)) if p == pos => return true,
                Some(Some(p)) if p > pos => return false,
                _ => match self.ra.pop() {
                    Some(Item::Error(e)) => {
                        self.error = Some(e);
                        return false;
                    }
                    Some(_) => {}
                    None => return false,
                },
            }
        }
    }
}

impl BlockSource for Blocks<'_> {
    fn next_block(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        match self.ra.pop() {
            Some(Item::Block(p, d)) => {
                self.last = Some(p);
                Ok(Some(d))
            }
            Some(Item::Filemark(_)) | Some(Item::Jump) | None => Ok(None),
            Some(Item::Error(e)) => {
                self.error = Some(e.clone());
                Err(std::io::Error::other(e))
            }
        }
    }
}

/// Merge block ranges (partition, first, last), sorted by first block, into extents,
/// reading through gaps of up to `gap` blocks (cheaper than a locate).
pub fn extents(ranges: &[(u8, u64, u64)], gap: u64) -> Vec<Extent> {
    let mut out: Vec<Extent> = Vec::new();
    for &(partition, first, last) in ranges {
        match out.last_mut() {
            Some(e) if e.partition == partition && first >= e.start && first <= e.end.unwrap_or(u64::MAX).saturating_add(gap) => {
                e.end = Some(e.end.unwrap_or(last).max(last));
            }
            _ => out.push(Extent { partition, start: first, end: Some(last) }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_extents() {
        let e = extents(&[(0, 10, 12), (0, 12, 14), (0, 100, 101), (0, 101, 103), (1, 5, 7)], 20);
        assert_eq!(
            e,
            vec![
                Extent { partition: 0, start: 10, end: Some(14) },
                Extent { partition: 0, start: 100, end: Some(103) },
                Extent { partition: 1, start: 5, end: Some(7) }
            ]
        );
    }
}
