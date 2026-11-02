//! Measure a drive's sustained write and read rates on an ltoarc cartridge, and optionally
//! how fast a source can be read.
//!
//! Test blocks of incompressible data are written after end of data, read back, and then
//! removed: the filemark just before them is rewritten, which makes that point end of data
//! again (tape is append-only, so writing truncates everything after it).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ltoarc_catalog::VolumeRow;
use ltoarc_device::{Position, ReadStatus, TapeDrive};

use crate::backup::{check_label, perf_counters};
use crate::tapeio::read_label;

#[derive(Debug, Default)]
pub struct Report {
    pub block_size: u32,
    pub bytes: u64,
    /// Including the final flush of the drive buffer to tape.
    pub write_mbps: f64,
    /// While streaming: everything after the first WRITE (which includes positioning),
    /// before the final flush.
    pub steady_write_mbps: f64,
    pub read_mbps: f64,
    pub write_cycles: Option<u64>,
    pub write_pauses: Option<u64>,
    pub source_mbps: Option<f64>,
    /// Per-WRITE latency in milliseconds: (min, median, 90th percentile, max).
    pub write_latency_ms: (f64, f64, f64, f64),
    /// Written through the kernel st driver instead of SCSI generic.
    pub via_st: bool,
    /// Speed-matching sweep, fastest first: (paced MB/s, tape starts, write pauses the drive
    /// counted) per step.
    pub sweep: Vec<(f64, u64, u64)>,
    /// Slowest rate of the sweep at which the drive did not pause.
    pub floor_mbps: Option<f64>,
    /// Rates the drive streamed at without pausing, as ranges in MB/s (slowest first): two
    /// with a gap between are what `[rates]` takes for a drive that cannot match the speeds
    /// in between.
    pub ranges: Vec<(f64, f64)>,
}

/// Write `bytes` of test data at `rate` bytes/s (by sleeping between blocks), as the backup
/// writer paces: after a slow write (the first after a standstill can take seconds) it
/// catches up at most 200 ms, instead of writing flat out until back on schedule, which on
/// a drive barely faster than `rate` would mean flat out for good.
fn write_paced(drive: &mut dyn TapeDrive, blocks: &[Vec<u8>], bytes: u64, rate: f64) -> Result<()> {
    let mut next_due = Instant::now();
    let mut sent = 0u64;
    let mut n = 0usize;
    while sent < bytes {
        if let Some(wait) = next_due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let b = &blocks[n % blocks.len()];
        drive.write_block(b).context("writing test data")?;
        sent += b.len() as u64;
        n += 1;
        next_due = next_due.max(Instant::now() - Duration::from_millis(200)) + Duration::from_secs_f64(b.len() as f64 / rate);
    }
    Ok(())
}

/// Pauses a sweep step may show and still count as streaming (one is often the start).
pub const SWEEP_PAUSES_OK: u64 = 1;

/// Rates (MB/s) a sweep tries, fastest first: down from `max` (rounded down to a multiple of
/// `step`) to `low`.
pub fn sweep_rates(max: f64, step: f64, low: f64) -> Vec<f64> {
    let mut r = (max / step).floor() * step;
    let mut out = Vec::new();
    while r >= low {
        out.push(r);
        r -= step;
    }
    out
}

/// Runs of rates the drive streamed at (`ok`), slowest first, from sweep steps given
/// fastest first; each run spans from its slowest to its fastest step.
pub fn streaming_ranges(steps: &[(f64, bool)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut run: Option<(f64, f64)> = None;
    for (rate, ok) in steps.iter().rev() {
        match (ok, run) {
            (true, None) => run = Some((*rate, *rate)),
            (true, Some((lo, _))) => run = Some((lo, *rate)),
            (false, Some(r)) => {
                out.push(r);
                run = None;
            }
            (false, None) => {}
        }
    }
    out.extend(run);
    out
}

fn percentiles(mut v: Vec<f64>) -> (f64, f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    (v[0], at(0.5), at(0.9), v[v.len() - 1])
}

/// Write blocks through the st driver (direct I/O), for comparison with the sg path.
fn write_via_st(path: &str, blocks: &[Vec<u8>], bytes: u64, max_time: Duration, lat: &mut Vec<f64>) -> Result<u64> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(path).with_context(|| format!("opening {path}"))?;
    // MTIOCTOP MTSETBLK 0: variable-block mode.
    #[repr(C)]
    struct MtOp {
        op: libc::c_short,
        count: libc::c_int,
    }
    let op = MtOp { op: 20, count: 0 };
    // SAFETY: MTIOCTOP with a valid struct mtop.
    if unsafe { libc::ioctl(f.as_raw_fd(), 0x4008_6d01u64 as _, &op as *const MtOp) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setting variable block mode on st");
    }
    let (mut written, mut n) = (0u64, 0usize);
    let t = Instant::now();
    while written < bytes && t.elapsed() < max_time {
        let b = &blocks[n % blocks.len()];
        let t0 = Instant::now();
        f.write_all(b)?;
        lat.push(t0.elapsed().as_secs_f64() * 1e3);
        written += b.len() as u64;
        n += 1;
    }
    // Closing after writing makes st write a filemark, which flushes the drive buffer.
    drop(f);
    Ok(written)
}

