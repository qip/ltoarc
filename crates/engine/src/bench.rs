//! Source and drive speed measurements, stored in the catalog once and reused by the
//! scheduler (how much to buffer before starting the drive, what rate to pace it at).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::walk::Walker;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceBench {
    /// Filesystem identity: "<fstype>:<source>@<mountpoint>".
    pub fs: String,
    /// Directory entries visited per second while scanning.
    pub scan_entries_per_sec: f64,
    /// One sequential reader on a large file.
    pub seq_mbps: f64,
    /// Four sequential readers on different large files.
    pub seq_parallel_mbps: f64,
    /// Eight readers on small files (up to 1 MiB).
    pub small_files_per_sec: f64,
    pub small_mbps: f64,
    pub sampled_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriveBench {
    pub generation: String,
    pub max_write_mbps: f64,
    pub read_mbps: f64,
    /// Slowest host rate the drive streams at without stopping (speed matching).
    pub floor_mbps: f64,
    /// False when `floor_mbps` is the default estimate (a third of the maximum).
    pub floor_measured: bool,
}

/// Filesystem a path lives on, from /proc/self/mountinfo (longest matching mount point).
pub fn fs_key(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<(PathBuf, String)> = None;
    if let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") {
        for line in info.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            let Some(sep) = f.iter().position(|x| *x == "-") else { continue };
            let (Some(mp), Some(fstype), Some(src)) = (f.get(4), f.get(sep + 1), f.get(sep + 2)) else { continue };
            let mp = PathBuf::from(mp.replace("\\040", " "));
            if path.starts_with(&mp) && best.as_ref().is_none_or(|(b, _)| mp.as_os_str().len() > b.as_os_str().len()) {
                best = Some((mp.clone(), format!("{fstype}:{src}@{}", mp.display())));
            }
        }
    }
    best.map(|b| b.1).unwrap_or_else(|| format!("unknown@{}", path.display()))
}

fn read_files(files: &[PathBuf], threads: usize, max_bytes: u64, max_time: Duration, block: usize) -> (u64, u64, f64) {
    let next = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| {
                use std::io::Read;
                let mut buf = vec![0u8; block];
                loop {
                    if t.elapsed() > max_time || bytes.load(Ordering::Relaxed) >= max_bytes {
                        return;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(p) = files.get(i) else { return };
                    let Ok(mut f) = std::fs::File::open(p) else { continue };
                    while let Ok(k) = f.read(&mut buf) {
                        if k == 0 {
                            done.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        bytes.fetch_add(k as u64, Ordering::Relaxed);
                        if t.elapsed() > max_time || bytes.load(Ordering::Relaxed) >= max_bytes {
                            return;
                        }
                    }
                }
            });
        }
    });
    (bytes.into_inner(), done.into_inner(), t.elapsed().as_secs_f64().max(1e-3))
}

/// Measure one source. Reads at most `max_bytes` per test and stops each test after
/// `max_time`. Results are optimistic if the files are already in the page cache / ARC.
pub fn source(root: &Path, max_bytes: u64, max_time: Duration) -> Result<SourceBench> {
    let root = std::fs::canonicalize(root)?;
    let (mut large, mut small) = (Vec::new(), Vec::new());
    let mut entries = 0u64;
    let t = Instant::now();
    let mut w = Walker::new(&Default::default())?;
    w.walk(std::slice::from_ref(&root), &mut |p, md| {
        entries += 1;
        if md.is_file() {
            if md.len() >= 64 << 20 {
                large.push((md.len(), p.to_path_buf()));
            } else if md.len() > 0 && md.len() <= 1 << 20 {
                small.push(p.to_path_buf());
            }
        }
        Ok(t.elapsed() < max_time && entries < 2_000_000 && (large.len() < 64 || small.len() < 20_000))
    })?;
    let scan_entries_per_sec = entries as f64 / t.elapsed().as_secs_f64().max(1e-3);
    large.sort_by_key(|l| std::cmp::Reverse(l.0));
    let large: Vec<PathBuf> = large.into_iter().map(|(_, p)| p).collect();
    let mut r = SourceBench { fs: fs_key(&root), scan_entries_per_sec, ..Default::default() };
    if let Some(first) = large.first() {
        let (b, _, s) = read_files(std::slice::from_ref(first), 1, max_bytes / 2, max_time, 16 << 20);
        r.seq_mbps = b as f64 / s / 1e6;
        r.sampled_bytes += b;
    }
    if large.len() > 1 {
        let (b, _, s) = read_files(&large[1..], 4, max_bytes, max_time, 16 << 20);
        r.seq_parallel_mbps = b as f64 / s / 1e6;
        r.sampled_bytes += b;
    }
    if !small.is_empty() {
        let (b, n, s) = read_files(&small, 8, max_bytes, max_time, 1 << 20);
        r.small_files_per_sec = n as f64 / s;
        r.small_mbps = b as f64 / s / 1e6;
        r.sampled_bytes += b;
    }
    if large.is_empty() && small.is_empty() {
        bail!("no regular files found under {}", root.display());
    }
    Ok(r)
}
