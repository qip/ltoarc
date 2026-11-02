//! Archive content: sessions, paths, objects (file versions), hashes, chunks, placements.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, OptionalExtension, Row};

use crate::{Catalog, Error, Result};

pub const ROOT_PATH: i64 = 1;



#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRow {
    pub id: i64,
    pub uuid: String,
    pub job: Option<String>,
    pub level: String,
    pub parent_id: Option<i64>,
    pub started: i64,
    pub ended: Option<i64>,
    /// running | complete | partial | failed
    pub status: String,
    pub host: String,
    /// JSON array of source paths.
    pub sources: String,
    pub files: i64,
    pub bytes: i64,
    pub stored_bytes: i64,
    pub stats: Option<String>,
    /// Wrapped data key (JSON) when the session's records are encrypted.
    pub encryption: Option<String>,
    /// When retention expired the session: it no longer shows in views, and its cartridges
    /// may be reused once nothing else needs them.
    pub expired_at: Option<i64>,
}

const SESSION_COLS: &str =
    "id, uuid, job, level, parent_id, started, ended, status, host, sources, files, bytes, stored_bytes, stats, encryption, expired_at";

fn session_row(r: &Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        uuid: r.get(1)?,
        job: r.get(2)?,
        level: r.get(3)?,
        parent_id: r.get(4)?,
        started: r.get(5)?,
        ended: r.get(6)?,
        status: r.get(7)?,
        host: r.get(8)?,
        sources: r.get(9)?,
        files: r.get(10)?,
        bytes: r.get(11)?,
        stored_bytes: r.get(12)?,
        stats: r.get(13)?,
        encryption: r.get(14)?,
        expired_at: r.get(15)?,
    })
}

