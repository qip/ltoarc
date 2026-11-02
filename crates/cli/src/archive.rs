//! Archive commands: backup, sessions, ls, find, restore, verify, scan.

use std::os::unix::ffi::OsStrExt;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Args;
use globset::Glob;
use ltoarc_catalog::{View, ROOT_PATH};
use ltoarc_engine::backup::{self, Progress};
use ltoarc_engine::hash::{self, Mode, Policy};
use ltoarc_engine::restore::{self, Restorer};
use ltoarc_engine::walk::WalkOptions;
use ltoarc_engine::{scan, verify};

use crate::ctx::Ctx;
use crate::sim::parse_size;
use crate::table::{when, Table};
use crate::tape;

#[derive(Args)]
pub struct BackupArgs {
    /// Files or directories to back up (absolute or relative paths).
    #[arg(required_unless_present = "stream")]
    pub sources: Vec<PathBuf>,
    /// Archive a command's output as a file: PATH=COMMAND (repeatable), e.g.
    /// /.ltoarc/streams/db/pg='pg_basebackup -Ft -D - -X fetch'. Restore it with `ltoarc cat`.
    #[arg(long = "stream")]
    pub stream: Vec<String>,
    /// Cartridges to write: a barcode, a comma-separated list, or a range such as
    /// DC8001L8..DC8020L8. The session appends to the first one that is not full and
    /// continues on the next ones in order as each fills; blank ones are labelled.
    #[arg(long)]
    pub tape: String,
    /// Also write copy 2 of the session to these cartridges at the same time, in other
    /// drives (tee: the source is read once; the faster drive is matched to the slower).
    #[arg(long)]
    pub tape2: Option<String>,
    /// Start each cartridge the session reaches afresh, instead of appending to it: what it
    /// held is overwritten and forgotten. Lists what would be lost and asks first.
    #[arg(long)]
    pub overwrite: bool,
    /// Do not ask (for --overwrite).
    #[arg(long)]
    pub yes: bool,
    /// Show which cartridges the session would use (and whether they suffice), then stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Write through this many drives at once, each filling its own cartridges from the set;
    /// files are split between them by size (drive compression must stay off so sizes hold).
    /// The source has to keep every drive busy.
    #[arg(long, default_value_t = 1)]
    pub drives: usize,
    /// Pool recorded on cartridges labelled during the backup (default: the first cartridge's).
    #[arg(long)]
    pub pool: Option<String>,
    /// Encrypt the session with this key file (`ltoarc key new FILE`). A cartridge holds
    /// either plaintext sessions or sessions encrypted with one key.
    #[arg(long)]
    pub encrypt: Option<PathBuf>,
    /// Drive to write in (default: a free one that can write each cartridge's generation).
    #[arg(long)]
    pub drive: Option<String>,
    /// Job name recorded with the session.
    #[arg(long)]
    pub job: Option<String>,
    /// Exclude glob (repeatable); without '/' it matches names anywhere, e.g. '*.tmp'.
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Stay on the filesystem of each source.
    #[arg(long)]
    pub one_file_system: bool,
    /// Hash profile (repeatable): blake3, sha256, sha512, sha1, md5, xxh3, or one defined
    /// under [hash.NAME] in the config file. Default: blake3.
    #[arg(long = "hash")]
    pub hashes: Vec<String>,
    /// use-existing | generate | verify
    #[arg(long, default_value = "use-existing")]
    pub hash_mode: String,
    /// Write content digests to the files' xattrs (on the source, and in the archived copy).
    #[arg(long)]
    pub write_back: bool,
    /// Extra xattr for each file, NAME=TEMPLATE (repeatable): written to the source after the
    /// session and kept in the archived copy. Templates may use {session}, {date} (session
    /// start), {barcodes} (cartridges holding the file), {job}, {<profile>} (its digest).
    #[arg(long = "set-xattr")]
    pub set_xattr: Vec<String>,
    /// Compress with zstd at this level (default: off; the drive compresses in hardware).
    #[arg(long)]
    pub zstd: Option<i32>,
    /// Never software-compress names matching this glob (adds to built-in media/archive types).
    #[arg(long)]
    pub compress_never: Vec<String>,
    /// Always software-compress names matching this glob (adds to built-in text types).
    #[arg(long)]
    pub compress_always: Vec<String>,
    /// Chunk size (default 16MiB).
    #[arg(long)]
    pub chunk_size: Option<String>,
    /// Memory buffer between readers and the drive (default: 8GiB or a quarter of free memory).
    #[arg(long)]
    pub buffer: Option<String>,
    /// Files up to this size are packed together (default 1MiB).
    #[arg(long)]
    pub pack_max: Option<String>,
    /// Small-file reader threads (default 8).
    #[arg(long)]
    pub workers: Option<usize>,
    /// Read sources at no more than this rate per second, e.g. 200MB.
    #[arg(long)]
    pub read_limit: Option<String>,
    /// Let the drive compress in hardware (default: off).
    #[arg(long)]
    pub drive_compression: bool,
    /// auto/on: feed the drive steadily at the source's rate, up to its rated speed (speed
    /// matching when the source is slower); off: bursts as fast as possible.
    #[arg(long, default_value = "auto")]
    pub pace: String,
    /// full; incremental (changes since the job's last session); differential (since its
    /// last full one); append (only files the job never archived: dash cams, logs).
    /// Anything but full needs --job.
    #[arg(long, default_value = "full")]
    pub level: String,
    /// Files whose data is on tape already are stored by reference instead of written
    /// again: moves (moved or renamed since the job's earlier sessions), all (any archived
    /// file with the same content), off. Found by content hash (xattr, or one read).
    #[arg(long, default_value = "moves")]
    pub dedup: String,
    /// A match on a fingerprint profile (a digest kept in an xattr by another tool) is taken
    /// as is ("trust"), or confirmed by hashing the file ("confirm").
    #[arg(long, default_value = "trust")]
    pub fingerprint_trust: String,
    /// Never refer to data in other sessions (a restore then needs no other session's tapes).
    #[arg(long)]
    pub self_contained: bool,
    /// Archive REAL under the name ARCHIVED (REAL=ARCHIVED, repeatable): back up a ZFS
    /// snapshot (/data/.zfs/snapshot/s1=/data) under the live path, so every run names the
    /// files alike.
    #[arg(long = "map")]
    pub map: Vec<String>,
    /// Once the session is complete, delete the source files it leaves archived unchanged
    /// (moving them to tape), then the directories left empty; the sources themselves stay.
    /// A file changed since it was read stays. See also `ltoarc prune`, which waits for
    /// verified copies.
    #[arg(long)]
    pub delete_source: bool,
    /// Append-only: leave files modified less than this long ago for the next session
    /// (e.g. 10m, 2h), so files still being written are not caught half-way.
    #[arg(long)]
    pub settle: Option<String>,
    /// Continue the newest session of the job (without --job: of these same sources) that
    /// ended partial, interrupted or out of cartridges: what it holds complete is neither read
    /// nor written again, and the new session builds on it.
    #[arg(long)]
    pub resume: bool,
    /// Plan before writing: "scan" (stat-only pass over the sources) or "list:FILE" (a list
    /// of paths, one per line or NUL-separated, e.g. from `find -print0` or a locate
    /// database; assumed current). Refines the pacing decision.
    #[arg(long)]
    pub plan: Option<String>,
}

#[derive(Args)]
pub struct ViewArgs {
    /// Show the archive as it was at this time ("2026-10-30", "2026-10-30 18:00", or unix seconds).
    #[arg(long)]
    pub as_of: Option<String>,
    /// Only this session (id or UUID prefix).
    #[arg(long)]
    pub session: Option<String>,
}

impl ViewArgs {
    pub fn view(&self, ctx: &Ctx) -> Result<View> {
        let as_of = self.as_of.as_deref().map(parse_time).transpose()?;
        let session = match &self.session {
            Some(s) => Some(ctx.cat.session(s)?.ok_or_else(|| anyhow!("no session {s}"))?.id),
            None => None,
        };
        Ok(View { as_of, session })
    }
}

pub fn parse_time(s: &str) -> Result<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let dt = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
        .or_else(|| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(23, 59, 59).unwrap()))
        .ok_or_else(|| anyhow!("cannot read time '{s}'"))?;
    Local.from_local_datetime(&dt).earliest().map(|t| t.timestamp()).ok_or_else(|| anyhow!("invalid local time '{s}'"))
}

/// Progress of a session on the terminal, from a thread of its own (so it keeps moving
/// while the drive positions): a bar redrawn in place when standard error is a terminal,
/// else a line every 10 s (logs; the web interface shows the last one). Rates are decimal
/// MB/s, "now" over the last 3 s.
struct Printer {
    counts: Arc<Counts>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The session writes several copies at once: name the copy of each cartridge.
    copies: bool,
}

#[derive(Default)]
struct Counts {
    files: AtomicU64,
    raw: AtomicU64,
    stored: AtomicU64,
}

impl Printer {
    fn new() -> Self {
        Self::start(None, None)
    }

    /// `total`: bytes the session will read, when planned; `meter`: its buffers and drives.
    fn start(total: Option<u64>, meter: Option<Arc<ltoarc_engine::ringbuf::Meter>>) -> Self {
        use std::io::IsTerminal;
        let counts = Arc::new(Counts::default());
        let stop = Arc::new(AtomicBool::new(false));
        let tty = std::io::stderr().is_terminal();
        let thread = {
            let (counts, stop) = (counts.clone(), stop.clone());
            std::thread::spawn(move || {
                let start = Instant::now();
                let every = if tty { Duration::from_millis(250) } else { Duration::from_secs(10) };
                let mut last = start;
                // (when, bytes read, bytes written to tape), for the rates of the last 3 s.
                let mut samples: VecDeque<(Instant, u64, u64)> = VecDeque::new();
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    let now = Instant::now();
                    let raw = counts.raw.load(Ordering::Relaxed);
                    let written = meter.as_ref().map_or(0, |m| m.written.load(Ordering::Relaxed));
                    samples.push_back((now, raw, written));
                    while samples.len() > 2 && now.duration_since(samples[1].0) >= Duration::from_secs(3) {
                        samples.pop_front();
                    }
                    if now.duration_since(last) < every {
                        continue;
                    }
                    last = now;
                    let (t0, r0, w0) = samples[0];
                    let span = now.duration_since(t0).as_secs_f64().max(0.001);
                    let st = Status {
                        elapsed: now.duration_since(start).as_secs_f64().max(0.001),
                        files: counts.files.load(Ordering::Relaxed),
                        raw,
                        total,
                        now_rate: (raw - r0) as f64 / span,
                        tape: meter.as_ref().map(|m| {
                            let cap = m.capacity.load(Ordering::Relaxed).max(1);
                            ((written - w0) as f64 / span, written, m.buffered.load(Ordering::Relaxed) as f64 / cap as f64)
                        }),
                    };
                    let _term = crate::ui::TERM.lock().unwrap_or_else(|e| e.into_inner());
                    if tty {
                        crate::ui::BAR.store(true, Ordering::Relaxed);
                        eprint!("\r\x1b[K{}", st.bar(crate::ui::width()));
                    } else {
                        eprintln!("{}", st.line());
                    }
                }
                if tty {
                    let _term = crate::ui::TERM.lock().unwrap_or_else(|e| e.into_inner());
                    crate::ui::clear_bar();
                    crate::ui::BAR.store(false, Ordering::Relaxed);
                }
            })
        };
        Printer { counts, stop, thread: Some(thread), copies: false }
    }
}

