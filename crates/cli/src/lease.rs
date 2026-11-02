//! Sharing drives and changers between ltoarc processes.
//!
//! A process uses a drive only while it holds the drive's lease: an advisory lock (flock) on
//! a file named after the drive's serial, which the kernel drops when the process ends,
//! crash included. The file names the holder, so others can say who they are waiting for.
//! Changer moves take the library's lock for the moment they last.
//!
//! Waiting processes queue by priority (restores and mount reads first, then backups and
//! copies, then verification), then by arrival: each waiter leaves a small file saying what
//! it waits for, and takes a free drive only when no better waiter wants it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use ltoarc_catalog::{DriveRow, LibraryRow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub enum Priority {
    /// Someone is waiting at a prompt or a mount for data.
    Interactive = 0,
    /// Backups and copies.
    #[default]
    Backup = 1,
    /// Verification and scrubs.
    Scrub = 2,
}

/// Where lock files for drives and libraries live: next to a simulated world, else in
/// /run/lock/ltoarc (shared by every user of the host's hardware).
pub fn lock_dir_for(backend: &str, location: Option<&str>, real_dir: &Path) -> PathBuf {
    match (backend, location) {
        ("sim", Some(world)) => Path::new(world).join("locks"),
        _ => real_dir.to_path_buf(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Waiter {
    pid: u32,
    /// Distinguishes several waiters in one process (tests).
    token: u64,
    priority: Priority,
    since: f64,
    what: String,
    /// Lock files of the drives it would take.
    wants: Vec<PathBuf>,
}

fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn flock(f: &File, how: i32) -> std::io::Result<bool> {
    // SAFETY: flock on a valid descriptor.
    if unsafe { libc::flock(f.as_raw_fd(), how) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(e)
    }
}

pub struct Leases {
    real_dir: PathBuf,
    priority: Priority,
    what: String,
    token: u64,
    /// Lock files held, by path.
    held: RefCell<HashMap<PathBuf, File>>,
    poll: Duration,
}

impl Leases {
    pub fn new(real_dir: PathBuf, priority: Priority, what: &str) -> Leases {
        let token = (now_f64().fract() * 1e9) as u64 ^ (std::process::id() as u64) << 32;
        Leases { real_dir, priority, what: what.into(), token, held: RefCell::new(HashMap::new()), poll: Duration::from_millis(500) }
    }

    pub fn set_purpose(&mut self, priority: Priority, what: &str) {
        self.priority = priority;
        self.what = what.into();
    }

    pub fn path(&self, d: &DriveRow) -> PathBuf {
        lock_dir_for(&d.backend, d.location.as_deref(), &self.real_dir).join(format!("drive-{}.lock", d.serial))
    }

    fn open(path: &Path) -> Result<File> {
        if let Some(dir) = path.parent() {
            if !dir.exists() {
                std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                // Shared by every user of the hardware.
                let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o1777));
            }
        }
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).with_context(|| format!("opening {}", path.display()))?;
        let _ = f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
        Ok(f)
    }

    /// Who holds drive `d` if another process (or another lease set) does.
    pub fn busy(&self, d: &DriveRow) -> Result<Option<String>> {
        let path = self.path(d);
        if self.held.borrow().contains_key(&path) {
            return Ok(None);
        }
        let mut f = Self::open(&path)?;
        if flock(&f, libc::LOCK_EX | libc::LOCK_NB)? {
            flock(&f, libc::LOCK_UN)?;
            return Ok(None);
        }
        let mut who = String::new();
        let _ = f.read_to_string(&mut who);
        Ok(Some(if who.trim().is_empty() { "another process".into() } else { who.trim().to_string() }))
    }

    /// Take drive `d` if it is free and no better waiter wants it.
    pub fn try_take(&self, d: &DriveRow) -> Result<bool> {
        let path = self.path(d);
        if self.held.borrow().contains_key(&path) {
            return Ok(true);
        }
        if self.better_waiter(&path)? {
            return Ok(false);
        }
        let mut f = Self::open(&path)?;
        if !flock(&f, libc::LOCK_EX | libc::LOCK_NB)? {
            return Ok(false);
        }
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        let when = chrono::Local::now().format("%H:%M:%S");
        write!(f, "pid {} ({}, since {when})", std::process::id(), self.what)?;
        self.held.borrow_mut().insert(path, f);
        Ok(true)
    }

    /// Take the first of `cands` to come free, waiting as long as it takes (in order of
    /// priority and arrival among waiters). `tell` is called once with who holds them.
    pub fn take_any(&self, cands: &[DriveRow], tell: &mut dyn FnMut(&str)) -> Result<DriveRow> {
        if cands.is_empty() {
            bail!("no drive to wait for");
        }
        let wants: Vec<PathBuf> = cands.iter().map(|d| self.path(d)).collect();
        let mut wait_file: Option<PathBuf> = None;
        let mut told = false;
        let result = loop {
            let mut taken = None;
            for d in cands {
                if self.try_take(d)? {
                    taken = Some(d.clone());
                    break;
                }
            }
            if let Some(d) = taken {
                break Ok(d);
            }
            if wait_file.is_none() {
                let dir = wants[0].parent().expect("lock files live in a directory").to_path_buf();
                let me = Waiter {
                    pid: std::process::id(),
                    token: self.token,
                    priority: self.priority,
                    since: now_f64(),
                    what: self.what.clone(),
                    wants: wants.clone(),
                };
                let p = dir.join(format!("wait-{}-{}.json", me.pid, me.token));
                std::fs::write(&p, serde_json::to_vec(&me)?)?;
                wait_file = Some(p);
            }
            if !told {
                let holders: Vec<String> = cands.iter().filter_map(|d| self.busy(d).ok().flatten().map(|w| format!("{}: {w}", d.name))).collect();
                tell(&holders.join("; "));
                told = true;
            }
            std::thread::sleep(self.poll);
        };
        if let Some(p) = wait_file {
            let _ = std::fs::remove_file(p);
        }
        result
    }

    /// Whether a live waiter that comes before this one (better priority, or the same and
    /// earlier) wants the drive behind `lock`.
    fn better_waiter(&self, lock: &Path) -> Result<bool> {
        let Some(dir) = lock.parent() else { return Ok(false) };
        let Ok(rd) = std::fs::read_dir(dir) else { return Ok(false) };
        let mine = self.my_wait(dir);
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("wait-") {
                continue;
            }
            let Ok(w) = serde_json::from_slice::<Waiter>(&std::fs::read(e.path()).unwrap_or_default()) else { continue };
            if w.token == self.token && w.pid == std::process::id() {
                continue;
            }
            if !alive(w.pid) {
                let _ = std::fs::remove_file(e.path());
                continue;
            }
            if !w.wants.iter().any(|p| p == lock) {
                continue;
            }
            let ahead = match &mine {
                Some(m) => (w.priority, w.since) < (m.priority, m.since),
                None => w.priority <= self.priority,
            };
            if ahead {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The lock directory for real hardware.
    pub fn real_dir(&self) -> &Path {
        &self.real_dir
    }

    /// Who holds drive `d`, found without touching its lock (from /proc/locks), for
    /// watching from outside: a probe with flock could make a process taking the drive at
    /// that moment wait.
    pub fn holder(&self, d: &DriveRow) -> Option<String> {
        use std::os::unix::fs::MetadataExt;
        let path = self.path(d);
        let md = std::fs::metadata(&path).ok()?;
        let dev = format!("{:02x}:{:02x}:{}", libc::major(md.dev()), libc::minor(md.dev()), md.ino());
        let locks = std::fs::read_to_string("/proc/locks").ok()?;
        // "1: FLOCK  ADVISORY  WRITE 1234 08:02:5678 0 EOF" (blocked requests have "->").
        let pid = locks.lines().find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.get(1) == Some(&"FLOCK") && f.get(5) == Some(&dev.as_str())).then(|| f[4].to_string())
        })?;
        let what = std::fs::read_to_string(&path).unwrap_or_default();
        Some(if what.trim().is_empty() { format!("pid {pid}") } else { what.trim().to_string() })
    }

    /// Processes waiting for drive `d`: (pid, what, waiting since in unix seconds).
    pub fn waiting_for(&self, d: &DriveRow) -> Vec<(u32, String, f64)> {
        let lock = self.path(d);
        let Some(dir) = lock.parent() else { return Vec::new() };
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut out: Vec<(Priority, u32, String, f64)> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("wait-"))
            .filter_map(|e| serde_json::from_slice::<Waiter>(&std::fs::read(e.path()).ok()?).ok())
            .filter(|w| alive(w.pid) && w.wants.contains(&lock))
            .map(|w| (w.priority, w.pid, w.what, w.since))
            .collect();
        out.sort_by(|a, b| (a.0, a.3).partial_cmp(&(b.0, b.3)).unwrap_or(std::cmp::Ordering::Equal));
        out.into_iter().map(|(_, pid, what, since)| (pid, what, since)).collect()
    }

    fn my_wait(&self, dir: &Path) -> Option<Waiter> {
        let p = dir.join(format!("wait-{}-{}.json", std::process::id(), self.token));
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    /// Give drive `d` back.
    pub fn release(&self, d: &DriveRow) {
        if let Some(f) = self.held.borrow_mut().remove(&self.path(d)) {
            let _ = f.set_len(0);
            let _ = flock(&f, libc::LOCK_UN);
        }
    }

    /// Hold the library's changer while moving cartridges (released when dropped).
    pub fn changer(&self, lib: &LibraryRow) -> Result<File> {
        let dir = lock_dir_for(&lib.backend, Some(&lib.location), &self.real_dir);
        let f = Self::open(&dir.join(format!("library-{}.lock", lib.serial)))?;
        flock(&f, libc::LOCK_EX)?;
        Ok(f)
    }
}

