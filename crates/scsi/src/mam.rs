//! Medium auxiliary memory (cartridge memory): READ ATTRIBUTE (8Ch) / WRITE ATTRIBUTE (8Dh).

use crate::{ascii_trim, be16, be32, be_uint, need, timeouts, DataDir, Result, Transport};

pub mod id {
    pub const REMAINING_CAPACITY: u16 = 0x0000;
    pub const MAXIMUM_CAPACITY: u16 = 0x0001;
    pub const TAPEALERT_FLAGS: u16 = 0x0002;
    pub const LOAD_COUNT: u16 = 0x0003;
    pub const MAM_SPACE_REMAINING: u16 = 0x0004;
    pub const FORMAT_DENSITY_CODE: u16 = 0x0006;
    pub const INITIALIZATION_COUNT: u16 = 0x0007;
    pub const VOLUME_CHANGE_REFERENCE: u16 = 0x0009;
    pub const DEVICE_AT_LAST_LOAD: u16 = 0x020a;
    pub const DEVICE_AT_LOAD_1: u16 = 0x020b;
    pub const DEVICE_AT_LOAD_2: u16 = 0x020c;
    pub const DEVICE_AT_LOAD_3: u16 = 0x020d;
    pub const TOTAL_MB_WRITTEN: u16 = 0x0220;
    pub const TOTAL_MB_READ: u16 = 0x0221;
    pub const LAST_LOAD_MB_WRITTEN: u16 = 0x0222;
    pub const LAST_LOAD_MB_READ: u16 = 0x0223;
    pub const MEDIUM_MANUFACTURER: u16 = 0x0400;
    pub const MEDIUM_SERIAL: u16 = 0x0401;
    pub const MEDIUM_LENGTH: u16 = 0x0402;
    pub const MEDIUM_WIDTH: u16 = 0x0403;
    pub const MEDIUM_DENSITY_CODE: u16 = 0x0405;
    pub const MEDIUM_MANUFACTURE_DATE: u16 = 0x0406;
    pub const MAM_CAPACITY: u16 = 0x0407;
    pub const MEDIUM_TYPE: u16 = 0x0408;
    pub const MEDIUM_TYPE_INFORMATION: u16 = 0x0409;
    pub const APPLICATION_VENDOR: u16 = 0x0800;
    pub const APPLICATION_NAME: u16 = 0x0801;
    pub const APPLICATION_VERSION: u16 = 0x0802;
    pub const USER_MEDIUM_TEXT_LABEL: u16 = 0x0803;
    pub const DATE_TIME_LAST_WRITTEN: u16 = 0x0804;
    pub const TEXT_LOCALIZATION_ID: u16 = 0x0805;
    pub const BARCODE: u16 = 0x0806;
    pub const OWNING_HOST: u16 = 0x0807;
    pub const MEDIA_POOL: u16 = 0x0808;
    pub const APPLICATION_FORMAT_VERSION: u16 = 0x080b;
    pub const VOLUME_COHERENCY_INFO: u16 = 0x080c;
    pub const MEDIUM_GUID: u16 = 0x0820;
    pub const MEDIA_POOL_GUID: u16 = 0x0821;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Binary,
    Ascii,
    Text,
    Reserved,
}

impl Format {
    fn from_bits(b: u8) -> Self {
        match b & 0x03 {
            0 => Format::Binary,
            1 => Format::Ascii,
            2 => Format::Text,
            _ => Format::Reserved,
        }
    }