impl Drop for Printer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One update of a session's progress.
struct Status {
    elapsed: f64,
    files: u64,
    raw: u64,
    total: Option<u64>,
    /// Bytes per second read over the last 3 s.
    now_rate: f64,
    /// (bytes per second the drives took over the last 3 s, bytes they took, buffer fill).
    tape: Option<(f64, u64, f64)>,
}

impl Status {
    fn avg(&self) -> f64 {
        self.raw as f64 / self.elapsed
    }

    fn tape_text(&self) -> Option<String> {
        self.tape.map(|(rate, written, fill)| {
            if written == 0 {
                format!("tape starting, buffer {:.0}%", fill * 100.0)
            } else {
                format!("tape {:.0} MB/s, buffer {:.0}%", rate / 1e6, fill * 100.0)
            }
        })
    }

    /// The log line: "  1234 entries, 56.7 GB, 151.0 MB/s (now 149.8), tape 150 MB/s, buffer 48%".
    fn line(&self) -> String {
        let pct = self.total.filter(|t| *t > 0).map(|t| format!(" of {} ({:.0}%)", human(t), 100.0 * self.raw as f64 / t as f64)).unwrap_or_default();
        let tape = self.tape_text().map(|t| format!(", {t}")).unwrap_or_default();
        format!("  {} entries, {}{pct}, {:.1} MB/s (now {:.1}){tape}", self.files, human(self.raw), self.avg() / 1e6, self.now_rate / 1e6)
    }

    /// The bar: " 42% ▕█████▌      ▏ 612.3 GB/1.4 TB · 296 MB/s (avg 281) · tape 300 MB/s,
    /// buffer 61% · ETA 47 min", fitted to `width` columns: the bar keeps its size and the
    /// parts that do not fit are left out, the tape's first, then the time.
    fn bar(&self, width: usize) -> String {
        let frac = self.total.filter(|t| *t > 0).map(|t| (self.raw as f64 / t as f64).min(1.0));
        let bar_w = if frac.is_some() { (width / 5).clamp(10, 30) } else { 0 };
        let room = width.saturating_sub(if frac.is_some() { bar_w + 9 } else { 1 });
        let bytes = match self.total {
            Some(t) if t > 0 => format!("{}/{}", human(self.raw), human(t)),
            _ => format!("{} files, {}", self.files, human(self.raw)),
        };
        let speed = format!("{:.0} MB/s (avg {:.0})", self.now_rate / 1e6, self.avg() / 1e6);
        let time = match self.total {
            Some(t) if self.raw < t && self.avg() > 0.0 => format!("ETA {}", eta((t - self.raw) as f64 / self.avg())),
            _ => format!("{} elapsed", eta(self.elapsed)),
        };
        // In order of display; kept by priority (lower first) while they fit.
        let parts = [(0, bytes), (1, speed), (3, self.tape_text().unwrap_or_default()), (2, time)];
        let mut keep = [false; 4];
        let mut used = 0;
        for prio in 0..4 {
            for (i, (p, t)) in parts.iter().enumerate() {
                let len = t.chars().count() + if used > 0 { 3 } else { 0 };
                if *p == prio && !t.is_empty() && used + len <= room {
                    keep[i] = true;
                    used += len;
                }
            }
        }
        let text = parts.iter().zip(keep).filter(|(_, k)| *k).map(|((_, t), _)| t.as_str()).collect::<Vec<_>>().join(" · ");
        let Some(f) = frac else { return text };
        let eighths = (f * bar_w as f64 * 8.0).round() as usize;
        let mut bar = "█".repeat(eighths / 8);
        if eighths % 8 > 0 {
            bar.push([' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'][eighths % 8]);
        }
        let pad = bar_w.saturating_sub(bar.chars().count());
        format!("{:>3.0}% ▕{bar}{}▏ {text}", f * 100.0, " ".repeat(pad))
    }
}

/// "47 min", "2 h 05 min", "12 s".
fn eta(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ => format!("{} h {:02} min", s / 3600, (s % 3600) / 60),
    }
}

impl Progress for Printer {
    fn entry(&mut self, _path: &Path, _size: u64) {
        self.counts.files.fetch_add(1, Ordering::Relaxed);
    }

    fn data(&mut self, raw: u64, stored: u64) {
        self.counts.raw.fetch_add(raw, Ordering::Relaxed);
        self.counts.stored.fetch_add(stored, Ordering::Relaxed);
    }

    fn warn(&mut self, msg: &str) {
        crate::note!("warning: {msg}");
    }

    fn volume(&mut self, barcode: &str, copy: u8, seq: u32) {
        if seq > 1 {
            let of = if self.copies || copy > 1 { format!("copy {copy}") } else { "the session".into() };
            crate::note!("  writing to {barcode} (cartridge {seq} of {of})");
        }
    }
}

pub fn human(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

/// A further drive of a parallel backup: its row, open device, first cartridge and pacing.
/// The tape sets of the copies a session writes at once: lanes `0..per_copy` write copy 1
/// from the first set, the next `per_copy` lanes copy 2 from the second.
struct CopySets<'c> {
    sets: Vec<tape::TapeSet<'c>>,
    per_copy: usize,
}

impl backup::NextVolume for CopySets<'_> {
    fn available(&mut self, lane: usize) -> bool {
        self.sets[lane / self.per_copy].available(lane % self.per_copy)
    }

    fn next(&mut self, lane: usize, drive: &mut dyn ltoarc_device::TapeDrive, full: &ltoarc_catalog::VolumeRow) -> Result<backup::Next> {
        self.sets[lane / self.per_copy].next(lane % self.per_copy, drive, full)
    }
}

type ExtraLane = (ltoarc_catalog::DriveRow, Box<dyn ltoarc_device::TapeDrive>, ltoarc_catalog::VolumeRow, Option<ltoarc_engine::ringbuf::Pace>);

