//! FUSE view of the catalog.
//!
//! Offline mode needs no drive: names, sizes, owners, modes, times and xattrs come from the
//! catalog; file reads return zeros (or EIO), so `find`, `ls`, `du` and `getfattr` work on
//! the whole archive. Online mode serves file contents from tape through a [`Data`] source.
//! Inode numbers are catalog path ids (the root path is id 1, which is also FUSE's root
//! inode).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner, MountOption,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};
use ltoarc_catalog::{Catalog, ObjectRow, View};
use ltoarc_format::record::from_cbor;
use serde_bytes::ByteBuf;

const TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub view: View,
    /// Reads fail with EIO instead of returning zeros.
    pub eio: bool,
}

/// Directory listing: (inode, type, name).
type Listing = Vec<(u64, FileType, Vec<u8>)>;

/// Where an online mount gets file contents.
pub trait Data: Send + Sync {
    /// `size` bytes of object `object_id` (of `len` bytes) from `offset`, fewer only at the
    /// end of the file; an errno when they cannot be had.
    fn read(&self, object_id: i64, len: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32>;
}

pub struct ArchiveFs {
    cat: Mutex<Catalog>,
    opts: Options,
    data: Option<Arc<dyn Data>>,
    dirs: Mutex<HashMap<u64, Listing>>,
    uid: u32,
    gid: u32,
}

enum Node {
    Object(Box<ObjectRow>),
    /// Directory present only as an ancestor of archived paths.
    Implied,
}

fn time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

fn file_type(kind: &str) -> FileType {
    match kind {
        "dir" => FileType::Directory,
        "symlink" => FileType::Symlink,
        "fifo" => FileType::NamedPipe,
        "char" => FileType::CharDevice,
        "block" => FileType::BlockDevice,
        "socket" => FileType::Socket,
        _ => FileType::RegularFile,
    }
}

impl ArchiveFs {
    pub fn new(cat: Catalog, opts: Options) -> Self {
        // SAFETY: getuid/getgid cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        ArchiveFs { cat: Mutex::new(cat), opts, data: None, dirs: Mutex::new(HashMap::new()), uid, gid }
    }

    fn node(&self, cat: &Catalog, ino: u64) -> Option<Node> {
        let id = ino as i64;
        match cat.latest(id, self.opts.view).ok()? {
            Some(mut o) => {
                if o.kind == "hardlink" {
                    // Present a hard link with the attributes of the file it links to.
                    let target = o.target.clone().unwrap_or_default();
                    if let Some(t) = cat.lookup_path(&target).ok().flatten().and_then(|t| cat.latest(t, self.opts.view).ok().flatten()) {
                        o = ObjectRow { id: t.id, kind: "file".into(), size: t.size, extents: t.extents, ..o };
                    }
                }
                Some(Node::Object(Box::new(o)))
            }
            None if cat.has_visible_below(id, self.opts.view).ok()? => Some(Node::Implied),
            None => None,
        }
    }

    fn attr(&self, ino: u64, node: &Node) -> FileAttr {
        match node {
            Node::Object(o) => {
                let kind = file_type(&o.kind);
                let size = if kind == FileType::Symlink { o.target.as_ref().map_or(0, |t| t.len() as u64) } else { o.size as u64 };
                FileAttr {
                    ino: INodeNo(ino),
                    size,
                    blocks: size.div_ceil(512),
                    atime: time(o.atime_ns),
                    mtime: time(o.mtime_ns),
                    ctime: time(o.ctime_ns),
                    crtime: time(o.mtime_ns),
                    kind,
                    perm: (o.mode & 0o7777) as u16,
                    nlink: if kind == FileType::Directory { 2 } else { o.nlink.max(1) as u32 },
                    uid: o.uid as u32,
                    gid: o.gid as u32,
                    rdev: o.rdev as u32,
                    blksize: 1 << 20,
                    flags: 0,
                }
            }
            Node::Implied => FileAttr {
                ino: INodeNo(ino),
                size: 0,
                blocks: 0,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: FileType::Directory,
                perm: 0o555,
                nlink: 2,
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: 1 << 20,
                flags: 0,
            },
        }
    }

    fn listing(&self, cat: &Catalog, ino: u64) -> Listing {
        cat.entries(ino as i64, self.opts.view)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, name, o)| {
                let kind = match o {
                    Some(o) if o.kind == "hardlink" => FileType::RegularFile,
                    Some(o) => file_type(&o.kind),
                    None => FileType::Directory,
                };
                (id as u64, kind, name)
            })
            .collect()
    }

    /// Archived xattrs plus virtual user.ltoarc.* attributes.
    fn xattrs(&self, cat: &Catalog, o: &ObjectRow) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = o
            .xattrs
            .as_ref()
            .and_then(|x| from_cbor::<Vec<(ByteBuf, ByteBuf)>>(x).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|(n, v)| (n.into_vec(), v.into_vec()))
            .collect();
        let mut add = |name: String, value: String| {
            if !out.iter().any(|(n, _)| n == name.as_bytes()) {
                out.push((name.into_bytes(), value.into_bytes()));
            }
        };
        for h in cat.hashes(o.id).unwrap_or_default() {
            let key = if h.kind == "content" { h.algo.clone() } else { h.profile.clone() };
            add(format!("user.ltoarc.{key}"), h.digest);
        }
        let locs = cat.object_locations(o.id).unwrap_or_default();
        if !locs.is_empty() {
            let copies: Vec<String> = locs.iter().map(|(c, b, _, _)| format!("{c}:{b}")).collect();
            add("user.ltoarc.copies".into(), copies.join(" "));
            let at: Vec<String> = locs.iter().map(|(c, b, p, blk)| format!("copy {c} {b} partition {p} block {blk}")).collect();
            add("user.ltoarc.location".into(), at.join("; "));
        }
        if let Some(s) = cat.sessions().unwrap_or_default().into_iter().find(|s| s.id == o.session_id) {
            add("user.ltoarc.session".into(), format!("{} {}", s.uuid, s.started));
        }
        if o.status != "ok" {
            add("user.ltoarc.status".into(), o.status.clone());
        }
        out
    }
}

