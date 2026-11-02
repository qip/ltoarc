//! LOG SENSE (4Dh): generic page parsing plus parameter name tables for the pages used for
//! drive and tape wear statistics.

use crate::{ascii_trim, be16, be_uint, need, timeouts, DataDir, Result, Transport};

pub const PAGE_SUPPORTED: u8 = 0x00;
pub const PAGE_WRITE_ERRORS: u8 = 0x02;
pub const PAGE_READ_ERRORS: u8 = 0x03;
pub const PAGE_SEQUENTIAL_ACCESS: u8 = 0x0c;
pub const PAGE_DEVICE_STATISTICS: u8 = 0x14;
pub const PAGE_VOLUME_STATISTICS: u8 = 0x17;
pub const PAGE_DATA_COMPRESSION: u8 = 0x1b;
pub const PAGE_TAPEALERT: u8 = 0x2e;
pub const PAGE_TAPE_CAPACITY: u8 = 0x31;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogParam {
    pub code: u16,
    pub control: u8,
    pub value: Vec<u8>,
}

impl LogParam {
    pub fn new(code: u16, value: Vec<u8>) -> Self {
        LogParam { code, control: 0, value }
    }

    pub fn as_u64(&self) -> Option<u64> {
        be_uint(&self.value)
    }

    /// Parameter is an ASCII/string parameter (FORMAT AND LINKING = 01b).
    pub fn is_ascii(&self) -> bool {
        self.control & 0x03 == 0x01
    }