pub fn backup(ctx: &Ctx, a: BackupArgs) -> Result<()> {
    let set = tape::tape_set(ctx, &a.tape)?;
    // Append to the first cartridge that is not full and that an attached drive can write
    // (with --overwrite, start it afresh, full or not); the rest follow in order.
    let mut queue: std::collections::VecDeque<String> =
        set.iter().filter(|v| crate::ctx::usable(v, a.overwrite)).map(|v| v.barcode.clone()).collect();
    let first = loop {
        let bc = queue.pop_front().ok_or_else(|| anyhow!("no cartridge in {} has room and a drive that can write it", a.tape))?;
        let v = ctx.volume(&bc)?;
        if v.library_id.is_some_and(|l| ctx.writers(l, &v).is_ok_and(|w| !w.is_empty())) {
            break bc;
        }
        println!("skipping {bc}: no attached drive can write it");
    };
    let names = if a.hashes.is_empty() { vec!["blake3".to_string()] } else { a.hashes.clone() };
    let mut extra = Vec::new();
    for s in &a.set_xattr {
        let (n, t) = s.split_once('=').ok_or_else(|| anyhow!("--set-xattr expects NAME=TEMPLATE"))?;
        extra.push((n.to_string(), t.to_string()));
    }
    let mut opts = backup::Options::new(a.sources.clone());
    opts.job = a.job.clone();
    opts.walk = WalkOptions { excludes: a.exclude.clone(), one_file_system: a.one_file_system };
    opts.hash = Policy {
        profiles: hash::resolve_profiles(&names, &ctx.profiles)?,
        mode: Mode::parse(&a.hash_mode)?,
        write_back: a.write_back,
        extra_xattrs: extra,
    };
    opts.compression = ltoarc_engine::compress::Policy::new(a.zstd, &a.compress_never, &a.compress_always)?;
    if let Some(cs) = &a.chunk_size {
        opts.chunk_size = parse_size(cs)? as usize;
    }
    if let Some(b) = &a.buffer {
        opts.ring = ltoarc_engine::ringbuf::Config::new(parse_size(b)?.max(4 << 20));
    }
    if let Some(p) = &a.pack_max {
        opts.pack_max = parse_size(p)?;
    }
    if let Some(w) = a.workers {
        opts.small_workers = w.max(1);
    }
    opts.read_limit = a.read_limit.as_deref().map(parse_size).transpose()?;
    opts.drive_compression = a.drive_compression;
    opts.encrypt = a.encrypt.as_deref().map(ltoarc_engine::crypt::Key::load).transpose()?;
    opts.level = ltoarc_engine::incr::Level::parse(&a.level)?;
    opts.dedup = ltoarc_engine::incr::Dedup::parse(&a.dedup)?;
    opts.trust = ltoarc_engine::incr::Trust::parse(&a.fingerprint_trust)?;
    opts.self_contained = a.self_contained;
    opts.settle = a.settle.as_deref().map(parse_duration).transpose()?;
    for st in &a.stream {
        let (path, command) = st.split_once('=').ok_or_else(|| anyhow!("--stream expects PATH=COMMAND, not '{st}'"))?;
        if !path.starts_with('/') {
            bail!("--stream {path}: the archived path must be absolute");
        }
        opts.streams.push(backup::StreamSource { path: PathBuf::from(path), command: command.to_string() });
    }
    for m in &a.map {
        let (real, archived) = m.split_once('=').ok_or_else(|| anyhow!("--map expects REAL=ARCHIVED, not '{m}'"))?;
        let real = std::fs::canonicalize(real).with_context(|| format!("--map {real}"))?;
        if !archived.starts_with('/') {
            bail!("--map {m}: the archived name must be an absolute path");
        }
        opts.map.push((real, PathBuf::from(archived)));
    }
    if opts.settle.is_some() && opts.level != ltoarc_engine::incr::Level::Append {
        bail!("--settle applies to --level append");
    }
    if opts.level != ltoarc_engine::incr::Level::Full && opts.job.is_none() {
        bail!("--level {} needs --job: it compares against the job's earlier sessions", a.level);
    }
    opts.resume = a.resume;
    if a.delete_source && !opts.map.is_empty() {
        bail!("--delete-source deletes the files that were read; with --map they are a snapshot's, which cannot be deleted");
    }
    // What the session will compare against: the plan leaves out what it will not save.
    let archived_sources: Vec<PathBuf> =
        opts.sources.iter().map(|s| std::fs::canonicalize(s).map(|c| opts.archived(&c))).collect::<std::io::Result<_>>()?;
    let (mut prev, level) = ltoarc_engine::incr::previous_for(&ctx.cat, opts.job.as_deref(), opts.level, opts.resume, &archived_sources)?;
    if !opts.resume && level == ltoarc_engine::incr::Level::Full {
        if let Some(p) = ltoarc_engine::incr::resumable(&ctx.cat, opts.job.as_deref(), &archived_sources)? {
            println!(
                "note      session {} of {} ended partial ({} files, {}); --resume would save only what it lacks",
                &p.uuid[..8],
                opts.job.as_deref().map_or_else(|| "these sources".to_string(), |j| format!("job {j}")),
                p.files,
                human(p.bytes as u64)
            );
        }
    }
    if opts.resume {
        let base = prev.base.as_ref().map(|b| b.uuid[..8].to_string()).unwrap_or_default();
        println!("resume    continuing session {base}: {} entries it holds are not saved again unless changed", prev.len());
    }
    let append = level == ltoarc_engine::incr::Level::Append;
    let mut keep = |p: &Path, md: &std::fs::Metadata| match prev.classify(&opts.archived(p), md) {
        ltoarc_engine::incr::Found::Unchanged => false,
        ltoarc_engine::incr::Found::Changed(_) => !append,
        ltoarc_engine::incr::Found::New => true,
    };
    // Plan and pacing from stored benchmarks.
    let plan = match a.plan.as_deref() {
        None => None,
        Some("scan") => {
            crate::note!("planning: scanning sources ...");
            Some(ltoarc_engine::plan::scan(&opts.sources, &opts.walk, opts.pack_max, &mut keep)?)
        }
        Some(p) if p.starts_with("list:") => Some(ltoarc_engine::plan::from_list(Path::new(&p[5..]), &opts.sources, opts.pack_max, &mut keep)?),
        Some(other) => bail!("--plan expects scan or list:FILE, not '{other}'"),
    };
    drop(keep);
    if let Some(p) = plan.as_ref().filter(|p| p.skipped > 0) {
        println!("unchanged {} entries ({}) are as the sessions this one builds on hold them: not saved again", p.skipped, human(p.skipped_bytes));
    }
    // The sessions this one builds on: with it, they hold what --delete-source may delete.
    let compared = prev.sessions.clone();
    drop(prev);
    let mode = match a.pace.as_str() {
        "auto" => ltoarc_engine::plan::PaceMode::Auto,
        "on" => ltoarc_engine::plan::PaceMode::On,
        "off" => ltoarc_engine::plan::PaceMode::Off,
        other => bail!("--pace expects auto, on or off, not '{other}'"),
    };
    let mut rates = Vec::new();
    for s in &opts.sources {
        let key = ltoarc_engine::bench::fs_key(s);
        if let Some((_, b)) = ctx.cat.benchmark("source", &key)? {
            if let Ok(b) = serde_json::from_value::<ltoarc_engine::bench::SourceBench>(b) {
                rates.push(ltoarc_engine::plan::source_rate(&b, plan.as_ref()));
                // Parallel streams slower than one (busy or fragmented disks): read large
                // files one at a time.
                if b.seq_parallel_mbps > 0.0 && b.seq_parallel_mbps < b.seq_mbps {
                    opts.large_workers = 1;
                }
            }
        }
    }
    // The slowest source sets the pace (one is enough to stall the drive).
    let source_rate = (rates.len() == opts.sources.len() && !rates.is_empty()).then(|| rates.iter().cloned().fold(f64::MAX, f64::min));
    // Which cartridges it would take (copy 1, and copy 2 with --tape2), before any is loaded.
    let need = plan.as_ref().map(|p| p.bytes());
    let order: Vec<ltoarc_catalog::VolumeRow> = std::iter::once(&first).chain(queue.iter()).map(|b| ctx.volume(b)).collect::<Result<_>>()?;
    let k = crate::fit::show(ctx, &order, need, a.overwrite, "tapes")?;
    let mut at_risk: Vec<ltoarc_catalog::VolumeRow> = order[..k.min(order.len())].to_vec();
    if let Some(t2) = &a.tape2 {
        let ids: Vec<i64> = set.iter().map(|v| v.id).collect();
        let order2: Vec<ltoarc_catalog::VolumeRow> = tape::tape_set(ctx, t2)?.into_iter().filter(|v| crate::ctx::usable(v, a.overwrite) && !ids.contains(&v.id)).collect();
        let k2 = crate::fit::show(ctx, &order2, need, a.overwrite, "copy 2")?;
        at_risk.extend(order2[..k2.min(order2.len())].iter().cloned());
    }
    if a.overwrite {
        confirm_overwrite(ctx, &at_risk, a.yes || a.dry_run)?;
    }
    if a.dry_run {
        return Ok(());
    }
    let (d, mut drive) = tape::load_rw(ctx, &first, a.drive.as_deref())?;
    let pool = a.pool.clone().or_else(|| set.iter().find_map(|v| v.pool.clone()));
    let first_vol = tape::ready_for_append(ctx, &d, &mut *drive, &first, pool.clone(), a.overwrite)?;
    let media_gen = first_vol.generation.clone().unwrap_or_default();
    // The drive's spec-sheet range; 90% of its rated rate is the planning estimate.
    // With several drives each gets a share of what the source delivers.
    let (decision, rates, rates_from) = tape::pacing(ctx, &d, &media_gen, mode, source_rate.map(|r| r / a.drives.max(1) as f64))?;
    let drive_est = rates.estimate();
    opts.ring.pace = decision.pace;
    // A source known to outrun the drive needs only a short head start before the drive
    // starts (about two seconds of drive throughput); otherwise fill half the buffer.
    if decision.pace.is_none() && source_rate.is_some_and(|r| r >= drive_est) && a.buffer.is_none() {
        opts.ring.high = opts.ring.high.min((2.0 * drive_est) as u64);
    }
    if let Some(p) = &plan {
        let eta = source_rate.map(|r| p.bytes() as f64 / r.min(drive_est)).unwrap_or(p.bytes() as f64 / drive_est);
        println!(
            "plan      {} large files ({}), {} small files ({}); about {:.0} min",
            p.large_files,
            human(p.large_bytes),
            p.small_files,
            human(p.small_bytes),
            eta / 60.0
        );
    }
    println!("pace      {}", decision.reason);
    println!(
        "drive     rated {:.0} MB/s (planned at {:.0}), speed matching down to {:.0} MB/s{} ({rates_from}{})",
        rates.rated / 1e6,
        drive_est / 1e6,
        rates.slowest / 1e6,
        rates.gap.map(|(lo, hi)| format!(", not between {:.0} and {:.0}", lo / 1e6, hi / 1e6)).unwrap_or_default(),
        if rates_from == "default" { format!("; set [rates.\"{}\"] {media_gen} = \"SLOWEST-RATED\" from the spec sheet", d.product.trim()) } else { String::new() }
    );
    // More drives: each starts on the next cartridge of the set that a free drive can write.
    if a.drives > 1 && a.drive_compression {
        bail!("--drives splits files by size, so drive compression must stay off");
    }
    let mut more: Vec<ExtraLane> = Vec::new();
    while 1 + more.len() < a.drives {
        let pick = queue.iter().position(|bc| tape::free_writer(ctx, bc).ok().flatten().is_some());
        let Some(i) = pick else {
            bail!("only {} drive(s) can take part: no further free drive can write a cartridge of the set", 1 + more.len());
        };
        let bc = queue.remove(i).expect("position is valid");
        let dr = tape::free_writer(ctx, &bc)?.expect("checked");
        let (d2, mut drive2) = tape::load_rw(ctx, &bc, Some(&dr.name))?;
        let v2 = tape::ready_for_append(ctx, &d2, &mut *drive2, &bc, pool.clone(), a.overwrite)?;
        let (dec2, _, _) = tape::pacing(ctx, &d2, v2.generation.as_deref().unwrap_or(""), mode, source_rate.map(|r| r / a.drives as f64))?;
        println!("drive     {} writes {bc} in parallel", d2.name);
        more.push((d2, drive2, v2, dec2.pace));
    }
    // Copy 2 at the same time: one drive per copy-1 drive, each on the next cartridge of
    // --tape2 that a free drive can write.
    let mut copy2: Vec<ExtraLane> = Vec::new();
    let mut queue2: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    if let Some(t2) = &a.tape2 {
        let set1: Vec<i64> = set.iter().map(|v| v.id).collect();
        let set2 = tape::tape_set(ctx, t2)?;
        queue2 = set2.iter().filter(|v| crate::ctx::usable(v, a.overwrite) && !set1.contains(&v.id)).map(|v| v.barcode.clone()).collect();
        let pool2 = set2.iter().find_map(|v| v.pool.clone()).or_else(|| pool.clone());
        while copy2.len() < a.drives.max(1) {
            let Some(i) = queue2.iter().position(|bc| tape::free_writer(ctx, bc).ok().flatten().is_some()) else {
                bail!("copy 2 needs {} more free drive(s) that can write a cartridge of {t2}", a.drives.max(1) - copy2.len());
            };
            let bc = queue2.remove(i).expect("position is valid");
            let dr = tape::free_writer(ctx, &bc)?.expect("checked");
            let (d2, mut drive2) = tape::load_rw(ctx, &bc, Some(&dr.name))?;
            let v2 = tape::ready_for_append(ctx, &d2, &mut *drive2, &bc, pool2.clone(), a.overwrite)?;
            let (dec2, rates2, _) = tape::pacing(ctx, &d2, v2.generation.as_deref().unwrap_or(""), mode, source_rate.map(|r| r / a.drives.max(1) as f64))?;
            let (e1, e2) = (drive_est, rates2.estimate());
            let (slow, fast) = if e2 < e1 { (&d2.name, &d.name) } else { (&d.name, &d2.name) };
            println!(
                "copy 2    to {bc} in drive {}; both copies go at the slower drive's pace (about {:.0} MB/s): drive {fast} is matched to drive {slow}",
                d2.name,
                e1.min(e2) / 1e6
            );
            copy2.push((d2, drive2, v2, dec2.pace));
        }
        opts.copies = 2;
    }
    crate::note!("backing up to {first} in drive {} ...", d.name);
    let meter = Arc::new(ltoarc_engine::ringbuf::Meter::default());
    opts.meter = Some(meter.clone());
    let mut p = Printer::start(plan.as_ref().map(|p| p.bytes()), Some(meter));
    p.copies = opts.copies > 1;
    let mut lane_drives = vec![d.clone()];
    lane_drives.extend(more.iter().map(|m| m.0.clone()));
    let mut sets = vec![tape::TapeSet { ctx, drives: lane_drives, rest: queue, pool: pool.clone(), mode, source_rate, overwrite: a.overwrite }];
    if !copy2.is_empty() {
        let drives2 = copy2.iter().map(|m| m.0.clone()).collect();
        sets.push(tape::TapeSet { ctx, drives: drives2, rest: queue2, pool, mode, source_rate, overwrite: a.overwrite });
    }
    let mut next = CopySets { sets, per_copy: 1 + more.len() };
    let mut starts = vec![backup::LaneStart { drive: &mut *drive, volume: first_vol.clone(), pace: opts.ring.pace }];
    for (_, dr, v, pace) in more.iter_mut() {
        starts.push(backup::LaneStart { drive: &mut **dr, volume: v.clone(), pace: *pace });
    }
    let mut copies = vec![starts];
    if !copy2.is_empty() {
        copies.push(copy2.iter_mut().map(|(_, dr, v, pace)| backup::LaneStart { drive: &mut **dr, volume: v.clone(), pace: *pace }).collect());
    }
    let r = backup::backup_copies(&ctx.cat, copies, &opts, &mut next, &mut p);
    drop(p);
    let r = r?;
    let barcodes: Vec<&str> = r.volumes.iter().map(|v| v.barcode.as_str()).collect();
    let all_drives: Vec<ltoarc_catalog::DriveRow> = next.sets.iter().flat_map(|s| s.drives.clone()).collect();
    // `drive` still holds what lane 0 wrote last (other lanes and copy 2 end on other drives).
    if next.sets[0].drives[0].id == d.id {
        ctx.snapshot(&d, &mut *drive, r.volumes.iter().rev().find(|v| v.lane == 0).map(|v| v.barcode.as_str()), "backup");
    }
    ctx.cat.log_event("backup", &r.session_uuid.to_string(), &format!("{} on {}: {} files, {}", r.status, barcodes.join(","), r.files, human(r.bytes)))?;
    let secs = r.elapsed.as_secs_f64().max(0.001);
    println!(
        "session   {} ({}, {}{})",
        r.session_uuid,
        r.status,
        r.level,
        r.parent.map(|p| format!(" after {}", &p.to_string()[..8])).unwrap_or_default()
    );
    if r.level == "full" && a.level != "full" {
        println!("note      job {} had no full session yet, so this one is full", a.job.as_deref().unwrap_or("?"));
    }
    println!("entries   {} files, {} directories, {} other", r.files, r.dirs, r.others);
    if r.level != "full" {
        println!("unchanged {} entries (not read)", r.unchanged);
    }
    if r.whiteouts > 0 {
        println!("deleted   {} entries since the previous session (recorded as deleted)", r.whiteouts);
    }
    if r.referenced > 0 {
        println!("by ref.   {} files, {} already on tape (moved or duplicate; not written again)", r.referenced, human(r.referenced_bytes));
    }
    if r.settling > 0 {
        println!("settling  {} files modified too recently; left for the next session", r.settling);
    }
    for c in r.changed_archived.iter().take(10) {
        println!("WARNING: changed after it was archived (not saved again): {c}");
    }
    if r.changed_archived.len() > 10 {
        println!("WARNING: ... {} more files changed after they were archived", r.changed_archived.len() - 10);
    }
    println!("data      {} read, {} written ({:.1} MB/s)", human(r.bytes), human(r.stored), r.bytes as f64 / secs / 1e6);
    for v in &r.volumes {
        println!(
            "tape      {}{} blocks {}..{}, catalog at {}",
            v.barcode,
            if opts.copies > 1 { format!(" (copy {})", v.copy) } else { String::new() },
            v.start_object,
            v.end_object,
            v.catalog_object.map_or("-".into(), |c| c.to_string())
        );
    }
    for (i, w) in r.writers.iter().enumerate() {
        let who = if r.writers.len() > 1 { format!("{}: ", all_drives[i].name) } else { String::new() };
        println!("drive     {who}{}", writer_line(w, secs));
    }
    for c in r.changed.iter().take(10) {
        println!("changed while reading: {c}");
    }
    for m in &r.mismatches {
        println!("HASH MISMATCH: {m}");
    }
    if !r.warnings.is_empty() {
        println!("{} warnings", r.warnings.len());
    }
    if r.early_warning {
        println!("{} is full and the tape set had no further cartridge; the session is partial", barcodes.last().unwrap_or(&"?"));
    }
    if a.delete_source {
        if r.status == "complete" {
            let mut ids = compared;
            ids.extend(ctx.cat.session(&r.session_uuid.to_string())?.map(|s| s.id));
            delete_sources(ctx, &opts.sources, &ids)?;
        } else {
            println!("deleted   nothing: the session is {} (--delete-source waits for a complete one; --resume completes it)", r.status);
        }
    }
    Ok(())
}

