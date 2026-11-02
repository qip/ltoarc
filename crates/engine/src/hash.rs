//! File-level hash profiles: built-in content hashes computed while data streams to tape,
//! hashes kept in extended attributes (with a freshness check), and external hash tools.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use md5::Digest;

use crate::meta;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algo {
    Blake3,
    Sha256,
    Sha512,
    Sha1,
    Md5,
    #[serde(rename = "xxh3")]
    Xxh3,
}

impl Algo {
    pub const ALL: [Algo; 6] = [Algo::Blake3, Algo::Sha256, Algo::Sha512, Algo::Sha1, Algo::Md5, Algo::Xxh3];

    pub fn name(self) -> &'static str {
        match self {
            Algo::Blake3 => "blake3",
            Algo::Sha256 => "sha256",
            Algo::Sha512 => "sha512",
            Algo::Sha1 => "sha1",
            Algo::Md5 => "md5",
            Algo::Xxh3 => "xxh3",
        }
    }

    pub fn parse(s: &str) -> Option<Algo> {
        Algo::ALL.into_iter().find(|a| a.name() == s.to_ascii_lowercase())
    }

    pub fn hasher(self) -> Hasher {
        match self {
            Algo::Blake3 => Hasher::Blake3(Box::new(blake3::Hasher::new())),
            Algo::Sha256 => Hasher::Ring(ring::digest::Context::new(&ring::digest::SHA256)),
            Algo::Sha512 => Hasher::Ring(ring::digest::Context::new(&ring::digest::SHA512)),
            Algo::Sha1 => Hasher::Ring(ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY)),
            Algo::Md5 => Hasher::Md5(md5::Md5::new()),
            Algo::Xxh3 => Hasher::Xxh3(Box::new(xxhash_rust::xxh3::Xxh3::new())),
        }
    }

    /// Hex digest of a byte string (for tests and small inputs).
    pub fn digest(self, data: &[u8]) -> String {
        let mut h = self.hasher();
        h.update(data);
        h.finish()
    }
}

pub enum Hasher {
    Blake3(Box<blake3::Hasher>),
    /// SHA-1/256/512 from ring (BoringSSL assembly: 2.5x the portable sha2 crate on CPUs
    /// without SHA instructions).
    Ring(ring::digest::Context),
    Md5(md5::Md5),
    Xxh3(Box<xxhash_rust::xxh3::Xxh3>),
}