    pub fn as_ascii(&self) -> String {
        ascii_trim(&self.value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogPage {
    pub page: u8,
    pub subpage: u8,
    pub params: Vec<LogParam>,
}

impl LogPage {
    pub fn get(&self, code: u16) -> Option<&LogParam> {
        self.params.iter().find(|p| p.code == code)
    }

    pub fn u64(&self, code: u16) -> Option<u64> {
        self.get(code).and_then(LogParam::as_u64)
    }
}

/// Read cumulative values (PC = 01b) of a log page.
pub fn log_sense(t: &dyn Transport, page: u8, subpage: u8) -> Result<LogPage> {
    let mut buf = vec![0u8; 0xfffc];
    let mut cdb = [0u8; 10];
    cdb[0] = 0x4d;
    cdb[2] = 0x40 | (page & 0x3f);
    cdb[3] = subpage;
    cdb[7..9].copy_from_slice(&(buf.len() as u16).to_be_bytes());
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    parse_log_page(&buf[..c.transferred])
}

/// Page codes listed in the Supported Log Pages page.
pub fn supported_pages(t: &dyn Transport) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    let mut cdb = [0u8; 10];
    cdb[0] = 0x4d;
    cdb[2] = 0x40;
    cdb[7..9].copy_from_slice(&(buf.len() as u16).to_be_bytes());
    let c = t.exec(&cdb, DataDir::In(&mut buf), timeouts::SHORT)?;
    let buf = &buf[..c.transferred];
    need(buf, 4, "supported log pages")?;
    let n = (be16(&buf[2..4]) as usize).min(buf.len() - 4);
    Ok(buf[4..4 + n].iter().map(|p| p & 0x3f).collect())
}

pub fn parse_log_page(buf: &[u8]) -> Result<LogPage> {
    need(buf, 4, "log page")?;
    let page = buf[0] & 0x3f;
    let subpage = buf[1];
    let end = (4 + be16(&buf[2..4]) as usize).min(buf.len());
    let mut params = Vec::new();
    let mut off = 4;
    while off + 4 <= end {
        let code = be16(&buf[off..off + 2]);
        let control = buf[off + 2];
        let len = buf[off + 3] as usize;
        let vend = (off + 4 + len).min(end);
        params.push(LogParam { code, control, value: buf[off + 4..vend].to_vec() });
        off += 4 + len;
    }
    Ok(LogPage { page, subpage, params })
}

pub fn encode_log_page(p: &LogPage) -> Vec<u8> {
    let mut body = Vec::new();
    for prm in &p.params {
        body.extend_from_slice(&prm.code.to_be_bytes());
        body.push(prm.control);
        body.push(prm.value.len() as u8);
        body.extend_from_slice(&prm.value);
    }
    let mut out = vec![p.page | if p.subpage != 0 { 0x40 } else { 0 }, p.subpage];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

/// Set TapeAlert flags (1..=64) from the TapeAlert page.
pub fn tapealert_flags(p: &LogPage) -> Vec<u8> {
    p.params
        .iter()
        .filter(|prm| (1..=64).contains(&prm.code) && prm.value.first().is_some_and(|v| v & 1 != 0))
        .map(|prm| prm.code as u8)
        .collect()
}

/// A named log parameter: (page, code, stable key, description).
pub struct ParamName {
    pub page: u8,
    pub code: u16,
    pub key: &'static str,
    pub label: &'static str,
}

macro_rules! names {
    ($($page:expr, $code:expr, $key:expr, $label:expr;)*) => {
        &[$(ParamName { page: $page, code: $code, key: $key, label: $label },)*]
    };
}

/// Parameters recorded in wear statistics. Codes follow SSC-4 and the IBM LTO SCSI Reference.
pub static PARAM_NAMES: &[ParamName] = names![
    0x0c, 0x0000, "channel_write_bytes", "Total channel write bytes";
    0x0c, 0x0001, "device_write_bytes", "Total device write bytes";
    0x0c, 0x0002, "device_read_bytes", "Total device read bytes";
    0x0c, 0x0003, "channel_read_bytes", "Total channel read bytes";
    0x0c, 0x0100, "cleaning_requested", "Cleaning requested";
    0x0c, 0x8000, "mb_since_cleaning", "Megabytes processed since cleaning";
    0x14, 0x0000, "lifetime_volume_loads", "Lifetime volume loads";
    0x14, 0x0001, "lifetime_cleanings", "Lifetime cleaning operations";
    0x14, 0x0002, "lifetime_power_on_hours", "Lifetime power-on hours";
    0x14, 0x0003, "lifetime_head_hours", "Lifetime medium motion (head) hours";
    0x14, 0x0004, "lifetime_meters", "Lifetime meters of tape processed";
    0x14, 0x0008, "head_hours_since_cleaning", "Head hours since last successful cleaning";
    0x14, 0x0009, "head_hours_since_cleaning_2", "Head hours since second-to-last cleaning";
    0x14, 0x000a, "head_hours_since_cleaning_3", "Head hours since third-to-last cleaning";
    0x14, 0x000c, "lifetime_power_cycles", "Lifetime power cycles";
    0x14, 0x000d, "loads_since_reset", "Volume loads since last parameter reset";
    0x14, 0x000e, "hard_write_errors", "Hard write errors";
    0x14, 0x000f, "hard_read_errors", "Hard read errors";
    0x14, 0x0041, "drive_serial", "Drive serial number";
    0x14, 0x0081, "max_temperature_exceeded", "Maximum recommended mechanism temperature exceeded";
    0x17, 0x0001, "volume_mounts", "Volume mounts";
    0x17, 0x0002, "datasets_written", "Volume datasets written";
    0x17, 0x0003, "recovered_write_errors", "Volume recovered write data errors";
    0x17, 0x0004, "unrecovered_write_errors", "Volume unrecovered write data errors";
    0x17, 0x0005, "write_servo_errors", "Volume write servo errors";
    0x17, 0x0006, "unrecovered_write_servo_errors", "Volume unrecovered write servo errors";
    0x17, 0x0007, "datasets_read", "Volume datasets read";
    0x17, 0x0008, "recovered_read_errors", "Volume recovered read errors";
    0x17, 0x0009, "unrecovered_read_errors", "Volume unrecovered read errors";
    0x17, 0x000c, "last_mount_unrecovered_write_errors", "Last mount unrecovered write errors";
    0x17, 0x000d, "last_mount_unrecovered_read_errors", "Last mount unrecovered read errors";
    0x17, 0x000e, "last_mount_mb_written", "Last mount megabytes written";
    0x17, 0x000f, "last_mount_mb_read", "Last mount megabytes read";
    0x17, 0x0010, "lifetime_mb_written", "Lifetime megabytes written";
    0x17, 0x0011, "lifetime_mb_read", "Lifetime megabytes read";
    0x17, 0x0012, "last_load_write_compression", "Last load write compression ratio";
    0x17, 0x0013, "last_load_read_compression", "Last load read compression ratio";
    0x17, 0x0016, "total_native_capacity_mb", "Total native capacity";
    0x17, 0x0017, "used_native_capacity_mb", "Total used native capacity";
    0x17, 0x0019, "volume_lifetime_remaining", "Volume lifetime remaining (%)";
    0x17, 0x0040, "volume_serial", "Volume serial number";
    0x17, 0x0042, "volume_barcode", "Volume barcode";
    0x17, 0x0043, "volume_manufacturer", "Volume manufacturer";
    0x17, 0x0080, "write_protect", "Write protect";
    0x17, 0x0081, "worm", "WORM";
    0x17, 0x0101, "bom_passes", "Beginning of medium passes";
    0x17, 0x0102, "mot_passes", "Middle of tape passes";
    0x31, 0x0001, "p0_remaining_mb", "Partition 0 remaining capacity (MB)";
    0x31, 0x0002, "p1_remaining_mb", "Partition 1 remaining capacity (MB)";
    0x31, 0x0003, "p0_max_mb", "Partition 0 maximum capacity (MB)";
    0x31, 0x0004, "p1_max_mb", "Partition 1 maximum capacity (MB)";
];

pub fn param_name(page: u8, code: u16) -> Option<&'static ParamName> {
    PARAM_NAMES.iter().find(|n| n.page == page && n.code == code)
}

pub fn tapealert_name(flag: u8) -> &'static str {
    match flag {
        0x01 => "Read warning",
        0x02 => "Write warning",
        0x03 => "Hard error",
        0x04 => "Media",
        0x05 => "Read failure",
        0x06 => "Write failure",
        0x07 => "Media life",
        0x08 => "Not data grade",
        0x09 => "Write protect",
        0x0a => "No removal",
        0x0b => "Cleaning media",
        0x0c => "Unsupported format",
        0x0d => "Recoverable mechanical cartridge failure",
        0x0e => "Unrecoverable mechanical cartridge failure",
        0x0f => "Memory chip in cartridge failure",
        0x10 => "Forced eject",
        0x11 => "Read only format",
        0x12 => "Tape directory corrupted on load",
        0x13 => "Nearing media life",
        0x14 => "Clean now",
        0x15 => "Clean periodic",
        0x16 => "Expired cleaning media",
        0x17 => "Invalid cleaning tape",
        0x18 => "Retension requested",
        0x19 => "Dual-port interface error",
        0x1a => "Cooling fan failure",
        0x1b => "Power supply failure",
        0x1c => "Power consumption",
        0x1d => "Drive maintenance",
        0x1e => "Hardware A",
        0x1f => "Hardware B",
        0x20 => "Interface",
        0x21 => "Eject media",
        0x22 => "Microcode update fail",
        0x23 => "Drive humidity",
        0x24 => "Drive temperature",
        0x25 => "Drive voltage",
        0x26 => "Predictive failure",
        0x27 => "Diagnostics required",
        0x32 => "Lost statistics",
        0x33 => "Tape directory invalid at unload",
        0x34 => "Tape system area write failure",
        0x35 => "Tape system area read failure",
        0x36 => "No start of data",
        0x37 => "Loading failure",
        0x38 => "Unrecoverable unload failure",
        0x39 => "Automation interface failure",
        0x3a => "Firmware failure",
        0x3b => "WORM medium integrity check failed",
        0x3c => "WORM medium overwrite attempted",
        _ => "Reserved",
    }
}

pub const TAPEALERT_CLEAN_NOW: u8 = 0x14;
pub const TAPEALERT_CLEAN_PERIODIC: u8 = 0x15;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_names() {
        let p = LogPage {
            page: PAGE_DEVICE_STATISTICS,
            subpage: 0,
            params: vec![
                LogParam::new(0x0000, 1234u32.to_be_bytes().to_vec()),
                LogParam::new(0x0003, 56u32.to_be_bytes().to_vec()),
            ],
        };
        let parsed = parse_log_page(&encode_log_page(&p)).unwrap();
        assert_eq!(parsed, p);
        assert_eq!(parsed.u64(0x0000), Some(1234));
        assert_eq!(param_name(0x14, 0x0003).unwrap().key, "lifetime_head_hours");
    }

    #[test]
    fn tapealert() {
        let mut params: Vec<LogParam> = (1..=64).map(|c| LogParam::new(c, vec![0])).collect();
        params[0x13].value = vec![1]; // flag 0x14
        let p = LogPage { page: PAGE_TAPEALERT, subpage: 0, params };
        assert_eq!(tapealert_flags(&p), vec![TAPEALERT_CLEAN_NOW]);
        assert_eq!(tapealert_name(0x14), "Clean now");
    }
}
