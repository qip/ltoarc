//! Software encryption (FORMAT.md §6.3): ChaCha20-Poly1305 (RFC 8439).
//!
//! A key file holds the user's key (the key-encryption key). Each session gets a fresh random
//! data key, wrapped with the user's key and stored in SESSION_BEGIN and the local catalog;
//! every record body after SESSION_BEGIN is sealed with the data key, with a counter nonce
//! (unique because the data key is never reused). Volume catalogs get a data key of their own.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ltoarc_format::record::WrappedKey;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde_bytes::ByteBuf;

pub const CIPHER: &str = "chacha20-poly1305";
pub const OVERHEAD: usize = NONCE_LEN + 16;

/// A 256-bit key: the user's (from a key file) or a data key.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 32]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.id())
    }
}

impl Key {
    pub fn generate() -> Key {
        let mut k = [0u8; 32];
        SystemRandom::new().fill(&mut k).expect("system random generator");
        Key(k)
    }

    /// Short public name of the key: it tells which key file a session needs, and reveals
    /// nothing about the key.
    pub fn id(&self) -> String {
        hex::encode(&blake3::derive_key("ltoarc key id v1", &self.0)[..8])
    }

    /// Read a key file: 64 hex digits (whitespace and `#` comment lines ignored).
    pub fn load(path: &Path) -> Result<Key> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading key file {}", path.display()))?;
        let hex_digits: String = text.lines().filter(|l| !l.trim_start().starts_with('#')).flat_map(|l| l.split_whitespace()).collect();
        let bytes = hex::decode(&hex_digits).map_err(|_| anyhow!("{}: not a key file (expected 64 hex digits)", path.display()))?;
        let k: [u8; 32] = bytes.try_into().map_err(|_| anyhow!("{}: a key is 64 hex digits", path.display()))?;
        Ok(Key(k))
    }

    /// Write a new key file readable only by its owner; refuses to overwrite.
    pub fn save_new(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating key file {}", path.display()))?;
        writeln!(f, "# ltoarc key {}: keep a copy somewhere safe; without it encrypted cartridges cannot be read", self.id())?;
        writeln!(f, "{}", hex::encode(self.0))?;
        Ok(())
    }

    fn aead(&self) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &self.0).expect("32-byte key"))
    }
}

fn seal_with(key: &LessSafeKey, nonce: [u8; NONCE_LEN], aad: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum::<usize>() + OVERHEAD);
    out.extend_from_slice(&nonce);
    for p in parts {
        out.extend_from_slice(p);
    }
    let tag = key
        .seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut out[NONCE_LEN..])
        .expect("sealing in memory cannot fail");
    out.extend_from_slice(tag.as_ref());
    out
}

/// Open `nonce || ciphertext || tag` sealed with `key` and `aad`.
pub fn open(key: &Key, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < OVERHEAD {
        bail!("encrypted body of {} bytes is too short", sealed.len());
    }
    let nonce: [u8; NONCE_LEN] = sealed[..NONCE_LEN].try_into().unwrap();
    let mut buf = sealed[NONCE_LEN..].to_vec();
    let n = key
        .aead()
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buf)
        .map_err(|_| anyhow!("decryption failed: wrong key or damaged data"))?
        .len();
    buf.truncate(n);
    Ok(buf)
}

/// Wrap a data key with the user's key (random nonce: a key wraps few data keys).
pub fn wrap(kek: &Key, dek: &Key) -> WrappedKey {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).expect("system random generator");
    WrappedKey { cipher: CIPHER.into(), key_id: kek.id(), wrapped: ByteBuf::from(seal_with(&kek.aead(), nonce, b"ltoarc data key", &[&dek.0])) }
}

/// The data key inside `w`, using whichever of `keys` it was wrapped with.
pub fn unwrap(keys: &[Key], w: &WrappedKey) -> Result<Key> {
    if w.cipher != CIPHER {
        bail!("unknown cipher {}", w.cipher);
    }
    let kek = keys
        .iter()
        .find(|k| k.id() == w.key_id)
        .ok_or_else(|| anyhow!("encrypted with key {}, which was not given (--key FILE)", w.key_id))?;
    let raw = open(kek, b"ltoarc data key", &w.wrapped)?;
    Ok(Key(raw.try_into().map_err(|_| anyhow!("wrapped data key has the wrong length"))?))
}

/// Seals the records of one session (or one catalog) with its data key.
pub struct Sealer {
    key: LessSafeKey,
    /// First 4 nonce bytes: 0 for the records a session is written with, the copy number for
    /// a later copy sealed afresh (so no nonce repeats under the session's key).
    prefix: u32,
    counter: u64,
}

impl Sealer {
    pub fn new(dek: &Key) -> Sealer {
        Sealer { key: dek.aead(), prefix: 0, counter: 0 }
    }

