//! Block ring buffer between the producer (record packing) and the tape writer thread.
//!
//! Without pacing the writer starts once the buffer holds `high` bytes (or the producer
//! has finished) and writes each burst at full speed (plain hysteresis, as mbuffer does).
//!
//! With pacing it streams at a rate that changes in steps (`Stepper`): held steady while
//! the buffer stays within its band, stepped toward the source's recent rate when the
//! buffer leaves the band and keeps going, kept between the drive's speed-matching floor
//! and its maximum, so the drive slows down rather than stopping and changes speed seldom.
//! Only a source slower than the floor empties the buffer; then the drive stops, and starts
//! again once the buffer is nearly full, so each burst is as long as the buffer allows.
//!
//! When a session moves to the next cartridge, the producer drains the ring (everything
//! queued goes to the full cartridge), swaps cartridges while the writer idles, and resizes
//! the ring for the new one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ltoarc_device::{TapeDrive, WriteStatus};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pace {
    /// Slowest rate the drive streams at without stopping (bytes/s).
    pub floor: f64,
    /// Fastest rate the drive writes at (bytes/s).
    pub max: f64,
    /// Rates between these the drive cannot match (bytes/s, exclusive): fed at them it
    /// stops again and again. An LTO-8 half-height drive writing LTO-7 media streamed at up
    /// to 155 MB/s and from 280 MB/s, and paused 7 to 73 times a minute in between.
    pub gap: Option<(f64, f64)>,
}

impl Pace {
    /// Within `floor..=max`.
    pub fn range(floor: f64, max: f64) -> Pace {
        Pace { floor, max, gap: None }
    }

