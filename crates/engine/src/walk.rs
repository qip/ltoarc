//! Depth-first traversal of source trees: a directory is visited before its contents.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Default, Clone)]
pub struct WalkOptions {
    pub excludes: Vec<String>,
    /// Do not descend into other filesystems below a source.
    pub one_file_system: bool,
}

pub struct Walker {
    excludes: GlobSet,
    one_file_system: bool,
    pub warnings: Vec<String>,
    /// Paths whose contents could not be listed or examined this time: an incremental
    /// session must not record what lies there as deleted.
    pub unknown: Vec<PathBuf>,
}

impl Walker {
    pub fn new(opts: &WalkOptions) -> Result<Self> {
        let mut b = GlobSetBuilder::new();
        for pat in &opts.excludes {
            // A pattern without '/' matches a name anywhere; otherwise the full path.
            let p = if pat.contains('/') { pat.clone() } else { format!("**/{pat}") };
            b.add(Glob::new(&p).with_context(|| format!("bad exclude pattern '{pat}'"))?);
        }
        Ok(Walker { excludes: b.build()?, one_file_system: opts.one_file_system, warnings: Vec::new(), unknown: Vec::new() })
    }

    pub fn excluded(&self, path: &Path) -> bool {
        self.excludes.is_match(path)
    }

    /// Visit every entry under each source (sources themselves included). `visit` returning
    /// `Ok(false)` stops the walk.
    pub fn walk(&mut self, sources: &[PathBuf], visit: &mut dyn FnMut(&Path, &Metadata) -> Result<bool>) -> Result<()> {
        for src in sources {
            if !src.is_absolute() {
                bail!("source {} is not an absolute path", src.display());
            }
            let md = std::fs::symlink_metadata(src).with_context(|| format!("cannot access {}", src.display()))?;
            if !self.visit_tree(src, &md, md.dev(), visit)? {
                return Ok(());
            }
        }
        Ok(())
    }

    fn visit_tree(
        &mut self,
        path: &Path,
        md: &Metadata,
        root_dev: u64,
        visit: &mut dyn FnMut(&Path, &Metadata) -> Result<bool>,
    ) -> Result<bool> {
        if self.excluded(path) {
            return Ok(true);
        }
        if !visit(path, md)? {
            return Ok(false);
        }
        if !md.is_dir() || (self.one_file_system && md.dev() != root_dev) {
            return Ok(true);
        }
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(path) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(e) => {
                self.warnings.push(format!("cannot read directory {}: {e}", path.display()));
                self.unknown.push(path.to_path_buf());
                return Ok(true);
            }
        };
        entries.sort();
        for child in entries {
            let cmd = match std::fs::symlink_metadata(&child) {
                Ok(m) => m,
                Err(e) => {
                    self.warnings.push(format!("cannot stat {}: {e}", child.display()));
                    self.unknown.push(child);
                    continue;
                }
            };
            if self.one_file_system && cmd.dev() != root_dev && cmd.is_dir() {
                // Record the mount point itself but not its contents.
                if !self.excluded(&child) && !visit(&child, &cmd)? {
                    return Ok(false);
                }
                continue;
            }
            if !self.visit_tree(&child, &cmd, root_dev, visit)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_and_excludes() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("a/b")).unwrap();
        std::fs::write(r.join("a/b/f.txt"), "x").unwrap();
        std::fs::write(r.join("a/skip.tmp"), "x").unwrap();
        std::fs::write(r.join("z"), "x").unwrap();
        let mut w = Walker::new(&WalkOptions { excludes: vec!["*.tmp".into()], one_file_system: false }).unwrap();
        let mut seen = Vec::new();
        w.walk(&[r.to_path_buf()], &mut |p, _| {
            seen.push(p.strip_prefix(r).unwrap().display().to_string());
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, ["", "a", "a/b", "a/b/f.txt", "z"]);
    }
}
