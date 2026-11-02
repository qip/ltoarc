//! Linux SCSI generic (`/dev/sgN`) transport using the `SG_IO` ioctl.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Completion, DataDir, Error, Result, Sense, SenseKey, Transport};

const SG_IO: u64 = 0x2285;
const SG_DXFER_NONE: i32 = -1;
const SG_DXFER_TO_DEV: i32 = -2;
const SG_DXFER_FROM_DEV: i32 = -3;
const SENSE_LEN: usize = 96;

const STATUS_GOOD: u8 = 0x00;
const STATUS_CHECK_CONDITION: u8 = 0x02;

/// `struct sg_io_hdr` from `<scsi/sg.h>`.
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *const u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

#[derive(Debug)]
pub struct SgDevice {
    file: File,
    path: PathBuf,
    /// Map user buffers directly (only when /proc/scsi/sg/allow_dio is 1).
    direct_io: bool,
}

const SG_SET_RESERVED_SIZE: u64 = 0x2275;
const SG_FLAG_DIRECT_IO: u32 = 1;

impl SgDevice {
    /// Open an sg node read-write. `O_NONBLOCK` keeps `open` from waiting on a busy device.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .map_err(|source| Error::Io { path: path.display().to_string(), source })?;
        let direct_io = std::fs::read_to_string("/proc/scsi/sg/allow_dio").is_ok_and(|s| s.trim() == "1");
        Ok(SgDevice { file, path, direct_io })
    }

    /// Pre-allocate a kernel buffer of `bytes` for this descriptor. Without it sg allocates
    /// and frees a buffer for every command larger than the default 32 KiB reserve, which
    /// costs milliseconds per 1 MiB tape block.
    pub fn reserve(&self, bytes: usize) -> Result<()> {
        let v: libc::c_int = bytes.min(i32::MAX as usize) as libc::c_int;
        // SAFETY: SG_SET_RESERVED_SIZE reads one int.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_SET_RESERVED_SIZE as _, &v as *const libc::c_int) };
        if rc < 0 {
            return Err(Error::Io { path: self.path.display().to_string(), source: std::io::Error::last_os_error() });
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Largest data transfer the host adapter accepts for one command (BLKSECTGET), in bytes.
    pub fn max_transfer(&self) -> Result<usize> {
        const BLKSECTGET: u64 = 0x1267;
        let mut v: libc::c_int = 0;
        // SAFETY: BLKSECTGET on an sg node writes one int.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKSECTGET as _, &mut v as *mut libc::c_int) };
        if rc < 0 {
            return Err(Error::Io { path: self.path.display().to_string(), source: std::io::Error::last_os_error() });
        }
        Ok(v.max(0) as usize)
    }
}

impl Transport for SgDevice {
    fn exec(&self, cdb: &[u8], data: DataDir<'_>, timeout: Duration) -> Result<Completion> {
        let mut sense = [0u8; SENSE_LEN];
        let (dir, ptr, len) = match data {
            DataDir::None => (SG_DXFER_NONE, std::ptr::null_mut(), 0usize),
            DataDir::In(buf) => (SG_DXFER_FROM_DEV, buf.as_mut_ptr() as *mut libc::c_void, buf.len()),
            DataDir::Out(buf) => (SG_DXFER_TO_DEV, buf.as_ptr() as *mut libc::c_void, buf.len()),
        };
        let mut hdr = SgIoHdr {
            interface_id: 'S' as i32,
            dxfer_direction: dir,
            cmd_len: cdb.len() as u8,
            mx_sb_len: SENSE_LEN as u8,
            iovec_count: 0,
            dxfer_len: len as u32,
            dxferp: ptr,
            cmdp: cdb.as_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout: timeout.as_millis().min(u32::MAX as u128) as u32,
            flags: if self.direct_io && len >= 65536 { SG_FLAG_DIRECT_IO } else { 0 },
            pack_id: 0,
            usr_ptr: std::ptr::null_mut(),
            status: 0,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: 0,
            driver_status: 0,
            resid: 0,
            duration: 0,
            info: 0,
        };
        // SAFETY: hdr points at live buffers (cdb, data, sense) for the duration of the call.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut hdr as *mut SgIoHdr) };
        if rc < 0 {
            return Err(Error::Io {
                path: self.path.display().to_string(),
                source: std::io::Error::last_os_error(),
            });
        }
        if hdr.host_status != 0 {
            return Err(Error::Transport { host: hdr.host_status, driver: hdr.driver_status });
        }
        let transferred = len.saturating_sub(hdr.resid.max(0) as usize);
        let sense = (hdr.sb_len_wr > 0)
            .then(|| Sense::parse(&sense[..hdr.sb_len_wr as usize]))
            .flatten();
        match hdr.status {
            STATUS_GOOD => Ok(Completion { transferred, sense }),
            STATUS_CHECK_CONDITION => match sense {
                Some(s) if matches!(s.key, SenseKey::NoSense | SenseKey::RecoveredError) => {
                    Ok(Completion { transferred, sense: Some(s) })
                }
                Some(s) => Err(Error::Check(s)),
                None => Err(Error::Status(hdr.status)),
            },
            other => Err(Error::Status(other)),
        }
    }
}