    /// The rate to pace at for `r`: within the drive's range, never inside its gap (down
    /// to its lower edge: the source waits a little, and the drive does not stop; full speed
    /// with the source unable to keep it fed made a real LTO-8 stall dozens of times a minute).
    pub fn snap(&self, r: f64) -> f64 {
        let r = r.clamp(self.floor, self.max);
        match self.gap {
            Some((lo, hi)) if r > lo && r < hi => lo,
            _ => r,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Most bytes held in memory.
    pub capacity: u64,
    /// Bytes buffered before the writer starts (or restarts after running dry).
    pub high: u64,
    /// Match the drive's speed to the producer instead of writing bursts at full speed.
    pub pace: Option<Pace>,
}

impl Config {
    pub fn new(capacity: u64) -> Self {
        Config { capacity, high: capacity / 2, pace: None }
    }

    /// Default: 8 GiB, or a quarter of available memory if that is less (at least 64 MiB).
    pub fn default_for_host() -> Self {
        let avail = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find(|l| l.starts_with("MemAvailable:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
            })
            .map(|kib| kib * 1024)
            .unwrap_or(4 << 30);
        Config::new((avail / 4).clamp(64 << 20, 8 << 30))
    }
}

/// The rates a drive was paced at: what speed it was matched to.
#[derive(Debug, Default, Clone, Copy)]
pub struct Matched {
    pub bytes: u64,
    /// Time those bytes take at the paced rates.
    pub secs: f64,
    pub min: f64,
    pub max: f64,
    /// Times the paced rate was changed.
    pub changes: u64,
}

impl Matched {
    pub fn add(&mut self, len: u64, rate: f64) {
        if self.bytes == 0 {
            (self.min, self.max) = (rate, rate);
        }
        self.bytes += len;
        self.secs += len as f64 / rate;
        self.min = self.min.min(rate);
        self.max = self.max.max(rate);
    }

    pub fn merge(&mut self, o: &Matched) {
        if o.bytes == 0 {
            self.changes += o.changes;
            return;
        }
        if self.bytes == 0 {
            let changes = self.changes;
            *self = *o;
            self.changes += changes;
            return;
        }
        self.changes += o.changes;
        self.bytes += o.bytes;
        self.secs += o.secs;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
    }

    /// Time-weighted average rate, if anything was paced.
    pub fn average(&self) -> Option<f64> {
        (self.bytes > 0 && self.secs > 0.0).then(|| self.bytes as f64 / self.secs)
    }

    /// "matched at about 152 MB/s (140-171, 3 rate changes)", or None when nothing was paced.
    pub fn describe(&self) -> Option<String> {
        let avg = self.average()?;
        Some(format!("matched at about {:.0} MB/s ({:.0}-{:.0}, {} rate change(s))", avg / 1e6, self.min / 1e6, self.max / 1e6, self.changes))
    }
}

#[derive(Debug, Default, Clone)]
pub struct WriterStats {
    pub blocks: u64,
    pub bytes: u64,
    /// Time spent inside drive writes.
    pub write_time: Duration,
    /// Bursts of continuous writing (each ends with the drive stopping).
    pub bursts: u64,
    /// Bursts that ended because the buffer ran dry while the source was still producing.
    pub underruns: u64,
    pub early_warning: bool,
    /// Rates the drive was paced at (empty when it wrote at full speed).
    pub matched: Matched,
    /// What the drive reported about its tape motion while writing the session's data.
    pub drive: Option<crate::backup::Motion>,
}

struct State {
    cfg: Config,
    q: VecDeque<Vec<u8>>,
    bytes: u64,
    closed: bool,
    early_warning: bool,
    failed: Option<String>,
    /// (time, total bytes pushed) samples over the last few seconds.
    samples: VecDeque<(Instant, u64)>,
    pushed: u64,
    /// The writer has started its first burst.
    started: bool,
    /// Write out everything queued regardless of the watermark (volume switch).
    draining: bool,
    /// The writer holds a block it has taken off the queue but not yet written.
    writing: bool,
}

impl State {
    /// Producer rate over the sample window (bytes/s), once there is a meaningful window
    /// (a tenth of a second will do at the start: the writer must not begin far off it).
    fn producer_rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let span = t1.duration_since(t0).as_secs_f64();
        (span >= 0.1).then(|| (b1 - b0) as f64 / span)
    }
}

/// Window the other side's rate is measured over: long enough not to follow every hiccup
/// of a filesystem.
const RATE_WINDOW: Duration = Duration::from_secs(8);

/// Writes restart after a stop once the buffer is this full (paced; bursts as long as
/// possible when the source is slower than the drive's slowest speed).
pub const WRITE_RESTART: f64 = 0.9;
/// Reads restart after the buffer filled once it is down to this (paced).
pub const READ_RESTART: f64 = 0.1;

/// Which way a buffer moves data: a writer's buffer drains into the drive (which stops
/// when it runs dry), a reader's fills from it (the drive stops when it is full).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Write,
    Read,
}

/// A paced rate that changes in steps. It is held steady while the buffer stays within
/// its band; when the buffer leaves the band and keeps moving away (the other side is
/// faster or slower), the rate steps toward the other side's recent rate, a little past it
/// so the buffer comes back: at most once per dwell (a quarter of the time the buffer lasts
/// at the current rate, 1 to 8 s), and at once near the edge where the drive would stop
/// (empty when writing, full when reading). Every change can make the drive change speed:
/// on a real LTO-8 a drifting rate caused pauses where a steady one caused none.
///
/// Bands (fractions of the buffer): writing, slower below 0.40 (at once below 0.10), and
/// faster only once the buffer has stayed full for a minute (the source waits meanwhile,
/// which costs the drive nothing), from the drive's gap to full speed after five: on a real LTO-8 a
/// constant rate wrote without a pause whatever the source did, while a jump up during a
/// burst cost dozens of pauses; a new burst picks its rate afresh from the source. Reading,
/// slower above 0.60 (at once above 0.90), faster below 0.25 (also when empty: the consumer
/// waits).
#[derive(Debug, Clone)]
pub struct Stepper {
    side: Side,
    rate: Option<f64>,
    changed: Instant,
    /// (when, fill) over the last trend window.
    fills: VecDeque<(Instant, f64)>,
    /// Since when the buffer has been full (writing): the source waits on it.
    full_since: Option<Instant>,
    /// Rate changes made.
    pub changes: u64,
}