    fn bits(self) -> u8 {
        match self {
            Format::Binary => 0,
            Format::Ascii => 1,
            Format::Text => 2,
            Format::Reserved => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub id: u16,
    pub read_only: bool,
    pub format: Format,
    pub value: Vec<u8>,
}

impl Attribute {
    pub fn binary(id: u16, value: Vec<u8>) -> Self {
        Attribute { id, read_only: false, format: Format::Binary, value }
    }

    /// Left-aligned ASCII padded with spaces to `len` (truncated if longer).
    pub fn ascii(id: u16, s: &str, len: usize) -> Self {
        let mut v: Vec<u8> = s.bytes().filter(|b| (0x20..0x7f).contains(b)).take(len).collect();
        v.resize(len, b' ');
        Attribute { id, read_only: false, format: Format::Ascii, value: v }
    }

    /// TEXT (UTF-8 when TEXT LOCALIZATION IDENTIFIER is 81h) padded with NULs to `len`.
    pub fn text(id: u16, s: &str, len: usize) -> Self {
        let mut end = s.len().min(len);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut v = s.as_bytes()[..end].to_vec();
        v.resize(len, 0);
        Attribute { id, read_only: false, format: Format::Text, value: v }
    }

    pub fn as_u64(&self) -> Option<u64> {
        (self.format == Format::Binary).then(|| be_uint(&self.value)).flatten()
    }

    pub fn as_string(&self) -> String {
        match self.format {
            Format::Text => String::from_utf8_lossy(&self.value)
                .trim_end_matches(['\0', ' '])
                .to_string(),
            _ => ascii_trim(&self.value),
        }
    }
}

pub fn read_attributes(t: &dyn Transport, partition: u8, first_id: u16) -> Result<Vec<Attribute>> {
    let mut buf = vec![0u8; 0x10000];
    let mut cdb = [0u8; 16];
    cdb[0] = 0x8c;
    cdb[1] = 0x00; // ATTRIBUTE VALUES
    cdb[7] = partition;
    cdb[8..10].copy_from_slice(&first_id.to_be_bytes());
    cdb[10..14].copy_from_slice(&(buf.len() as u32).to_be_bytes());
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_attributes(&buf[..c.transferred])
}

pub fn write_attributes(t: &dyn Transport, partition: u8, attrs: &[Attribute]) -> Result<()> {
    let data = encode_attributes(attrs);
    let mut cdb = [0u8; 16];
    cdb[0] = 0x8d;
    cdb[1] = 0x01; // WTC: write through the cache to the cartridge
    cdb[7] = partition;
    cdb[10..14].copy_from_slice(&(data.len() as u32).to_be_bytes());
    t.exec(&cdb, DataDir::Out(&data), timeouts::SHORT)?;
    Ok(())
}

pub fn parse_attributes(buf: &[u8]) -> Result<Vec<Attribute>> {
    need(buf, 4, "READ ATTRIBUTE")?;
    let end = (4 + be32(&buf[0..4]) as usize).min(buf.len());
    let mut out = Vec::new();
    let mut off = 4;
    while off + 5 <= end {
        let id = be16(&buf[off..off + 2]);
        let flags = buf[off + 2];
        let len = be16(&buf[off + 3..off + 5]) as usize;
        let vend = (off + 5 + len).min(end);
        out.push(Attribute {
            id,
            read_only: flags & 0x80 != 0,
            format: Format::from_bits(flags),
            value: buf[off + 5..vend].to_vec(),
        });
        off += 5 + len;
    }
    Ok(out)
}

pub fn encode_attributes(attrs: &[Attribute]) -> Vec<u8> {
    let mut body = Vec::new();
    for a in attrs {
        body.extend_from_slice(&a.id.to_be_bytes());
        body.push(((a.read_only as u8) << 7) | a.format.bits());
        body.extend_from_slice(&(a.value.len() as u16).to_be_bytes());
        body.extend_from_slice(&a.value);
    }
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

pub fn attr_name(id: u16) -> Option<&'static str> {
    use id::*;
    Some(match id {
        REMAINING_CAPACITY => "Remaining capacity in partition (MiB)",
        MAXIMUM_CAPACITY => "Maximum capacity in partition (MiB)",
        TAPEALERT_FLAGS => "TapeAlert flags",
        LOAD_COUNT => "Load count",
        MAM_SPACE_REMAINING => "MAM space remaining",
        0x0005 => "Assigning organization",
        FORMAT_DENSITY_CODE => "Formatted density code",
        INITIALIZATION_COUNT => "Initialization count",
        0x0008 => "Volume identifier",
        VOLUME_CHANGE_REFERENCE => "Volume change reference",
        DEVICE_AT_LAST_LOAD => "Device at last load",
        DEVICE_AT_LOAD_1 => "Device at load -1",
        DEVICE_AT_LOAD_2 => "Device at load -2",
        DEVICE_AT_LOAD_3 => "Device at load -3",
        TOTAL_MB_WRITTEN => "Total MiB written (lifetime)",
        TOTAL_MB_READ => "Total MiB read (lifetime)",
        LAST_LOAD_MB_WRITTEN => "MiB written (current/last load)",
        LAST_LOAD_MB_READ => "MiB read (current/last load)",
        0x0224 => "First encrypted block",
        0x0225 => "First unencrypted block after encrypted",
        0x0340 => "Medium usage history",
        0x0341 => "Partition usage history",
        MEDIUM_MANUFACTURER => "Medium manufacturer",
        MEDIUM_SERIAL => "Medium serial number",
        MEDIUM_LENGTH => "Medium length (m)",
        MEDIUM_WIDTH => "Medium width (0.1 mm)",
        0x0404 => "Assigning organization",
        MEDIUM_DENSITY_CODE => "Medium density code",
        MEDIUM_MANUFACTURE_DATE => "Medium manufacture date",
        MAM_CAPACITY => "MAM capacity",
        MEDIUM_TYPE => "Medium type",
        MEDIUM_TYPE_INFORMATION => "Medium type information",
        0x040a => "Numeric medium serial number",
        0x040b => "Supported density codes",
        APPLICATION_VENDOR => "Application vendor",
        APPLICATION_NAME => "Application name",
        APPLICATION_VERSION => "Application version",
        USER_MEDIUM_TEXT_LABEL => "User medium text label",
        DATE_TIME_LAST_WRITTEN => "Date and time last written",
        TEXT_LOCALIZATION_ID => "Text localization identifier",
        BARCODE => "Barcode",
        OWNING_HOST => "Owning host",
        MEDIA_POOL => "Media pool",
        APPLICATION_FORMAT_VERSION => "Application format version",
        VOLUME_COHERENCY_INFO => "Volume coherency information",
        MEDIUM_GUID => "Medium globally unique identifier",
        MEDIA_POOL_GUID => "Media pool globally unique identifier",
        _ => return None,
    })
}

/// Parse a "DEVICE VENDOR/SERIAL NUMBER AT LAST LOAD" value (8-byte vendor + 32-byte serial).
pub fn parse_device_at_load(v: &[u8]) -> Option<(String, String)> {
    if v.len() < 8 || v.iter().all(|&b| b == 0 || b == b' ') {
        return None;
    }
    Some((ascii_trim(&v[..8]), ascii_trim(&v[8..])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let attrs = vec![
            Attribute::binary(id::LOAD_COUNT, 42u64.to_be_bytes().to_vec()),
            Attribute::ascii(id::BARCODE, "DC8001L8", 32),
            Attribute::text(id::MEDIA_POOL, "dashcam ✓", 160),
        ];
        let parsed = parse_attributes(&encode_attributes(&attrs)).unwrap();
        assert_eq!(parsed, attrs);
        assert_eq!(parsed[0].as_u64(), Some(42));
        assert_eq!(parsed[1].as_string(), "DC8001L8");
        assert_eq!(parsed[2].as_string(), "dashcam ✓");
    }

    #[test]
    fn text_truncates_on_char_boundary() {
        let a = Attribute::text(id::MEDIA_POOL, "ééé", 5);
        assert_eq!(a.value.len(), 5);
        assert_eq!(a.as_string(), "éé");
    }
}
