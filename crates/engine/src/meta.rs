//! File metadata: lstat, extended attributes (without following symlinks), sparse extents,
//! user/group names, and applying all of it on restore.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use ltoarc_format::record::{FileKind, FileMeta};
use serde_bytes::ByteBuf;

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// All extended attributes of `path` (symlinks are not followed). Attributes that cannot be
/// read (e.g. trusted.* without privilege) are skipped.
pub fn list_xattrs(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let c = cpath(path)?;
    let mut names = vec![0u8; 4096];
    let n = loop {
        // SAFETY: buffer pointer and length describe `names`.
        let n = unsafe { libc::llistxattr(c.as_ptr(), names.as_mut_ptr() as *mut libc::c_char, names.len()) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ERANGE) => names.resize(names.len() * 4, 0),
            Some(libc::ENOTSUP) => return Ok(Vec::new()),
            _ => return Err(e),
        }
    };
    let mut out = Vec::new();
    for name in names[..n].split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if let Ok(Some(v)) = get_xattr(path, name) {
            out.push((name.to_vec(), v));
        }
    }
    Ok(out)
}

pub fn get_xattr(path: &Path, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let c = cpath(path)?;
    let cn = CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL"))?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: buffer pointer and length describe `buf`.
        let n = unsafe { libc::lgetxattr(c.as_ptr(), cn.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(Some(buf));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ERANGE) => buf.resize(buf.len() * 8, 0),
            Some(libc::ENODATA) | Some(libc::ENOTSUP) => return Ok(None),
            _ => return Err(e),
        }
    }
}

pub fn set_xattr(path: &Path, name: &[u8], value: &[u8]) -> io::Result<()> {
    let c = cpath(path)?;
    let cn = CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "xattr name contains NUL"))?;
    // SAFETY: value pointer and length describe `value`.
    let rc = unsafe { libc::lsetxattr(c.as_ptr(), cn.as_ptr(), value.as_ptr() as *const libc::c_void, value.len(), 0) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Data extents of a file that has holes; None when the file is (or may be treated as) dense.
/// Holes smaller than this are read as data (zeros) rather than skipped.
pub const SMALL_HOLE: u64 = 1 << 20;

pub fn data_extents(f: &File, md: &Metadata) -> io::Result<Option<Vec<(u64, u64)>>> {
    let size = md.len();
    if size == 0 || md.blocks() * 512 >= size {
        return Ok(None);
    }
    let fd = f.as_raw_fd();
    let mut extents = Vec::new();
    let mut pos: i64 = 0;
    while (pos as u64) < size {
        // SAFETY: lseek on a valid descriptor.
        let data = unsafe { libc::lseek(fd, pos, libc::SEEK_DATA) };
        if data < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENXIO) => break, // no more data: trailing hole
                Some(libc::EINVAL) => return Ok(None), // SEEK_DATA unsupported
                _ => return Err(io::Error::last_os_error()),
            }
        }
        // SAFETY: as above.
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        let end = if hole < 0 { size as i64 } else { hole.min(size as i64) };
        if end > data {
            match extents.last_mut() {
                // Read through small holes: skipping each makes reads non-sequential, and
                // the kernel then stops reading ahead (a VM image of 16 KiB extents read at
                // 30 MB/s that way, from a disk that delivers 3 GB/s).
                Some((s, l)) if data as u64 - (*s + *l) < SMALL_HOLE => *l = end as u64 - *s,
                _ => extents.push((data as u64, (end - data) as u64)),
            }
        }
        pos = end;
    }
    if extents.len() == 1 && extents[0] == (0, size) {
        return Ok(None);
    }
    Ok(Some(extents))
}

/// Cache of uid/gid -> name lookups.
#[derive(Default)]
pub struct Names {
    users: HashMap<u32, Option<String>>,
    groups: HashMap<u32, Option<String>>,
}

impl Names {
    pub fn user(&mut self, uid: u32) -> Option<String> {
        self.users.entry(uid).or_insert_with(|| lookup_user(uid)).clone()
    }

    pub fn group(&mut self, gid: u32) -> Option<String> {
        self.groups.entry(gid).or_insert_with(|| lookup_group(gid)).clone()
    }
}