/// Delete what `sources` hold that `sessions` (the new session and those it builds on) have
/// archived unchanged: files and symbolic links whose newest version there is intact and
/// still matches them (size, modification and change times, inode), then directories left
/// empty (not the sources themselves).
fn delete_sources(ctx: &Ctx, sources: &[PathBuf], sessions: &[i64]) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let (mut files, mut bytes, mut kept, mut failed, mut dirs) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let state = ctx.cat.state_of(sessions)?;
    for src in sources {
        let src = std::fs::canonicalize(src)?;
        let mut dir_list: Vec<PathBuf> = Vec::new();
        for (path, o) in &state {
            let p = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            if !p.starts_with(&src) {
                continue;
            }
            match o.kind.as_str() {
                "dir" => {
                    if p != src {
                        dir_list.push(p);
                    }
                    continue;
                }
                "file" | "symlink" | "hardlink" => {}
                _ => continue,
            }
            let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
            let same = o.status == "ok"
                && ltoarc_engine::meta::mtime_ns(&md) == o.mtime_ns
                && ltoarc_engine::meta::ctime_ns(&md) == o.ctime_ns
                && md.ino() as i64 == o.ino
                && (o.kind != "file" || md.len() as i64 == o.size);
            if !same {
                kept += 1;
                continue;
            }
            match std::fs::remove_file(&p) {
                Ok(()) => {
                    files += 1;
                    bytes += md.len();
                }
                Err(e) => {
                    failed += 1;
                    crate::note!("warning: not deleted: {}: {e}", p.display());
                }
            }
        }
        // Deepest first, so a directory's empty subdirectories go before it.
        dir_list.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dir_list {
            if std::fs::remove_dir(&d).is_ok() {
                dirs += 1;
            }
        }
    }
    println!(
        "deleted   {files} files ({}) and {dirs} empty directories from the sources{}{}",
        human(bytes),
        if kept > 0 { format!("; {kept} kept (changed since archived, or not archived complete)") } else { String::new() },
        if failed > 0 { format!("; {failed} could not be deleted") } else { String::new() }
    );
    ctx.cat.log_event("delete-source", &sources.iter().map(|s| s.display().to_string()).collect::<Vec<_>>().join(","), &format!("{files} files, {}", human(bytes)))?;
    Ok(())
}

/// With --overwrite: what the cartridges that may be rewritten hold (sessions, and those of
/// them no other copy holds), and the go-ahead for it.
fn confirm_overwrite(ctx: &Ctx, vols: &[ltoarc_catalog::VolumeRow], yes: bool) -> Result<()> {
    confirm_loss(ctx, vols, yes, "overwrite")
}

/// What the cartridges about to be overwritten (`verb` "overwrite") or forgotten ("forget")
/// hold: their sessions, and how many of those no other copy holds. Asks unless `yes`.
pub fn confirm_loss(ctx: &Ctx, vols: &[ltoarc_catalog::VolumeRow], yes: bool, verb: &str) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let ids: std::collections::HashSet<i64> = vols.iter().map(|v| v.id).collect();
    let sessions = ctx.cat.sessions()?;
    let mut lines = Vec::new();
    let mut lost = 0;
    for v in vols {
        let parts = ctx.cat.volume_sessions(v.id)?;
        if parts.is_empty() {
            continue;
        }
        let mut sids: Vec<i64> = parts.iter().map(|p| p.session_id).collect();
        sids.sort();
        sids.dedup();
        let mut only = 0;
        for sid in &sids {
            // Safe if some copy of it lies wholly on cartridges that stay.
            let svs = ctx.cat.session_volumes(*sid)?;
            let mut copies: Vec<i64> = svs.iter().map(|sv| sv.copy).collect();
            copies.dedup();
            let safe = copies.iter().any(|c| svs.iter().filter(|sv| sv.copy == *c).all(|sv| !ids.contains(&sv.volume_id)));
            if !safe {
                only += 1;
            }
        }
        lost += only;
        let when = |id: &i64| sessions.iter().find(|s| s.id == *id).map(|s| crate::table::when(s.started)).unwrap_or_default();
        lines.push(format!(
            "          {}: {} session(s), {} .. {}; {only} held nowhere else",
            v.barcode,
            sids.len(),
            when(&sids[0]),
            when(&sids[sids.len() - 1])
        ));
    }
    if lines.is_empty() {
        return Ok(());
    }
    if verb == "overwrite" {
        println!("overwrite these cartridges are rewritten from the start:");
    } else {
        println!("{verb:<9} these cartridges leave the catalog with what they hold:");
    }
    for l in &lines {
        println!("{l}");
    }
    if lost > 0 {
        println!("WARNING   {lost} session(s) would be lost: no other copy holds them");
    }
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("add --yes to {verb} them");
    }
    let mut cap = verb.to_string();
    cap[..1].make_ascii_uppercase();
    print!("{cap} them? Type yes: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim() != "yes" {
        bail!("nothing changed");
    }
    Ok(())
}

/// How a drive wrote: speed, the rate it was matched to, and the stops it reported.
fn writer_line(w: &ltoarc_engine::ringbuf::WriterStats, secs: f64) -> String {
    let busy = w.write_time.as_secs_f64().max(0.001);
    format!(
        "{} at {:.1} MB/s while writing ({:.0}% of the time), {}, buffer ran dry {} time(s); drive: {}",
        human(w.bytes),
        w.bytes as f64 / busy / 1e6,
        100.0 * busy / secs.max(0.001),
        w.matched.describe().unwrap_or_else(|| "full speed".into()),
        w.underruns,
        w.drive.map_or_else(|| "no motion counters".into(), |m| m.describe_write(w.bytes))
    )
}

/// "90s", "10m", "2h", "1d" (or plain seconds).
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()));
    let n: f64 = num.parse().map_err(|_| anyhow!("bad duration '{s}' (e.g. 90s, 10m, 2h, 1d)"))?;
    let secs = match unit.trim() {
        "" | "s" => n,
        "m" | "min" => n * 60.0,
        "h" => n * 3600.0,
        "d" => n * 86400.0,
        other => bail!("bad duration unit '{other}' in '{s}' (s, m, h, d)"),
    };
    Ok(Duration::from_secs_f64(secs))
}

pub fn sessions(ctx: &Ctx, tape: Option<&str>) -> Result<()> {
    let only = tape.map(|t| ctx.volume(t)).transpose()?;
    let mut t = Table::new(&["SESSION", "JOB", "LEVEL", "STARTED", "STATUS", "FILES", "DATA", "STORED", "TAPES"]);
    for s in ctx.cat.sessions()? {
        let svs = ctx.cat.session_volumes(s.id)?;
        if only.as_ref().is_some_and(|v| !svs.iter().any(|sv| sv.volume_id == v.id)) {
            continue;
        }
        let tapes: Vec<String> = ctx
            .cat
            .volumes()?
            .into_iter()
            .filter(|v| svs.iter().any(|sv| sv.volume_id == v.id))
            .map(|v| v.barcode)
            .collect();
        t.row(vec![
            s.uuid[..8].to_string(),
            s.job.clone().unwrap_or_else(|| "-".into()),
            s.level.clone(),
            when(s.started),
            if s.expired_at.is_some() { "expired".into() } else { s.status.clone() },
            s.files.to_string(),
            human(s.bytes as u64),
            human(s.stored_bytes as u64),
            tapes.join(","),
        ]);
    }
    if t.is_empty() {
        println!("no sessions");
    } else {
        t.print();
    }
    Ok(())
}