/// Incompressible test data: a few distinct pseudo-random blocks reused in turn.
fn test_blocks(block: usize) -> Vec<Vec<u8>> {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    (0..16)
        .map(|_| {
            (0..block)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 24) as u8
                })
                .collect()
        })
        .collect()
}

/// Write up to `bytes` (or `max_time`) of test data, read it back, remove it. With `via_st`
/// (the drive's /dev/nstN), the writes go through the st driver.
/// How a speed-matching sweep runs: seconds written at each rate, and the rates (MB/s) from
/// the measured maximum down to `low` in steps of `step`.
#[derive(Debug, Clone, Copy)]
pub struct Sweep {
    pub secs: f64,
    pub step: f64,
    pub low: Option<f64>,
}

pub fn drive(
    drive: &mut dyn TapeDrive,
    volume: &VolumeRow,
    bytes: u64,
    max_time: Duration,
    via_st: Option<&str>,
    sweep: Option<Sweep>,
) -> Result<Report> {
    let label = read_label(drive)?;
    check_label(&label, volume)?;
    let bs = label.block_size as usize;
    let start = drive.locate_eod(0)?.pos.object;
    if start < 2 {
        bail!("cartridge has no label file");
    }
    // The object before end of data must be a filemark: rewriting it is how we clean up.
    drive.locate(Position { partition: 0, object: start - 1 })?;
    let mut probe = vec![0u8; bs];
    if drive.read_block(&mut probe)? != ReadStatus::Filemark {
        bail!("end of data does not follow a filemark; refusing to calibrate on this cartridge");
    }
    // Measure the tape, not the compressor (the data is incompressible anyway).
    let _ = drive.set_compression(false);
    let blocks = test_blocks(bs);
    let perf0 = perf_counters(drive);
    let mut lat = Vec::new();
    // Reading the filemark left the drive in read mode; a LOCATE puts it where a backup
    // would start writing, so the measurement does not include switching from reading.
    drive.locate(Position { partition: 0, object: start })?;
    let t = Instant::now();
    let written = match via_st {
        Some(path) => write_via_st(path, &blocks, bytes, max_time, &mut lat)?,
        None => {
            let mut written = 0u64;
            let mut n = 0u64;
            while written < bytes && t.elapsed() < max_time {
                let t0 = Instant::now();
                drive.write_block(&blocks[(n % 16) as usize]).context("writing test data")?;
                lat.push(t0.elapsed().as_secs_f64() * 1e3);
                written += bs as u64;
                n += 1;
            }
            drive.write_filemarks(1)?; // flushes the drive buffer, so the timing includes the tape
            written
        }
    };
    let n = written / bs as u64;
    let write_secs = t.elapsed().as_secs_f64();
    // Skip the first quarter: positioning to append and filling the drive buffer.
    let skip = lat.len() / 4;
    let steady_secs: f64 = lat.iter().skip(skip).sum::<f64>() / 1e3;
    let steady_write_mbps = ((lat.len() - skip) * bs) as f64 / steady_secs.max(1e-3) / 1e6;
    let perf1 = perf_counters(drive);
    // Speed-matching sweep, appended after the full-speed data (removed with it): each rate
    // from a standstill to a standstill, as a burst of a backup runs, with the drive's
    // counters read only while it stands (reading them while it writes fast disturbs it).
    let mut steps = Vec::new();
    if let (Some(sw), Some(_)) = (sweep, perf1) {
        drive.locate_eod(0)?;
        let low = sw.low.unwrap_or((steady_write_mbps / 6.0).max(40.0));
        for rate in sweep_rates(steady_write_mbps, sw.step, low) {
            drive.write_filemarks(1)?; // the drive writes out its buffer and stops
            let before = perf_counters(drive).unwrap_or((0, 0));
            write_paced(drive, &blocks, (rate * 1e6 * sw.secs) as u64, rate * 1e6)?;
            drive.write_filemarks(1)?;
            let after = perf_counters(drive).unwrap_or(before);
            steps.push((rate, after.0.saturating_sub(before.0), after.1.saturating_sub(before.1)));
        }
    }
    let ok: Vec<(f64, bool)> = steps.iter().map(|(r, _, p)| (*r, *p <= SWEEP_PAUSES_OK)).collect();
    let ranges = streaming_ranges(&ok);
    let floor_mbps = ranges.first().map(|r| r.0);
    drive.locate(Position { partition: 0, object: start })?;
    let mut buf = vec![0u8; bs];
    let t = Instant::now();
    let mut read = 0u64;
    for _ in 0..n {
        match drive.read_block(&mut buf)? {
            ReadStatus::Data(k) => read += k as u64,
            other => bail!("unexpected {other:?} while reading test data back"),
        }
    }
    let read_secs = t.elapsed().as_secs_f64();
    // Remove the test data: rewrite the filemark before it.
    drive.locate(Position { partition: 0, object: start - 1 })?;
    drive.write_filemarks(1)?;
    let eod = drive.locate_eod(0)?.pos.object;
    if eod != start {
        bail!("cleanup left end of data at block {eod}, expected {start}");
    }
    let (write_cycles, write_pauses) = match (perf0, perf1) {
        (Some(a), Some(b)) => (Some(b.0.saturating_sub(a.0)), Some(b.1.saturating_sub(a.1))),
        _ => (None, None),
    };
    Ok(Report {
        block_size: label.block_size,
        bytes: written,
        write_mbps: written as f64 / write_secs.max(1e-3) / 1e6,
        steady_write_mbps,
        read_mbps: read as f64 / read_secs.max(1e-3) / 1e6,
        write_cycles,
        write_pauses,
        source_mbps: None,
        write_latency_ms: percentiles(lat),
        via_st: via_st.is_some(),
        sweep: steps,
        floor_mbps,
        ranges,
    })
}

