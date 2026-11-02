//! Software compression policy: which files are worth compressing with zstd.
//!
//! Software compression is off unless a level is set (the drive compresses in hardware for
//! free). When on: names matching `never` are stored as is, names matching `always` are
//! compressed, and anything else is decided by compressing a 1 MiB sample. Every chunk is
//! stored raw anyway if compression does not make it smaller.

use std::path::Path;

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Formats that are already compressed.
pub const DEFAULT_NEVER: &[&str] = &[
    "*.mp4", "*.m4v", "*.mkv", "*.mov", "*.avi", "*.ts", "*.m2ts", "*.mts", "*.webm", "*.wmv", "*.flv", "*.mpg",
    "*.mpeg", "*.vob", "*.jpg", "*.jpeg", "*.png", "*.gif", "*.webp", "*.heic", "*.heif", "*.avif", "*.jxl",
    "*.mp3", "*.m4a", "*.aac", "*.flac", "*.ogg", "*.opus", "*.wma", "*.dts", "*.ac3", "*.zip", "*.7z", "*.rar",
    "*.gz", "*.tgz", "*.bz2", "*.xz", "*.zst", "*.lz4", "*.lzma", "*.br", "*.jar", "*.apk", "*.docx", "*.xlsx",
    "*.pptx", "*.odt", "*.ods", "*.epub", "*.cbz",
];

/// Formats that compress well.
pub const DEFAULT_ALWAYS: &[&str] = &[
    "*.txt", "*.log", "*.csv", "*.tsv", "*.json", "*.xml", "*.html", "*.htm", "*.sql", "*.md", "*.yaml", "*.yml",
    "*.toml", "*.ini", "*.conf", "*.c", "*.h", "*.cpp", "*.rs", "*.py", "*.go", "*.java", "*.sh", "*.svg",
    "*.dump", "*.tar",
];

const SAMPLE: usize = 1 << 20;
/// Compressed sample must be at most this fraction of the original to be worth it.
const WORTH: f64 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Store,
    Compress,
    Probe,
}

#[derive(Clone)]
pub struct Policy {
    /// zstd level; None disables software compression.
    pub level: Option<i32>,
    never: GlobSet,
    always: GlobSet,
}

impl Default for Policy {
    fn default() -> Self {
        Policy::new(None, &[], &[]).expect("default patterns are valid")
    }
}

fn set(patterns: impl IntoIterator<Item = String>) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let full = if p.contains('/') { p.clone() } else { format!("**/{p}") };
        b.add(GlobBuilder::new(&full).case_insensitive(true).build().with_context(|| format!("bad pattern '{p}'"))?);
    }
    Ok(b.build()?)
}

impl Policy {
    /// `never`/`always` patterns are added to the defaults; `never` wins over `always`.
    pub fn new(level: Option<i32>, never: &[String], always: &[String]) -> Result<Policy> {
        Ok(Policy {
            level,
            never: set(DEFAULT_NEVER.iter().map(|s| s.to_string()).chain(never.iter().cloned()))?,
            always: set(DEFAULT_ALWAYS.iter().map(|s| s.to_string()).chain(always.iter().cloned()))?,
        })
    }

    pub fn enabled(&self) -> bool {
        self.level.is_some()
    }

    pub fn decide(&self, path: &Path) -> Decision {
        if self.level.is_none() || self.never.is_match(path) {
            Decision::Store
        } else if self.always.is_match(path) {
            Decision::Compress
        } else {
            Decision::Probe
        }
    }

    /// Resolve `Probe` by compressing (at most) the first MiB of `sample`.
    pub fn probe(&self, sample: &[u8]) -> bool {
        let s = &sample[..sample.len().min(SAMPLE)];
        if s.len() < 64 {
            return true;
        }
        match zstd::bulk::compress(s, 1) {
            Ok(c) => (c.len() as f64) <= WORTH * s.len() as f64,
            Err(_) => false,
        }
    }

    /// Compressed bytes if compression is on and actually saves space.
    pub fn compress(&self, data: &[u8]) -> Option<Vec<u8>> {
        let level = self.level?;
        zstd::bulk::compress(data, level).ok().filter(|c| c.len() < data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions() {
        let p = Policy::new(Some(3), &["*.bin".into()], &["*.dat".into()]).unwrap();
        assert_eq!(p.decide(Path::new("/x/Movie.MKV")), Decision::Store);
        assert_eq!(p.decide(Path::new("/x/notes.TXT")), Decision::Compress);
        assert_eq!(p.decide(Path::new("/x/a.bin")), Decision::Store);
        assert_eq!(p.decide(Path::new("/x/a.dat")), Decision::Compress);
        assert_eq!(p.decide(Path::new("/x/db.img")), Decision::Probe);
        assert_eq!(Policy::default().decide(Path::new("/x/notes.txt")), Decision::Store, "off without a level");
        assert!(p.probe(&vec![b'a'; 100_000]));
        let mut x = 0x9e3779b97f4a7c15u64;
        let noise: Vec<u8> = (0..100_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 32) as u8
            })
            .collect();
        assert!(!p.probe(&noise));
        assert!(p.compress(&noise).is_none());
    }
}