/// One run of a scheduled job.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobRun {
    pub id: i64,
    pub job: String,
    /// Run id (a timestamp like 20261031T020000), as hooks see it in {run}.
    pub run: String,
    pub started: i64,
    pub ended: Option<i64>,
    /// running | ok | failed
    pub status: String,
    pub summary: Option<String>,
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionVolume {
    pub session_id: i64,
    pub copy: i64,
    pub seq: i64,
    pub volume_id: i64,
    pub partition: i64,
    pub start_object: i64,
    pub end_object: Option<i64>,
    pub catalog_object: Option<i64>,
    /// When `ltoarc verify` last read this part back without errors.
    pub verified_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectRow {
    pub id: i64,
    pub session_id: i64,
    pub path_id: i64,
    /// Object number within the session (the FILE record id).
    pub seq: i64,
    pub kind: String,
    pub mode: i64,
    pub uid: i64,
    pub gid: i64,
    pub user: Option<String>,
    pub grp: Option<String>,
    pub size: i64,
    pub mtime_ns: i64,
    pub atime_ns: i64,
    pub ctime_ns: i64,
    pub dev: i64,
    pub ino: i64,
    pub nlink: i64,
    pub rdev: i64,
    pub target: Option<Vec<u8>>,
    /// CBOR list of (name, value) pairs.
    pub xattrs: Option<Vec<u8>>,
    /// CBOR list of (offset, length) data extents for sparse files.
    pub extents: Option<Vec<u8>>,
    /// ok | changed | unreadable | truncated
    pub status: String,
    /// The data is that of this earlier object (a moved or duplicate file): its chunks hold
    /// it. Always the object that has the chunks, never another reference.
    pub data_from: Option<i64>,
}

impl ObjectRow {
    /// The object whose chunks hold this one's data.
    pub fn data_object(&self) -> i64 {
        self.data_from.unwrap_or(self.id)
    }
}

const OBJECT_COLS: &str = "o.id, o.session_id, o.path_id, o.seq, o.kind, o.mode, o.uid, o.gid, o.user, o.grp, o.size, \
     o.mtime_ns, o.atime_ns, o.ctime_ns, o.dev, o.ino, o.nlink, o.rdev, o.target, o.xattrs, o.extents, o.status, o.data_from";

fn object_row(r: &Row) -> rusqlite::Result<ObjectRow> {
    Ok(ObjectRow {
        id: r.get(0)?,
        session_id: r.get(1)?,
        path_id: r.get(2)?,
        seq: r.get(3)?,
        kind: r.get(4)?,
        mode: r.get(5)?,
        uid: r.get(6)?,
        gid: r.get(7)?,
        user: r.get(8)?,
        grp: r.get(9)?,
        size: r.get(10)?,
        mtime_ns: r.get(11)?,
        atime_ns: r.get(12)?,
        ctime_ns: r.get(13)?,
        dev: r.get(14)?,
        ino: r.get(15)?,
        nlink: r.get(16)?,
        rdev: r.get(17)?,
        target: r.get(18)?,
        xattrs: r.get(19)?,
        extents: r.get(20)?,
        status: r.get(21)?,
        data_from: r.get(22)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChunkRow {
    pub id: i64,
    pub object_id: i64,
    pub seq: i64,
    pub offset: i64,
    pub raw_len: i64,
    pub stored_len: i64,
    pub codec: i64,
    pub blake3: Vec<u8>,
    /// For files stored inside a PACK record: offset of this file's bytes in the raw pack.
    /// `codec` and `stored_len` then describe the whole pack.
    pub pack_offset: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    pub chunk_id: i64,
    pub copy: i64,
    pub volume_id: i64,
    pub partition: i64,
    pub block: i64,
    pub offset: i64,
    /// Stored size of this copy's record when it differs from the chunk's (a copy made
    /// again from the source may compress differently).
    pub stored_len: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashRow {
    pub profile: String,
    pub kind: String,
    pub algo: String,
    pub digest: String,
}

/// Chunk with its placements, for restore planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlacement {
    pub chunk: ChunkRow,
    pub placement: Placement,
    pub barcode: String,
}

/// Which sessions are visible: complete or partial ones, optionally started no later than
/// `as_of`, optionally a single session.
#[derive(Debug, Clone, Copy, Default)]
pub struct View {
    pub as_of: Option<i64>,
    pub session: Option<i64>,
}

/// Objects a view shows. A file cut short (`truncated`: its session ran out of tape or was
/// interrupted) shows only when its session is asked for; otherwise the newest complete
/// version stands.
const VISIBLE: &str = "s.status IN ('complete', 'partial') AND s.expired_at IS NULL AND (?9 IS NULL OR s.started <= ?9) AND (?10 IS NULL OR s.id = ?10)
     AND (o.status != 'truncated' OR ?10 IS NOT NULL)";

impl Catalog {
    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN")?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        !self.conn.is_autocommit()
    }

    // --- sessions --------------------------------------------------------------------

    pub fn create_session(&self, s: &SessionRow) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (uuid, job, level, parent_id, started, ended, status, host, sources, files, bytes,
                                   stored_bytes, stats, encryption)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                s.uuid,
                s.job,
                s.level,
                s.parent_id,
                s.started,
                s.ended,
                s.status,
                s.host,
                s.sources,
                s.files,
                s.bytes,
                s.stored_bytes,
                s.stats,
                s.encryption
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Remove a session and everything recorded for it (paths are kept).
    pub fn delete_session(&self, id: i64) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT del")?;
        let r = (|| -> Result<()> {
            for sql in [
                // Sessions built on it lose their parent; files that took their data from
                // its files have lost it (retention never gets here while they are live).
                "UPDATE sessions SET parent_id = NULL WHERE parent_id = ?1",
                "UPDATE objects SET data_from = NULL, status = 'lost'
                   WHERE data_from IN (SELECT id FROM objects WHERE session_id = ?1) AND session_id != ?1",
                "DELETE FROM placements WHERE chunk_id IN
                   (SELECT c.id FROM chunks c JOIN objects o ON o.id = c.object_id WHERE o.session_id = ?1)",
                "DELETE FROM chunks WHERE object_id IN (SELECT id FROM objects WHERE session_id = ?1)",
                "DELETE FROM object_hashes WHERE object_id IN (SELECT id FROM objects WHERE session_id = ?1)",
                "DELETE FROM objects WHERE session_id = ?1",
                "DELETE FROM session_volumes WHERE session_id = ?1",
                "DELETE FROM sessions WHERE id = ?1",
            ] {
                self.conn.execute(sql, [id])?;
            }
            Ok(())
        })();
        self.conn.execute_batch(if r.is_ok() { "RELEASE del" } else { "ROLLBACK TO del; RELEASE del" })?;
        r
    }

    /// The cartridge was erased: drop what the catalog places on it. Sessions left with no
    /// cartridge at all are deleted; others keep their parts on other cartridges and copies.
    /// Returns the number of sessions deleted.
    pub fn forget_volume(&self, volume_id: i64) -> Result<usize> {
        let ids: Vec<i64> = self
            .conn
            .prepare("SELECT DISTINCT session_id FROM session_volumes WHERE volume_id = ?1")?
            .query_map([volume_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.conn.execute("DELETE FROM placements WHERE volume_id = ?1", [volume_id])?;
        self.conn.execute("DELETE FROM session_volumes WHERE volume_id = ?1", [volume_id])?;
        let mut deleted = 0;
        for id in ids {
            let left: i64 = self.conn.query_row("SELECT COUNT(*) FROM session_volumes WHERE session_id = ?1", [id], |r| r.get(0))?;
            if left == 0 {
                self.delete_session(id)?;
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    pub fn update_session(&self, s: &SessionRow) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended = ?2, status = ?3, files = ?4, bytes = ?5, stored_bytes = ?6, stats = ?7
             WHERE id = ?1",
            params![s.id, s.ended, s.status, s.files, s.bytes, s.stored_bytes, s.stats],
        )?;
        Ok(())
    }

    pub fn session(&self, key: &str) -> Result<Option<SessionRow>> {
        let by_id: Option<i64> = key.parse().ok();
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE uuid = ?1 OR uuid LIKE ?1 || '%' OR id = ?2"),
                params![key, by_id],
                session_row,
            )
            .optional()?)
    }

    /// Encryption of the sessions on a volume: None if it holds none yet, else the wrapped
    /// key JSON of each (None for plaintext sessions).
    pub fn volume_encryption(&self, volume_id: i64) -> Result<Option<Vec<Option<String>>>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT s.id, s.encryption FROM sessions s JOIN session_volumes sv ON sv.session_id = s.id
             WHERE sv.volume_id = ?1",
        )?;
        let v: Vec<Option<String>> = st.query_map([volume_id], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?;
        Ok((!v.is_empty()).then_some(v))
    }

    pub fn sessions(&self) -> Result<Vec<SessionRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {SESSION_COLS} FROM sessions ORDER BY started, id"))?;
        Ok(st.query_map([], session_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_session_volume(&self, v: &SessionVolume) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO session_volumes
               (session_id, copy, seq, volume_id, partition, start_object, end_object, catalog_object, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                v.session_id,
                v.copy,
                v.seq,
                v.volume_id,
                v.partition,
                v.start_object,
                v.end_object,
                v.catalog_object,
                v.verified_at
            ],
        )?;
        Ok(())
    }

    pub fn session_volumes(&self, session_id: i64) -> Result<Vec<SessionVolume>> {
        self.sv_query("WHERE session_id = ?1 ORDER BY copy, seq", session_id)
    }

    /// Session pieces stored on a volume, in tape order.
    pub fn volume_sessions(&self, volume_id: i64) -> Result<Vec<SessionVolume>> {
        self.sv_query("WHERE volume_id = ?1 ORDER BY partition, start_object", volume_id)
    }

    fn sv_query(&self, tail: &str, id: i64) -> Result<Vec<SessionVolume>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT session_id, copy, seq, volume_id, partition, start_object, end_object, catalog_object, verified_at
             FROM session_volumes {tail}"
        ))?;
        Ok(st
            .query_map([id], |r| {
                Ok(SessionVolume {
                    session_id: r.get(0)?,
                    copy: r.get(1)?,
                    seq: r.get(2)?,
                    volume_id: r.get(3)?,
                    partition: r.get(4)?,
                    start_object: r.get(5)?,
                    end_object: r.get(6)?,
                    catalog_object: r.get(7)?,
                    verified_at: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    // --- paths -----------------------------------------------------------------------

    /// Child `name` of `parent`, created if missing.
    pub fn path_child(&self, parent: i64, name: &[u8]) -> Result<i64> {
        let mut st = self.conn.prepare_cached("SELECT id FROM paths WHERE parent = ?1 AND name = ?2")?;
        if let Some(id) = st.query_row(params![parent, name], |r| r.get(0)).optional()? {
            return Ok(id);
        }
        self.conn
            .prepare_cached("INSERT INTO paths (parent, name) VALUES (?1, ?2)")?
            .execute(params![parent, name])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Child `name` of `parent`, if recorded.
    pub fn find_child(&self, parent: i64, name: &[u8]) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM paths WHERE parent = ?1 AND name = ?2")?
            .query_row(params![parent, name], |r| r.get(0))
            .optional()?)
    }

    /// Id of an absolute path, created if missing.
    pub fn intern_path(&self, path: &[u8]) -> Result<i64> {
        let mut id = ROOT_PATH;
        for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
            id = self.path_child(id, comp)?;
        }
        Ok(id)
    }

    /// Id of an absolute path if it is known.
    pub fn lookup_path(&self, path: &[u8]) -> Result<Option<i64>> {
        let mut st = self.conn.prepare_cached("SELECT id FROM paths WHERE parent = ?1 AND name = ?2")?;
        let mut id = ROOT_PATH;
        for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
            match st.query_row(params![id, comp], |r| r.get(0)).optional()? {
                Some(child) => id = child,
                None => return Ok(None),
            }
        }
        Ok(Some(id))
    }

    /// Full absolute paths of the given path ids.
    pub fn full_paths(&self, ids: impl IntoIterator<Item = i64>) -> Result<HashMap<i64, Vec<u8>>> {
        let mut parent: HashMap<i64, (Option<i64>, Vec<u8>)> = HashMap::new();
        let mut st = self.conn.prepare_cached("SELECT parent, name FROM paths WHERE id = ?1")?;
        let mut out = HashMap::new();
        for id in ids {
            let mut chain = Vec::new();
            let mut cur = Some(id);
            while let Some(c) = cur {
                if c == ROOT_PATH {
                    break;
                }
                if let std::collections::hash_map::Entry::Vacant(e) = parent.entry(c) {
                    e.insert(st.query_row([c], |r| Ok((r.get(0)?, r.get(1)?)))?);
                }
                let (p, name) = &parent[&c];
                chain.push(name.clone());
                cur = *p;
            }
            let mut path = Vec::new();
            for name in chain.iter().rev() {
                path.push(b'/');
                path.extend_from_slice(name);
            }
            if path.is_empty() {
                path.push(b'/');
            }
            out.insert(id, path);
        }
        Ok(out)
    }

    pub fn path_name(&self, id: i64) -> Result<Option<(Option<i64>, Vec<u8>)>> {
        Ok(self
            .conn
            .prepare_cached("SELECT parent, name FROM paths WHERE id = ?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    // --- objects, hashes, chunks -----------------------------------------------------

    pub fn insert_object(&self, o: &ObjectRow) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO objects (session_id, path_id, seq, kind, mode, uid, gid, user, grp, size, mtime_ns,
                    atime_ns, ctime_ns, dev, ino, nlink, rdev, target, xattrs, extents, status, data_from)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
            )?
            .execute(params![
                o.session_id,
                o.path_id,
                o.seq,
                o.kind,
                o.mode,
                o.uid,
                o.gid,
                o.user,
                o.grp,
                o.size,
                o.mtime_ns,
                o.atime_ns,
                o.ctime_ns,
                o.dev,
                o.ino,
                o.nlink,
                o.rdev,
                o.target,
                o.xattrs,
                o.extents,
                o.status,
                o.data_from
            ])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_object_status(&self, id: i64, status: &str, size: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE objects SET status = ?2, size = ?3 WHERE id = ?1")?
            .execute(params![id, status, size])?;
        Ok(())
    }

    pub fn set_object_xattrs(&self, id: i64, xattrs: Option<&[u8]>) -> Result<()> {
        self.conn.prepare_cached("UPDATE objects SET xattrs = ?2 WHERE id = ?1")?.execute(params![id, xattrs])?;
        Ok(())
    }

    pub fn set_object_ctime(&self, id: i64, ctime_ns: i64) -> Result<()> {
        self.conn.prepare_cached("UPDATE objects SET ctime_ns = ?2 WHERE id = ?1")?.execute(params![id, ctime_ns])?;
        Ok(())
    }

    pub fn object(&self, id: i64) -> Result<Option<ObjectRow>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {OBJECT_COLS} FROM objects o WHERE o.id = ?1"), [id], object_row)
            .optional()?)
    }

    pub fn object_by_seq(&self, session_id: i64, seq: i64) -> Result<Option<ObjectRow>> {
        Ok(self
            .conn
            .prepare_cached(&format!("SELECT {OBJECT_COLS} FROM objects o WHERE o.session_id = ?1 AND o.seq = ?2"))?
            .query_row(params![session_id, seq], object_row)
            .optional()?)
    }

    pub fn session_objects(&self, session_id: i64) -> Result<Vec<ObjectRow>> {
        let mut st =
            self.conn.prepare(&format!("SELECT {OBJECT_COLS} FROM objects o WHERE o.session_id = ?1 ORDER BY o.seq"))?;
        Ok(st.query_map([session_id], object_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Entries of a session (deletions aside) of which no later complete, unexpired session
    /// holds an intact version: what only this session has. 0 for a partial session that a
    /// later run covered entirely.
    pub fn uncovered_entries(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM objects o JOIN sessions s ON s.id = o.session_id
             WHERE o.session_id = ?1 AND o.kind != 'whiteout' AND NOT EXISTS (
               SELECT 1 FROM objects o2 JOIN sessions s2 ON s2.id = o2.session_id
               WHERE o2.path_id = o.path_id AND o2.kind != 'whiteout' AND o2.status = 'ok'
                 AND s2.status = 'complete' AND s2.expired_at IS NULL
                 AND (s2.started > s.started OR (s2.started = s.started AND s2.id > s.id)))",
            [session_id],
            |r| r.get(0),
        )?)
    }

    /// Every recorded version of a path (deletions included), newest session first.
    pub fn path_versions(&self, path_id: i64) -> Result<Vec<ObjectRow>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {OBJECT_COLS} FROM objects o JOIN sessions s ON s.id = o.session_id WHERE o.path_id = ?1 ORDER BY s.started DESC, o.id DESC"
        ))?;
        Ok(st.query_map([path_id], object_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_hash(&self, object_id: i64, h: &HashRow) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO object_hashes (object_id, profile, kind, algo, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![object_id, h.profile, h.kind, h.algo, h.digest])?;
        Ok(())
    }

    pub fn hashes(&self, object_id: i64) -> Result<Vec<HashRow>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT profile, kind, algo, digest FROM object_hashes WHERE object_id = ?1 ORDER BY profile")?;
        Ok(st
            .query_map([object_id], |r| Ok(HashRow { profile: r.get(0)?, kind: r.get(1)?, algo: r.get(2)?, digest: r.get(3)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_chunk(&self, c: &ChunkRow) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO chunks (object_id, seq, offset, raw_len, stored_len, codec, blake3, pack_offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![c.object_id, c.seq, c.offset, c.raw_len, c.stored_len, c.codec, c.blake3, c.pack_offset])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_placement(&self, p: &Placement) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO placements (chunk_id, copy, volume_id, partition, block, offset, stored_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![p.chunk_id, p.copy, p.volume_id, p.partition, p.block, p.offset, p.stored_len])?;
        Ok(())
    }

    /// Every chunk in the record of copy `copy` at (volume, block, offset), with the FILE
    /// record id and session of its object: the members of a pack.
    pub fn record_chunks(&self, volume_id: i64, copy: i64, block: i64, offset: i64) -> Result<Vec<(ChunkRow, i64, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.id, c.object_id, c.seq, c.offset, c.raw_len, c.stored_len, c.codec, c.blake3, c.pack_offset, o.seq, o.session_id
             FROM placements p JOIN chunks c ON c.id = p.chunk_id JOIN objects o ON o.id = c.object_id
             WHERE p.volume_id = ?1 AND p.copy = ?2 AND p.block = ?3 AND p.offset = ?4",
        )?;
        Ok(st
            .query_map(params![volume_id, copy, block, offset], |r| {
                Ok((
                    ChunkRow {
                        id: r.get(0)?,
                        object_id: r.get(1)?,
                        seq: r.get(2)?,
                        offset: r.get(3)?,
                        raw_len: r.get(4)?,
                        stored_len: r.get(5)?,
                        codec: r.get(6)?,
                        blake3: r.get(7)?,
                        pack_offset: r.get(8)?,
                    },
                    r.get(9)?,
                    r.get(10)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Forget where copy `copy` holds an object's data (a copy that could not take all of it).
    pub fn drop_placements(&self, object_id: i64, copy: i64) -> Result<()> {
        self.conn.execute("DELETE FROM placements WHERE copy = ?2 AND chunk_id IN (SELECT id FROM chunks WHERE object_id = ?1)", params![object_id, copy])?;
        Ok(())
    }

    /// The data records of copy `copy` of a session, in the order they lie on tape: every
    /// chunk with its placement (a pack's members share one, ordered by pack offset).
    pub fn session_records(&self, session_id: i64, copy: i64) -> Result<Vec<(ChunkRow, Placement)>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, c.object_id, c.seq, c.offset, c.raw_len, c.stored_len, c.codec, c.blake3, c.pack_offset,
                    p.volume_id, p.partition, p.block, p.offset, p.stored_len
             FROM chunks c JOIN objects o ON o.id = c.object_id JOIN placements p ON p.chunk_id = c.id AND p.copy = ?2
               JOIN session_volumes sv ON sv.session_id = o.session_id AND sv.volume_id = p.volume_id AND sv.copy = p.copy
             WHERE o.session_id = ?1
             GROUP BY c.id
             ORDER BY MIN(sv.seq), p.block, p.offset, c.pack_offset",
        )?;
        Ok(st
            .query_map(params![session_id, copy], |r| {
                Ok((
                    ChunkRow {
                        id: r.get(0)?,
                        object_id: r.get(1)?,
                        seq: r.get(2)?,
                        offset: r.get(3)?,
                        raw_len: r.get(4)?,
                        stored_len: r.get(5)?,
                        codec: r.get(6)?,
                        blake3: r.get(7)?,
                        pack_offset: r.get(8)?,
                    },
                    Placement {
                        chunk_id: r.get(0)?,
                        copy,
                        volume_id: r.get(9)?,
                        partition: r.get(10)?,
                        block: r.get(11)?,
                        offset: r.get(12)?,
                        stored_len: r.get(13)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// How many chunks share a record position (the members of a pack; 1 for a chunk).
    pub fn record_members(&self, volume_id: i64, copy: i64, block: i64, offset: i64) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT COUNT(*) FROM placements WHERE volume_id = ?1 AND copy = ?2 AND block = ?3 AND offset = ?4")?
            .query_row(params![volume_id, copy, block, offset], |r| r.get(0))?)
    }

    /// Chunks whose copy `copy` sits in the record at (volume, block, offset): one for a
    /// CHUNK record, every member for a PACK.
    pub fn chunks_at(&self, volume_id: i64, copy: i64, block: i64, offset: i64) -> Result<Vec<i64>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT chunk_id FROM placements WHERE volume_id = ?1 AND copy = ?2 AND block = ?3 AND offset = ?4")?;
        Ok(st.query_map(params![volume_id, copy, block, offset], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Chunks of an object with every placement (one row per chunk and copy).
    pub fn chunk_placements(&self, object_id: i64) -> Result<Vec<ChunkPlacement>> {
        let mut st = self.conn.prepare_cached(
            "SELECT c.id, c.object_id, c.seq, c.offset, c.raw_len, c.stored_len, c.codec, c.blake3,
                    p.copy, p.volume_id, p.partition, p.block, p.offset, v.barcode, c.pack_offset, p.stored_len
             FROM chunks c JOIN placements p ON p.chunk_id = c.id JOIN volumes v ON v.id = p.volume_id
             WHERE c.object_id = COALESCE((SELECT data_from FROM objects WHERE id = ?1), ?1) ORDER BY c.seq, p.copy",
        )?;
        Ok(st
            .query_map([object_id], |r| {
                let chunk = ChunkRow {
                    id: r.get(0)?,
                    object_id: r.get(1)?,
                    seq: r.get(2)?,
                    offset: r.get(3)?,
                    raw_len: r.get(4)?,
                    stored_len: r.get(5)?,
                    codec: r.get(6)?,
                    blake3: r.get(7)?,
                    pack_offset: r.get(14)?,
                };
                Ok(ChunkPlacement {
                    placement: Placement {
                        chunk_id: chunk.id,
                        copy: r.get(8)?,
                        volume_id: r.get(9)?,
                        partition: r.get(10)?,
                        block: r.get(11)?,
                        offset: r.get(12)?,
                        stored_len: r.get(15)?,
                    },
                    chunk,
                    barcode: r.get(13)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Every chunk placed on a volume, in tape order: (placement, chunk, session id).
    pub fn volume_placements(&self, volume_id: i64) -> Result<Vec<(Placement, ChunkRow, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT p.chunk_id, p.copy, p.volume_id, p.partition, p.block, p.offset,
                    c.object_id, c.seq, c.offset, c.raw_len, c.stored_len, c.codec, c.blake3, o.session_id, c.pack_offset
             FROM placements p JOIN chunks c ON c.id = p.chunk_id JOIN objects o ON o.id = c.object_id
             WHERE p.volume_id = ?1 ORDER BY p.partition, p.block, p.offset",
        )?;
        Ok(st
            .query_map([volume_id], |r| {
                Ok((
                    Placement {
                        chunk_id: r.get(0)?,
                        copy: r.get(1)?,
                        volume_id: r.get(2)?,
                        partition: r.get(3)?,
                        block: r.get(4)?,
                        offset: r.get(5)?,
                        stored_len: None,
                    },
                    ChunkRow {
                        id: r.get(0)?,
                        object_id: r.get(6)?,
                        seq: r.get(7)?,
                        offset: r.get(8)?,
                        raw_len: r.get(9)?,
                        stored_len: r.get(10)?,
                        codec: r.get(11)?,
                        blake3: r.get(12)?,
                        pack_offset: r.get(14)?,
                    },
                    r.get(13)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    // --- views -----------------------------------------------------------------------

    /// Latest visible version of every path under `prefix` (inclusive), with full paths.
    pub fn resolve(&self, prefix: i64, view: View) -> Result<Vec<(Vec<u8>, ObjectRow)>> {
        let sql = format!(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT p.id FROM paths p JOIN sub ON p.parent = sub.id)
             SELECT * FROM (
               SELECT {OBJECT_COLS}, ROW_NUMBER() OVER (PARTITION BY o.path_id ORDER BY s.started DESC, o.id DESC) AS rn
               FROM objects o JOIN sessions s ON s.id = o.session_id
               WHERE o.path_id IN (SELECT id FROM sub) AND {VISIBLE}
             ) WHERE rn = 1 AND kind != 'whiteout'"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows: Vec<ObjectRow> = st
            .query_map(
                params![prefix, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, view.as_of, view.session],
                object_row,
            )?
            .collect::<rusqlite::Result<_>>()?;
        let paths = self.full_paths(rows.iter().map(|o| o.path_id))?;
        let mut out: Vec<(Vec<u8>, ObjectRow)> = rows.into_iter().map(|o| (paths[&o.path_id].clone(), o)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Latest visible version of one path.
    pub fn latest(&self, path_id: i64, view: View) -> Result<Option<ObjectRow>> {
        let sql = format!(
            "SELECT {OBJECT_COLS} FROM objects o JOIN sessions s ON s.id = o.session_id
             WHERE o.path_id = ?1 AND {VISIBLE} ORDER BY s.started DESC, o.id DESC LIMIT 1"
        );
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_row(
                params![path_id, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, view.as_of, view.session],
                object_row,
            )
            .optional()?
            .filter(|o| o.kind != "whiteout"))
    }

    /// Children of a directory path that have a visible version: (path id, name, object).
    pub fn children(&self, dir: i64, view: View) -> Result<Vec<(i64, Vec<u8>, ObjectRow)>> {
        let sql = format!(
            "SELECT * FROM (
               SELECT {OBJECT_COLS}, ROW_NUMBER() OVER (PARTITION BY o.path_id ORDER BY s.started DESC, o.id DESC) AS rn,
                      pa.name
               FROM paths pa JOIN objects o ON o.path_id = pa.id JOIN sessions s ON s.id = o.session_id
               WHERE pa.parent = ?1 AND {VISIBLE}
             ) WHERE rn = 1 AND kind != 'whiteout' ORDER BY name"
        );
        let mut st = self.conn.prepare_cached(&sql)?;
        Ok(st
            .query_map(
                params![dir, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, view.as_of, view.session],
                |r| {
                    let o = object_row(r)?;
                    Ok((o.path_id, r.get::<_, Vec<u8>>(24)?, o))
                },
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Whether any path at or below `path_id` is live in the view, its latest visible version
    /// not a whiteout (for directories implied by backed-up descendants, e.g. "/" or "/nebula"
    /// when only "/nebula/Movies" was saved). A deleted or renamed directory is not.
    pub fn has_visible_below(&self, path_id: i64, view: View) -> Result<bool> {
        let sql = format!(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION ALL SELECT p.id FROM paths p JOIN sub ON p.parent = sub.id)
             SELECT EXISTS (SELECT 1 FROM sub WHERE (
               SELECT o.kind FROM objects o JOIN sessions s ON s.id = o.session_id
               WHERE o.path_id = sub.id AND {VISIBLE} ORDER BY s.started DESC, o.id DESC LIMIT 1
             ) != 'whiteout')"
        );
        Ok(self.conn.prepare_cached(&sql)?.query_row(
            params![path_id, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, None::<i64>, view.as_of, view.session],
            |r| r.get(0),
        )?)
    }

    /// What a listing of a directory shows: its children with a visible version, and
    /// directories that exist only as ancestors of live paths (no object of their own), by name.
    pub fn entries(&self, dir: i64, view: View) -> Result<Vec<(i64, Vec<u8>, Option<ObjectRow>)>> {
        let mut out: Vec<(i64, Vec<u8>, Option<ObjectRow>)> = self.children(dir, view)?.into_iter().map(|(id, n, o)| (id, n, Some(o))).collect();
        let listed: HashSet<i64> = out.iter().map(|(id, _, _)| *id).collect();
        for (cid, name) in self.child_paths(dir)? {
            if !listed.contains(&cid) && self.has_visible_below(cid, view)? {
                out.push((cid, name, None));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(out)
    }

    /// Child path ids of a directory (whether or not they have objects).
    pub fn child_paths(&self, dir: i64) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut st = self.conn.prepare_cached("SELECT id, name FROM paths WHERE parent = ?1 ORDER BY name")?;
        Ok(st.query_map([dir], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Barcodes and positions holding an object's first chunk, per copy.
    pub fn object_locations(&self, object_id: i64) -> Result<Vec<(i64, String, i64, i64)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT p.copy, v.barcode, p.partition, MIN(p.block)
             FROM chunks c JOIN placements p ON p.chunk_id = c.id JOIN volumes v ON v.id = p.volume_id
             WHERE c.object_id = COALESCE((SELECT data_from FROM objects WHERE id = ?1), ?1)
             GROUP BY p.copy, v.barcode ORDER BY p.copy, MIN(p.block)",
        )?;
        Ok(st.query_map([object_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    // --- incremental sessions, references, retention ------------------------------------

    /// A second connection to the same catalog file, for reading while this one writes
    /// (None for an in-memory catalog).
    pub fn reader(&self) -> Result<Option<Catalog>> {
        match self.conn.path().filter(|p| !p.is_empty()) {
            Some(p) => Ok(Some(Catalog::open(Path::new(p))?)),
            None => Ok(None),
        }
    }

    /// A job's sessions that views show (finished, not expired), oldest first.
    pub fn job_sessions(&self, job: &str) -> Result<Vec<SessionRow>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions
             WHERE job = ?1 AND status IN ('complete', 'partial') AND expired_at IS NULL ORDER BY started, id"
        ))?;
        Ok(st.query_map([job], session_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// The newest object of every path among `sessions` (whiteouts included), with its
    /// full path: the state an incremental session compares against.
    pub fn state_of(&self, sessions: &[i64]) -> Result<Vec<(Vec<u8>, ObjectRow)>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let ids = sessions.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT * FROM (
               SELECT {OBJECT_COLS}, ROW_NUMBER() OVER (PARTITION BY o.path_id ORDER BY s.started DESC, o.id DESC) AS rn
               FROM objects o JOIN sessions s ON s.id = o.session_id WHERE o.session_id IN ({ids})
             ) WHERE rn = 1"
        );
        let rows: Vec<ObjectRow> = self.conn.prepare(&sql)?.query_map([], object_row)?.collect::<rusqlite::Result<_>>()?;
        let paths = self.full_paths(rows.iter().map(|o| o.path_id))?;
        Ok(rows.into_iter().map(|o| (paths[&o.path_id].clone(), o)).collect())
    }

    /// Hashes of every object in `sessions`, by object.
    pub fn session_hashes(&self, sessions: &[i64]) -> Result<HashMap<i64, Vec<HashRow>>> {
        let mut out: HashMap<i64, Vec<HashRow>> = HashMap::new();
        if sessions.is_empty() {
            return Ok(out);
        }
        let ids = sessions.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let mut st = self.conn.prepare(&format!(
            "SELECT h.object_id, h.profile, h.kind, h.algo, h.digest FROM object_hashes h JOIN objects o ON o.id = h.object_id
             WHERE o.session_id IN ({ids})"
        ))?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, HashRow { profile: r.get(1)?, kind: r.get(2)?, algo: r.get(3)?, digest: r.get(4)? })))?;
        for row in rows {
            let (id, h) = row?;
            out.entry(id).or_default().push(h);
        }
        Ok(out)
    }

    /// Whether copy `copy` of every chunk of the object holding `object_id`'s data is on
    /// tape.
    pub fn placed(&self, object_id: i64, copy: i64) -> Result<bool> {
        Ok(self.conn.prepare_cached(
            "SELECT NOT EXISTS (SELECT 1 FROM chunks c WHERE c.object_id = COALESCE((SELECT data_from FROM objects WHERE id = ?1), ?1)
               AND NOT EXISTS (SELECT 1 FROM placements p WHERE p.chunk_id = c.id AND p.copy = ?2))",
        )?
        .query_row(params![object_id, copy], |r| r.get(0))?)
    }

    /// An archived file of `size` bytes with content digest `digest` (algorithm `algo`)
    /// whose data copy `copy` holds, from a session that is not expired: (the object holding
    /// the data, that object's session uuid).
    pub fn find_content(&self, algo: &str, digest: &str, size: i64, copy: i64) -> Result<Option<(ObjectRow, String)>> {
        let sql = format!(
            "SELECT {OBJECT_COLS}, s.uuid FROM object_hashes h JOIN objects o ON o.id = h.object_id JOIN sessions s ON s.id = o.session_id
             WHERE h.algo = ?1 AND h.digest = ?2 AND o.size = ?3 AND o.kind = 'file' AND o.status = 'ok' AND o.data_from IS NULL
               AND s.status IN ('complete', 'partial') AND s.expired_at IS NULL
             ORDER BY s.started DESC LIMIT 8"
        );
        let found: Vec<(ObjectRow, String)> = self
            .conn
            .prepare_cached(&sql)?
            .query_map(params![algo, digest, size], |r| Ok((object_row(r)?, r.get(23)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (o, uuid) in found {
            if self.placed(o.id, copy)? {
                return Ok(Some((o, uuid)));
            }
        }
        Ok(None)
    }

    /// Whether any archived file has exactly `size` bytes (cheap test before hashing a file
    /// to look for a duplicate).
    pub fn any_file_of_size(&self, size: i64) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM objects WHERE kind = 'file' AND size = ?1 AND status = 'ok' AND data_from IS NULL)")?
            .query_row([size], |r| r.get(0))?)
    }

    /// Record that every session part on `volume_id` read back without errors.
    pub fn set_verified(&self, volume_id: i64, at: i64) -> Result<()> {
        self.conn.execute("UPDATE session_volumes SET verified_at = ?2 WHERE volume_id = ?1", params![volume_id, at])?;
        Ok(())
    }

    pub fn expire_session(&self, id: i64, at: i64) -> Result<()> {
        self.conn.execute("UPDATE sessions SET expired_at = ?2 WHERE id = ?1", params![id, at])?;
        Ok(())
    }

    /// Volumes holding nothing still needed: every session part on them is expired, and no
    /// object of a live session takes its data from chunks placed on them.
    pub fn reusable_volumes(&self) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare(
            "SELECT v.id FROM volumes v
             WHERE EXISTS (SELECT 1 FROM session_volumes sv WHERE sv.volume_id = v.id)
               AND NOT EXISTS (SELECT 1 FROM session_volumes sv JOIN sessions s ON s.id = sv.session_id
                               WHERE sv.volume_id = v.id AND s.expired_at IS NULL)
               AND NOT EXISTS (SELECT 1 FROM placements p JOIN chunks c ON c.id = p.chunk_id
                               JOIN objects o ON o.data_from = c.object_id JOIN sessions s ON s.id = o.session_id
                               WHERE p.volume_id = v.id AND s.expired_at IS NULL)",
        )?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// Copies of an object's data that are complete on tape and were verified after being
    /// written (`ltoarc verify` of the cartridges holding them).
    pub fn verified_copies(&self, object_id: i64) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached(
            "WITH d(id) AS (SELECT COALESCE((SELECT data_from FROM objects WHERE id = ?1), ?1))
             SELECT p.copy FROM chunks c JOIN placements p ON p.chunk_id = c.id JOIN objects o ON o.id = c.object_id
               LEFT JOIN session_volumes sv ON sv.session_id = o.session_id AND sv.volume_id = p.volume_id AND sv.copy = p.copy
             WHERE c.object_id = (SELECT id FROM d)
             GROUP BY p.copy
             HAVING COUNT(DISTINCT c.id) = (SELECT COUNT(*) FROM chunks WHERE object_id = (SELECT id FROM d))
                AND MIN(sv.verified_at IS NOT NULL) = 1
             ORDER BY p.copy",
        )?;
        Ok(st.query_map([object_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    // --- job runs ---------------------------------------------------------------------

    pub fn start_job_run(&self, job: &str, run: &str, log: Option<&str>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO job_runs (job, run, started, status, log) VALUES (?1, ?2, ?3, 'running', ?4)",
            params![job, run, crate::now(), log],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn end_job_run(&self, id: i64, status: &str, summary: &str) -> Result<()> {
        self.conn.execute("UPDATE job_runs SET ended = ?2, status = ?3, summary = ?4 WHERE id = ?1", params![id, crate::now(), status, summary])?;
        Ok(())
    }

    /// A job's runs (every job's with `job` None), newest first.
    pub fn job_runs_of(&self, job: Option<&str>, limit: usize) -> Result<Vec<JobRun>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, job, run, started, ended, status, summary, log FROM job_runs WHERE ?1 IS NULL OR job = ?1
             ORDER BY started DESC, id DESC LIMIT ?2",
        )?;
        Ok(st
            .query_map(params![job, limit as i64], |r| {
                Ok(JobRun { id: r.get(0)?, job: r.get(1)?, run: r.get(2)?, started: r.get(3)?, ended: r.get(4)?, status: r.get(5)?, summary: r.get(6)?, log: r.get(7)? })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// A job's runs, newest first.
    pub fn job_runs(&self, job: &str, limit: usize) -> Result<Vec<JobRun>> {
        self.job_runs_of(Some(job), limit)
    }

    pub fn job_run(&self, id: i64) -> Result<Option<JobRun>> {
        Ok(self
            .conn
            .query_row("SELECT id, job, run, started, ended, status, summary, log FROM job_runs WHERE id = ?1", [id], |r| {
                Ok(JobRun { id: r.get(0)?, job: r.get(1)?, run: r.get(2)?, started: r.get(3)?, ended: r.get(4)?, status: r.get(5)?, summary: r.get(6)?, log: r.get(7)? })
            })
            .optional()?)
    }

    /// Write a consistent copy of the whole catalog to `dest` (replaced atomically).
    pub fn copy_to(&self, dest: &Path) -> Result<()> {
        let tmp = dest.with_extension("tmp-copy");
        let _ = std::fs::remove_file(&tmp);
        self.conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()])?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }

    // --- volume catalog export -------------------------------------------------------

    /// Write a self-contained catalog of everything stored on `volume_id` to `dest`
    /// (a new SQLite file with the same schema). Must not run inside a transaction.
    pub fn export_volume(&self, volume_id: i64, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        drop(Catalog::open_plain(dest)?);
        let dest_s = dest.to_str().ok_or_else(|| Error::Invalid("export path is not UTF-8".into()))?;
        self.conn.execute_batch("PRAGMA foreign_keys = OFF")?;
        self.conn.execute("ATTACH DATABASE ?1 AS v", [dest_s])?;
        let result = (|| -> Result<()> {
            self.conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS export_sessions (id INTEGER PRIMARY KEY);
                 DELETE FROM export_sessions;",
            )?;
            self.conn.execute(
                "INSERT INTO export_sessions SELECT DISTINCT session_id FROM session_volumes WHERE volume_id = ?1",
                [volume_id],
            )?;
            self.conn.execute_batch(
                "BEGIN;
                 INSERT INTO v.volumes (id, barcode, generation, kind, state, uuid, pool, library_id, element, capacity_mb,
                                        registered_at, updated_at, notes)
                   SELECT id, barcode, generation, kind, state, uuid, pool, NULL, NULL, capacity_mb, registered_at,
                          updated_at, notes FROM volumes
                   WHERE id IN (SELECT volume_id FROM session_volumes WHERE session_id IN (SELECT id FROM export_sessions));
                 INSERT INTO v.sessions SELECT * FROM sessions WHERE id IN (SELECT id FROM export_sessions);
                 INSERT INTO v.session_volumes SELECT * FROM session_volumes
                   WHERE session_id IN (SELECT id FROM export_sessions);
                 INSERT INTO v.objects SELECT * FROM objects WHERE session_id IN (SELECT id FROM export_sessions);
                 INSERT INTO v.object_hashes SELECT h.* FROM object_hashes h JOIN v.objects o ON o.id = h.object_id;
                 INSERT INTO v.chunks SELECT c.* FROM chunks c JOIN v.objects o ON o.id = c.object_id;
                 INSERT INTO v.placements SELECT p.* FROM placements p JOIN v.chunks c ON c.id = p.chunk_id;
                 INSERT OR IGNORE INTO v.paths
                   WITH RECURSIVE need(id) AS (
                     SELECT DISTINCT path_id FROM v.objects
                     UNION SELECT p.parent FROM paths p JOIN need ON p.id = need.id WHERE p.parent IS NOT NULL)
                   SELECT p.* FROM paths p WHERE p.id IN (SELECT id FROM need) AND p.id != 1;
                 COMMIT;",
            )?;
            Ok(())
        })();
        if result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        self.conn.execute_batch("DETACH DATABASE v; PRAGMA foreign_keys = ON")?;
        result
    }
}

/// Migration v2 as one batch: archive tables plus the `labelled` flag on volumes.
pub(crate) const MIGRATION_V2_ALL: &str = concat!(
    "ALTER TABLE volumes ADD COLUMN labelled INTEGER NOT NULL DEFAULT 1;",
    include_str!("migration_v2.sql")
);

/// Migration v3: files packed together in PACK records.
pub(crate) const MIGRATION_V3: &str = "ALTER TABLE chunks ADD COLUMN pack_offset INTEGER;";

/// Migration v6: files stored by reference to earlier data (moves, duplicates), a size
/// index to find candidates, when each session part was last verified, and expiry.
/// Migration v8: runs of scheduled jobs.
pub(crate) const MIGRATION_V8: &str = "
    CREATE TABLE job_runs (
        id INTEGER PRIMARY KEY,
        job TEXT NOT NULL,
        run TEXT NOT NULL,
        started INTEGER NOT NULL,
        ended INTEGER,
        status TEXT NOT NULL,
        summary TEXT,
        log TEXT
    );
    CREATE INDEX job_runs_by_job ON job_runs(job, started);
";

/// Migration v7: a copy's own stored size where it differs from the chunk's.
pub(crate) const MIGRATION_V7: &str = "ALTER TABLE placements ADD COLUMN stored_len INTEGER;";

pub(crate) const MIGRATION_V6: &str = "
    ALTER TABLE objects ADD COLUMN data_from INTEGER REFERENCES objects(id);
    CREATE INDEX objects_by_size ON objects(size) WHERE kind = 'file';
    CREATE INDEX objects_by_data ON objects(data_from) WHERE data_from IS NOT NULL;
    ALTER TABLE session_volumes ADD COLUMN verified_at INTEGER;
    ALTER TABLE sessions ADD COLUMN expired_at INTEGER;
";

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(session_id: i64, path_id: i64, seq: i64, kind: &str, size: i64) -> ObjectRow {
        ObjectRow { session_id, path_id, seq, kind: kind.into(), size, status: "ok".into(), ..Default::default() }
    }

    #[test]
    fn sessions_views_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let c = Catalog::open(&dir.path().join("c.db")).unwrap();
        let lib = c
            .upsert_library(
                &crate::LibraryRow {
                    id: 0,
                    name: String::new(),
                    serial: "L".into(),
                    backend: "sim".into(),
                    location: "/".into(),
                    vendor: "v".into(),
                    product: "p".into(),
                    revision: "r".into(),
                    slots: 4,
                    ie_slots: 0,
                    bays: 1,
                    slot_base: Some(0x1000),
                    ie_base: None,
                    bay_base: Some(0x100),
                    last_seen: 0,
                },
                "lib1",
            )
            .unwrap();
        let (vol, _) = c.volume_seen("V1", Some("LTO-8"), "data", lib.id, 0x1000).unwrap();
        let un = c.register_unlabelled("PRGP04", Some("LTO-7"), lib.id, 0x1003).unwrap();
        assert!(!un.labelled);
        assert_eq!(c.unlabelled_at(lib.id, 0x1003).unwrap().unwrap().barcode, "PRGP04");
        assert!(c.register_unlabelled("V1", None, lib.id, 0x1002).is_err());

        let mut sessions = Vec::new();
        for (n, started) in [(1, 100), (2, 200)] {
            let sid = c
                .create_session(&SessionRow {
                    uuid: format!("uuid-{n}"),
                    level: "full".into(),
                    started,
                    status: "complete".into(),
                    host: "h".into(),
                    sources: "[]".into(),
                    ..Default::default()
                })
                .unwrap();
            c.add_session_volume(&SessionVolume {
                session_id: sid,
                copy: 1,
                seq: 1,
                volume_id: vol.id,
                start_object: n * 10,
                ..Default::default()
            })
            .unwrap();
            sessions.push(sid);
        }
        let dir_id = c.intern_path(b"/data/sub").unwrap();
        let f1 = c.intern_path(b"/data/sub/a.txt").unwrap();
        let f2 = c.intern_path(b"/data/b.bin").unwrap();
        assert_eq!(c.lookup_path(b"/data/sub/a.txt").unwrap(), Some(f1));
        assert_eq!(c.lookup_path(b"/data/nope").unwrap(), None);
        c.insert_object(&obj(sessions[0], dir_id, 0, "dir", 0)).unwrap();
        let o1 = c.insert_object(&obj(sessions[0], f1, 1, "file", 5)).unwrap();
        c.insert_object(&obj(sessions[0], f2, 2, "file", 7)).unwrap();
        let o1b = c.insert_object(&obj(sessions[1], f1, 0, "file", 6)).unwrap();
        let ch = c
            .insert_chunk(&ChunkRow { object_id: o1, seq: 0, offset: 0, raw_len: 5, stored_len: 5, codec: 0, blake3: vec![1; 32], ..Default::default() })
            .unwrap();
        c.insert_placement(&Placement { chunk_id: ch, copy: 1, volume_id: vol.id, partition: 0, block: 12, offset: 0, stored_len: None }).unwrap();
        c.insert_hash(o1, &HashRow { profile: "blake3".into(), kind: "content".into(), algo: "blake3".into(), digest: "ab".into() })
            .unwrap();

        let data = c.lookup_path(b"/data").unwrap().unwrap();
        let all = c.resolve(data, View::default()).unwrap();
        let names: Vec<_> = all.iter().map(|(p, o)| (String::from_utf8_lossy(p).to_string(), o.size)).collect();
        assert_eq!(names, [("/data/b.bin".into(), 7), ("/data/sub".into(), 0), ("/data/sub/a.txt".into(), 6)]);
        let old = c.resolve(data, View { as_of: Some(150), session: None }).unwrap();
        assert_eq!(old.iter().find(|(p, _)| p.ends_with(b"a.txt")).unwrap().1.id, o1);
        assert_eq!(c.latest(f1, View::default()).unwrap().unwrap().id, o1b);
        let kids = c.children(data, View::default()).unwrap();
        assert_eq!(kids.iter().map(|k| k.1.clone()).collect::<Vec<_>>(), vec![b"b.bin".to_vec(), b"sub".to_vec()]);
        assert!(c.has_visible_below(ROOT_PATH, View::default()).unwrap());
        assert_eq!(c.chunk_placements(o1).unwrap()[0].barcode, "V1");
        assert_eq!(c.object_locations(o1).unwrap(), vec![(1, "V1".to_string(), 0, 12)]);
        assert_eq!(c.volume_placements(vol.id).unwrap().len(), 1);

        let out = dir.path().join("vol.db");
        c.export_volume(vol.id, &out).unwrap();
        let e = Catalog::open_plain(&out).unwrap();
        assert_eq!(e.sessions().unwrap().len(), 2);
        let ed = e.lookup_path(b"/data").unwrap().unwrap();
        assert_eq!(e.resolve(ed, View::default()).unwrap().len(), 3);
        assert_eq!(e.hashes(o1).unwrap()[0].digest, "ab");
        assert_eq!(e.volume("V1").unwrap().unwrap().library_id, None);
    }

    /// A folder renamed between sessions: the incremental one records whiteouts for the old
    /// folder and its files, which leave the listings of later views, and so does a folder
    /// implied only by files that were deleted.
    #[test]
    fn renamed_folder_leaves_later_views() {
        let c = Catalog::in_memory().unwrap();
        let session = |started| {
            c.create_session(&SessionRow {
                uuid: format!("uuid-{started}"),
                level: "full".into(),
                started,
                status: "complete".into(),
                host: "h".into(),
                sources: "[]".into(),
                ..Default::default()
            })
            .unwrap()
        };
        let (full, inc) = (session(100), session(200));
        let photos = c.intern_path(b"/data/photos").unwrap();
        let old = c.intern_path(b"/data/photos/kyoto").unwrap();
        let new = c.intern_path(b"/data/photos/kyoto-autumn").unwrap();
        let gone = c.intern_path(b"/data/tmp/x.log").unwrap();
        c.insert_object(&obj(full, photos, 0, "dir", 0)).unwrap();
        c.insert_object(&obj(full, old, 1, "dir", 0)).unwrap();
        c.insert_object(&obj(full, gone, 2, "file", 1)).unwrap();
        c.insert_object(&obj(inc, photos, 0, "dir", 0)).unwrap();
        c.insert_object(&obj(inc, new, 1, "dir", 0)).unwrap();
        c.insert_object(&obj(inc, old, 2, "whiteout", 0)).unwrap();
        c.insert_object(&obj(inc, gone, 3, "whiteout", 0)).unwrap();
        for (n, name) in [b"a.ARW", b"b.ARW"].into_iter().enumerate() {
            let n = n as i64;
            c.insert_object(&obj(full, c.path_child(old, name).unwrap(), 3 + n, "file", 9)).unwrap();
            c.insert_object(&obj(inc, c.path_child(new, name).unwrap(), 4 + n, "file", 9)).unwrap();
            c.insert_object(&obj(inc, c.path_child(old, name).unwrap(), 6 + n, "whiteout", 0)).unwrap();
        }
        let data = c.lookup_path(b"/data").unwrap().unwrap();
        let tmp = c.lookup_path(b"/data/tmp").unwrap().unwrap();
        let names = |dir, view| -> Vec<(String, bool)> {
            c.entries(dir, view).unwrap().into_iter().map(|(_, n, o)| (String::from_utf8(n).unwrap(), o.is_some())).collect()
        };
        let latest = View::default();
        assert_eq!(names(photos, latest), [("kyoto-autumn".into(), true)]);
        assert_eq!(names(new, latest), [("a.ARW".into(), true), ("b.ARW".into(), true)]);
        assert!(names(old, latest).is_empty());
        assert!(c.latest(old, latest).unwrap().is_none());
        assert!(!c.has_visible_below(old, latest).unwrap());
        assert_eq!(names(data, latest), [("photos".into(), true)], "tmp/ held only a deleted file");
        assert_eq!(names(ROOT_PATH, latest), [("data".into(), false)]);
        assert_eq!(c.resolve(data, latest).unwrap().len(), 4);
        // Earlier views still show the folder as it was.
        for view in [View { as_of: Some(150), session: None }, View { as_of: None, session: Some(full) }] {
            assert_eq!(names(photos, view), [("kyoto".into(), true)]);
            assert_eq!(names(old, view), [("a.ARW".into(), true), ("b.ARW".into(), true)]);
            assert_eq!(names(data, view), [("photos".into(), true), ("tmp".into(), false)]);
        }
        assert!(names(tmp, View { as_of: Some(250), session: None }).is_empty());
    }
}