fn mode_string(kind: &str, mode: i64) -> String {
    let t = match kind {
        "dir" => 'd',
        "symlink" => 'l',
        "fifo" => 'p',
        "char" => 'c',
        "block" => 'b',
        "socket" => 's',
        "hardlink" => 'h',
        _ => '-',
    };
    let mut s = String::from(t);
    for shift in [6, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub fn ls(ctx: &Ctx, path: Option<PathBuf>, long: bool, view: View) -> Result<()> {
    let path = path.unwrap_or_else(|| PathBuf::from("/"));
    let id = ctx.cat.lookup_path(path.as_os_str().as_bytes())?.ok_or_else(|| anyhow!("{} is not in the catalog", path.display()))?;
    let entries = ctx.cat.entries(id, view)?;
    if entries.is_empty() {
        match ctx.cat.latest(id, view)? {
            Some(o) => println!("{} {:>12} {}", mode_string(&o.kind, o.mode), o.size, path.display()),
            None if id != ROOT_PATH => bail!("{} has no version in this view", path.display()),
            None => {}
        }
        return Ok(());
    }
    for (_, name, o) in entries {
        let name = String::from_utf8_lossy(&name);
        match (long, o) {
            (true, Some(o)) => {
                let t = when(o.mtime_ns.div_euclid(1_000_000_000));
                let link = o.target.as_ref().map(|t| format!(" -> {}", String::from_utf8_lossy(t))).unwrap_or_default();
                let status = if o.status == "ok" { String::new() } else { format!("  [{}]", o.status) };
                println!("{} {:>8} {:>8} {:>14} {t} {name}{link}{status}", mode_string(&o.kind, o.mode), o.user.clone().unwrap_or(o.uid.to_string()), o.grp.clone().unwrap_or(o.gid.to_string()), o.size);
            }
            (true, None) => println!("d?????????        -        - {:>14} {:16} {name}", "-", ""),
            (false, Some(o)) if o.kind == "dir" => println!("{name}/"),
            (false, None) => println!("{name}/"),
            (false, Some(_)) => println!("{name}"),
        }
    }
    Ok(())
}

pub fn find(ctx: &Ctx, pattern: &str, under: Option<PathBuf>, view: View) -> Result<()> {
    let root = match &under {
        Some(p) => ctx.cat.lookup_path(p.as_os_str().as_bytes())?.ok_or_else(|| anyhow!("{} is not in the catalog", p.display()))?,
        None => ROOT_PATH,
    };
    let pat = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
    let m = Glob::new(&pat)?.compile_matcher();
    let mut n = 0;
    for (path, o) in ctx.cat.resolve(root, view)? {
        let p = String::from_utf8_lossy(&path);
        if m.is_match(p.as_ref()) {
            let locs: Vec<String> = ctx.cat.object_locations(o.id)?.into_iter().map(|(_, b, _, blk)| format!("{b}@{blk}")).collect();
            println!("{p}{}", if locs.is_empty() { String::new() } else { format!("  [{}]", locs.join(" ")) });
            n += 1;
        }
    }
    crate::note!("{n} matches");
    Ok(())
}

#[derive(Args)]
pub struct RestoreArgs {
    /// Archived paths to restore (each with everything below it).
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Destination directory.
    #[arg(long)]
    pub to: PathBuf,
    /// Remove this prefix from archived paths (default: keep the full path under --to).
    #[arg(long)]
    pub strip_prefix: Option<PathBuf>,
    /// Replace files that already exist (default: keep them and stop).
    #[arg(long)]
    pub overwrite: bool,
    /// Use only this copy.
    #[arg(long)]
    pub copy: Option<i64>,
    /// Drive to read in (default: a free one that can read each cartridge's generation).
    #[arg(long)]
    pub drive: Option<String>,
    /// Show what would be read from which cartridges, and stop.
    #[arg(long)]
    pub dry_run: bool,
    /// Memory for reading ahead of the restore, so the drive keeps streaming when the
    /// destination is slower (default: 2GiB or a quarter of free memory).
    #[arg(long)]
    pub buffer: Option<String>,
    /// Read at full speed in bursts, instead of slowing the drive to the restore's pace.
    #[arg(long)]
    pub no_pace: bool,
    #[command(flatten)]
    pub view: ViewArgs,
}

pub fn restore(ctx: &Ctx, a: RestoreArgs) -> Result<()> {
    let opts = restore::Options {
        dest: a.to.clone(),
        strip_prefix: a.strip_prefix.clone(),
        overwrite: a.overwrite,
        view: a.view.view(ctx)?,
        copy: a.copy,
        owners: None,
        keys: ctx.keys.clone(),
        read_buffer: a.buffer.as_deref().map(parse_size).transpose()?,
    };
    let plan = restore::plan(&ctx.cat, &a.paths, &opts)?;
    let barcodes = plan.barcodes();
    println!("{} entries, {} of data, from {}", plan.items.len(), human(plan.bytes), if barcodes.is_empty() { "no cartridge".into() } else { barcodes.join(", ") });
    for b in &barcodes {
        let v = ctx.volume(b)?;
        println!("  {b}: {} chunk(s); {}", plan.volumes[b].len(), ctx.location(&v)?);
    }
    if a.dry_run {
        return Ok(());
    }
    let mut r = Restorer::new(plan, opts)?;
    // One drive works through the cartridges, swapping them in place within a library.
    let mut current: Option<(ltoarc_catalog::DriveRow, Box<dyn ltoarc_device::TapeDrive>, String)> = None;
    for b in &barcodes {
        let (d, mut drive) = match current.take() {
            Some((d, mut drive, held))
                if ctx.volume(b)?.library_id == d.library_id
                    && crate::ctx::access(&d, &ctx.volume(b)?) != ltoarc_device::Access::None
                    && ctx.drive_holding(b)?.is_none() =>
            {
                tape::exchange(ctx, &d, &mut *drive, &held, b)?;
                (d, drive)
            }
            other => {
                // Another drive takes this cartridge: the previous one is free for others.
                if let Some((d, _, _)) = &other {
                    ctx.leases.release(d);
                }
                drop(other);
                tape::load(ctx, b, a.drive.as_deref())?
            }
        };
        crate::note!("reading {b} in drive {} ...", d.name);
        r.set_pace(read_pace(ctx, &d, &ctx.volume(b)?, a.no_pace)?);
        r.volume(b, &mut *drive).with_context(|| format!("restoring from {b}"))?;
        current = Some((d, drive, b.clone()));
    }
    let rep = r.finish()?;
    println!(
        "restored {} files, {} directories, {} other; {} in {} chunks; {} hashes verified",
        rep.files,
        rep.dirs,
        rep.others,
        human(rep.bytes),
        rep.chunks,
        rep.verified_hashes
    );
    println!("drive     {}", rep.read.describe());
    for w in rep.warnings.iter().take(20) {
        crate::note!("warning: {w}");
    }
    for e in &rep.errors {
        crate::note!("ERROR: {e}");
    }
    if !rep.errors.is_empty() {
        bail!("{} errors during restore", rep.errors.len());
    }
    Ok(())
}

/// Speed-matching range for reading `v` in drive `d` (None: full speed, `--no-pace`).
pub fn read_pace(ctx: &Ctx, d: &ltoarc_catalog::DriveRow, v: &ltoarc_catalog::VolumeRow, no_pace: bool) -> Result<Option<ltoarc_engine::ringbuf::Pace>> {
    if no_pace {
        return Ok(None);
    }
    let (_, rates, _) = tape::pacing(ctx, d, v.generation.as_deref().unwrap_or(""), ltoarc_engine::plan::PaceMode::Off, None)?;
    // Reads stream at any rate on the drives measured (no stops even far below the slowest
    // write speed), so a gap in write speed matching does not apply.
    Ok(Some(ltoarc_engine::ringbuf::Pace { gap: None, ..rates.pace() }))
}

pub fn verify(ctx: &Ctx, barcode: &str, drive: Option<&str>, no_pace: bool) -> Result<()> {
    let v = ctx.volume(barcode)?;
    let (d, mut dr) = tape::load(ctx, barcode, drive)?;
    crate::note!("verifying {barcode} in drive {} ...", d.name);
    let start = Instant::now();
    let r = verify::verify_volume_paced(&ctx.cat, &mut *dr, &v, &ctx.keys, read_pace(ctx, &d, &v, no_pace)?)?;
    ctx.snapshot(&d, &mut *dr, Some(barcode), "verify");
    println!(
        "{} sessions, {} files, {} chunks, {} read ({:.1} MB/s overall); {} file hashes and {} catalog(s) checked",
        r.sessions,
        r.files,
        r.chunks,
        human(r.bytes),
        r.bytes as f64 / start.elapsed().as_secs_f64().max(0.001) / 1e6,
        r.file_hashes,
        r.catalogs
    );
    println!("drive     {}", r.read.describe());
    ctx.cat.log_event("verify", barcode, &format!("{} errors, {} missing chunks", r.errors.len(), r.missing))?;
    if r.errors.is_empty() && r.missing == 0 {
        ctx.cat.set_verified(v.id, ltoarc_catalog::now())?;
    }
    for e in &r.errors {
        crate::note!("ERROR: {e}");
    }
    if !r.errors.is_empty() || r.missing > 0 {
        bail!("{barcode}: {} errors, {} chunks missing", r.errors.len(), r.missing);
    }
    println!("{barcode}: OK");
    Ok(())
}

pub fn scan(ctx: &Ctx, barcode: &str, replace: bool, drive: Option<&str>) -> Result<()> {
    let v = ctx.volume(barcode)?;
    let (d, mut dr) = tape::load(ctx, barcode, drive)?;
    crate::note!("scanning {barcode} in drive {} ...", d.name);
    let r = scan::scan_volume(&ctx.cat, &mut *dr, &v, replace, &ctx.keys)?;
    println!(
        "{} sessions added, {} already known; {} entries, {} chunks, {}; {} catalog stream(s), {}{} other file(s)",
        r.added.len(),
        r.skipped.len(),
        r.files,
        r.chunks,
        human(r.bytes),
        r.catalogs,
        if r.bootstrap { "bootstrap file, " } else { "" },
        r.foreign_files
    );
    for e in &r.errors {
        crate::note!("ERROR: {e}");
    }
    Ok(())
}

pub struct CalibrateArgs {
    pub tape: Option<String>,
    pub drive: Option<String>,
    pub size: String,
    pub seconds: u64,
    pub source: Option<PathBuf>,
    pub via_st: bool,
    pub sweep: Option<String>,
    pub sweep_step: f64,
    pub sweep_low: Option<f64>,
    pub yes: bool,
}

pub fn calibrate(ctx: &Ctx, a: &CalibrateArgs) -> Result<()> {
    let bytes = parse_size(&a.size)?;
    let limit = Duration::from_secs(a.seconds);
    let sweep = a
        .sweep
        .as_deref()
        .map(parse_duration)
        .transpose()?
        .map(|d| ltoarc_engine::calibrate::Sweep { secs: d.as_secs_f64().max(5.0), step: a.sweep_step.max(1.0), low: a.sweep_low });
    let mut drive_rate = None;
    if let Some(bc) = a.tape.as_deref() {
        let v = ctx.volume(bc)?;
        if v.uuid.is_none() {
            bail!("{bc} is not an ltoarc cartridge; run `ltoarc tape init` first");
        }
        if !a.yes {
            println!(
                "dry run: would append up to {} of test data to {bc} after its end of data{}, read it back, and remove it.",
                human(bytes),
                sweep.map(|s| format!(", then write {:.0} s at each rate of the sweep", s.secs)).unwrap_or_default()
            );
            println!("re-run with --yes");
            return Ok(());
        }
        let (d, mut dr) = tape::load_rw(ctx, bc, a.drive.as_deref())?;
        crate::note!("calibrating drive {} with {bc} ...", d.name);
        let st = if a.via_st {
            Some(d.st_path.clone().ok_or_else(|| anyhow!("drive {} has no st device", d.name))?)
        } else {
            None
        };
        let r = ltoarc_engine::calibrate::drive(&mut *dr, &v, bytes, limit, st.as_deref(), sweep)?;
        ctx.snapshot(&d, &mut *dr, Some(bc), "calibrate");
        println!(
            "drive {}: write {:.1} MB/s streaming, {:.1} MB/s including the final flush; read {:.1} MB/s ({} in {} KiB blocks){}",
            d.name,
            r.steady_write_mbps,
            r.write_mbps,
            r.read_mbps,
            human(r.bytes),
            r.block_size / 1024,
            match r.write_cycles {
                Some(c) => format!("; {c} tape start/stop cycle(s)"),
                None => String::new(),
            }
        );
        let (lo, p50, p90, hi) = r.write_latency_ms;
        println!(
            "  per-WRITE latency via {}: min {lo:.2} ms, median {p50:.2} ms, p90 {p90:.2} ms, max {hi:.1} ms",
            if r.via_st { "st" } else { "sg" }
        );
        if !r.sweep.is_empty() {
            println!("  sweep (each rate from a standstill to a standstill; pauses counted by the drive):");
            let secs = sweep.map_or(30.0, |s| s.secs);
            for (rate, cycles, pauses) in &r.sweep {
                let verdict = if *pauses <= ltoarc_engine::calibrate::SWEEP_PAUSES_OK { "streams" } else { "pauses" };
                let per_gb = *pauses as f64 / (rate * secs / 1000.0);
                println!("    {rate:5.0} MB/s: {pauses:3} pause(s) ({per_gb:4.1} per GB), {cycles} tape start(s)  {verdict}");
            }
        }
        let floor = r.floor_mbps.unwrap_or(r.steady_write_mbps / 3.0);
        let fmt = |rs: &[(f64, f64)]| rs.iter().map(|(a, b)| format!("{a:.0}-{b:.0}")).collect::<Vec<_>>().join(", ");
        match r.ranges.as_slice() {
            [] if r.sweep.is_empty() => println!("  speed-matching floor: about {floor:.0} MB/s (estimated as a third of the maximum; use --sweep to map the speeds the drive matches)"),
            [] => println!("  the drive paused at every rate tried"),
            rs => {
                println!("  streams without pausing at {} MB/s{}", fmt(rs), if rs.len() > 1 { "; pauses in between" } else { "" });
                // [rates] takes one range, or two with the gap between: the slowest and the fastest.
                let pick: Vec<(f64, f64)> = if rs.len() > 2 { vec![rs[0], rs[rs.len() - 1]] } else { rs.to_vec() };
                let generation = v.generation.as_deref().unwrap_or("LTO-?");
                println!("  for the config (the slowest step tried was {:.0} MB/s; the drive may go lower):", r.sweep.last().map_or(0.0, |s| s.0));
                println!("    [rates.\"{}\"]", d.product.trim());
                println!("    {generation} = \"{}\"", fmt(&pick));
            }
        }
        let bench = ltoarc_engine::bench::DriveBench {
            generation: v.generation.clone().unwrap_or_default(),
            max_write_mbps: r.steady_write_mbps,
            read_mbps: r.read_mbps,
            floor_mbps: floor,
            floor_measured: r.floor_mbps.is_some(),
        };
        ctx.cat.add_benchmark("drive", &drive_bench_key(&d.serial, v.generation.as_deref()), &bench)?;
        println!(
            "  (for diagnosis only: backups plan with 90% of the rated rate from [rates] in the config, or of {:.0} MB/s if none is set)",
            ltoarc_engine::plan::rated_mbps(v.generation.as_deref().unwrap_or(""))
        );
        drive_rate = Some(r.steady_write_mbps);
        ctx.cat.log_event("calibrate", &d.name, &format!("write {:.1} MB/s, read {:.1} MB/s, floor {floor:.0} MB/s", r.write_mbps, r.read_mbps))?;
    }
    if let Some(src) = a.source.as_deref() {
        crate::note!("measuring {} ...", src.display());
        let b = ltoarc_engine::bench::source(src, bytes, limit)?;
        print_source_bench(&b);
        ctx.cat.add_benchmark("source", &b.fs, &b)?;
        if let Some(d) = drive_rate {
            let floor = d / 3.0;
            let mbps = b.seq_mbps.max(b.seq_parallel_mbps);
            if mbps >= d {
                println!("the source can keep the drive streaming at full speed");
            } else if mbps >= floor {
                println!("the drive will slow down to match the source and keep streaming");
            } else {
                println!("the source is below the drive's speed-matching floor (about {floor:.0} MB/s): ltoarc will write in buffered bursts; a larger --buffer means fewer stops");
            }
        }
    }
    if a.tape.is_none() && a.source.is_none() {
        bail!("give --tape and/or --source");
    }
    Ok(())
}

pub fn drive_bench_key(serial: &str, generation: Option<&str>) -> String {
    format!("{serial}/{}", generation.unwrap_or("?"))
}

fn print_source_bench(b: &ltoarc_engine::bench::SourceBench) {
    println!("source {}:", b.fs);
    println!("  scan             {:>10.0} entries/s", b.scan_entries_per_sec);
    println!("  sequential       {:>10.1} MB/s (1 reader)", b.seq_mbps);
    println!("  sequential       {:>10.1} MB/s (4 readers)", b.seq_parallel_mbps);
    println!("  small files      {:>10.0} files/s, {:.1} MB/s (8 readers)", b.small_files_per_sec, b.small_mbps);
}

pub fn bench(ctx: &Ctx, sources: &[PathBuf], size: &str, seconds: u64, list: bool) -> Result<()> {
    if list || sources.is_empty() {
        let mut t = Table::new(&["KIND", "KEY", "MEASURED", "RESULT"]);
        for (kind, key, at, data) in ctx.cat.benchmarks()? {
            let summary = match kind.as_str() {
                "source" => format!(
                    "seq {:.0}/{:.0} MB/s, small {:.0} files/s, scan {:.0}/s",
                    data["seq_mbps"].as_f64().unwrap_or(0.0),
                    data["seq_parallel_mbps"].as_f64().unwrap_or(0.0),
                    data["small_files_per_sec"].as_f64().unwrap_or(0.0),
                    data["scan_entries_per_sec"].as_f64().unwrap_or(0.0)
                ),
                _ => format!(
                    "write {:.0} MB/s, read {:.0} MB/s, floor {:.0} MB/s{}",
                    data["max_write_mbps"].as_f64().unwrap_or(0.0),
                    data["read_mbps"].as_f64().unwrap_or(0.0),
                    data["floor_mbps"].as_f64().unwrap_or(0.0),
                    if data["floor_measured"].as_bool() == Some(true) { "" } else { " (estimated)" }
                ),
            };
            t.row(vec![kind, key, when(at), summary]);
        }
        if t.is_empty() {
            println!("no benchmarks stored; run `ltoarc bench SOURCE` and `ltoarc calibrate --tape ...`");
        } else {
            t.print();
        }
        return Ok(());
    }
    let max_bytes = parse_size(size)?;
    for src in sources {
        crate::note!("measuring {} ...", src.display());
        let b = ltoarc_engine::bench::source(src, max_bytes, Duration::from_secs(seconds))?;
        print_source_bench(&b);
        ctx.cat.add_benchmark("source", &b.fs, &b)?;
    }
    println!("(results are optimistic for data already in memory caches)");
    Ok(())
}

#[derive(Args)]
pub struct CopyArgs {
    /// Cartridges holding the sessions to copy (barcodes, lists or ranges).
    #[arg(long, required_unless_present = "from_source")]
    pub from: Option<String>,
    /// Make the copy from the source disks instead of tape: the files are read again and
    /// checked against the session's BLAKE3 (files changed since are left out and named).
    /// Sessions: --session, or those on --from.
    #[arg(long)]
    pub from_source: bool,
    /// Cartridges for the new copy (barcodes, lists or ranges; any generation, any library).
    #[arg(long)]
    pub to: String,
    /// Only these sessions (id or UUID prefix; repeatable). Default: every session on --from.
    #[arg(long = "session")]
    pub sessions: Vec<String>,
    /// Drive to read the source cartridges in.
    #[arg(long)]
    pub source_drive: Option<String>,
    /// Pool recorded on destination cartridges labelled on the way.
    #[arg(long)]
    pub pool: Option<String>,
    /// Destination buffer (default: as for backups).
    #[arg(long)]
    pub buffer: Option<String>,
    /// Read the source at full speed in bursts, instead of matching it to the destination.
    #[arg(long)]
    pub no_pace: bool,
    /// Start each destination cartridge afresh instead of appending (asks first).
    #[arg(long)]
    pub overwrite: bool,
    /// Do not ask (for --overwrite).
    #[arg(long)]
    pub yes: bool,
    /// Show which destination cartridges the copy would use, then stop.
    #[arg(long)]
    pub dry_run: bool,
}

/// `ltoarc copy --from-source`: another copy of sessions from the files on disk.
fn copy_from_source(ctx: &Ctx, a: &CopyArgs) -> Result<()> {
    use std::collections::VecDeque;
    let mut sessions: Vec<ltoarc_catalog::SessionRow> = Vec::new();
    for key in &a.sessions {
        sessions.push(ctx.cat.session(key)?.ok_or_else(|| anyhow!("no session {key}"))?);
    }
    if let Some(f) = &a.from {
        let ids: Vec<i64> = tape::tape_set(ctx, f)?.iter().map(|v| v.id).collect();
        for s in ctx.cat.sessions()? {
            if !sessions.iter().any(|x| x.id == s.id) && ctx.cat.session_volumes(s.id)?.iter().any(|sv| ids.contains(&sv.volume_id)) {
                sessions.push(s);
            }
        }
    }
    if sessions.is_empty() {
        bail!("name the sessions to copy with --session (or --from CARTRIDGES)");
    }
    let to = tape::tape_set(ctx, &a.to)?;
    let mut queue: VecDeque<String> = to.iter().filter(|v| crate::ctx::usable(v, a.overwrite)).map(|v| v.barcode.clone()).collect();
    let need: u64 = sessions.iter().map(|s| s.stored_bytes as u64).sum();
    let order: Vec<ltoarc_catalog::VolumeRow> = queue.iter().map(|b| ctx.volume(b)).collect::<Result<_>>()?;
    let k = crate::fit::show(ctx, &order, Some(need), a.overwrite, "tapes")?;
    if a.overwrite {
        confirm_overwrite(ctx, &order[..k.min(order.len())], a.yes || a.dry_run)?;
    }
    if a.dry_run {
        return Ok(());
    }
    let pos = queue
        .iter()
        .position(|bc| tape::free_writer(ctx, bc).ok().flatten().is_some())
        .ok_or_else(|| anyhow!("no free drive can write a cartridge of {}", a.to))?;
    let bc = queue.remove(pos).expect("position is valid");
    let dd = tape::free_writer(ctx, &bc)?.expect("checked");
    let (dd, mut ddrive) = tape::load_rw(ctx, &bc, Some(&dd.name))?;
    let pool = a.pool.clone().or_else(|| to.iter().find_map(|v| v.pool.clone()));
    let mut dvol = tape::ready_for_append(ctx, &dd, &mut *ddrive, &bc, pool.clone(), a.overwrite)?;
    let mut ring = ltoarc_engine::ringbuf::Config::default_for_host();
    if let Some(b) = &a.buffer {
        ring = ltoarc_engine::ringbuf::Config::new(parse_size(b)?.max(4 << 20));
    }
    let fo = backup::from_source::FromSourceOptions { ring, keys: ctx.keys.clone(), zstd_level: None };
    let mut set = tape::TapeSet { ctx, drives: vec![dd.clone()], rest: queue, pool, mode: ltoarc_engine::plan::PaceMode::Auto, source_rate: None, overwrite: a.overwrite };
    let mut cur_drive = dd;
    let mut failed = 0;
    for s in sessions {
        let to_copy = ctx.cat.session_volumes(s.id)?.iter().map(|sv| sv.copy).max().unwrap_or(0) + 1;
        let generation = dvol.generation.clone().unwrap_or_default();
        let (decision, _, _) = tape::pacing(ctx, &cur_drive, &generation, ltoarc_engine::plan::PaceMode::Auto, None)?;
        println!("copying session {} from the source files as copy {to_copy} in drive {}", &s.uuid[..8], cur_drive.name);
        let start = Instant::now();
        let mut p = Printer::new();
        let c = backup::from_source::copy_from_source(
            &ctx.cat,
            s.id,
            to_copy,
            backup::LaneStart { drive: &mut *ddrive, volume: dvol.clone(), pace: decision.pace },
            &fo,
            &mut set,
            &mut p,
        )
        .with_context(|| format!("copying session {}", &s.uuid[..8]))?;
        let tapes: Vec<&str> = c.volumes.iter().map(|v| v.barcode.as_str()).collect();
        let secs = start.elapsed().as_secs_f64().max(0.001);
        println!(
            "session   {} copy {to_copy} on {}: {} records, {} ({:.1} MB/s overall), {} chunks checked against the session's BLAKE3",
            &s.uuid[..8],
            tapes.join(","),
            c.records,
            human(c.bytes),
            c.bytes as f64 / secs / 1e6,
            c.verified
        );
        println!("dest      {}: {}", cur_drive.name, writer_line(&c.writer, secs));
        for e in c.errors.iter().take(20) {
            crate::note!("MISSING: {e}");
        }
        failed += c.errors.len();
        ctx.cat.log_event("copy", &s.uuid, &format!("copy {to_copy} from source on {}: {} files left out", tapes.join(","), c.errors.len()))?;
        if set.drives[0].id != cur_drive.id {
            cur_drive = set.drives[0].clone();
            ddrive = ctx.open_drive(&cur_drive)?;
        }
        let last = ctx.volume(tapes.last().copied().unwrap_or(&dvol.barcode))?;
        dvol = if last.state == "full" {
            let n = ltoarc_engine::backup::NextVolume::next(&mut set, 0, &mut *ddrive, &last)?;
            if let Some(d) = n.drive {
                ddrive = d;
                cur_drive = set.drives[0].clone();
            }
            n.volume
        } else {
            last
        };
    }
    if failed > 0 {
        println!("{failed} file(s) changed since their session and are not in the new copy; their other copies still hold them");
    }
    println!("check the new copy with `ltoarc verify` on each destination cartridge");
    Ok(())
}

/// The source cartridges, one after another in one drive.
struct CopySource<'c> {
    ctx: &'c Ctx,
    drive_row: ltoarc_catalog::DriveRow,
    drive: Box<dyn ltoarc_device::TapeDrive>,
    holding: String,
}

impl backup::copy::SourceVolumes for CopySource<'_> {
    fn load(&mut self, volume: &ltoarc_catalog::VolumeRow) -> Result<&mut dyn ltoarc_device::TapeDrive> {
        if volume.barcode != self.holding {
            if crate::ctx::access(&self.drive_row, volume) == ltoarc_device::Access::None || volume.library_id != self.drive_row.library_id {
                bail!("{} cannot be read in drive {} (generation or library)", volume.barcode, self.drive_row.name);
            }
            crate::note!("reading {} in drive {} next", volume.barcode, self.drive_row.name);
            tape::exchange(self.ctx, &self.drive_row, &mut *self.drive, &self.holding, &volume.barcode)?;
            self.holding = volume.barcode.clone();
        }
        Ok(&mut *self.drive)
    }
}