fn lookup_user(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 16384];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pw` and `buf`, which outlive the call.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: pw_name points into `buf` and is NUL-terminated.
    Some(unsafe { CStr::from_ptr(pw.pw_name) }.to_string_lossy().into_owned())
}

fn lookup_group(gid: u32) -> Option<String> {
    let mut buf = vec![0u8; 16384];
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: getgrgid_r writes into `gr` and `buf`, which outlive the call.
    let rc = unsafe { libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr() as *mut libc::c_char, buf.len(), &mut result) };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: gr_name points into `buf` and is NUL-terminated.
    Some(unsafe { CStr::from_ptr(gr.gr_name) }.to_string_lossy().into_owned())
}

pub fn kind_of(md: &Metadata) -> FileKind {
    let ft = md.file_type();
    if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_fifo() {
        FileKind::Fifo
    } else if ft.is_char_device() {
        FileKind::Char
    } else if ft.is_block_device() {
        FileKind::Block
    } else if ft.is_socket() {
        FileKind::Socket
    } else {
        FileKind::File
    }
}

fn ns(secs: i64, nsec: i64) -> i64 {
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// FILE record metadata for `path` (extents and fingerprints are filled in by the caller).
pub fn file_meta(path: &Path, md: &Metadata, id: u64, names: &mut Names) -> io::Result<FileMeta> {
    let kind = kind_of(md);
    let target = if kind == FileKind::Symlink {
        Some(ByteBuf::from(std::fs::read_link(path)?.as_os_str().as_bytes().to_vec()))
    } else {
        None
    };
    let xattrs = list_xattrs(path)
        .unwrap_or_default()
        .into_iter()
        .map(|(n, v)| (ByteBuf::from(n), ByteBuf::from(v)))
        .collect();
    Ok(FileMeta {
        id,
        path: ByteBuf::from(path.as_os_str().as_bytes().to_vec()),
        kind,
        mode: md.mode(),
        uid: md.uid(),
        gid: md.gid(),
        user: names.user(md.uid()),
        group: names.group(md.gid()),
        // Directories keep their st_size too, so a mounted catalog looks like the original.
        size: if matches!(kind, FileKind::File | FileKind::Dir) { md.len() } else { 0 },
        mtime_ns: ns(md.mtime(), md.mtime_nsec()),
        atime_ns: ns(md.atime(), md.atime_nsec()),
        ctime_ns: ns(md.ctime(), md.ctime_nsec()),
        dev: md.dev(),
        ino: md.ino(),
        nlink: md.nlink(),
        rdev: md.rdev(),
        target,
        xattrs,
        extents: None,
        fingerprints: Default::default(),
        same_as: None,
    })
}

/// FILE record for a path deleted since the session an incremental one builds on.
pub fn whiteout(path: &[u8], id: u64) -> FileMeta {
    FileMeta {
        id,
        path: ByteBuf::from(path.to_vec()),
        kind: FileKind::Whiteout,
        mode: 0,
        uid: 0,
        gid: 0,
        user: None,
        group: None,
        size: 0,
        mtime_ns: 0,
        atime_ns: 0,
        ctime_ns: 0,
        dev: 0,
        ino: 0,
        nlink: 0,
        rdev: 0,
        target: None,
        xattrs: Vec::new(),
        extents: None,
        fingerprints: Default::default(),
        same_as: None,
    }
}

pub fn mtime_ns(md: &Metadata) -> i64 {
    ns(md.mtime(), md.mtime_nsec())
}

pub fn ctime_ns(md: &Metadata) -> i64 {
    ns(md.ctime(), md.ctime_nsec())
}

/// Set atime/mtime without following symlinks.
pub fn set_times(path: &Path, atime_ns: i64, mtime_ns: i64) -> io::Result<()> {
    let c = cpath(path)?;
    let ts = |t: i64| libc::timespec { tv_sec: t.div_euclid(1_000_000_000), tv_nsec: t.rem_euclid(1_000_000_000) };
    let times = [ts(atime_ns), ts(mtime_ns)];
    // SAFETY: valid path and timespec array of length 2.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

pub fn mknod(path: &Path, mode: u32, rdev: u64) -> io::Result<()> {
    let c = cpath(path)?;
    // SAFETY: valid path.
    let rc = unsafe { libc::mknod(c.as_ptr(), mode as libc::mode_t, rdev as libc::dev_t) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub fn os(b: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn xattrs_and_sparse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let mut f = File::create(&p).unwrap();
        f.write_all(b"head").unwrap();
        f.seek(SeekFrom::Start(64 << 20)).unwrap();
        f.write_all(b"tail").unwrap();
        drop(f);
        if set_xattr(&p, b"user.ltoarc.test", b"value").is_ok() {
            let x = list_xattrs(&p).unwrap();
            assert!(x.contains(&(b"user.ltoarc.test".to_vec(), b"value".to_vec())));
            assert_eq!(get_xattr(&p, b"user.ltoarc.test").unwrap().unwrap(), b"value");
            assert_eq!(get_xattr(&p, b"user.nope").unwrap(), None);
        }
        let f = File::open(&p).unwrap();
        let md = f.metadata().unwrap();
        if let Some(ext) = data_extents(&f, &md).unwrap() {
            assert_eq!(ext.first().unwrap().0, 0);
            let last = ext.last().unwrap();
            assert_eq!(last.0 + last.1, md.len());
            assert!(ext.iter().map(|e| e.1).sum::<u64>() < md.len());
        }
        let mut names = Names::default();
        let m = file_meta(&p, &std::fs::symlink_metadata(&p).unwrap(), 7, &mut names).unwrap();
        assert_eq!((m.kind, m.size, m.id), (FileKind::File, (64 << 20) + 4, 7));
        // Small holes are read through; large ones skipped.
        let q = dir.path().join("g");
        let mut f = File::create(&q).unwrap();
        for at in [0u64, 64 << 10, 128 << 10, 16 << 20] {
            f.seek(SeekFrom::Start(at)).unwrap();
            f.write_all(&[1u8; 4096]).unwrap();
        }
        drop(f);
        let f = File::open(&q).unwrap();
        if let Some(ext) = data_extents(&f, &f.metadata().unwrap()).unwrap() {
            assert_eq!(ext, [(0, (128 << 10) + 4096), (16 << 20, 4096)]);
        }
    }
}
