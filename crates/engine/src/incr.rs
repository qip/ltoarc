//! Incremental, differential and append-only sessions: what a job's earlier sessions hold
//! per path, what a file found now amounts to against that (unchanged, changed or new), and
//! where its data may already be on tape (a moved or duplicate file, found by content hash).

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use ltoarc_catalog::{Catalog, HashRow, ObjectRow, SessionRow};
use ltoarc_format::record::FileKind;
use uuid::Uuid;

use crate::meta;

/// What a session compares against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// Everything.
    #[default]
    Full,
    /// What changed since the job's last session.
    Incremental,
    /// What changed since the job's last full session.
    Differential,
    /// Only files the job has never archived (dash cams, logs, camera cards): nothing is
    /// recorded as deleted, and a change to a file already archived is reported, not saved.
    Append,
}

impl Level {
    pub fn parse(s: &str) -> Result<Level> {
        Ok(match s {
            "full" => Level::Full,
            "incremental" | "incr" => Level::Incremental,
            "differential" | "diff" => Level::Differential,
            "append" | "append-only" => Level::Append,
            other => bail!("unknown level '{other}' (full, incremental, differential, append)"),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Full => "full",
            Level::Incremental => "incremental",
            Level::Differential => "differential",
            Level::Append => "append",
        }
    }
}

/// Which earlier data a new or changed file may refer to instead of being written again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dedup {
    Off,
    /// Files moved or renamed since the job's earlier sessions: same content, and the path
    /// it was archived under is gone.
    #[default]
    Moves,
    /// Any archived file with the same content.
    All,
}

impl Dedup {
    pub fn parse(s: &str) -> Result<Dedup> {
        Ok(match s {
            "off" => Dedup::Off,
            "moves" => Dedup::Moves,
            "all" => Dedup::All,
            other => bail!("unknown dedup mode '{other}' (off, moves, all)"),
        })
    }
}

/// Whether a match on a fingerprint (a digest another tool keeps in an xattr, e.g. one built
/// from the filesystem's block checksums, which are not cryptographic) is taken as is, or
/// confirmed by hashing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trust {
    #[default]
    Trust,
    Confirm,
}

impl Trust {
    pub fn parse(s: &str) -> Result<Trust> {
        Ok(match s {
            "trust" => Trust::Trust,
            "confirm" => Trust::Confirm,
            other => bail!("unknown fingerprint trust '{other}' (trust, confirm)"),
        })
    }
}

/// A path as the job's earlier sessions left it.
#[derive(Debug, Clone)]
pub struct Prev {
    pub object: ObjectRow,
    pub hashes: Vec<HashRow>,
    pub session_uuid: Uuid,
}

/// An archived file whose data a new file may share.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The object whose chunks hold the data, its session and id there.
    pub data_object: i64,
    pub session: Uuid,
    pub seq: u64,
    pub size: u64,
    /// Its hashes (content and fingerprints).
    pub hashes: Vec<HashRow>,
}

impl Candidate {
    pub fn from_prev(p: &Prev) -> Candidate {
        Candidate {
            data_object: p.object.data_object(),
            session: p.session_uuid,
            seq: p.object.seq as u64,
            size: p.object.size as u64,
            hashes: p.hashes.clone(),
        }
    }

    pub fn matches(&self, algo: &str, digest: &str) -> bool {
        self.hashes.iter().any(|h| h.algo == algo && h.digest == digest)
    }
}

/// The session a `--resume` continues: the newest one of `job` (without a job: the newest
/// job-less one of exactly these sources), when it ended partial (interrupted, or out of
/// cartridges). None when that newest session is complete (or there is none).
pub fn resumable(cat: &Catalog, job: Option<&str>, archived_sources: &[PathBuf]) -> Result<Option<SessionRow>> {
    let sources = serde_json::to_string(&archived_sources.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>())?;
    let newest = cat
        .sessions()?
        .into_iter()
        .filter(|s| s.expired_at.is_none() && (s.status == "complete" || s.status == "partial"))
        .filter(|s| s.job.as_deref() == job && (job.is_some() || s.sources == sources))
        .max_by_key(|s| (s.started, s.id));
    Ok(newest.filter(|s| s.status == "partial"))
}

