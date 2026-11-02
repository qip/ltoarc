# ltoarc on-tape format, version 1 (draft)

This document is the normative description of what `ltoarc` writes to tape and to cartridge
memory. It is also stored on every tape inside the bootstrap file. Someone with this document,
`dd`, and a small script should be able to get files back without `ltoarc`.

Status: draft. Sections marked *(phase N)* are specified but not implemented yet.

## 1. Conventions

- All integers are **little-endian** unless stated otherwise. SCSI and MAM values are big-endian,
  as SCSI defines them.
- `u8/u16/u32/u64` are unsigned integers of that width.
- A **UUID** is 16 raw bytes (RFC 9562), shown as the usual 36-character text form.
- **CRC32C** is the Castagnoli CRC: polynomial `0x1EDC6F41`, reflected, init `0xFFFFFFFF`, final
  xor `0xFFFFFFFF`. This is the variant used by iSCSI and ext4.
- **BLAKE3** is the 32-byte default-mode BLAKE3 hash.
- The tape is written in **variable-block mode**. Each SCSI WRITE carries exactly one tape block,
  and a tape block is one *logical object* in SCSI terms. Filemarks are also logical objects.
  A position on tape is `(partition, logical object number)`, as reported by READ POSITION and
  used by LOCATE.

## 2. Volume layout

A volume is one cartridge, written as a single partition. (There is no separate index
partition: cartridge memory points at the newest catalog, §8.2, which a LOCATE reaches in a
minute or two at worst.)

### 2.1 Single partition (default)

```
object 0        : LABEL block                      (§4)
object 1        : filemark
objects 2..     : BOOTSTRAP file (plain tar)        (§5)
                  filemark
                  SESSION 1: data blocks (§3, §6)
                  filemark
                  VOLUME CATALOG v1 (§7)
                  filemark
                  SESSION 2 ...
                  filemark
                  VOLUME CATALOG v2
                  filemark
                  ... end of data (EOD)
```

- Each session appends after the last catalog's trailing filemark, so the tape is only ever
  appended to.
- The newest volume catalog is always the last file before EOD. Each catalog covers every
  session on the volume, so the newest one is the only one needed.
- The MAM pointer (§8) gives the catalog's block address, so a reader can LOCATE to it directly
  without spacing filemarks from EOD.

## 3. Blocks

Every block that `ltoarc` writes, except the bootstrap tar, has this structure:

```
offset size field
0      6    magic                "LTOARC"
6      2    header_version       1
8      4    block_size           total bytes written to tape for this block (header + payload + padding)
12     4    payload_len          bytes of payload that are meaningful
16     16   volume_uuid
32     8    session_id           0 for LABEL blocks
40     8    block_seq            sequence number within the current stream (see below), from 0
48     4    first_record_offset  offset in payload of the first record header that starts in
                                 this block; 0xFFFFFFFF if no record starts here
52     2    kind                 1=LABEL 2=DATA 3=CATALOG
54     2    flags                reserved, 0
56     4    payload_crc32c       CRC32C of payload[0..payload_len]
60     4    header_crc32c        CRC32C of header bytes 0..60
64     ...  payload
```

- **Block size.** The default is 1 MiB (1048576 bytes), with 512 KiB as an alternative. It is
  recorded in the LABEL. All blocks of one volume have the same `block_size`, except the last block
  of a stream, which is written shorter: exactly `64 + payload_len` bytes, with no padding.
- **Streams.** A *stream* is a run of blocks between two filemarks: one session's data, or one
  catalog. `block_seq` restarts at 0 for each stream.
- **Resync.** A reader may land on any block after a LOCATE. It finds the next record using
  `first_record_offset`. A record that continues from the previous block occupies payload
  `[0..first_record_offset)`.

## 4. LABEL block

The first block of every partition `ltoarc` writes. Its payload is UTF-8 JSON, kept human-readable
on purpose:

