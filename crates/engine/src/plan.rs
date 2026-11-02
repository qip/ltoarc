//! Planning a session from the stored benchmarks: what the source can deliver, and how the
//! drive will be paced.
//!
//! The writer always feeds the drive at a steady rate: the source's, up to the drive's rated
//! speed. A source slower than the drive gets speed matching (the tape slows down instead
//! of stopping); a faster one still gets a steady feed, because on a real LTO-8 writing as
//! fast as the host could made the drive pause about four times as often as a steady feed
//! at up to the rated speed. `--pace off` writes bursts as fast as possible instead.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::bench::SourceBench;
use crate::ringbuf::Pace;
use crate::walk::{WalkOptions, Walker};

/// Composition of what a session will read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub entries: u64,
    pub large_files: u64,
    pub large_bytes: u64,
    pub small_files: u64,
    pub small_bytes: u64,
    /// Entries left out: unchanged since the sessions this one compares against.
    pub skipped: u64,
    pub skipped_bytes: u64,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.large_bytes + self.small_bytes
    }

    fn found(&mut self, path: &Path, md: &std::fs::Metadata, pack_max: u64, keep: Keep) {
        if keep(path, md) {
            self.add(md, pack_max);
        } else {
            self.skipped += 1;
            if md.is_file() {
                self.skipped_bytes += md.len();
            }
        }
    }

    fn add(&mut self, md: &std::fs::Metadata, pack_max: u64) {
        self.entries += 1;
        if md.is_file() {
            if md.len() > pack_max {
                self.large_files += 1;
                self.large_bytes += md.len();
            } else {
                self.small_files += 1;
                self.small_bytes += md.len();
            }
        }
    }
}

/// Whether an entry found by a plan will be saved (false: unchanged, left out).
pub type Keep<'a> = &'a mut dyn FnMut(&Path, &std::fs::Metadata) -> bool;

/// Stat-only walk of the sources (no xattrs, no data).
pub fn scan(sources: &[PathBuf], walk: &WalkOptions, pack_max: u64, keep: Keep) -> Result<Plan> {
    let sources: Vec<PathBuf> = sources.iter().map(std::fs::canonicalize).collect::<std::io::Result<_>>()?;
    let mut plan = Plan::default();
    let mut w = Walker::new(walk)?;
    w.walk(&sources, &mut |p, md| {
        plan.found(p, md, pack_max, keep);
        Ok(true)
    })?;
    Ok(plan)
}

/// Plan from a list of paths (newline- or NUL-separated), e.g. `find -print0` output or a
/// locate database's, assuming nothing changed since. Paths outside `sources` are ignored;
/// sizes come from stat, so no directory has to be read.
pub fn from_list(list: &Path, sources: &[PathBuf], pack_max: u64, keep: Keep) -> Result<Plan> {
    let data = std::fs::read(list).with_context(|| format!("reading {}", list.display()))?;
    let sources: Vec<PathBuf> = sources.iter().map(std::fs::canonicalize).collect::<std::io::Result<_>>()?;
    let sep = if data.contains(&0) { 0 } else { b'\n' };
    let mut plan = Plan::default();
    for raw in data.split(|&b| b == sep).filter(|l| !l.is_empty()) {
        let p = Path::new(std::ffi::OsStr::from_bytes(raw));
        if !sources.iter().any(|s| p.starts_with(s)) {
            continue;
        }
        if let Ok(md) = std::fs::symlink_metadata(p) {
            plan.found(p, &md, pack_max, keep);
        }
    }
    Ok(plan)
}

/// Bytes per second the source is expected to deliver. With a plan, large files stream at
/// the single-reader rate while small files are read alongside at the measured small-file
/// rate (optimistic: they overlap). Without one, the source is assumed to be large files.
pub fn source_rate(bench: &SourceBench, plan: Option<&Plan>) -> f64 {
    let seq = bench.seq_mbps.max(1.0) * 1e6;
    let Some(p) = plan else { return seq };
    let t_large = p.large_bytes as f64 / seq;
    let t_small = if p.small_files == 0 {
        0.0
    } else {
        p.small_files as f64 / bench.small_files_per_sec.max(1.0)
    };
    let t = t_large.max(t_small).max(1e-3);
    p.bytes() as f64 / t
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaceMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub pace: Option<Pace>,
    /// Expected source rate (bytes/s), when known.
    pub source_rate: Option<f64>,
    pub reason: String,
}