/// What a session of `job` at `level` compares against, and the level it is recorded with:
/// with `resume`, the partial session it continues (an incremental session after it); an
/// incremental or differential one without any full session of the job becomes full.
pub fn previous_for(cat: &Catalog, job: Option<&str>, level: Level, resume: bool, archived_sources: &[PathBuf]) -> Result<(Previous, Level)> {
    if resume {
        let Some(partial) = resumable(cat, job, archived_sources)? else {
            bail!(
                "nothing to resume: the newest session of {} is complete (or there is none)",
                job.map_or_else(|| "these sources".to_string(), |j| format!("job {j}"))
            );
        };
        let level = if level == Level::Append { Level::Append } else { Level::Incremental };
        return Ok((Previous::continuing(cat, &partial)?, level));
    }
    let prev = Previous::load(cat, job, level)?;
    let level = if prev.no_full { Level::Full } else { level };
    Ok((prev, level))
}

/// What a path found now amounts to.
#[derive(Debug)]
pub enum Found<'a> {
    New,
    Unchanged,
    Changed(&'a Prev),
}

/// The state the job's earlier sessions left, per path.
#[derive(Default)]
pub struct Previous {
    /// The session this one builds on (its parent).
    pub base: Option<SessionRow>,
    /// The sessions compared against.
    pub sessions: Vec<i64>,
    /// An incremental or differential session asked for when the job has no full one yet.
    pub no_full: bool,
    entries: Vec<(Vec<u8>, Prev)>,
    index: HashMap<Vec<u8>, usize>,
    seen: Vec<bool>,
    /// File entries by size, for finding moved files.
    by_size: HashMap<i64, Vec<usize>>,
}

impl Previous {
    /// The state of `job` that a session of `level` compares against.
    pub fn load(cat: &Catalog, job: Option<&str>, level: Level) -> Result<Previous> {
        let Some(job) = job else {
            if level != Level::Full {
                bail!("a {} session needs --job: it compares against the job's earlier sessions", level.as_str());
            }
            return Ok(Previous::default());
        };
        let sessions = cat.job_sessions(job)?;
        let last_full = sessions.iter().rposition(|s| s.level == "full");
        let chain: Vec<&SessionRow> = match (level, last_full) {
            (Level::Full, _) => Vec::new(),
            (Level::Incremental | Level::Differential, None) => {
                return Ok(Previous { no_full: true, ..Default::default() });
            }
            (Level::Incremental, Some(f)) => sessions[f..].iter().collect(),
            (Level::Differential, Some(f)) => vec![&sessions[f]],
            (Level::Append, _) => sessions.iter().collect(),
        };
        Self::from_chain(cat, &chain)
    }

    /// The state to continue `partial` from (`--resume`): that session with the ones it
    /// builds on (back to their full session), so what it holds complete is not saved again.
    pub fn continuing(cat: &Catalog, partial: &SessionRow) -> Result<Previous> {
        let mut chain = vec![partial.clone()];
        while let Some(parent) = chain.last().and_then(|s| (s.level != "full").then_some(s.parent_id).flatten()) {
            match cat.session(&parent.to_string())? {
                Some(p) if p.expired_at.is_none() => chain.push(p),
                _ => break,
            }
        }
        chain.reverse();
        Self::from_chain(cat, &chain.iter().collect::<Vec<_>>())
    }