fn reply_xattr(reply: ReplyXattr, size: u32, data: &[u8]) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if data.len() <= size as usize {
        reply.data(data);
    } else {
        reply.error(Errno::ERANGE);
    }
}

impl Filesystem for ArchiveFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        // Reads of up to 1 MiB per request (the request size follows max_write), and as much
        // kernel readahead as it allows: an online mount streams from tape.
        let _ = config.set_max_write(1 << 20);
        if let Err(max) = config.set_max_readahead(1 << 20) {
            let _ = config.set_max_readahead(max);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let cat = self.cat.lock().unwrap();
        let Some(id) = cat.find_child(parent.0 as i64, name.as_bytes()).ok().flatten() else {
            return reply.error(Errno::ENOENT);
        };
        match self.node(&cat, id as u64) {
            Some(n) => reply.entry(&TTL, &self.attr(id as u64, &n), Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let cat = self.cat.lock().unwrap();
        match self.node(&cat, ino.0) {
            Some(n) => reply.attr(&TTL, &self.attr(ino.0, &n)),
            None if ino.0 == 1 => reply.attr(&TTL, &self.attr(1, &Node::Implied)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let cat = self.cat.lock().unwrap();
        match self.node(&cat, ino.0) {
            Some(Node::Object(o)) if o.kind == "symlink" => reply.data(o.target.as_deref().unwrap_or_default()),
            _ => reply.error(Errno::EINVAL),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        reply.opened(FileHandle(ino.0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let (id, len) = {
            let cat = self.cat.lock().unwrap();
            match self.node(&cat, ino.0) {
                Some(Node::Object(o)) => (o.id, o.size as u64),
                _ => (0, 0),
            }
        };
        match &self.data {
            // The catalog stays free for other requests while the data comes from tape.
            Some(d) if len > 0 => match d.read(id, len, offset, size) {
                Ok(bytes) => reply.data(&bytes),
                Err(e) => reply.error(Errno::from_i32(e)),
            },
            _ if self.opts.eio => reply.error(Errno::EIO),
            _ => {
                let n = len.saturating_sub(offset).min(size as u64) as usize;
                reply.data(&vec![0u8; n]);
            }
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(ino.0), FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut dirs = self.dirs.lock().unwrap();
        if offset == 0 || !dirs.contains_key(&ino.0) {
            let cat = self.cat.lock().unwrap();
            let parent = cat.path_name(ino.0 as i64).ok().flatten().and_then(|(p, _)| p).unwrap_or(1) as u64;
            let mut entries = vec![(ino.0, FileType::Directory, b".".to_vec()), (parent, FileType::Directory, b"..".to_vec())];
            entries.extend(self.listing(&cat, ino.0));
            dirs.insert(ino.0, entries);
        }
        for (i, (id, kind, name)) in dirs[&ino.0].iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(*id), (i + 1) as u64, *kind, OsStr::from_bytes(name)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 1 << 20, 4096, 1 << 20);
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let cat = self.cat.lock().unwrap();
        let Some(Node::Object(o)) = self.node(&cat, ino.0) else { return reply.error(Errno::ENODATA) };
        match self.xattrs(&cat, &o).into_iter().find(|(n, _)| n == name.as_bytes()) {
            Some((_, v)) => reply_xattr(reply, size, &v),
            None => reply.error(Errno::ENODATA),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let cat = self.cat.lock().unwrap();
        let mut names = Vec::new();
        if let Some(Node::Object(o)) = self.node(&cat, ino.0) {
            for (n, _) in self.xattrs(&cat, &o) {
                names.extend_from_slice(&n);
                names.push(0);
            }
        }
        reply_xattr(reply, size, &names);
    }
}

/// Mount read-only at `dir` and serve until unmounted (`fusermount3 -u DIR`).
pub fn mount(cat: Catalog, dir: &Path, opts: Options) -> std::io::Result<()> {
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::RO, MountOption::FSName("ltoarc".into()), MountOption::Subtype("ltoarc".into())];
    cfg.n_threads = Some(4);
    fuser::mount(ArchiveFs::new(cat, opts), dir, &cfg)
}

/// Mount read-only at `dir` in a background thread, serving file contents from `data`;
/// unmounted when the returned session is dropped (or with `fusermount3 -u DIR`).
pub fn spawn_online(cat: Catalog, dir: &Path, opts: Options, data: Arc<dyn Data>) -> std::io::Result<fuser::BackgroundSession> {
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::RO, MountOption::FSName("ltoarc".into()), MountOption::Subtype("ltoarc".into())];
    cfg.n_threads = Some(8);
    let mut fs = ArchiveFs::new(cat, opts);
    fs.data = Some(data);
    fuser::spawn_mount(fs, dir, &cfg)
}

/// Mount in a background thread; unmounted when the returned session is dropped.
pub fn spawn(cat: Catalog, dir: &Path, opts: Options) -> std::io::Result<fuser::BackgroundSession> {
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::RO, MountOption::FSName("ltoarc".into()), MountOption::Subtype("ltoarc".into())];
    fuser::spawn_mount(ArchiveFs::new(cat, opts), dir, &cfg)
}