/// What a drive can do with a media generation, from its spec sheet (bytes/s).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriveRates {
    /// Slowest speed-matching rate: below it the drive must stop and wait.
    pub slowest: f64,
    /// Rated native (uncompressed) rate.
    pub rated: f64,
    /// Rates it cannot match between its two ranges (exclusive), when it has a gap.
    pub gap: Option<(f64, f64)>,
}

/// Share of the rated rate taken as the drive's expected speed in planning (instead of a
/// benchmark: real speed varies from run to run, and estimating a little low only means
/// pacing a little more often). The drive is never held to it.
pub const ESTIMATE_SHARE: f64 = 0.9;

impl DriveRates {
    /// From a spec-sheet range in MB/s.
    pub fn from_spec(slowest_mbps: f64, rated_mbps: f64) -> DriveRates {
        DriveRates { slowest: slowest_mbps * 1e6, rated: rated_mbps * 1e6, gap: None }
    }

    /// From one or two ranges in MB/s (`parse_ranges`): two leave a gap between them.
    pub fn from_ranges(r: &[(f64, f64)]) -> DriveRates {
        let mut d = DriveRates::from_spec(r[0].0, r[r.len() - 1].1);
        if r.len() == 2 {
            d.gap = Some((r[0].1 * 1e6, r[1].0 * 1e6));
        }
        d
    }

    /// Without a configured range: the rated rate below, and a third of it as the slowest.
    pub fn default_for(generation: &str) -> DriveRates {
        let rated = rated_mbps(generation);
        DriveRates::from_spec(rated / 3.0, rated)
    }

    /// Expected write speed for planning.
    pub fn estimate(&self) -> f64 {
        self.rated * ESTIMATE_SHARE
    }

    /// Pacing limits: down to the slowest rate, up to the full rated rate.
    pub fn pace(&self) -> Pace {
        Pace { floor: self.slowest, max: self.rated, gap: self.gap }
    }
}

/// Pace the drive only when the source cannot keep up with it (its estimated speed).
pub fn decide(mode: PaceMode, drive: DriveRates, source_rate: Option<f64>) -> Decision {
    let mut d = decide_range(mode, drive, source_rate);
    if let (Some(_), Some((lo, hi))) = (d.pace, drive.gap) {
        d.reason += &format!(
            "; never between {:.0} and {:.0} MB/s, where the drive cannot match speed (a source in there is written at {:.0} MB/s and waits a little)",
            lo / 1e6,
            hi / 1e6,
            lo / 1e6
        );
    }
    d
}

fn decide_range(mode: PaceMode, drive: DriveRates, source_rate: Option<f64>) -> Decision {
    let mb = |b: f64| b / 1e6;
    let est = drive.estimate();
    match (mode, source_rate) {
        (PaceMode::Off, _) => Decision { pace: None, source_rate, reason: "bursts as fast as possible (--pace off)".into() },
        (PaceMode::On, _) => Decision { pace: Some(drive.pace()), source_rate, reason: "pacing on (--pace on)".into() },
        (PaceMode::Auto, None) => Decision {
            pace: Some(drive.pace()),
            source_rate,
            reason: format!(
                "steady feed at the source's rate, up to the drive's rated {:.0} MB/s (no source benchmark: `ltoarc bench SOURCE` tells whether the source keeps up)",
                mb(drive.rated)
            ),
        },
        (PaceMode::Auto, Some(r)) if r >= est => Decision {
            pace: Some(drive.pace()),
            source_rate,
            reason: format!(
                "steady feed: the source delivers about {:.0} MB/s, the drive about {:.0} MB/s; fed steadily at up to its rated {:.0} MB/s",
                mb(r),
                mb(est),
                mb(drive.rated)
            ),
        },
        (PaceMode::Auto, Some(r)) => Decision {
            pace: Some(drive.pace()),
            source_rate,
            reason: if r >= drive.slowest {
                format!(
                    "speed matching: the source delivers about {:.0} MB/s, below the drive's {:.0} MB/s; the drive slows down instead of stopping",
                    mb(r),
                    mb(est)
                )
            } else {
                format!(
                    "speed matching at the floor: the source delivers about {:.0} MB/s, below even the drive's slowest {:.0} MB/s; expect a stop each time the buffer runs dry",
                    mb(r),
                    mb(drive.slowest)
                )
            },
        },
    }
}

/// Rated native rate (MB/s) by media generation when the spec sheet range is not configured:
/// half-height drive figures, which only errs on the slow side for full-height drives.
pub fn rated_mbps(generation: &str) -> f64 {
    match generation {
        g if g.contains("LTO-5") => 140.0,
        g if g.contains("LTO-6") => 160.0,
        _ => 300.0,
    }
}

