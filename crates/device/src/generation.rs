//! LTO generations, barcode media identifiers, and the drive/media compatibility table.

use std::fmt;
use std::str::FromStr;

use ltoarc_scsi::density;
use serde::{Deserialize, Serialize};

/// Media format generation. `M8` is an LTO-7 cartridge initialized as Type M by an LTO-8 drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum Generation {
    Lto3,
    Lto4,
    Lto5,
    Lto6,
    Lto7,
    M8,
    Lto8,
    Lto9,
}

use Generation::*;

impl Generation {
    pub const ALL: [Generation; 8] = [Lto3, Lto4, Lto5, Lto6, Lto7, M8, Lto8, Lto9];

    pub fn name(self) -> &'static str {
        match self {
            Lto3 => "LTO-3",
            Lto4 => "LTO-4",
            Lto5 => "LTO-5",
            Lto6 => "LTO-6",
            Lto7 => "LTO-7",
            M8 => "LTO-7 M8",
            Lto8 => "LTO-8",
            Lto9 => "LTO-9",
        }
    }

    /// Two-character media identifier used on barcode labels.
    pub fn suffix(self) -> &'static str {
        match self {
            Lto3 => "L3",
            Lto4 => "L4",
            Lto5 => "L5",
            Lto6 => "L6",
            Lto7 => "L7",
            M8 => "M8",
            Lto8 => "L8",
            Lto9 => "L9",
        }
    }

    pub fn density(self) -> u8 {
        match self {
            Lto3 => density::LTO3,
            Lto4 => density::LTO4,
            Lto5 => density::LTO5,
            Lto6 => density::LTO6,
            Lto7 => density::LTO7,
            M8 => density::LTO7_M8,
            Lto8 => density::LTO8,
            Lto9 => density::LTO9,
        }
    }

    pub fn from_density(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.density() == code)
    }

    /// Native (uncompressed) capacity in bytes.
    pub fn native_capacity(self) -> u64 {
        const GB: u64 = 1_000_000_000;
        match self {
            Lto3 => 400 * GB,
            Lto4 => 800 * GB,
            Lto5 => 1500 * GB,
            Lto6 => 2500 * GB,
            Lto7 => 6000 * GB,
            M8 => 9000 * GB,
            Lto8 => 12000 * GB,
            Lto9 => 18000 * GB,
        }
    }

    /// The physical cartridge generation (M8 media is an LTO-7 cartridge).
    pub fn cartridge(self) -> Generation {
        if self == M8 { Lto7 } else { self }
    }

    /// Generation of a drive from its INQUIRY product id, e.g. "ULT3580-HH8", "Ultrium 6-SCSI".
    pub fn from_drive_product(product: &str) -> Option<Self> {
        let digit = product.chars().rev().find(|c| c.is_ascii_digit())?;
        Some(match digit {
            '3' => Lto3,
            '4' => Lto4,
            '5' => Lto5,
            '6' => Lto6,
            '7' => Lto7,
            '8' => Lto8,
            '9' => Lto9,
            _ => return None,
        })
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Generation {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let norm: String = s.to_ascii_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let g = match norm.as_str() {
            "LTO3" | "L3" | "3" => Lto3,
            "LTO4" | "L4" | "4" => Lto4,
            "LTO5" | "L5" | "5" => Lto5,
            "LTO6" | "L6" | "6" => Lto6,
            "LTO7" | "L7" | "7" => Lto7,
            "LTO7M8" | "M8" => M8,
            "LTO8" | "L8" | "8" => Lto8,
            "LTO9" | "L9" | "9" => Lto9,
            _ => return Err(format!("unknown LTO generation '{s}'")),
        };
        Ok(g)
    }
}

impl From<Generation> for String {
    fn from(g: Generation) -> String {
        g.name().to_string()
    }
}