```json
{
  "format": "ltoarc",
  "format_version": 1,
  "volume_uuid": "6f1c…",
  "barcode": "DC8001L8",
  "generation": "LTO-8",
  "block_size": 1048576,
  "partitions": 1,
  "pool": "dashcam",
  "created": "2026-10-30T13:05:00Z",
  "created_by": "ltoarc 0.1.0",
  "encryption": null
}
```

- The `generation` field records the media format. `LTO-7 M8` marks a Type M cartridge.
- `encryption` is `null`. Encryption is per session (§6.3), so the label never needs a key.
- Unknown JSON fields must be ignored by readers.
- `ltoarc dump-label` prints this block.
- `dd if=/dev/nst0 bs=1M count=1 | tail -c +65 | head -c 4096` shows it raw.

## 5. BOOTSTRAP file

Written when the cartridge is labelled: a plain POSIX ustar archive in tape blocks of the
volume's block size (`tar -b <block_size / 512>`, 2048 for 1 MiB blocks), zero-padded to a
whole block, holding:

- `ltoarc`: the program that labelled the cartridge (release builds up to 32 MiB only)
- `FORMAT.md`: this document
- `README.txt`: recovery instructions

Recovery without ltoarc installed:

```
mt -f /dev/nst0 rewind
mt -f /dev/nst0 fsf 1
tar -b 2048 -xvf /dev/nst0
```

Readers recognise the file by the `ustar` magic at byte 257 of its first block and skip it.
Cartridges labelled before the bootstrap file existed go straight from the label's filemark
to the first session.

## 6. Records *(phase 2)*

The payloads of the DATA blocks in a session stream are concatenated into one byte stream. That
stream is a sequence of records, and a record may span blocks.

```
offset size field
0      4    magic        "LREC"
4      2    type
6      2    flags        bit0: body compressed (zstd); bit1: body encrypted;
                         bit2: CONTINUED (FILE repeated on a continuation volume, §6.2)
8      8    body_len
16     4    seq          record sequence number in the session, wrapping
20     4    header_crc32c over bytes 0..20
24     ...  body (body_len bytes)
```

| type | name | body |
|---|---|---|
| 1 | SESSION_BEGIN | CBOR map: session id, job, level (`full`, `incremental`, `differential`, `append`), started, host, tool version, parent session (the one an incremental or differential session builds on) |
| 2 | FILE | CBOR map: id, path (bytes), kind, mode, uid, gid, user, group, size, mtime/atime/ctime (ns), dev, ino, nlink, rdev, target (symlink target, or first path of a hard link), xattrs (name, value byte pairs), data extents of sparse files, fingerprints known before reading, same_as (§6.4). Kind `whiteout`: the path was deleted since the parent session (§6.4) |
| 3 | CHUNK | 72-byte header (§6.1) followed by the stored bytes |
| 4 | PACK | 72-byte header as for CHUNK (object 0, seq = pack number, offset 0; raw_len, stored_len, codec and BLAKE3 describe the whole pack), u32 member table length, the member table (CBOR array of {object, offset, len, blake3} within the raw pack), then the stored bytes. Small files are packed whole. |
| 5 | STREAM | the same layout as CHUNK, for stream sources |
| 6 | FILE_END | CBOR: object id, size actually stored, status (`ok`, `changed`, `unreadable`, `truncated`), content hashes as (profile, algorithm, hex digest) |
| 7, 8 | | reserved (deletions and references are FILE records, §6.4) |
| 14 | VOLUME_END | CBOR: session id, volume_seq, running totals (files, bytes, stored), time. Last record on a volume when the session continues on the next one (§6.2) |
| 15 | SESSION_END | CBOR: counts, bytes, end time, status |
| 16 | PAD | ignored |

### 6.1 CHUNK header

```
offset size field
0      8    object      FILE record id within the session
8      4    seq         chunk number within the object, from 0
12     1    codec       0 = stored, 1 = zstd
13     1    flags       reserved, 0
14     2    reserved
16     8    offset      position of the data within the file
24     8    raw_len     bytes of data after decoding
32     8    stored_len  bytes that follow this header
40     32   blake3      BLAKE3 of the raw data
```