/// Read up to `bytes` of regular files under `root` with `threads` readers; returns MB/s.
pub fn source(root: &std::path::Path, bytes: u64, threads: usize, max_time: Duration) -> Result<f64> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut total = 0u64;
    let mut w = crate::walk::Walker::new(&Default::default())?;
    w.walk(&[std::fs::canonicalize(root)?], &mut |p, md| {
        if md.is_file() && md.len() > 0 {
            files.push(p.to_path_buf());
            total += md.len();
        }
        Ok(total < bytes)
    })?;
    if files.is_empty() {
        bail!("no files to read under {}", root.display());
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let read = std::sync::atomic::AtomicU64::new(0);
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                use std::io::Read;
                let mut buf = vec![0u8; 16 << 20];
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(p) = files.get(i) else { return };
                    let Ok(mut f) = std::fs::File::open(p) else { continue };
                    while let Ok(k) = f.read(&mut buf) {
                        if k == 0 || t.elapsed() > max_time {
                            break;
                        }
                        read.fetch_add(k as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    if t.elapsed() > max_time || read.load(std::sync::atomic::Ordering::Relaxed) >= bytes {
                        return;
                    }
                }
            });
        }
    });
    Ok(read.into_inner() as f64 / t.elapsed().as_secs_f64().max(1e-3) / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_finds_the_gap() {
        assert_eq!(sweep_rates(296.4, 20.0, 60.0), vec![280.0, 260.0, 240.0, 220.0, 200.0, 180.0, 160.0, 140.0, 120.0, 100.0, 80.0, 60.0]);
        // As measured on an IBM LTO-8 half-height drive with LTO-7 media.
        let steps: Vec<(f64, bool)> = [300.0, 280.0, 250.0, 220.0, 200.0, 180.0, 160.0, 150.0, 140.0, 100.0, 60.0, 50.0]
            .iter()
            .map(|r| (*r, !(160.0..=250.0).contains(r) && *r > 55.0))
            .collect();
        assert_eq!(streaming_ranges(&steps), vec![(60.0, 150.0), (280.0, 300.0)]);
        assert_eq!(streaming_ranges(&[(100.0, true), (90.0, true)]), vec![(90.0, 100.0)]);
        assert!(streaming_ranges(&[(100.0, false)]).is_empty());
    }
}