/// Parse a spec-sheet range "SLOWEST-RATED" in MB/s, e.g. "112-300".
pub fn parse_range(s: &str) -> Option<(f64, f64)> {
    let (a, b) = s.split_once("..").or_else(|| s.split_once('-'))?;
    let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a > 0.0 && b >= a).then_some((a, b))
}

/// One range, or two with a gap between ("66-155, 280-300": the drive matches speeds up to
/// 155 MB/s and from 280, and stops again and again in between).
pub fn parse_ranges(s: &str) -> Option<Vec<(f64, f64)>> {
    let r: Vec<(f64, f64)> = s.split(',').map(parse_range).collect::<Option<_>>()?;
    match r.as_slice() {
        [_] => Some(r),
        [a, b] if b.0 > a.1 => Some(r),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench(seq: f64, small_fps: f64) -> SourceBench {
        SourceBench { seq_mbps: seq, small_files_per_sec: small_fps, ..Default::default() }
    }

    #[test]
    fn decisions() {
        let drive = DriveRates::from_spec(100.0, 300.0);
        // Always a steady feed, up to the rated rate, whatever the source.
        for r in [Some(600e6), Some(280e6), Some(250e6), Some(150e6), None] {
            assert_eq!(decide(PaceMode::Auto, drive, r).pace.unwrap(), Pace { floor: 100e6, max: 300e6, gap: None });
        }
        assert!(decide(PaceMode::Auto, drive, Some(600e6)).reason.contains("steady feed"));
        assert!(decide(PaceMode::Auto, drive, Some(150e6)).reason.contains("speed matching"));
        assert!(decide(PaceMode::On, drive, Some(600e6)).pace.is_some());
        assert!(decide(PaceMode::Off, drive, Some(50e6)).pace.is_none());
    }

    #[test]
    fn spec_ranges() {
        assert_eq!(parse_range("112-300"), Some((112.0, 300.0)));
        assert_eq!(parse_range("40 .. 140"), Some((40.0, 140.0)));
        assert_eq!(parse_range("300-112"), None);
        let d = DriveRates::from_spec(112.0, 300.0);
        assert_eq!((d.pace().floor, d.pace().max, d.estimate()), (112e6, 300e6, 270e6));
        let two = parse_ranges("66-155, 280-300").unwrap();
        let d = DriveRates::from_ranges(&two);
        assert_eq!((d.slowest, d.rated, d.gap), (66e6, 300e6, Some((155e6, 280e6))));
        assert!(parse_ranges("66-155, 150-300").is_none() && parse_ranges("1-2,3-4,5-6").is_none());
        // In the gap: down to its lower edge.
        let p = d.pace();
        assert_eq!((p.snap(140e6), p.snap(165e6), p.snap(250e6), p.snap(290e6), p.snap(40e6)), (140e6, 155e6, 155e6, 290e6, 66e6));
        assert_eq!(DriveRates::default_for("LTO-5").estimate(), 126e6);
    }

    #[test]
    fn rate_from_composition() {
        // 100 GB of large files at 500 MB/s (200 s), 1M small files at 2000/s (500 s).
        let p = Plan { entries: 1_000_100, large_files: 100, large_bytes: 100_000_000_000, small_files: 1_000_000, small_bytes: 10_000_000_000, ..Default::default() };
        let r = source_rate(&bench(500.0, 2000.0), Some(&p));
        assert!((r - 220e6).abs() < 1e6, "{r}");
        assert_eq!(source_rate(&bench(500.0, 2000.0), None), 500e6);
    }

    #[test]
    fn scan_and_list_agree() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("big"), vec![0u8; 3 << 20]).unwrap();
        std::fs::write(d.path().join("small"), b"x").unwrap();
        let src = vec![d.path().to_path_buf()];
        let a = scan(&src, &Default::default(), 1 << 20, &mut |_, _| true).unwrap();
        assert_eq!((a.large_files, a.small_files, a.large_bytes), (1, 1, 3 << 20));
        let list = d.path().join("list");
        let root = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(&list, format!("{0}\n{0}/big\n{0}/small\n/elsewhere/x\n", root.display())).unwrap();
        let b = from_list(&list, &src, 1 << 20, &mut |_, _| true).unwrap();
        assert_eq!((b.large_files, b.small_files, b.large_bytes), (1, 1, 3 << 20));
    }
}