pub fn copy(ctx: &Ctx, a: CopyArgs) -> Result<()> {
    use std::collections::{BTreeMap, VecDeque};
    if a.from_source {
        return copy_from_source(ctx, &a);
    }
    let from_spec = a.from.clone().expect("required without --from-source");
    let from = tape::tape_set(ctx, &from_spec)?;
    let to = tape::tape_set(ctx, &a.to)?;
    let from_ids: Vec<i64> = from.iter().map(|v| v.id).collect();
    let wanted: Vec<i64> = a.sessions.iter().map(|s| ctx.cat.session(s)?.map(|s| s.id).ok_or_else(|| anyhow!("no session {s}"))).collect::<Result<_>>()?;
    // Sessions with a copy on the source cartridges: session -> (that copy, the new copy).
    let mut plan: BTreeMap<i64, (i64, i64, ltoarc_catalog::SessionRow)> = BTreeMap::new();
    for s in ctx.cat.sessions()? {
        if !wanted.is_empty() && !wanted.contains(&s.id) {
            continue;
        }
        let svs = ctx.cat.session_volumes(s.id)?;
        let Some(from_copy) = svs.iter().filter(|sv| from_ids.contains(&sv.volume_id)).map(|sv| sv.copy).min() else { continue };
        let next_copy = svs.iter().map(|sv| sv.copy).max().unwrap_or(0) + 1;
        plan.insert(s.id, (from_copy, next_copy, s));
    }
    if plan.is_empty() {
        bail!("no session to copy on {from_spec}");
    }
    // Source drive: holding the first source cartridge needed.
    let (first_sid, first_copy) = plan.iter().next().map(|(k, v)| (*k, v.0)).expect("plan is not empty");
    let first_src = ctx
        .cat
        .session_volumes(first_sid)?
        .into_iter()
        .filter(|sv| sv.copy == first_copy)
        .min_by_key(|sv| sv.seq)
        .and_then(|sv| ctx.cat.volume_by_id(sv.volume_id).ok().flatten())
        .ok_or_else(|| anyhow!("source cartridge missing from the catalog"))?;
    // Destination: the cartridges of --to, in order; first which of them the copy would take.
    let mut queue: VecDeque<String> = to
        .iter()
        .filter(|v| crate::ctx::usable(v, a.overwrite) && !from_ids.contains(&v.id))
        .map(|v| v.barcode.clone())
        .collect();
    let need: u64 = plan.values().map(|(_, _, s)| s.stored_bytes as u64).sum();
    let order: Vec<ltoarc_catalog::VolumeRow> = queue.iter().map(|b| ctx.volume(b)).collect::<Result<_>>()?;
    let k = crate::fit::show(ctx, &order, Some(need), a.overwrite, "tapes")?;
    if a.overwrite {
        confirm_overwrite(ctx, &order[..k.min(order.len())], a.yes || a.dry_run)?;
    }
    if a.dry_run {
        return Ok(());
    }
    let (sd, sdrive) = tape::load(ctx, &first_src.barcode, a.source_drive.as_deref())?;
    let pos = queue
        .iter()
        .position(|bc| tape::free_writer(ctx, bc).ok().flatten().is_some_and(|d| d.id != sd.id))
        .ok_or_else(|| anyhow!("no free drive besides {} can write a cartridge of {}", sd.name, a.to))?;
    let bc = queue.remove(pos).expect("position is valid");
    let dd = tape::free_writer(ctx, &bc)?.expect("checked");
    let (dd, mut ddrive) = tape::load_rw(ctx, &bc, Some(&dd.name))?;
    let pool = a.pool.clone().or_else(|| to.iter().find_map(|v| v.pool.clone()));
    let mut dvol = tape::ready_for_append(ctx, &dd, &mut *ddrive, &bc, pool.clone(), a.overwrite)?;
    // Speed matching: the faster drive runs at the slower one's pace, within its own
    // speed-matching range, so neither stops; the buffers only absorb variation.
    let src_gen = first_src.generation.clone().unwrap_or_default();
    let (_, src_rates, _) = tape::pacing(ctx, &sd, &src_gen, ltoarc_engine::plan::PaceMode::Off, None)?;
    let (_, dst_rates, _) = tape::pacing(ctx, &dd, dvol.generation.as_deref().unwrap_or(""), ltoarc_engine::plan::PaceMode::Off, None)?;
    let src_rate = Some(src_rates.estimate());
    let matched = src_rates.estimate().min(dst_rates.estimate());
    let mb = |x: f64| x / 1e6;
    if a.no_pace {
        println!("pace      source reads at full speed (--no-pace); it stops each time the read-ahead buffer fills");
    } else if dst_rates.estimate() < src_rates.estimate() {
        if matched >= src_rates.slowest {
            println!(
                "pace      drive {} reads at drive {}'s pace, about {:.0} MB/s (it can slow down to {:.0} MB/s), so neither has to stop",
                sd.name,
                dd.name,
                mb(matched),
                mb(src_rates.slowest)
            );
        } else {
            println!(
                "pace      drive {} cannot read slower than {:.0} MB/s and drive {} writes about {:.0} MB/s: the source stops each time the read-ahead buffer fills",
                sd.name,
                mb(src_rates.slowest),
                dd.name,
                mb(matched)
            );
        }
    } else if matched >= dst_rates.slowest {
        println!("pace      drive {} writes at drive {}'s pace, about {:.0} MB/s, so neither has to stop", dd.name, sd.name, mb(matched));
    } else {
        println!("pace      drive {} reads about {:.0} MB/s, below drive {}'s slowest {:.0} MB/s: the destination stops each time its buffer runs dry", sd.name, mb(matched), dd.name, mb(dst_rates.slowest));
    }
    let mut ring = ltoarc_engine::ringbuf::Config::default_for_host();
    if let Some(b) = &a.buffer {
        ring = ltoarc_engine::ringbuf::Config::new(parse_size(b)?.max(4 << 20));
    }
    let copts = ltoarc_engine::backup::copy::CopyOptions {
        ring,
        read_buffer: restore::default_read_buffer(),
        keys: ctx.keys.clone(),
        source_pace: (!a.no_pace).then(|| src_rates.pace()),
        match_rate: Some(matched),
    };
    let mut source = CopySource { ctx, drive_row: sd.clone(), drive: sdrive, holding: first_src.barcode.clone() };
    let mut set = tape::TapeSet {
        ctx,
        drives: vec![dd.clone()],
        rest: queue,
        pool,
        mode: ltoarc_engine::plan::PaceMode::Auto,
        source_rate: src_rate,
        overwrite: a.overwrite,
    };
    println!("copying {} session(s) from drive {} to drive {}", plan.len(), sd.name, dd.name);
    let mut cur_drive = dd;
    for (sid, (from_copy, to_copy, s)) in plan {
        let generation = dvol.generation.clone().unwrap_or_default();
        let (decision, _, _) = tape::pacing(ctx, &cur_drive, &generation, ltoarc_engine::plan::PaceMode::Auto, src_rate)?;
        let start = Instant::now();
        let mut p = Printer::new();
        let c = backup::copy::copy_session(
            &ctx.cat,
            sid,
            from_copy,
            to_copy,
            &mut source,
            backup::LaneStart { drive: &mut *ddrive, volume: dvol.clone(), pace: decision.pace },
            &copts,
            &mut set,
            &mut p,
        )
        .with_context(|| format!("copying session {}", &s.uuid[..8]))?;
        let tapes: Vec<&str> = c.volumes.iter().map(|v| v.barcode.as_str()).collect();
        let secs = start.elapsed().as_secs_f64().max(0.001);
        println!(
            "session   {} copy {from_copy} -> copy {to_copy} on {}: {} records, {} chunks, {} ({:.1} MB/s overall), {} checked",
            &s.uuid[..8],
            tapes.join(","),
            c.records,
            c.chunks,
            human(c.bytes),
            c.bytes as f64 / secs / 1e6,
            c.verified
        );
        println!("source    {}: {}", sd.name, c.read.describe());
        println!("dest      {}: {}", cur_drive.name, writer_line(&c.writer, secs));
        for e in c.errors.iter().take(10) {
            crate::note!("ERROR: {e}");
        }
        for w in &c.warnings {
            crate::note!("warning: {w}");
        }
        ctx.cat.log_event("copy", &s.uuid, &format!("copy {to_copy} on {}: {} errors", tapes.join(","), c.errors.len()))?;
        // The next session continues where this one ended (the tape set may have moved it
        // to another drive, or filled the cartridge).
        if set.drives[0].id != cur_drive.id {
            cur_drive = set.drives[0].clone();
            ddrive = ctx.open_drive(&cur_drive)?;
        }
        let last = ctx.volume(tapes.last().copied().unwrap_or(&dvol.barcode))?;
        dvol = if last.state == "full" {
            let n = ltoarc_engine::backup::NextVolume::next(&mut set, 0, &mut *ddrive, &last)?;
            if let Some(d) = n.drive {
                ddrive = d;
                cur_drive = set.drives[0].clone();
            }
            n.volume
        } else {
            last
        };
    }
    println!("check the new copy with `ltoarc verify` on each destination cartridge");
    Ok(())
}