/// Writing: how long the buffer must stay full before the rate goes up during a burst, and
/// before it goes from the gap's lower edge to full speed (the source may be that fast).
const FULL_HOLD: Duration = Duration::from_secs(60);
const GAP_HOLD: Duration = Duration::from_secs(300);

impl Stepper {
    pub fn new(side: Side) -> Self {
        Stepper { side, rate: None, changed: Instant::now(), fills: VecDeque::new(), full_since: None, changes: 0 }
    }

    /// The rate for the next block. `fill`: how full the buffer is (0..1) of `capacity`
    /// bytes; `other`: the rate of the other side over the last seconds (the source's when
    /// writing, the consumer's when reading), when known; `start`: the rate to begin with
    /// when it is not.
    pub fn next(&mut self, fill: f64, capacity: u64, other: Option<f64>, start: f64, p: Pace) -> f64 {
        let now = Instant::now();
        let Some(r) = self.rate else {
            let r = p.snap(other.unwrap_or(start));
            self.rate = Some(r);
            self.changed = now;
            return r;
        };
        let dwell = Duration::from_secs_f64((capacity as f64 / r / 4.0).clamp(1.0, 8.0));
        self.fills.push_back((now, fill));
        while self.fills.len() > 2 && now.duration_since(self.fills[1].0) >= dwell / 2 {
            self.fills.pop_front();
        }
        let (t0, f0) = self.fills[0];
        // Rising or falling by more than 1% of the buffer over the last half dwell.
        let trend = if now.duration_since(t0) >= dwell / 4 { fill - f0 } else { 0.0 };
        let (rising, falling) = (trend > 0.01, trend < -0.01);
        // `waiting`: the other side waits on the buffer (a source blocked by a full one, a
        // consumer starved by an empty one), so its rate is capped by ours: step by a factor.
        let full_for = if self.side == Side::Write && fill > 0.95 {
            now.duration_since(*self.full_since.get_or_insert(now))
        } else {
            self.full_since = None;
            Duration::ZERO
        };
        let (faster, slower, urgent, waiting) = match self.side {
            Side::Write => (full_for >= FULL_HOLD, fill < 0.10 || (fill < 0.40 && falling), fill < 0.10, fill > 0.95),
            Side::Read => (fill < 0.05 || (fill < 0.25 && falling), fill > 0.90 || (fill > 0.60 && rising), fill > 0.90, fill < 0.05),
        };
        let wait = if urgent { Duration::from_millis(500).min(dwell) } else { dwell };
        if !(faster || slower) || now.duration_since(self.changed) < wait {
            return r;
        }
        let o = other.unwrap_or(r);
        // Writing at the gap's lower edge with the source waiting: only full speed is faster.
        if let (Side::Write, true, Some((lo, hi))) = (self.side, faster, p.gap) {
            if r >= lo * 0.999 && r < hi {
                if full_for < GAP_HOLD {
                    return r;
                }
                self.rate = Some(p.max);
                self.changed = now;
                self.changes += 1;
                self.fills.clear();
                self.full_since = None;
                return p.max;
            }
        }
        let new = p.snap(if faster {
            (r * if waiting { 1.25 } else { 1.10 }).max(o * 1.05)
        } else {
            (r * if urgent { 0.80 } else { 0.95 }).min(o * if urgent { 0.90 } else { 0.95 })
        });
        if (new - r).abs() > r * 0.005 {
            self.rate = Some(new);
            self.changed = now;
            self.changes += 1;
            self.fills.clear();
            self.full_since = None;
        }
        self.rate.unwrap_or(new)
    }

    /// The rate while the other side is done (finishing): held, or `start` if none yet.
    pub fn hold(&mut self, start: f64, p: Pace) -> f64 {
        *self.rate.get_or_insert(p.snap(start))
    }
}

pub struct Ring {
    st: Mutex<State>,
    cv: Condvar,
    meter: Option<Arc<Meter>>,
}