impl Drop for Leases {
    fn drop(&mut self) {
        for (_, f) in self.held.borrow_mut().drain() {
            let _ = f.set_len(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(dir: &Path, serial: &str) -> DriveRow {
        DriveRow {
            id: 1,
            name: format!("d-{serial}"),
            serial: serial.into(),
            backend: "sim".into(),
            location: Some(dir.display().to_string()),
            st_path: None,
            vendor: String::new(),
            product: String::new(),
            revision: String::new(),
            generation: None,
            library_id: None,
            element: None,
            state: "attached".into(),
            last_seen: 0,
        }
    }

    #[test]
    fn one_holder_at_a_time() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (drive(t.path(), "A"), drive(t.path(), "B"));
        let backup = Leases::new(t.path().into(), Priority::Backup, "backup DC0001L8");
        let other = Leases::new(t.path().into(), Priority::Backup, "verify");
        assert!(backup.try_take(&a).unwrap());
        assert!(backup.busy(&a).unwrap().is_none(), "not busy for its holder");
        assert!(other.busy(&a).unwrap().unwrap().contains("backup DC0001L8"));
        assert!(!other.try_take(&a).unwrap());
        // The other drive is free: taken at once.
        let mut told = String::new();
        assert_eq!(other.take_any(&[a.clone(), b.clone()], &mut |w| told = w.into()).unwrap().serial, "B");
        assert!(told.is_empty(), "no wait");
        backup.release(&a);
        assert!(other.try_take(&a).unwrap());
    }

    #[test]
    fn waiters_go_by_priority() {
        let t = tempfile::tempdir().unwrap();
        let a = drive(t.path(), "A");
        let holder = Leases::new(t.path().into(), Priority::Backup, "backup");
        assert!(holder.try_take(&a).unwrap());
        let order = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for (prio, name, delay) in [(Priority::Scrub, "scrub", 0), (Priority::Interactive, "restore", 200)] {
                let (a, order, dir) = (a.clone(), &order, t.path().to_path_buf());
                s.spawn(move || {
                    std::thread::sleep(Duration::from_millis(delay));
                    let mut l = Leases::new(dir, prio, name);
                    l.poll = Duration::from_millis(50);
                    l.take_any(std::slice::from_ref(&a), &mut |_| {}).unwrap();
                    order.lock().unwrap().push(name);
                    std::thread::sleep(Duration::from_millis(300));
                    l.release(&a);
                });
            }
            // Both are waiting when the drive comes free: the restore goes first although
            // the scrub has waited longer.
            std::thread::sleep(Duration::from_millis(600));
            holder.release(&a);
        });
        assert_eq!(*order.lock().unwrap(), ["restore", "scrub"]);
    }
}