Sparse files: FILE carries the data extents; only extents are stored, holes read as zeros.
File content hashes cover the whole file, holes included.

- **Order.** A FILE record precedes its CHUNK records, so a linear scan can rebuild the catalog
  without one. This is `ltoarc scan`.
- **Codec.** 0 = stored, 1 = zstd. When the session is encrypted, the whole CHUNK body is
  sealed (§6.3).

### 6.2 Sessions spanning volumes

When a volume fills, its session stream ends with VOLUME_END instead of SESSION_END, followed
as usual by a filemark, that volume's catalog and a filemark. The session continues on the
next volume of the tape set with SESSION_BEGIN (same session id, `volume_seq` + 1, the new
volume's UUID). Each copy of a session spans a tape set of its own, so `volume_seq` counts the
volumes of that copy (1 for its first). If a large file was cut, its FILE record is repeated
there with the CONTINUED flag before its remaining CHUNK records; CHUNK offsets are always
within the whole file, so the parts on each volume combine directly. FILE_END follows on the
volume that holds the end of the file. A reader with only one volume knows every file whose
data it holds; content hashes of a cut file can be checked only with all its volumes.

A volume's stream is complete on its own: a scan of any volume, in any order, adds that
volume's part to the session, matching files by record id.

### 6.3 Encryption

Optional per session; a volume holds either plaintext sessions or sessions encrypted with one
key. The cipher is ChaCha20-Poly1305 (RFC 8439): 32-byte key, 12-byte nonce, 16-byte tag.

- **User key:** 32 bytes from a key file (64 hex digits; `#` lines are comments). Its public
  id is the first 8 bytes, in hex, of BLAKE3 `derive_key("ltoarc key id v1", key)`.
- **Data key:** each session draws a random 32-byte data key. SESSION_BEGIN (always in the
  clear) carries it wrapped: `encryption = {cipher: "chacha20-poly1305", key_id, wrapped}`,
  where `wrapped` = nonce (random) ‖ ciphertext of the data key ‖ tag, with associated data
  `"ltoarc data key"`. The same wrapped key is repeated on every volume of the session.
- **Records:** every other record body is sealed with the data key and has flag bit1 set:
  body = nonce ‖ ciphertext ‖ tag, where the nonce is a 4-byte big-endian prefix (0 for the
  records a session is written with; the copy number for a later copy sealed again from the
  source files) then a big-endian u64 counter starting at 0 (so no nonce repeats under one
  data key), and the
  associated data is the record type as u16 little-endian. The ciphertext holds the whole
  plain body (for CHUNK, the 72-byte header too), compressed first when bit0 is set. Block
  and record headers stay in the clear, so positioning, resync and scans work unchanged.
- **Volume catalog:** a CATALOG record of an encrypted volume has flag bit1 set and its body
  is: u16 LE length n, n bytes of CBOR wrapped key (as above, a fresh data key), then that data
  key's seal (counter 0, associated data `20 00`) of the zstd-compressed database.

### 6.4 Incremental sessions, deletions and references *(phase 5)*

An incremental session holds what changed since its parent session, a differential one what
changed since the job's last full session, and an append-only one only files the job never
archived. Entries not recorded were unchanged: a reader builds the state at a session by
taking, for every path, its newest entry among that session and those it builds on.

- **Deletions:** a FILE record of kind `whiteout` (only id and path are meaningful) says the
  path was deleted since the parent session. From that session on the path is absent, until
  a later session records it again. Append-only sessions record no deletions.
- **References:** a file whose data is already on tape (moved, renamed, or a duplicate) has
  `same_as = {session, id, algo, digest}` in its FILE record and no data records: its data is
  that of file `id` of session `session`, whose content digest under `algo` is `digest`.
  FILE_END follows as usual. A reader restores the data from that session's CHUNK or PACK
  records (on whichever volumes hold them) and checks it against the digest. The referenced
  file never is a reference itself. A scan must know the referenced session to resolve it:
  scanning volumes in session order does that.

## 7. Volume catalog *(phase 2)*

The catalog stream is DATA-framed (kind 3). Its record stream holds one record of type 32,
CATALOG, whose body is a zstd-compressed SQLite database (encrypted when the pool is). The
database holds this volume's sessions, objects, chunks and placements, plus the set manifest:
the list of volumes in each copy set. The schema version is stored in the database
(`PRAGMA user_version`).

## 8. Cartridge memory (MAM)

### 8.1 Host attributes

`ltoarc` writes these host-type MAM attributes. On IBM drives these are the only ones supported;
vendor-unique host attributes are not.

| id | name | value |
|---|---|---|
| 0800h | APPLICATION VENDOR | `LTOARC  ` |
| 0801h | APPLICATION NAME | `ltoarc` |
| 0802h | APPLICATION VERSION | the tool version |
| 0803h | USER MEDIUM TEXT LABEL | free text (pool + description), UTF-8 |
| 0804h | DATE AND TIME LAST WRITTEN | `YYYYMMDDHHMM` (UTC) |
| 0805h | TEXT LOCALIZATION IDENTIFIER | 81h (UTF-8) |
| 0806h | BARCODE | barcode |
| 0808h | MEDIA POOL | pool name |
| 080Bh | APPLICATION FORMAT VERSION | `ltoarc-1` |
| 080Ch | VOLUME COHERENCY INFORMATION | §8.2, one per partition *(phase 4)* |
| 0820h | MEDIUM GLOBALLY UNIQUE IDENTIFIER | the 36-character volume UUID text |

### 8.2 Volume Coherency Information (080Ch)

This follows SSC volume coherency (IBM LTO SCSI Reference §4.21, table 319). All integers are
big-endian.

```
u8   n = length of the volume change reference
n    VOLUME CHANGE REFERENCE      MAM 0009h as read when this record was written (4 bytes on LTO)
u64  VOLUME COHERENCY COUNT       number of volume catalogs written (0 after labelling)
u64  VOLUME COHERENCY SET ID      logical object of the first block of the newest catalog
                                  stream (0: none yet; the label)
u16  length of the application information (40)
     APPLICATION CLIENT SPECIFIC INFORMATION:
       "LTOARC" (6 bytes), u8 version = 1, volume UUID (16 bytes),
       u8 state (0 = clean, 1 = session open),
       u64 open session id (low 64 bits of its UUID, as in block headers), u64 its start object
```

- Labelling writes a clean record (count 0).
- A session marks the volume open before its first block, keeping the count and set id of
  the previous catalog.
- After the session's volume catalog and filemark are written, the volume is marked clean with
  the new count, the catalog's object and the then-current change reference.

A reader trusts the pointer only when the VOLUME CHANGE REFERENCE still matches MAM 0009h.
Otherwise the tape was written after the catalog, and the reader recovers (§9).

## 9. Crash recovery

A volume marked open (§8.2) was left by a session that never finished. Before anything else
is appended (`ltoarc tape recover`, or automatically before a backup):

1. If end of data is at or before the open session's start object, nothing reached the tape:
   the volume is marked clean again with the previous count and set id.
2. Otherwise locate to the start object and read records to end of data. Files with FILE_END
   are complete; files still open where the stream stops are marked `truncated` with the bytes
   that reached the tape. A record cut by the crash is ignored. The session is `partial`
   unless its SESSION_END made it.
3. If the object just before end of data is not a filemark, write one (this ends the cut
   session stream, or a cut catalog stream, which is then simply superseded).
4. Append a new volume catalog and filemark, and mark the volume clean.

A clean record whose change reference no longer matches MAM 0009h means something other than
ltoarc wrote to the tape after the newest catalog; ltoarc warns and appends after it.

## 10. Versioning

- `header_version` (blocks), `format_version` (label) and the record `type` space evolve
  compatibly.
- New record types and new CBOR keys may be added. Readers skip record types they do not
  recognise, using `body_len`.
- An incompatible change bumps `format_version`. Readers refuse a newer major version unless
  forced.