#[derive(Args)]
pub struct ExpireArgs {
    /// The job whose sessions to expire.
    #[arg(long)]
    pub job: String,
    /// Keep this many of the newest chains (a full session with those built on it).
    #[arg(long)]
    pub keep_fulls: Option<usize>,
    /// Keep chains with a session newer than this (e.g. 90d, 12h).
    #[arg(long)]
    pub keep: Option<String>,
    /// Expire them (default: only list).
    #[arg(long)]
    pub yes: bool,
}

pub fn expire(ctx: &Ctx, a: ExpireArgs) -> Result<()> {
    let policy = ltoarc_engine::retention::Policy {
        keep_fulls: a.keep_fulls,
        keep_secs: a.keep.as_deref().map(parse_duration).transpose()?.map(|d| d.as_secs() as i64),
    };
    if policy.keep_fulls.is_none() && policy.keep_secs.is_none() {
        bail!("give --keep-fulls N and/or --keep DURATION");
    }
    let sessions = ctx.cat.job_sessions(&a.job)?;
    let ids = ltoarc_engine::retention::expired(&sessions, policy, ltoarc_catalog::now());
    if ids.is_empty() {
        println!("nothing of job {} has expired ({} session(s) kept)", a.job, sessions.len());
        return Ok(());
    }
    for s in sessions.iter().filter(|s| ids.contains(&s.id)) {
        let when = chrono::DateTime::from_timestamp(s.started, 0).unwrap_or_default().format("%Y-%m-%d %H:%M");
        println!("{} {}  {:<12} {:>6} files  {}", if a.yes { "expired " } else { "expires " }, &s.uuid[..8], s.level, s.files, when);
    }
    if !a.yes {
        println!("{} session(s) would expire; add --yes", ids.len());
        return Ok(());
    }
    let now = ltoarc_catalog::now();
    ctx.cat.begin()?;
    for id in &ids {
        ctx.cat.expire_session(*id, now)?;
    }
    let mut freed = Vec::new();
    for vid in ctx.cat.reusable_volumes()? {
        if let Some(mut v) = ctx.cat.volume_by_id(vid)? {
            if v.state != "expired" {
                v.state = "expired".into();
                ctx.cat.update_volume(&v)?;
                freed.push(v.barcode);
            }
        }
    }
    ctx.cat.commit()?;
    ctx.cat.log_event("expire", &a.job, &format!("{} session(s); reusable: {}", ids.len(), freed.join(",")))?;
    if freed.is_empty() {
        println!("no cartridge became reusable (they hold sessions still kept, or data those refer to)");
    } else {
        println!("reusable now (relabelled when a backup reaches them): {}", freed.join(", "));
    }
    Ok(())
}