    fn from_chain(cat: &Catalog, chain: &[&SessionRow]) -> Result<Previous> {
        let ids: Vec<i64> = chain.iter().map(|s| s.id).collect();
        let uuids: HashMap<i64, Uuid> = chain.iter().filter_map(|s| Some((s.id, s.uuid.parse().ok()?))).collect();
        let mut hashes = cat.session_hashes(&ids)?;
        let mut prev = Previous { base: chain.last().map(|s| (*s).clone()), sessions: ids.clone(), ..Default::default() };
        for (path, object) in cat.state_of(&ids)? {
            let i = prev.entries.len();
            if object.kind == "file" && object.status == "ok" {
                prev.by_size.entry(object.size).or_default().push(i);
            }
            prev.index.insert(path.clone(), i);
            let h = hashes.remove(&object.id).unwrap_or_default();
            let session_uuid = uuids.get(&object.session_id).copied().unwrap_or_default();
            prev.entries.push((path, Prev { object, hashes: h, session_uuid }));
        }
        prev.seen = vec![false; prev.entries.len()];
        Ok(prev)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// What `path` (with metadata `md`) amounts to; marks it as still present.
    pub fn classify(&mut self, path: &Path, md: &Metadata) -> Found<'_> {
        let Some(&i) = self.index.get(path.as_os_str().as_bytes()) else { return Found::New };
        self.seen[i] = true;
        let p = &self.entries[i].1;
        let o = &p.object;
        if o.kind == "whiteout" {
            return Found::New;
        }
        let kind = meta::kind_of(md);
        let same_kind = o.kind == kind.as_str() || (o.kind == "hardlink" && kind == FileKind::File && md.nlink() > 1);
        let same = same_kind
            && o.status == "ok"
            && o.mtime_ns == meta::mtime_ns(md)
            && o.ctime_ns == meta::ctime_ns(md)
            && o.ino == md.ino() as i64
            && (kind != FileKind::File || o.kind == "hardlink" || o.size == md.len() as i64);
        if same {
            Found::Unchanged
        } else {
            Found::Changed(p)
        }
    }

    /// Paths the earlier sessions hold under `sources` that this walk did not find (deleted
    /// since), except those below `unknown` (directories that could not be read now).
    pub fn deleted(&self, sources: &[PathBuf], unknown: &[PathBuf]) -> Vec<&(Vec<u8>, Prev)> {
        let under = |p: &[u8], dir: &Path| {
            let d = dir.as_os_str().as_bytes();
            p == d || (p.starts_with(d) && (d.ends_with(b"/") || p.get(d.len()) == Some(&b'/')))
        };
        self.entries
            .iter()
            .zip(&self.seen)
            .filter(|((path, p), seen)| {
                !**seen && p.object.kind != "whiteout" && sources.iter().any(|s| under(path, s)) && !unknown.iter().any(|u| under(path, u))
            })
            .map(|(e, _)| e)
            .collect()
    }

    /// Archived files of `size` bytes whose path is gone or holds something else now:
    /// where a file of that size may have moved from.
    /// `real` gives where an archived path is on disk now.
    pub fn moved_candidates(&self, size: u64, real: &dyn Fn(&Path) -> PathBuf) -> Vec<Candidate> {
        let Some(idx) = self.by_size.get(&(size as i64)) else { return Vec::new() };
        idx.iter()
            .filter_map(|&i| {
                let (path, p) = &self.entries[i];
                let still_there = std::fs::symlink_metadata(real(meta::os(path)))
                    .is_ok_and(|md| md.ino() as i64 == p.object.ino && meta::mtime_ns(&md) == p.object.mtime_ns && md.len() == size);
                (!still_there).then(|| Candidate::from_prev(p))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse() {
        assert_eq!(Level::parse("incr").unwrap(), Level::Incremental);
        assert_eq!(Level::parse("differential").unwrap().as_str(), "differential");
        assert!(Level::parse("weekly").is_err());
        assert_eq!(Dedup::parse("all").unwrap(), Dedup::All);
        assert_eq!(Trust::parse("confirm").unwrap(), Trust::Confirm);
    }
}