impl TryFrom<String> for Generation {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Data,
    Worm,
    Cleaning,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Data => "data",
            MediaKind::Worm => "worm",
            MediaKind::Cleaning => "cleaning",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarcodeInfo {
    pub barcode: String,
    pub generation: Option<Generation>,
    pub kind: MediaKind,
}

/// Interpret an LTO barcode label. Labels are normally 6 characters of volume serial plus a
/// 2-character media identifier ("DC8001L8"); libraries may be set to report only the first 6,
/// in which case the generation is unknown until the cartridge is loaded.
pub fn parse_barcode(barcode: &str) -> BarcodeInfo {
    let b = barcode.trim().to_ascii_uppercase();
    let kind_gen = if b.starts_with("CLN") {
        (MediaKind::Cleaning, None)
    } else if b.len() >= 8 {
        match &b[b.len() - 2..] {
            "L3" => (MediaKind::Data, Some(Lto3)),
            "L4" => (MediaKind::Data, Some(Lto4)),
            "L5" => (MediaKind::Data, Some(Lto5)),
            "L6" => (MediaKind::Data, Some(Lto6)),
            "L7" => (MediaKind::Data, Some(Lto7)),
            "M8" => (MediaKind::Data, Some(M8)),
            "L8" => (MediaKind::Data, Some(Lto8)),
            "L9" => (MediaKind::Data, Some(Lto9)),
            "LT" => (MediaKind::Worm, Some(Lto3)),
            "LU" => (MediaKind::Worm, Some(Lto4)),
            "LV" => (MediaKind::Worm, Some(Lto5)),
            "LW" => (MediaKind::Worm, Some(Lto6)),
            "LX" => (MediaKind::Worm, Some(Lto7)),
            "LY" => (MediaKind::Worm, Some(Lto8)),
            "LZ" => (MediaKind::Worm, Some(Lto9)),
            "CU" => (MediaKind::Cleaning, None),
            _ => (MediaKind::Data, None),
        }
    } else {
        (MediaKind::Data, None)
    };
    BarcodeInfo { barcode: barcode.trim().to_string(), generation: kind_gen.1, kind: kind_gen.0 }
}

/// Expand "DC8001L8..DC8005L8" into the five labels in between (inclusive). The counted
/// number is the run of digits where the two ends differ (so the media suffix "L8" is left
/// alone); everything else must match. A single label without ".." expands to itself.
pub fn expand_barcode_range(spec: &str) -> Result<Vec<String>, String> {
    let Some((a, b)) = spec.split_once("..") else { return Ok(vec![spec.trim().to_string()]) };
    let (a, b) = (a.trim().as_bytes(), b.trim().as_bytes());
    let text = |x: &[u8]| String::from_utf8_lossy(x).to_string();
    if a.len() != b.len() || !a.is_ascii() || !b.is_ascii() {
        return Err(format!("range ends '{}' and '{}' must have the same length", text(a), text(b)));
    }
    if a == b {
        return Ok(vec![text(a)]);
    }
    let first = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(0);
    let last = a.iter().zip(b).rposition(|(x, y)| x != y).unwrap_or(0);
    let mut lo = first;
    while lo > 0 && a[lo - 1].is_ascii_digit() {
        lo -= 1;
    }
    let mut hi = last + 1;
    while hi < a.len() && a[hi].is_ascii_digit() {
        hi += 1;
    }
    let (na, nb) = (&a[lo..hi], &b[lo..hi]);
    if !na.iter().chain(nb).all(u8::is_ascii_digit) {
        return Err(format!("range ends '{}' and '{}' differ outside a number", text(a), text(b)));
    }
    let (x, y): (u64, u64) = (text(na).parse().map_err(|_| "bad number")?, text(nb).parse().map_err(|_| "bad number")?);
    if y < x || y - x > 100_000 {
        return Err(format!("range {}..{} is empty or too large", text(a), text(b)));
    }
    let (prefix, suffix, width) = (text(&a[..lo]), text(&a[hi..]), hi - lo);
    Ok((x..=y).map(|n| format!("{prefix}{n:0width$}{suffix}")).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
    None,
}

/// (drive generation, media it reads and writes, media it only reads).
const COMPAT: &[(Generation, &[Generation], &[Generation])] = &[
    (Lto3, &[Lto3], &[]),
    (Lto4, &[Lto3, Lto4], &[]),
    (Lto5, &[Lto4, Lto5], &[Lto3]),
    (Lto6, &[Lto5, Lto6], &[Lto4]),
    (Lto7, &[Lto6, Lto7], &[Lto5]),
    (Lto8, &[Lto7, M8, Lto8], &[]),
    (Lto9, &[Lto8, Lto9], &[]),
];

pub fn drive_access(drive: Generation, media: Generation) -> Access {
    match COMPAT.iter().find(|(d, _, _)| *d == drive) {
        Some((_, rw, _)) if rw.contains(&media) => Access::ReadWrite,
        Some((_, _, ro)) if ro.contains(&media) => Access::ReadOnly,
        _ => Access::None,
    }
}

/// Media generations a drive can read (read-write first).
pub fn drive_media(drive: Generation) -> (Vec<Generation>, Vec<Generation>) {
    COMPAT
        .iter()
        .find(|(d, _, _)| *d == drive)
        .map(|(_, rw, ro)| (rw.to_vec(), ro.to_vec()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat() {
        assert_eq!(drive_access(Lto8, M8), Access::ReadWrite);
        assert_eq!(drive_access(Lto9, M8), Access::None);
        assert_eq!(drive_access(Lto7, M8), Access::None);
        assert_eq!(drive_access(Lto6, Lto5), Access::ReadWrite);
        assert_eq!(drive_access(Lto6, Lto4), Access::ReadOnly);
        assert_eq!(drive_access(Lto8, Lto6), Access::None);
    }

    #[test]
    fn barcodes() {
        assert_eq!(parse_barcode("DC8001L8").generation, Some(Lto8));
        assert_eq!(parse_barcode("DC7001M8").generation, Some(M8));
        assert_eq!(parse_barcode("AB0001LV").kind, MediaKind::Worm);
        assert_eq!(parse_barcode("CLNU01").kind, MediaKind::Cleaning);
        assert_eq!(parse_barcode("T0481S").generation, None);
    }

    #[test]
    fn ranges() {
        assert_eq!(expand_barcode_range("DC8001L8..DC8003L8").unwrap(), ["DC8001L8", "DC8002L8", "DC8003L8"]);
        assert_eq!(expand_barcode_range("A0098L5..A0101L5").unwrap().len(), 4);
        assert_eq!(expand_barcode_range("A0101L5").unwrap(), ["A0101L5"]);
        assert!(expand_barcode_range("A001L5..B003L5").is_err());
    }

    #[test]
    fn product_and_names() {
        assert_eq!(Generation::from_drive_product("ULT3580-HH8"), Some(Lto8));
        assert_eq!(Generation::from_drive_product("Ultrium 6-SCSI"), Some(Lto6));
        assert_eq!("lto-7 m8".parse::<Generation>().unwrap(), M8);
        assert_eq!(Generation::from_density(0x5d), Some(M8));
        let json = serde_json::to_string(&M8).unwrap();
        assert_eq!(serde_json::from_str::<Generation>(&json).unwrap(), M8);
    }
}