impl Hasher {
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Blake3(h) => {
                h.update(data);
            }
            Hasher::Ring(h) => h.update(data),
            Hasher::Md5(h) => h.update(data),
            Hasher::Xxh3(h) => h.update(data),
        }
    }

    pub fn finish(self) -> String {
        match self {
            Hasher::Blake3(h) => h.finalize().to_hex().to_string(),
            Hasher::Ring(h) => hex::encode(h.finish().as_ref()),
            Hasher::Md5(h) => hex::encode(h.finalize()),
            Hasher::Xxh3(h) => format!("{:032x}", h.digest128()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Hash of the file's bytes; verifiable after restore.
    Content,
    /// Identifies a file but cannot be recomputed from restored bytes (e.g. a digest another
    /// tool derives from the filesystem's block checksums).
    Fingerprint,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Content => "content",
            Kind::Fingerprint => "fingerprint",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    #[serde(skip)]
    pub name: String,
    pub kind: Kind,
    /// Built-in algorithm (content profiles).
    #[serde(default)]
    pub algo: Option<Algo>,
    /// External tool: `{path}` is replaced by the file path; the first word of its output is
    /// the digest.
    #[serde(default)]
    pub command: Option<String>,
    /// Extended attribute holding the digest.
    #[serde(default)]
    pub xattr: Option<String>,
    /// Extended attribute holding "mtime=SECS.NANOS;size=N" so stale digests are ignored.
    #[serde(default)]
    pub meta_xattr: Option<String>,
}

impl Profile {
    /// Name used in the catalog's `algo` column.
    pub fn algo_name(&self) -> String {
        self.algo.map_or_else(|| self.name.clone(), |a| a.name().to_string())
    }
}

/// Built-in profiles: one content profile per algorithm (digest written to
/// `user.ltoarc.<algo>`). Fingerprints kept by other tools are configured as `[hash.NAME]`.
pub fn builtin_profiles() -> Vec<Profile> {
    Algo::ALL
        .into_iter()
        .map(|a| Profile {
            name: a.name().into(),
            kind: Kind::Content,
            algo: Some(a),
            command: None,
            xattr: Some(format!("user.ltoarc.{}", a.name())),
            meta_xattr: Some("user.ltoarc.meta".into()),
        })
        .collect()
}

/// Profiles by name: built-ins overridden or extended by `configured`.
pub fn resolve_profiles(names: &[String], configured: &BTreeMap<String, Profile>) -> Result<Vec<Profile>> {
    let builtins = builtin_profiles();
    names
        .iter()
        .map(|n| {
            let mut p = configured
                .get(n)
                .cloned()
                .or_else(|| builtins.iter().find(|b| &b.name == n).cloned())
                .ok_or_else(|| {
                    let known: Vec<&str> = Algo::ALL.iter().map(|a| a.name()).chain(configured.keys().map(String::as_str)).collect();
                    anyhow!("unknown hash profile '{n}' ({}; or define [hash.{n}] in the config file)", known.join(", "))
                })?;
            p.name = n.clone();
            if p.kind == Kind::Content && p.algo.is_none() && p.command.is_none() {
                bail!("hash profile '{n}': a content profile needs `algo` or `command`");
            }
            if p.kind == Kind::Fingerprint && p.xattr.is_none() && p.command.is_none() {
                bail!("hash profile '{n}': a fingerprint profile needs `xattr` or `command`");
            }
            Ok(p)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Use a fresh digest from the xattr when present; otherwise compute it.
    #[default]
    UseExisting,
    /// Always compute.
    Generate,
    /// Compute and compare with a fresh xattr digest; mismatches suggest source bitrot.
    Verify,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        Ok(match s {
            "use-existing" => Mode::UseExisting,
            "generate" => Mode::Generate,
            "verify" => Mode::Verify,
            other => bail!("unknown hash mode '{other}' (use-existing, generate, verify)"),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub profiles: Vec<Profile>,
    pub mode: Mode,
    /// Write content digests (and their meta) back to the source files' xattrs.
    pub write_back: bool,
    /// Additional xattrs written to source files after the session: (name, template). The
    /// template may use {session}, {date}, {barcodes}, {job}, and {<profile>} digests.
    pub extra_xattrs: Vec<(String, String)>,
}

impl Policy {
    pub fn inline_content(&self) -> impl Iterator<Item = (&Profile, Algo)> {
        self.profiles.iter().filter(|p| p.kind == Kind::Content && p.command.is_none()).filter_map(|p| p.algo.map(|a| (p, a)))
    }
}

/// "mtime=SECS.NANOS;size=N" (plus optional "alg=..."), a format other checksum tools can
/// share, so a digest they keep is known to be fresh.
pub fn meta_value(algos: &str, mtime_ns: i64, size: u64) -> String {
    format!(
        "alg={algos};mtime={}.{:09};size={size}",
        mtime_ns.div_euclid(1_000_000_000),
        mtime_ns.rem_euclid(1_000_000_000)
    )
}

/// Parse (mtime_ns, size) from a meta xattr value.
pub fn parse_meta(v: &str) -> Option<(i64, u64)> {
    let mut mtime = None;
    let mut size = None;
    for part in v.trim_matches(|c: char| c == '"' || c == '\0' || c.is_whitespace()).split(';') {
        let (k, val) = part.split_once('=')?;
        match k.trim() {
            "mtime" => {
                let (s, frac) = val.split_once('.').unwrap_or((val, "0"));
                let frac: String = format!("{frac:0<9}").chars().take(9).collect();
                mtime = Some(s.parse::<i64>().ok()? * 1_000_000_000 + frac.parse::<i64>().ok()?);
            }
            "size" => size = val.parse().ok(),
            _ => {}
        }
    }
    Some((mtime?, size?))
}

fn clean_digest(v: &[u8]) -> String {
    String::from_utf8_lossy(v).trim_matches(|c: char| c == '"' || c == '\0' || c.is_whitespace()).to_ascii_lowercase()
}

/// Digest stored in the profile's xattr, if present and (when a meta xattr is configured)
/// still describing the file's current mtime and size.
pub fn fresh_xattr(path: &Path, p: &Profile, mtime_ns: i64, size: u64) -> Option<String> {
    let name = p.xattr.as_ref()?;
    let digest = clean_digest(&meta::get_xattr(path, name.as_bytes()).ok()??);
    if digest.is_empty() {
        return None;
    }
    if let Some(m) = &p.meta_xattr {
        let raw = meta::get_xattr(path, m.as_bytes()).ok()??;
        let (mt, sz) = parse_meta(&String::from_utf8_lossy(&raw))?;
        if mt != mtime_ns || sz != size {
            return None;
        }
    }
    Some(digest)
}

/// Run the profile's external command for `path`.
pub fn run_command(p: &Profile, path: &Path) -> Result<String> {
    let cmd = p.command.as_ref().ok_or_else(|| anyhow!("profile {} has no command", p.name))?;
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"));
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd.replace("{path}", &quoted))
        .output()
        .with_context(|| format!("running hash command of profile {}", p.name))?;
    if !out.status.success() {
        bail!("hash command of profile {} failed: {}", p.name, String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let digest = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if digest.is_empty() {
        bail!("hash command of profile {} printed nothing", p.name);
    }
    Ok(digest)
}

enum PoolMsg {
    Data(u64, std::sync::Arc<Vec<u8>>),
    Zeros(u64, u64),
    End(u64),
}

/// Content hashing beside a reader (verify, restore): one thread per algorithm, each keeping
/// a hasher per file, fed in file order with shared buffers, so the algorithms run side by
/// side and alongside the decoding instead of one after another.
pub struct HashPool {
    lanes: Vec<(Algo, crossbeam_channel::Sender<PoolMsg>, PoolThread)>,
}

/// A pool thread: returns file id -> hex digest.
type PoolThread = std::thread::JoinHandle<std::collections::HashMap<u64, String>>;

impl HashPool {
    pub fn new() -> HashPool {
        HashPool { lanes: Vec::new() }
    }

    fn lane(&mut self, algo: Algo) -> &crossbeam_channel::Sender<PoolMsg> {
        if let Some(i) = self.lanes.iter().position(|(a, _, _)| *a == algo) {
            return &self.lanes[i].1;
        }
        let (tx, rx) = crossbeam_channel::bounded::<PoolMsg>(16);
        let h = std::thread::spawn(move || {
            let mut open: std::collections::HashMap<u64, Hasher> = std::collections::HashMap::new();
            let mut done = std::collections::HashMap::new();
            let zeros = vec![0u8; 1 << 20];
            for msg in rx {
                match msg {
                    PoolMsg::Data(id, d) => open.entry(id).or_insert_with(|| algo.hasher()).update(&d),
                    PoolMsg::Zeros(id, mut n) => {
                        let h = open.entry(id).or_insert_with(|| algo.hasher());
                        while n > 0 {
                            let k = n.min(zeros.len() as u64) as usize;
                            h.update(&zeros[..k]);
                            n -= k as u64;
                        }
                    }
                    PoolMsg::End(id) => {
                        let h = open.remove(&id).unwrap_or_else(|| algo.hasher());
                        done.insert(id, h.finish());
                    }
                }
            }
            done
        });
        self.lanes.push((algo, tx, h));
        &self.lanes.last().unwrap().1
    }

    pub fn data(&mut self, id: u64, algos: &[Algo], data: &std::sync::Arc<Vec<u8>>) {
        for &a in algos {
            let _ = self.lane(a).send(PoolMsg::Data(id, data.clone()));
        }
    }

    pub fn zeros(&mut self, id: u64, algos: &[Algo], n: u64) {
        if n > 0 {
            for &a in algos {
                let _ = self.lane(a).send(PoolMsg::Zeros(id, n));
            }
        }
    }

    pub fn end(&mut self, id: u64, algos: &[Algo]) {
        for &a in algos {
            let _ = self.lane(a).send(PoolMsg::End(id));
        }
    }

    /// Wait for every hash: file id -> (algorithm name, hex digest).
    pub fn finish(self) -> std::collections::HashMap<u64, Vec<(String, String)>> {
        let mut out: std::collections::HashMap<u64, Vec<(String, String)>> = std::collections::HashMap::new();
        for (algo, tx, h) in self.lanes {
            drop(tx);
            for (id, d) in h.join().unwrap_or_default() {
                out.entry(id).or_default().push((algo.name().to_string(), d));
            }
        }
        out
    }
}

impl Default for HashPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_digests() {
        assert_eq!(Algo::Sha256.digest(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(Algo::Md5.digest(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(Algo::Sha1.digest(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(Algo::Blake3.digest(b"abc"), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
        assert_eq!(Algo::Xxh3.digest(b"").len(), 32);
    }

    #[test]
    fn zfssum_meta_format() {
        let v = "alg=blake3,size,holefill;mtime=1747454229.587269900;size=3671483958";
        assert_eq!(parse_meta(v), Some((1_747_454_229_587_269_900, 3_671_483_958)));
        assert_eq!(parse_meta(&meta_value("sha256", 1_747_454_229_587_269_900, 5)), Some((1_747_454_229_587_269_900, 5)));
        assert_eq!(parse_meta("mtime=12.5;size=1"), Some((12_500_000_000, 1)));
        assert_eq!(parse_meta("garbage"), None);
    }

    #[test]
    fn profiles_resolve() {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "zfssum".to_string(),
            Profile {
                name: String::new(),
                kind: Kind::Fingerprint,
                algo: None,
                command: Some("zfssum.py {path}".into()),
                xattr: Some("user.zfssum.checksum".into()),
                meta_xattr: Some("user.zfssum.meta".into()),
            },
        );
        let ps = resolve_profiles(&["sha256".into(), "zfssum".into()], &cfg).unwrap();
        assert_eq!(ps[0].algo, Some(Algo::Sha256));
        assert_eq!(ps[1].command.as_deref(), Some("zfssum.py {path}"));
        assert!(resolve_profiles(&["nope".into()], &cfg).is_err());
        // Fingerprints of other tools are configured, not built in.
        assert!(resolve_profiles(&["zfssum".into()], &BTreeMap::new()).is_err());
    }

    #[test]
    fn fresh_and_stale_xattrs() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("f");
        std::fs::write(&f, "hello").unwrap();
        let p = &builtin_profiles()[1]; // sha256
        if meta::set_xattr(&f, b"user.ltoarc.sha256", b"\"ABCDEF\"").is_err() {
            return; // filesystem without user xattrs
        }
        meta::set_xattr(&f, b"user.ltoarc.meta", meta_value("sha256", 42, 5).as_bytes()).unwrap();
        assert_eq!(fresh_xattr(&f, p, 42, 5).as_deref(), Some("abcdef"));
        assert_eq!(fresh_xattr(&f, p, 43, 5), None);
        assert_eq!(fresh_xattr(&f, p, 42, 6), None);
    }

    #[test]
    fn external_command() {
        let p = Profile {
            name: "echo".into(),
            kind: Kind::Content,
            algo: None,
            command: Some("printf 'DEADBEEF  %s\\n' {path}".into()),
            xattr: None,
            meta_xattr: None,
        };
        assert_eq!(run_command(&p, Path::new("/tmp/it's here")).unwrap(), "deadbeef");
    }
}