/// Counters for showing a session's progress while it writes, summed over its buffers:
/// bytes waiting in them, their size, and bytes the drives have taken.
#[derive(Debug, Default)]
pub struct Meter {
    pub buffered: AtomicU64,
    pub capacity: AtomicU64,
    pub written: AtomicU64,
}

/// The drive a session writes to: the caller's, or one the tape set moved the session to
/// (a cartridge of another generation or in another library).
pub struct DriveSlot<'a> {
    first: &'a mut dyn TapeDrive,
    other: Option<Box<dyn TapeDrive>>,
}

impl<'a> DriveSlot<'a> {
    pub fn new(drive: &'a mut dyn TapeDrive) -> Self {
        DriveSlot { first: drive, other: None }
    }

    pub fn get(&mut self) -> &mut dyn TapeDrive {
        match &mut self.other {
            Some(d) => &mut **d,
            None => &mut *self.first,
        }
    }

    pub fn replace(&mut self, drive: Box<dyn TapeDrive>) {
        self.other = Some(drive);
    }
}

/// The drive, shared between the writer thread and the volume switch.
pub type SharedDrive<'a> = Mutex<DriveSlot<'a>>;

impl Ring {
    pub fn new(cfg: Config) -> Self {
        Ring {
            st: Mutex::new(State {
                cfg,
                q: VecDeque::new(),
                bytes: 0,
                closed: false,
                early_warning: false,
                failed: None,
                samples: VecDeque::new(),
                pushed: 0,
                started: false,
                draining: false,
                writing: false,
            }),
            cv: Condvar::new(),
            meter: None,
        }
    }

    /// A ring that counts into `meter` as well.
    pub fn metered(cfg: Config, meter: Arc<Meter>) -> Self {
        meter.capacity.fetch_add(cfg.capacity, Ordering::Relaxed);
        Ring { meter: Some(meter), ..Ring::new(cfg) }
    }

    fn count(&self, f: impl FnOnce(&Meter)) {
        if let Some(m) = &self.meter {
            f(m);
        }
    }