#[derive(Args)]
pub struct PruneArgs {
    /// The append-only job whose sources to prune.
    #[arg(long)]
    pub job: String,
    /// Copies each file needs on tape, each verified (`ltoarc verify`) since it was written.
    #[arg(long, default_value_t = 2)]
    pub copies: usize,
    /// Delete them (default: only list).
    #[arg(long)]
    pub yes: bool,
}

pub fn prune(ctx: &Ctx, a: PruneArgs) -> Result<()> {
    let sessions = ctx.cat.job_sessions(&a.job)?;
    if sessions.is_empty() {
        bail!("job {} has no sessions", a.job);
    }
    if sessions.iter().any(|s| s.level != "append") {
        bail!("prune is for append-only jobs; job {} has {} sessions", a.job, sessions.iter().find(|s| s.level != "append").map_or("", |s| s.level.as_str()));
    }
    let ids: Vec<i64> = sessions.iter().map(|s| s.id).collect();
    let (mut n, mut bytes, mut short) = (0u64, 0u64, 0u64);
    for (path, o) in ctx.cat.state_of(&ids)? {
        if o.kind != "file" || o.status != "ok" || o.size == 0 {
            continue;
        }
        let p = std::path::Path::new(std::ffi::OsStr::from_bytes(&path));
        // Only what is on disk exactly as archived.
        let Ok(md) = std::fs::symlink_metadata(p) else { continue };
        if !md.is_file() || md.len() as i64 != o.size || ltoarc_engine::meta::mtime_ns(&md) != o.mtime_ns {
            continue;
        }
        if ctx.cat.verified_copies(o.id)?.len() < a.copies {
            short += 1;
            continue;
        }
        if a.yes {
            if let Err(e) = std::fs::remove_file(p) {
                crate::note!("warning: {}: {e}", p.display());
                continue;
            }
        } else {
            println!("{}", p.display());
        }
        n += 1;
        bytes += o.size as u64;
    }
    println!(
        "{} {n} file(s), {}; {short} archived file(s) still lack {} verified copies",
        if a.yes { "deleted" } else { "would delete" },
        human(bytes),
        a.copies
    );
    if a.yes {
        ctx.cat.log_event("prune", &a.job, &format!("{n} files, {}", human(bytes)))?;
    }
    Ok(())
}

/// `ltoarc cat PATH`: one archived file to standard output (a stream source back into its
/// tool, e.g. `ltoarc cat /.ltoarc/streams/tank/send | zfs receive tank/restored`).
pub fn cat(ctx: &Ctx, path: &Path, view: ltoarc_catalog::View, copy: Option<i64>) -> Result<()> {
    use std::io::Write;
    let id = ctx.cat.lookup_path(path.as_os_str().as_bytes())?.ok_or_else(|| anyhow!("{} is not in the catalog", path.display()))?;
    let o = ctx.cat.latest(id, view)?.ok_or_else(|| anyhow!("{} has no version in this view", path.display()))?;
    if o.kind != "file" {
        bail!("{} is a {}, not a file", path.display(), o.kind);
    }
    if o.status != "ok" {
        crate::note!("warning: {} is {} in the archive", path.display(), o.status);
    }
    let mut starts = std::collections::HashMap::new();
    let pieces = restore::object_pieces(&ctx.cat, &o, 0, copy, &mut starts)?;
    // Cartridges in the order of the file's data, each read in tape order.
    let mut by_tape: Vec<(String, Vec<restore::Piece>)> = Vec::new();
    for (bc, p) in pieces {
        match by_tape.iter_mut().find(|(b, _)| *b == bc) {
            Some((_, v)) => v.push(p),
            None => by_tape.push((bc, vec![p])),
        }
    }
    by_tape.sort_by_key(|(_, v)| v.iter().map(|p| p.chunk.offset).min().unwrap_or(0));
    let sessions: std::collections::HashSet<i64> = by_tape.iter().flat_map(|(_, v)| v.iter().map(|p| p.session_id)).collect();
    let deks = restore::session_keys(&ctx.cat, &sessions, &ctx.keys)?;
    let hashes = ctx.cat.hashes(o.data_object())?;
    let mut hasher = hash::Algo::parse("blake3").expect("built in").hasher();
    let mut out = std::io::BufWriter::with_capacity(4 << 20, std::io::stdout().lock());
    let mut at = 0u64;
    let mut errors = Vec::new();
    for (bc, mut ps) in by_tape {
        ps.sort_by_key(|p| (p.placement.partition, p.placement.block, p.placement.offset));
        let (d, mut drive) = tape::load(ctx, &bc, None)?;
        let v = ctx.volume(&bc)?;
        let pace = read_pace(ctx, &d, &v, false)?;
        restore::read_volume(&mut *drive, &bc, &ps, restore::default_read_buffer(), pace, &deks, &mut errors, &mut |_, offset, data| {
            if offset < at {
                bail!("the file's data is not stored in order; use `ltoarc restore` instead");
            }
            // Holes of a sparse file.
            let zeros = vec![0u8; (offset - at).min(1 << 20) as usize];
            while at < offset {
                let n = ((offset - at) as usize).min(zeros.len());
                out.write_all(&zeros[..n])?;
                hasher.update(&zeros[..n]);
                at += n as u64;
            }
            out.write_all(data)?;
            hasher.update(data);
            at += data.len() as u64;
            Ok(())
        })?;
    }
    let zeros = vec![0u8; 1 << 20];
    while at < o.size as u64 {
        let n = ((o.size as u64 - at) as usize).min(zeros.len());
        out.write_all(&zeros[..n])?;
        hasher.update(&zeros[..n]);
        at += n as u64;
    }
    out.flush()?;
    for (_, e) in &errors {
        crate::note!("ERROR: {e}");
    }
    if let Some(h) = hashes.iter().find(|h| h.kind == "content" && h.algo == "blake3") {
        if hasher.finish() != h.digest {
            bail!("{}: the data written does not match its BLAKE3 {}", path.display(), h.digest);
        }
    }
    if !errors.is_empty() {
        bail!("{} error(s) reading {}", errors.len(), path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Status {
        Status { elapsed: 100.0, files: 812, raw: 28_100_000_000, total: Some(66_900_000_000), now_rate: 296e6, tape: Some((300e6, 27_000_000_000, 0.61)) }
    }

    #[test]
    fn progress_bar_fits_the_terminal() {
        let wide = status().bar(160);
        assert!(wide.starts_with(" 42% ▕"), "{wide}");
        assert!(wide.contains("28.1 GB/66.9 GB · 296 MB/s (avg 281) · tape 300 MB/s, buffer 61% · ETA 2 min"), "{wide}");
        assert!(wide.chars().count() <= 160);
        // Narrow: the bar keeps its size, the tape's part goes first.
        let narrow = status().bar(80);
        assert!(narrow.chars().count() <= 80, "{narrow}");
        assert!(narrow.contains("296 MB/s (avg 281) · ETA 2 min") && !narrow.contains("tape"), "{narrow}");
        // Size unknown: no bar, the time spent instead.
        let open = Status { total: None, ..status() }.bar(160);
        assert!(open.starts_with("812 files, 28.1 GB · 296 MB/s") && open.ends_with("1 min elapsed"), "{open}");
    }

    #[test]
    fn progress_line_for_logs() {
        let l = status().line();
        assert_eq!(l, "  812 entries, 28.1 GB of 66.9 GB (42%), 281.0 MB/s (now 296.0), tape 300 MB/s, buffer 61%");
    }
}