    /// For records of copy `copy` of a session sealed again from the source.
    pub fn for_copy(dek: &Key, copy: u8) -> Sealer {
        Sealer { key: dek.aead(), prefix: copy as u32, counter: 0 }
    }

    pub fn seal(&mut self, aad: &[u8], data: &[u8]) -> Vec<u8> {
        self.seal_parts(aad, &[data])
    }

    /// Seal the concatenation of `parts`.
    pub fn seal_parts(&mut self, aad: &[u8], parts: &[&[u8]]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        nonce[..4].copy_from_slice(&self.prefix.to_be_bytes());
        nonce[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        seal_with(&self.key, nonce, aad, parts)
    }
}

/// A record's body in the clear: opened with the session's data key when it is encrypted.
pub fn record_body<'a>(dek: Option<&Key>, rtype: u16, flags: u16, body: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>> {
    if flags & ltoarc_format::record::flags::ENCRYPTED == 0 {
        return Ok(std::borrow::Cow::Borrowed(body));
    }
    let dek = dek.ok_or_else(|| anyhow!("record is encrypted but its session has no key"))?;
    Ok(std::borrow::Cow::Owned(open(dek, &record_aad(rtype), body)?))
}

/// Associated data of a record body: its type, so bodies cannot be swapped between kinds.
pub fn record_aad(rtype: u16) -> [u8; 2] {
    rtype.to_le_bytes()
}

/// JSON for the local catalog (sessions.encryption).
pub fn to_json(w: &WrappedKey) -> String {
    serde_json::json!({"cipher": w.cipher, "key_id": w.key_id, "wrapped": hex::encode(&w.wrapped)}).to_string()
}

pub fn from_json(s: &str) -> Result<WrappedKey> {
    let v: serde_json::Value = serde_json::from_str(s)?;
    let field = |k: &str| v[k].as_str().map(str::to_string).ok_or_else(|| anyhow!("session encryption record lacks {k}"));
    Ok(WrappedKey { cipher: field("cipher")?, key_id: field("key_id")?, wrapped: ByteBuf::from(hex::decode(field("wrapped")?)?) })
}

/// Encrypted CATALOG body: u16 length of the CBOR wrapped key, the wrapped key, the sealed
/// compressed database.
pub fn seal_catalog(kek: &Key, data: &[u8]) -> Vec<u8> {
    let dek = Key::generate();
    let w = ltoarc_format::record::to_cbor(&wrap(kek, &dek));
    let mut out = (w.len() as u16).to_le_bytes().to_vec();
    out.extend_from_slice(&w);
    out.extend_from_slice(&Sealer::new(&dek).seal(&record_aad(ltoarc_format::record::rtype::CATALOG), data));
    out
}

pub fn open_catalog(keys: &[Key], body: &[u8]) -> Result<Vec<u8>> {
    let n = u16::from_le_bytes([*body.first().unwrap_or(&0), *body.get(1).unwrap_or(&0)]) as usize;
    let w: WrappedKey = ltoarc_format::record::from_cbor(body.get(2..2 + n).ok_or_else(|| anyhow!("encrypted catalog is truncated"))?)?;
    let dek = unwrap(keys, &w)?;
    open(&dek, &record_aad(ltoarc_format::record::rtype::CATALOG), &body[2 + n..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_seal_open() {
        let kek = Key::generate();
        let dek = Key::generate();
        let w = wrap(&kek, &dek);
        assert_eq!(unwrap(std::slice::from_ref(&kek), &w).unwrap(), dek);
        assert!(unwrap(&[Key::generate()], &w).is_err());
        assert_eq!(from_json(&to_json(&w)).unwrap(), w);
        let mut s = Sealer::new(&dek);
        let a = s.seal(&record_aad(3), b"chunk data");
        let b = s.seal(&record_aad(3), b"chunk data");
        assert_ne!(a, b, "nonces differ");
        assert_eq!(open(&dek, &record_aad(3), &a).unwrap(), b"chunk data");
        assert!(open(&dek, &record_aad(2), &a).is_err(), "type is authenticated");
        let mut bad = a.clone();
        bad[20] ^= 1;
        assert!(open(&dek, &record_aad(3), &bad).is_err());
        let c = seal_catalog(&kek, b"SQLite format 3\0...");
        assert_eq!(open_catalog(&[kek], &c).unwrap(), b"SQLite format 3\0...");
    }

    #[test]
    fn key_files() {
        let d = tempfile::tempdir().unwrap();
        let k = Key::generate();
        k.save_new(&d.path().join("k")).unwrap();
        assert_eq!(Key::load(&d.path().join("k")).unwrap(), k);
        assert!(k.save_new(&d.path().join("k")).is_err(), "never overwrites");
        assert_eq!(k.id().len(), 16);
    }
}