    /// Queue a block; blocks while the buffer is full. Fails if the writer has failed.
    pub fn push(&self, block: Vec<u8>) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        while st.bytes + block.len() as u64 > st.cfg.capacity && !st.q.is_empty() && st.failed.is_none() {
            st = self.cv.wait(st).unwrap();
        }
        if let Some(e) = &st.failed {
            return Err(e.clone());
        }
        st.bytes += block.len() as u64;
        st.pushed += block.len() as u64;
        self.count(|m| {
            m.buffered.fetch_add(block.len() as u64, Ordering::Relaxed);
        });
        let now = Instant::now();
        let pushed = st.pushed;
        if st.samples.back().is_none_or(|(t, _)| now.duration_since(*t) >= Duration::from_millis(50)) {
            st.samples.push_back((now, pushed));
            while st.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > RATE_WINDOW) {
                st.samples.pop_front();
            }
        }
        st.q.push_back(block);
        self.cv.notify_all();
        Ok(())
    }

    /// No more blocks: the writer drains the buffer and returns.
    pub fn close(&self) {
        self.st.lock().unwrap().closed = true;
        self.cv.notify_all();
    }

    pub fn early_warning(&self) -> bool {
        self.st.lock().unwrap().early_warning
    }

    pub fn buffered(&self) -> u64 {
        self.st.lock().unwrap().bytes
    }

    pub fn started(&self) -> bool {
        self.st.lock().unwrap().started
    }

    pub fn config(&self) -> Config {
        self.st.lock().unwrap().cfg
    }

    /// Have the writer write out everything queued, and wait until it has (and is idle, so
    /// the caller may use the drive). Used before a volume switch.
    pub fn drain(&self) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        st.draining = true;
        self.cv.notify_all();
        while (!st.q.is_empty() || st.writing) && st.failed.is_none() {
            st = self.cv.wait(st).unwrap();
        }
        st.draining = false;
        match &st.failed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// After a drained volume switch: clear early warning, size the ring for the new
    /// cartridge, and pace for its drive.
    pub fn new_volume(&self, capacity: u64, high: u64, pace: Option<Pace>) {
        let mut st = self.st.lock().unwrap();
        st.early_warning = false;
        let old = st.cfg.capacity;
        self.count(|m| {
            m.capacity.fetch_add(capacity, Ordering::Relaxed);
            m.capacity.fetch_sub(old, Ordering::Relaxed);
        });
        st.cfg.capacity = capacity;
        st.cfg.high = high.min(capacity);
        st.cfg.pace = pace;
        self.cv.notify_all();
    }

    /// Writer thread body: write bursts until the ring is closed and empty.
    pub fn write_to(&self, drive: &SharedDrive<'_>) -> Result<WriterStats, String> {
        let mut stats = WriterStats::default();
        loop {
            {
                let mut st = self.st.lock().unwrap();
                // Paced, a burst starts with the buffer nearly full: as long a burst as it holds.
                let start_at = if st.cfg.pace.is_some() { ((st.cfg.capacity as f64 * WRITE_RESTART) as u64).max(st.cfg.high.min(st.cfg.capacity)) } else { st.cfg.high };
                while !(st.closed || st.bytes >= start_at || (st.draining && !st.q.is_empty())) {
                    st = self.cv.wait(st).unwrap();
                }
                if st.closed && st.q.is_empty() {
                    stats.early_warning = st.early_warning;
                    return Ok(stats);
                }
            }
            stats.bursts += 1;
            self.st.lock().unwrap().started = true;
            let mut next_due = Instant::now();
            // The drive starts afresh after a stop: at the source's recent rate.
            let mut step = Stepper::new(Side::Write);
            loop {
                let (block, rate) = {
                    let mut st = self.st.lock().unwrap();
                    match st.q.pop_front() {
                        Some(b) => {
                            st.bytes -= b.len() as u64;
                            self.count(|m| {
                                m.buffered.fetch_sub(b.len() as u64, Ordering::Relaxed);
                            });
                            st.writing = true;
                            self.cv.notify_all();
                            let rate = st.cfg.pace.map(|p| {
                                // Stepped against the buffer and the producer's recent rate;
                                // once the producer is done, held while the buffer drains.
                                let fill = st.bytes as f64 / st.cfg.capacity.max(1) as f64;
                                if st.closed || st.draining {
                                    step.hold(p.max, p)
                                } else {
                                    step.next(fill, st.cfg.capacity, st.producer_rate(), p.max, p)
                                }
                            });
                            (b, rate)
                        }
                        None => {
                            if !st.closed && !st.draining {
                                stats.underruns += 1;
                            }
                            break;
                        }
                    }
                };
                if let Some(rate) = rate {
                    stats.matched.add(block.len() as u64, rate);
                    if let Some(wait) = next_due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    next_due = next_due.max(Instant::now() - Duration::from_millis(200))
                        + Duration::from_secs_f64(block.len() as f64 / rate);
                }
                let t = Instant::now();
                let r = drive.lock().unwrap().get().write_block(&block);
                stats.write_time += t.elapsed();
                {
                    let mut st = self.st.lock().unwrap();
                    st.writing = false;
                    self.cv.notify_all();
                }
                match r {
                    Ok(WriteStatus::EarlyWarning) => {
                        stats.early_warning = true;
                        self.st.lock().unwrap().early_warning = true;
                    }
                    Ok(WriteStatus::Ok) => {}
                    Err(e) => {
                        let msg = format!("tape write failed: {e}");
                        let mut st = self.st.lock().unwrap();
                        st.failed = Some(msg.clone());
                        st.q.clear();
                        let dropped = st.bytes;
                        self.count(|m| {
                            m.buffered.fetch_sub(dropped, Ordering::Relaxed);
                        });
                        st.bytes = 0;
                        self.cv.notify_all();
                        return Err(msg);
                    }
                }
                stats.blocks += 1;
                stats.bytes += block.len() as u64;
                self.count(|m| {
                    m.written.fetch_add(block.len() as u64, Ordering::Relaxed);
                });
            }
            stats.matched.changes += step.changes;
        }
    }
}
