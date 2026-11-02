//! Local catalog (SQLite): hardware, volumes, wear statistics, events.
//! Later phases add sessions, objects, chunks and placements.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

pub mod archive;

pub use archive::{ChunkPlacement, ChunkRow, HashRow, JobRun, ObjectRow, Placement, SessionRow, SessionVolume, View, ROOT_PATH};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

const MIGRATIONS: &[&str] = &[
    // v1: hardware, volumes, statistics, events
    r#"
    CREATE TABLE libraries (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        serial TEXT NOT NULL UNIQUE,
        backend TEXT NOT NULL,
        location TEXT NOT NULL,
        vendor TEXT NOT NULL,
        product TEXT NOT NULL,
        revision TEXT NOT NULL,
        slots INTEGER NOT NULL DEFAULT 0,
        ie_slots INTEGER NOT NULL DEFAULT 0,
        bays INTEGER NOT NULL DEFAULT 0,
        slot_base INTEGER,
        ie_base INTEGER,
        bay_base INTEGER,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE drives (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        serial TEXT NOT NULL UNIQUE,
        backend TEXT NOT NULL,
        location TEXT,
        st_path TEXT,
        vendor TEXT NOT NULL,
        product TEXT NOT NULL,
        revision TEXT NOT NULL,
        generation TEXT,
        library_id INTEGER REFERENCES libraries(id),
        element INTEGER,
        state TEXT NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE volumes (
        id INTEGER PRIMARY KEY,
        barcode TEXT NOT NULL UNIQUE,
        generation TEXT,
        kind TEXT NOT NULL,
        state TEXT NOT NULL,
        uuid TEXT UNIQUE,
        pool TEXT,
        library_id INTEGER REFERENCES libraries(id),
        element INTEGER,
        capacity_mb INTEGER,
        registered_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        notes TEXT
    );
    CREATE TABLE drive_stats (
        id INTEGER PRIMARY KEY,
        drive_id INTEGER NOT NULL REFERENCES drives(id),
        taken_at INTEGER NOT NULL,
        event TEXT NOT NULL,
        data TEXT NOT NULL
    );
    CREATE INDEX drive_stats_by_drive ON drive_stats(drive_id, taken_at);
    CREATE TABLE volume_stats (
        id INTEGER PRIMARY KEY,
        volume_id INTEGER NOT NULL REFERENCES volumes(id),
        drive_id INTEGER REFERENCES drives(id),
        taken_at INTEGER NOT NULL,
        event TEXT NOT NULL,
        data TEXT NOT NULL
    );
    CREATE INDEX volume_stats_by_volume ON volume_stats(volume_id, taken_at);
    CREATE TABLE events (
        id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        kind TEXT NOT NULL,
        subject TEXT NOT NULL,
        message TEXT NOT NULL
    );
    "#,
    // v2: archive content, and cartridges named by hand (no readable barcode label)
    archive::MIGRATION_V2_ALL,
    // v3: small files packed into PACK records
    archive::MIGRATION_V3,
    // v4: measured speeds of sources and drives, reused by the scheduler
    r#"
    CREATE TABLE benchmarks (
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL,
        key TEXT NOT NULL,
        taken_at INTEGER NOT NULL,
        data TEXT NOT NULL
    );
    CREATE INDEX benchmarks_by_key ON benchmarks(kind, key, taken_at);
    "#,
    // v5: encrypted sessions (wrapped data key, JSON)
    "ALTER TABLE sessions ADD COLUMN encryption TEXT;",
    // v6: references to earlier data, verification times, expiry
    archive::MIGRATION_V6,
    // v7: per-copy stored size
    archive::MIGRATION_V7,
    // v8: runs of scheduled jobs
    archive::MIGRATION_V8,
    // v9: block size of each labelled cartridge (appending needs it without reading the label)
    "ALTER TABLE volumes ADD COLUMN block_size INTEGER;",
];

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryRow {
    pub id: i64,
    pub name: String,
    pub serial: String,
    pub backend: String,
    pub location: String,
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub slots: i64,
    pub ie_slots: i64,
    pub bays: i64,
    /// First element address of storage slots, import/export slots and drive bays.
    pub slot_base: Option<i64>,
    pub ie_base: Option<i64>,
    pub bay_base: Option<i64>,
    pub last_seen: i64,
}

impl LibraryRow {
    /// Human-readable name of an element address ("slot 4", "I/E 1", "drive bay 2").
    pub fn element_name(&self, addr: i64) -> String {
        let pick = |base: Option<i64>, count: i64| base.filter(|b| addr >= *b && addr < b + count.max(1)).map(|b| addr - b + 1);
        if let Some(n) = pick(self.slot_base, self.slots) {
            format!("slot {n}")
        } else if let Some(n) = pick(self.ie_base, self.ie_slots) {
            format!("I/E {n}")
        } else if let Some(n) = pick(self.bay_base, self.bays) {
            format!("drive bay {n}")
        } else {
            format!("element {addr:#06x}")
        }
    }

    /// Element address of a 1-based storage slot number.
    pub fn slot_address(&self, slot: i64) -> Option<i64> {
        (slot >= 1 && slot <= self.slots).then(|| self.slot_base.map(|b| b + slot - 1)).flatten()
    }

    /// Element address of a 1-based import/export slot number.
    pub fn ie_address(&self, n: i64) -> Option<i64> {
        (n >= 1 && n <= self.ie_slots).then(|| self.ie_base.map(|b| b + n - 1)).flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveRow {
    pub id: i64,
    pub name: String,
    pub serial: String,
    pub backend: String,
    /// sg path or simulated world directory; None for drives not attached to this host.
    pub location: Option<String>,
    pub st_path: Option<String>,
    pub vendor: String,
    pub product: String,
    pub revision: String,
    pub generation: Option<String>,
    pub library_id: Option<i64>,
    pub element: Option<i64>,
    /// "attached" or "unattached".
    pub state: String,
    pub last_seen: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeRow {
    pub id: i64,
    pub barcode: String,
    pub generation: Option<String>,
    /// data | worm | cleaning
    pub kind: String,
    /// new | blank | foreign | scratch | filling | full | cleaning | retired
    pub state: String,
    pub uuid: Option<String>,
    pub pool: Option<String>,
    pub library_id: Option<i64>,
    pub element: Option<i64>,
    pub capacity_mb: Option<i64>,
    pub registered_at: i64,
    pub updated_at: i64,
    pub notes: Option<String>,
    /// False when the library cannot read a barcode and the name was assigned by hand; the
    /// cartridge is then tracked by element address.
    pub labelled: bool,
    /// Block size its ltoarc label gives (known once ltoarc labelled or read the label).
    pub block_size: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatsRow {
    pub taken_at: i64,
    pub event: String,
    pub drive_id: Option<i64>,
    pub data: serde_json::Map<String, Value>,
}

pub struct Catalog {
    conn: Connection,
}

fn library_row(r: &Row) -> rusqlite::Result<LibraryRow> {
    Ok(LibraryRow {
        id: r.get(0)?,
        name: r.get(1)?,
        serial: r.get(2)?,
        backend: r.get(3)?,
        location: r.get(4)?,
        vendor: r.get(5)?,
        product: r.get(6)?,
        revision: r.get(7)?,
        slots: r.get(8)?,
        ie_slots: r.get(9)?,
        bays: r.get(10)?,
        slot_base: r.get(11)?,
        ie_base: r.get(12)?,
        bay_base: r.get(13)?,
        last_seen: r.get(14)?,
    })
}

const LIBRARY_COLS: &str =
    "id, name, serial, backend, location, vendor, product, revision, slots, ie_slots, bays, slot_base, ie_base, bay_base, last_seen";

fn drive_row(r: &Row) -> rusqlite::Result<DriveRow> {
    Ok(DriveRow {
        id: r.get(0)?,
        name: r.get(1)?,
        serial: r.get(2)?,
        backend: r.get(3)?,
        location: r.get(4)?,
        st_path: r.get(5)?,
        vendor: r.get(6)?,
        product: r.get(7)?,
        revision: r.get(8)?,
        generation: r.get(9)?,
        library_id: r.get(10)?,
        element: r.get(11)?,
        state: r.get(12)?,
        last_seen: r.get(13)?,
    })
}

const DRIVE_COLS: &str =
    "id, name, serial, backend, location, st_path, vendor, product, revision, generation, library_id, element, state, last_seen";

fn volume_row(r: &Row) -> rusqlite::Result<VolumeRow> {
    Ok(VolumeRow {
        id: r.get(0)?,
        barcode: r.get(1)?,
        generation: r.get(2)?,
        kind: r.get(3)?,
        state: r.get(4)?,
        uuid: r.get(5)?,
        pool: r.get(6)?,
        library_id: r.get(7)?,
        element: r.get(8)?,
        capacity_mb: r.get(9)?,
        registered_at: r.get(10)?,
        updated_at: r.get(11)?,
        notes: r.get(12)?,
        labelled: r.get(13)?,
        block_size: r.get(14)?,
    })
}

const VOLUME_COLS: &str =
    "id, barcode, generation, kind, state, uuid, pool, library_id, element, capacity_mb, registered_at, updated_at, notes, labelled, block_size";

impl Catalog {
    pub fn open(path: &Path) -> Result<Catalog> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// Open without WAL (a single self-contained file, e.g. an exported volume catalog).
    pub fn open_plain(path: &Path) -> Result<Catalog> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Catalog> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Catalog> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Catalog { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // --- libraries -------------------------------------------------------------------

    /// Insert or update by serial; a new library gets `default_name`. Returns its row.
    pub fn upsert_library(&self, l: &LibraryRow, default_name: &str) -> Result<LibraryRow> {
        self.conn.execute(
            "INSERT INTO libraries (name, serial, backend, location, vendor, product, revision, slots, ie_slots, bays,
                                    slot_base, ie_base, bay_base, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(serial) DO UPDATE SET backend=excluded.backend, location=excluded.location,
               vendor=excluded.vendor, product=excluded.product, revision=excluded.revision,
               slots=excluded.slots, ie_slots=excluded.ie_slots, bays=excluded.bays, slot_base=excluded.slot_base,
               ie_base=excluded.ie_base, bay_base=excluded.bay_base, last_seen=excluded.last_seen",
            params![
                default_name,
                l.serial,
                l.backend,
                l.location,
                l.vendor,
                l.product,
                l.revision,
                l.slots,
                l.ie_slots,
                l.bays,
                l.slot_base,
                l.ie_base,
                l.bay_base,
                l.last_seen
            ],
        )?;
        self.library(&l.serial)?.ok_or_else(|| Error::Invalid("library vanished".into()))
    }

    pub fn libraries(&self) -> Result<Vec<LibraryRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {LIBRARY_COLS} FROM libraries ORDER BY name"))?;
        Ok(st.query_map([], library_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// By name or serial.
    pub fn library(&self, key: &str) -> Result<Option<LibraryRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {LIBRARY_COLS} FROM libraries WHERE name = ?1 OR serial = ?1"),
                [key],
                library_row,
            )
            .optional()?)
    }

    pub fn library_by_id(&self, id: i64) -> Result<Option<LibraryRow>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {LIBRARY_COLS} FROM libraries WHERE id = ?1"), [id], library_row)
            .optional()?)
    }

    // --- drives ----------------------------------------------------------------------

    pub fn upsert_drive(&self, d: &DriveRow, default_name: &str) -> Result<DriveRow> {
        self.conn.execute(
            "INSERT INTO drives (name, serial, backend, location, st_path, vendor, product, revision, generation,
                                 library_id, element, state, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(serial) DO UPDATE SET backend=excluded.backend, location=excluded.location,
               st_path=excluded.st_path, vendor=excluded.vendor, product=excluded.product,
               revision=CASE WHEN excluded.revision = '' THEN drives.revision ELSE excluded.revision END,
               generation=COALESCE(excluded.generation, drives.generation),
               library_id=excluded.library_id, element=excluded.element, state=excluded.state,
               last_seen=excluded.last_seen",
            params![
                default_name,
                d.serial,
                d.backend,
                d.location,
                d.st_path,
                d.vendor,
                d.product,
                d.revision,
                d.generation,
                d.library_id,
                d.element,
                d.state,
                d.last_seen
            ],
        )?;
        self.drive(&d.serial)?.ok_or_else(|| Error::Invalid("drive vanished".into()))
    }

    pub fn drives(&self) -> Result<Vec<DriveRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {DRIVE_COLS} FROM drives ORDER BY name"))?;
        Ok(st.query_map([], drive_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// By name or serial.
    pub fn drive(&self, key: &str) -> Result<Option<DriveRow>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {DRIVE_COLS} FROM drives WHERE name = ?1 OR serial = ?1"), [key], drive_row)
            .optional()?)
    }

    /// Mark attached drives not seen since `since` as offline. Returns their names.
    pub fn mark_unseen_drives_offline(&self, since: i64) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for d in self.drives()?.into_iter().filter(|d| d.last_seen < since && d.state == "attached") {
            self.conn.execute("UPDATE drives SET state = 'offline' WHERE id = ?1", [d.id])?;
            out.push(d.name);
        }
        Ok(out)
    }

    pub fn rename(&self, table: &str, old: &str, new: &str) -> Result<()> {
        let sql = match table {
            "drive" => "UPDATE drives SET name = ?2 WHERE name = ?1 OR serial = ?1",
            "library" => "UPDATE libraries SET name = ?2 WHERE name = ?1 OR serial = ?1",
            _ => return Err(Error::Invalid(format!("cannot rename {table}"))),
        };
        match self.conn.execute(sql, params![old, new])? {
            0 => Err(Error::Invalid(format!("no {table} named {old}"))),
            _ => Ok(()),
        }
    }

    // --- volumes ---------------------------------------------------------------------

    pub fn volumes(&self) -> Result<Vec<VolumeRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {VOLUME_COLS} FROM volumes ORDER BY barcode"))?;
        Ok(st.query_map([], volume_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn volume_by_id(&self, id: i64) -> Result<Option<VolumeRow>> {
        Ok(self.conn.query_row(&format!("SELECT {VOLUME_COLS} FROM volumes WHERE id = ?1"), [id], volume_row).optional()?)
    }

    pub fn volume(&self, barcode: &str) -> Result<Option<VolumeRow>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {VOLUME_COLS} FROM volumes WHERE barcode = ?1"), [barcode], volume_row)
            .optional()?)
    }

    /// Record that a cartridge was seen at a library element. New barcodes are registered
    /// with state "new" (or "cleaning"); a known generation is never replaced by unknown.
    pub fn volume_seen(
        &self,
        barcode: &str,
        generation: Option<&str>,
        kind: &str,
        library_id: i64,
        element: i64,
    ) -> Result<(VolumeRow, bool)> {
        let t = now();
        let state = if kind == "cleaning" { "cleaning" } else { "new" };
        let inserted = self.conn.execute(
            "INSERT INTO volumes (barcode, generation, kind, state, library_id, element, registered_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(barcode) DO NOTHING",
            params![barcode, generation, kind, state, library_id, element, t],
        )? == 1;
        if !inserted {
            self.conn.execute(
                "UPDATE volumes SET library_id = ?2, element = ?3, updated_at = ?4,
                    generation = COALESCE(generation, ?5) WHERE barcode = ?1",
                params![barcode, library_id, element, t, generation],
            )?;
        }
        Ok((self.volume(barcode)?.ok_or_else(|| Error::Invalid("volume vanished".into()))?, inserted))
    }

    /// Register a cartridge whose barcode the library cannot read, under a chosen name, at a
    /// library element. Fails if the name is already used by another cartridge.
    pub fn register_unlabelled(
        &self,
        name: &str,
        generation: Option<&str>,
        library_id: i64,
        element: i64,
    ) -> Result<VolumeRow> {
        if let Some(v) = self.volume(name)? {
            if v.labelled {
                return Err(Error::Invalid(format!("{name} is already the barcode of another cartridge")));
            }
        }
        // One name per cartridge: the place may not already hold another unlabelled one (a
        // barcoded one recorded there is out of date: the library reads no barcode there).
        let other: Option<String> = self
            .conn
            .query_row(
                "SELECT barcode FROM volumes WHERE library_id = ?1 AND element = ?2 AND labelled = 0 AND barcode != ?3",
                params![library_id, element, name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(o) = other {
            return Err(Error::Invalid(format!(
                "the catalog has {o} there already (no barcode); to name it {name} instead, `ltoarc tape unregister {o}` first \
                 (if {o} was moved, register it at its new slot first)"
            )));
        }
        let t = now();
        self.conn.execute(
            "INSERT INTO volumes (barcode, generation, kind, state, library_id, element, registered_at, updated_at, labelled)
             VALUES (?1, ?2, 'data', 'new', ?3, ?4, ?5, ?5, 0)
             ON CONFLICT(barcode) DO UPDATE SET library_id = ?3, element = ?4, updated_at = ?5,
               generation = COALESCE(volumes.generation, ?2)",
            params![name, generation, library_id, element, t],
        )?;
        self.volume(name)?.ok_or_else(|| Error::Invalid("volume vanished".into()))
    }

    /// Remove a cartridge from the catalog, with its statistics. What it holds must have been
    /// forgotten first (`forget_volume`).
    pub fn delete_volume(&self, id: i64) -> Result<()> {
        let held: i64 = self.conn.query_row(
            "SELECT (SELECT COUNT(*) FROM session_volumes WHERE volume_id = ?1) + (SELECT COUNT(*) FROM placements WHERE volume_id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if held > 0 {
            return Err(Error::Invalid("the cartridge still holds sessions".into()));
        }
        self.conn.execute("DELETE FROM volume_stats WHERE volume_id = ?1", [id])?;
        self.conn.execute("DELETE FROM volumes WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Unlabelled cartridge recorded at a library element.
    pub fn unlabelled_at(&self, library_id: i64, element: i64) -> Result<Option<VolumeRow>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {VOLUME_COLS} FROM volumes WHERE library_id = ?1 AND element = ?2 AND labelled = 0"),
                params![library_id, element],
                volume_row,
            )
            .optional()?)
    }

    /// Volumes recorded in `library_id` that are not in `present`: mark their location unknown.
    /// Unlabelled cartridges are kept while their element still holds a cartridge without a
    /// barcode (listed in `unlabelled_full`).
    pub fn volumes_missing_from(&self, library_id: i64, present: &[String]) -> Result<Vec<String>> {
        self.volumes_missing_from_with(library_id, present, &[])
    }

    pub fn volumes_missing_from_with(
        &self,
        library_id: i64,
        present: &[String],
        unlabelled_full: &[i64],
    ) -> Result<Vec<String>> {
        let mut gone = Vec::new();
        for v in self.volumes()?.into_iter().filter(|v| v.library_id == Some(library_id)) {
            if !v.labelled && v.element.is_some_and(|e| unlabelled_full.contains(&e)) {
                continue;
            }
            if !present.contains(&v.barcode) {
                self.conn.execute(
                    "UPDATE volumes SET library_id = NULL, element = NULL, updated_at = ?2 WHERE id = ?1",
                    params![v.id, now()],
                )?;
                gone.push(v.barcode);
            }
        }
        Ok(gone)
    }

    pub fn update_volume(&self, v: &VolumeRow) -> Result<()> {
        self.conn.execute(
            "UPDATE volumes SET generation = ?2, kind = ?3, state = ?4, uuid = ?5, pool = ?6, library_id = ?7,
               element = ?8, capacity_mb = ?9, updated_at = ?10, notes = ?11, block_size = ?12 WHERE id = ?1",
            params![
                v.id,
                v.generation,
                v.kind,
                v.state,
                v.uuid,
                v.pool,
                v.library_id,
                v.element,
                v.capacity_mb,
                now(),
                v.notes,
                v.block_size
            ],
        )?;
        Ok(())
    }

    // --- statistics ------------------------------------------------------------------

    pub fn add_drive_stats<T: serde::Serialize>(&self, drive_id: i64, event: &str, data: &T) -> Result<()> {
        let json = serde_json::to_string(data).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO drive_stats (drive_id, taken_at, event, data) VALUES (?1, ?2, ?3, ?4)",
            params![drive_id, now(), event, json],
        )?;
        Ok(())
    }

    pub fn add_volume_stats<T: serde::Serialize>(
        &self,
        volume_id: i64,
        drive_id: Option<i64>,
        event: &str,
        data: &T,
    ) -> Result<()> {
        let json = serde_json::to_string(data).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO volume_stats (volume_id, drive_id, taken_at, event, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![volume_id, drive_id, now(), event, json],
        )?;
        Ok(())
    }

    /// Most recent first.
    pub fn drive_stats(&self, drive_id: i64, limit: usize) -> Result<Vec<StatsRow>> {
        self.stats_query(
            "SELECT taken_at, event, drive_id, data FROM drive_stats WHERE drive_id = ?1
             ORDER BY taken_at DESC, id DESC LIMIT ?2",
            drive_id,
            limit,
        )
    }

    pub fn volume_stats(&self, volume_id: i64, limit: usize) -> Result<Vec<StatsRow>> {
        self.stats_query(
            "SELECT taken_at, event, drive_id, data FROM volume_stats WHERE volume_id = ?1
             ORDER BY taken_at DESC, id DESC LIMIT ?2",
            volume_id,
            limit,
        )
    }

    fn stats_query(&self, sql: &str, id: i64, limit: usize) -> Result<Vec<StatsRow>> {
        let mut st = self.conn.prepare(sql)?;
        let rows = st.query_map(params![id, limit as i64], |r| {
            let data: String = r.get(3)?;
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, data))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (taken_at, event, drive_id, data) = row?;
            let data = match serde_json::from_str::<Value>(&data) {
                Ok(Value::Object(m)) => m,
                _ => serde_json::Map::new(),
            };
            out.push(StatsRow { taken_at, event, drive_id, data });
        }
        Ok(out)
    }

    // --- benchmarks ------------------------------------------------------------------

    /// Record a benchmark result (`kind` "source" keyed by filesystem, or "drive" keyed by
    /// drive serial and media generation).
    pub fn add_benchmark<T: serde::Serialize>(&self, kind: &str, key: &str, data: &T) -> Result<()> {
        let json = serde_json::to_string(data).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.execute(
            "INSERT INTO benchmarks (kind, key, taken_at, data) VALUES (?1, ?2, ?3, ?4)",
            params![kind, key, now(), json],
        )?;
        Ok(())
    }

    /// Latest benchmark of every (kind, key): (kind, key, taken_at, data).
    pub fn benchmarks(&self) -> Result<Vec<(String, String, i64, Value)>> {
        let mut st = self.conn.prepare(
            "SELECT b.kind, b.key, b.taken_at, b.data FROM benchmarks b
             WHERE b.id = (SELECT id FROM benchmarks c WHERE c.kind = b.kind AND c.key = b.key
                           ORDER BY taken_at DESC, id DESC LIMIT 1)
             ORDER BY b.kind, b.key",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, key, t, d) = row?;
            out.push((k, key, t, serde_json::from_str(&d).unwrap_or(Value::Null)));
        }
        Ok(out)
    }

    /// Most recent benchmark: (taken_at, data).
    pub fn benchmark(&self, kind: &str, key: &str) -> Result<Option<(i64, Value)>> {
        let row: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT taken_at, data FROM benchmarks WHERE kind = ?1 AND key = ?2 ORDER BY taken_at DESC, id DESC LIMIT 1",
                params![kind, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(t, d)| serde_json::from_str(&d).ok().map(|v| (t, v))))
    }

    // --- events ----------------------------------------------------------------------

    pub fn log_event(&self, kind: &str, subject: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (at, kind, subject, message) VALUES (?1, ?2, ?3, ?4)",
            params![now(), kind, subject, message],
        )?;
        Ok(())
    }

    pub fn events(&self, subject: Option<&str>, limit: usize) -> Result<Vec<(i64, String, String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT at, kind, subject, message FROM events WHERE ?1 IS NULL OR subject = ?1
             ORDER BY at DESC, id DESC LIMIT ?2",
        )?;
        Ok(st
            .query_map(params![subject, limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> LibraryRow {
        LibraryRow {
            id: 0,
            name: String::new(),
            serial: "LIB1".into(),
            backend: "sim".into(),
            location: "/w".into(),
            vendor: "IBM".into(),
            product: "3573-TL".into(),
            revision: "A".into(),
            slots: 23,
            ie_slots: 1,
            bays: 2,
            slot_base: Some(0x1000),
            ie_base: Some(0x10),
            bay_base: Some(0x100),
            last_seen: 1,
        }
    }

    #[test]
    fn hardware_and_volumes() {
        let c = Catalog::in_memory().unwrap();
        assert_eq!(c.schema_version().unwrap(), MIGRATIONS.len() as i64);
        let l = c.upsert_library(&lib(), "lib1").unwrap();
        let l2 = c.upsert_library(&LibraryRow { slots: 24, ..lib() }, "ignored").unwrap();
        assert_eq!((l.id, l2.name.as_str(), l2.slots), (l2.id, "lib1", 24));
        assert_eq!(l2.element_name(0x1003), "slot 4");
        assert_eq!(l2.element_name(0x101), "drive bay 2");
        assert_eq!(l2.element_name(0x10), "I/E 1");
        assert_eq!(l2.slot_address(23), Some(0x1016));

        let d = DriveRow {
            id: 0,
            name: String::new(),
            serial: "90WT093075".into(),
            backend: "real".into(),
            location: None,
            st_path: None,
            vendor: "IBM".into(),
            product: "ULT3580-HH6".into(),
            revision: String::new(),
            generation: Some("LTO-6".into()),
            library_id: Some(l.id),
            element: Some(0x101),
            state: "unattached".into(),
            last_seen: 1,
        };
        let row = c.upsert_drive(&d, "lto6-1").unwrap();
        assert_eq!(row.name, "lto6-1");
        let row = c.upsert_drive(&DriveRow { generation: None, ..d.clone() }, "x").unwrap();
        assert_eq!(row.generation.as_deref(), Some("LTO-6"), "known generation kept");
        c.rename("drive", "lto6-1", "left").unwrap();
        assert!(c.drive("left").unwrap().is_some());

        let (v, new) = c.volume_seen("DC8001L8", Some("LTO-8"), "data", l.id, 0x1000).unwrap();
        assert!(new && v.state == "new");
        let (v2, new) = c.volume_seen("DC8001L8", None, "data", l.id, 0x1003).unwrap();
        assert!(!new);
        assert_eq!((v2.generation.as_deref(), v2.element), (Some("LTO-8"), Some(0x1003)));
        let gone = c.volumes_missing_from(l.id, &[]).unwrap();
        assert_eq!(gone, vec!["DC8001L8".to_string()]);
        assert_eq!(c.volume("DC8001L8").unwrap().unwrap().library_id, None);

        c.add_volume_stats(v.id, None, "load", &serde_json::json!({"load_count": 3})).unwrap();
        c.add_volume_stats(v.id, None, "unload", &serde_json::json!({"load_count": 3, "x": 1})).unwrap();
        let h = c.volume_stats(v.id, 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].event, "unload");
        assert_eq!(h[1].data["load_count"], serde_json::json!(3));
    }
}
