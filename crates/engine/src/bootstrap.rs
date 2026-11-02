//! The bootstrap file after the label (FORMAT.md §5): a plain ustar archive holding the
//! ltoarc binary, FORMAT.md and recovery notes, so a cartridge can be read with `mt` and
//! `tar` alone, decades from now, without anything installed.

use anyhow::{Context, Result};
use ltoarc_device::TapeDrive;

const FORMAT_MD: &str = include_str!("../../../docs/FORMAT.md");
/// A larger executable is a debug or test build; the archive then goes without it.
pub const MAX_BINARY: u64 = 32 << 20;

fn octal(field: &mut [u8], v: u64) {
    let s = format!("{v:0width$o}\0", width = field.len() - 1);
    field.copy_from_slice(s.as_bytes());
}

fn header(name: &str, size: u64, mode: u64, mtime: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], mode);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..271].copy_from_slice(b"ltoarc");
    h[297..303].copy_from_slice(b"ltoarc");
    // Checksum: header bytes summed with the checksum field read as spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|&b| b as u64).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

fn readme(block_size: u32, binary: bool) -> String {
    let b = block_size / 512;
    let tool = if binary {
        format!(
            "ltoarc        the program that wrote this cartridge ({} {}; check with `file ltoarc`:\n              a dynamically linked build needs a compatible C library)\n",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    } else {
        "(no ltoarc binary: the writing program was not a release build)\n".to_string()
    };
    format!(
        "This cartridge was written by ltoarc {ver}.\n\n\
         Tape file 0 is the volume label: one block, a 64-byte header followed by JSON.\n\
         Tape file 1 is this tar archive:\n\n\
         {tool}\
         FORMAT.md     how everything on the cartridge is laid out\n\
         README.txt    this file\n\n\
         To get it back:\n\n\
         \x20   mt -f /dev/nst0 rewind\n\
         \x20   mt -f /dev/nst0 fsf 1\n\
         \x20   tar -b {b} -xvf /dev/nst0\n\n\
         Then rebuild a catalog from the cartridge alone and restore from it:\n\n\
         \x20   ./ltoarc --catalog ./catalog.db tape register   (or: tape register --slot N --name NAME)\n\
         \x20   ./ltoarc --catalog ./catalog.db tape scan BARCODE\n\
         \x20   ./ltoarc --catalog ./catalog.db restore / --to ./restored\n\n\
         The newest volume catalog (a zstd-compressed SQLite database) is the last tape file\n\
         before end of data; FORMAT.md §7 and §8.2 say how to find it directly.\n",
        ver = env!("CARGO_PKG_VERSION"),
    )
}

/// The archive, padded to whole tape blocks of `block_size`. Also says whether the binary
/// is in it.
pub fn archive(block_size: u32) -> Result<(Vec<u8>, bool)> {
    let mtime = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let exe = std::env::current_exe().ok();
    let binary = exe
        .as_ref()
        .filter(|p| p.file_name().is_some_and(|n| n == "ltoarc"))
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() <= MAX_BINARY))
        .map(std::fs::read)
        .transpose()
        .context("reading the ltoarc binary for the bootstrap file")?;
    let readme = readme(block_size, binary.is_some());
    let mut files: Vec<(&str, &[u8], u64)> = Vec::new();
    if let Some(b) = &binary {
        files.push(("ltoarc", b, 0o755));
    }
    files.push(("FORMAT.md", FORMAT_MD.as_bytes(), 0o644));
    files.push(("README.txt", readme.as_bytes(), 0o644));
    let mut out = Vec::new();
    for (name, data, mode) in files {
        out.extend_from_slice(&header(name, data.len() as u64, mode, mtime));
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    // End-of-archive marker, then zeros up to a whole tape block.
    out.resize(out.len() + 1024, 0);
    out.resize(out.len().div_ceil(block_size as usize) * block_size as usize, 0);
    Ok((out, binary.is_some()))
}

/// Write the bootstrap file (blocks plus filemark) at the current position.
pub fn write(drive: &mut dyn TapeDrive, block_size: u32) -> Result<bool> {
    let (data, binary) = archive(block_size)?;
    for block in data.chunks(block_size as usize) {
        drive.write_block(block)?;
    }
    drive.write_filemarks(1)?;
    Ok(binary)
}

/// Whether a tape block starts a ustar archive.
pub fn is_tar(block: &[u8]) -> bool {
    block.get(257..262) == Some(b"ustar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_reads_back() {
        let (data, binary) = archive(64 << 10).unwrap();
        assert!(!binary, "test executables are not included");
        assert_eq!(data.len() % (64 << 10), 0);
        assert!(is_tar(&data));
        let dir = tempfile::tempdir().unwrap();
        let tar = dir.path().join("b.tar");
        std::fs::write(&tar, &data).unwrap();
        let st = std::process::Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(dir.path()).status();
        if let Ok(st) = st {
            assert!(st.success());
            assert_eq!(std::fs::read_to_string(dir.path().join("FORMAT.md")).unwrap(), FORMAT_MD);
            assert!(std::fs::read_to_string(dir.path().join("README.txt")).unwrap().contains("tar -b 128"));
        }
    }
}
