CREATE TABLE paths (
        id INTEGER PRIMARY KEY,
        parent INTEGER REFERENCES paths(id),
        name BLOB NOT NULL
    );
    CREATE UNIQUE INDEX paths_by_parent ON paths(parent, name);
    INSERT INTO paths (id, parent, name) VALUES (1, NULL, x'');
    CREATE TABLE sessions (
        id INTEGER PRIMARY KEY,
        uuid TEXT NOT NULL UNIQUE,
        job TEXT,
        level TEXT NOT NULL,
        parent_id INTEGER REFERENCES sessions(id),
        started INTEGER NOT NULL,
        ended INTEGER,
        status TEXT NOT NULL,
        host TEXT NOT NULL,
        sources TEXT NOT NULL,
        files INTEGER NOT NULL DEFAULT 0,
        bytes INTEGER NOT NULL DEFAULT 0,
        stored_bytes INTEGER NOT NULL DEFAULT 0,
        stats TEXT
    );
    CREATE TABLE session_volumes (
        session_id INTEGER NOT NULL REFERENCES sessions(id),
        copy INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        volume_id INTEGER NOT NULL REFERENCES volumes(id),
        partition INTEGER NOT NULL DEFAULT 0,
        start_object INTEGER NOT NULL,
        end_object INTEGER,
        catalog_object INTEGER,
        PRIMARY KEY (session_id, copy, seq)
    );
    CREATE TABLE objects (
        id INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES sessions(id),
        path_id INTEGER NOT NULL REFERENCES paths(id),
        seq INTEGER NOT NULL,
        kind TEXT NOT NULL,
        mode INTEGER NOT NULL,
        uid INTEGER NOT NULL,
        gid INTEGER NOT NULL,
        user TEXT,
        grp TEXT,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL,
        atime_ns INTEGER NOT NULL,
        ctime_ns INTEGER NOT NULL,
        dev INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        nlink INTEGER NOT NULL,
        rdev INTEGER NOT NULL,
        target BLOB,
        xattrs BLOB,
        extents BLOB,
        status TEXT NOT NULL
    );
    CREATE INDEX objects_by_path ON objects(path_id, session_id);
    CREATE INDEX objects_by_session ON objects(session_id, seq);
    CREATE TABLE object_hashes (
        object_id INTEGER NOT NULL REFERENCES objects(id),
        profile TEXT NOT NULL,
        kind TEXT NOT NULL,
        algo TEXT NOT NULL,
        digest TEXT NOT NULL,
        PRIMARY KEY (object_id, profile)
    );
    CREATE INDEX hashes_by_digest ON object_hashes(algo, digest);
    CREATE TABLE chunks (
        id INTEGER PRIMARY KEY,
        object_id INTEGER NOT NULL REFERENCES objects(id),
        seq INTEGER NOT NULL,
        offset INTEGER NOT NULL,
        raw_len INTEGER NOT NULL,
        stored_len INTEGER NOT NULL,
        codec INTEGER NOT NULL,
        blake3 BLOB NOT NULL,
        UNIQUE (object_id, seq)
    );
    CREATE TABLE placements (
        chunk_id INTEGER NOT NULL REFERENCES chunks(id),
        copy INTEGER NOT NULL,
        volume_id INTEGER NOT NULL REFERENCES volumes(id),
        partition INTEGER NOT NULL,
        block INTEGER NOT NULL,
        offset INTEGER NOT NULL,
        PRIMARY KEY (chunk_id, copy)
    );
    CREATE INDEX placements_by_volume ON placements(volume_id, partition, block, offset);
