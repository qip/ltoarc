//! Real hardware backend: everything goes through the SCSI generic node (as LTFS does), so
//! early-warning, filemark and partition handling are exact and no `st` state can drift.

use std::fs;
use std::path::Path;

use ltoarc_scsi::sg::SgDevice;
use ltoarc_scsi::smc::{self, ElementAddresses, ElementType};
use ltoarc_scsi::{density, inquiry, log, mam, mode, tape, SenseKey};

use crate::{
    Attribute, Backend, Changer, ChangerInfo, DensityDescriptor, DriveInfo, Element, Error, Generation,
    Inventory, LogPage, MediaInfo, PartitionLayout, Position, PositionInfo, ReadStatus, Readiness, Result,
    TapeDrive, WriteStatus,
};

/// A device found under /sys/class/scsi_generic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub sg_path: String,
    pub peripheral_type: u8,
    pub st_path: Option<String>,
}

/// List tape drives and changers by peripheral type, without opening them.
pub fn discover() -> Vec<Discovered> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/scsi_generic") else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let dev = e.path().join("device");
        let Ok(t) = fs::read_to_string(dev.join("type")) else { continue };
        let Ok(pt) = t.trim().parse::<u8>() else { continue };
        if pt != inquiry::PDT_SEQUENTIAL && pt != inquiry::PDT_CHANGER {
            continue;
        }
        let st_path = (pt == inquiry::PDT_SEQUENTIAL).then(|| nst_name(&dev.join("scsi_tape"))).flatten();
        out.push(Discovered { sg_path: format!("/dev/{name}"), peripheral_type: pt, st_path });
    }
    out.sort_by_key(|d| sg_number(&d.sg_path));
    out
}

fn sg_number(p: &str) -> u32 {
    p.trim_start_matches("/dev/sg").parse().unwrap_or(u32::MAX)
}

/// The non-rewinding device without mode suffix ("nst0", not "nst0a").
fn nst_name(dir: &Path) -> Option<String> {
    fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().to_string();
        (n.starts_with("nst") && n[3..].chars().all(|c| c.is_ascii_digit())).then(|| format!("/dev/{n}"))
    })
}

pub struct RealDrive {
    info: DriveInfo,
    sg: SgDevice,
    variable_mode: bool,
    max_block: Option<usize>,
}

impl RealDrive {
    pub fn open(sg_path: &str, st_path: Option<String>) -> Result<Self> {
        let sg = SgDevice::open(sg_path)?;
        let inq = inquiry::inquiry(&sg)?;
        if inq.peripheral_type != inquiry::PDT_SEQUENTIAL {
            return Err(Error::Invalid(format!("{sg_path} is not a tape drive")));
        }
        let serial = inquiry::unit_serial(&sg).unwrap_or_default();
        // Room for the largest tape block plus slack, so no command allocates a buffer.
        let _ = sg.reserve(sg.max_transfer().unwrap_or(1 << 20).max(1 << 20));
        let info = DriveInfo {
            generation: Generation::from_drive_product(&inq.product),
            vendor: inq.vendor,
            product: inq.product,
            revision: inq.revision,
            serial,
            backend: Backend::Real,
            location: sg_path.to_string(),
            st_path,
        };
        Ok(RealDrive { info, sg, variable_mode: false, max_block: None })
    }

    fn ensure_variable_mode(&mut self) -> Result<()> {
        if !self.variable_mode {
            mode::set_variable_block(&self.sg)?;
            self.variable_mode = true;
        }
        Ok(())
    }
}

fn map_err(e: ltoarc_scsi::Error) -> Error {
    match e.sense() {
        Some(s) if s.is_medium_not_present() => Error::NoMedium,
        Some(s) if s.is_write_protected() => Error::WriteProtected,
        Some(s) if s.is_volume_overflow() => Error::EndOfMedium,
        Some(s) if s.is_end_of_data() => Error::EndOfData,
        _ => Error::Scsi(e),
    }
}

impl TapeDrive for RealDrive {
    fn info(&self) -> &DriveInfo {
        &self.info
    }

    fn readiness(&mut self) -> Result<Readiness> {
        for _ in 0..3 {
            match tape::test_unit_ready(&self.sg) {
                Ok(()) => return Ok(Readiness::Ready),
                Err(e) => match e.sense() {
                    Some(s) if s.is_unit_attention() => {
                        self.variable_mode = false;
                        continue;
                    }
                    Some(s) if s.is_medium_not_present() => return Ok(Readiness::NoMedium),
                    Some(s) if s.is_becoming_ready() => return Ok(Readiness::Becoming),
                    Some(s) if s.key == SenseKey::NotReady => return Ok(Readiness::NotReady(s.to_string())),
                    _ => return Err(Error::Scsi(e)),
                },
            }
        }
        Ok(Readiness::NotReady("repeated unit attention".into()))
    }

    fn load(&mut self) -> Result<()> {
        self.variable_mode = false;
        tape::load_unload(&self.sg, true, false, false).map_err(map_err)
    }

    fn unload(&mut self) -> Result<()> {
        self.variable_mode = false;
        tape::load_unload(&self.sg, false, false, false).map_err(map_err)
    }

    fn media_info(&mut self) -> Result<MediaInfo> {
        let m = mode::mode_sense10(&self.sg, 0x10, 0, false).map_err(map_err)?;
        let bd = m.blocks.first().copied().unwrap_or(mode::BlockDescriptor { density: 0, blocks: 0, block_length: 0 });
        Ok(MediaInfo {
            density: bd.density,
            generation: Generation::from_density(bd.density),
            medium_type: m.medium_type,
            write_protected: m.write_protected(),
            block_length: bd.block_length,
        })
    }

    fn max_block_size(&mut self) -> Result<usize> {
        if let Some(m) = self.max_block {
            return Ok(m);
        }
        let (drive_max, _) = tape::read_block_limits(&self.sg).map_err(map_err)?;
        let host = self.sg.max_transfer().unwrap_or(drive_max as usize);
        let m = (drive_max as usize).min(if host == 0 { drive_max as usize } else { host });
        self.max_block = Some(m);
        Ok(m)
    }

    fn report_density(&mut self, media: bool) -> Result<Vec<DensityDescriptor>> {
        density::report_density_support(&self.sg, media).map_err(map_err)
    }

    fn position(&mut self) -> Result<PositionInfo> {
        let p = tape::read_position(&self.sg).map_err(map_err)?;
        Ok(PositionInfo {
            pos: Position { partition: p.partition as u8, object: p.object },
            file: p.file,
            bop: p.bop,
            eop: p.eop,
        })
    }

    fn locate(&mut self, pos: Position) -> Result<()> {
        tape::locate16(&self.sg, Some(pos.partition), tape::LocateTarget::Object(pos.object)).map_err(map_err)
    }

    fn locate_eod(&mut self, partition: u8) -> Result<PositionInfo> {
        tape::locate16(&self.sg, Some(partition), tape::LocateTarget::EndOfData).map_err(map_err)?;
        self.position()
    }

    fn rewind(&mut self) -> Result<()> {
        tape::rewind(&self.sg).map_err(map_err)
    }

    fn space_filemarks(&mut self, count: i32) -> Result<()> {
        tape::space6(&self.sg, tape::SpaceCode::Filemarks, count).map_err(map_err)
    }

    fn write_block(&mut self, data: &[u8]) -> Result<WriteStatus> {
        self.ensure_variable_mode()?;
        tape::write6(&self.sg, data).map_err(map_err)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteStatus> {
        tape::write_filemarks(&self.sg, count).map_err(map_err)
    }

    fn read_block(&mut self, buf: &mut [u8]) -> Result<ReadStatus> {
        self.ensure_variable_mode()?;
        tape::read6(&self.sg, buf).map_err(map_err)
    }

    fn read_attributes(&mut self, partition: u8) -> Result<Vec<Attribute>> {
        mam::read_attributes(&self.sg, partition, 0).map_err(map_err)
    }

    fn write_attributes(&mut self, partition: u8, attrs: &[Attribute]) -> Result<()> {
        mam::write_attributes(&self.sg, partition, attrs).map_err(map_err)
    }

    fn supported_log_pages(&mut self) -> Result<Vec<u8>> {
        log::supported_pages(&self.sg).map_err(map_err)
    }

    fn log_page(&mut self, page: u8, subpage: u8) -> Result<LogPage> {
        log::log_sense(&self.sg, page, subpage).map_err(map_err)
    }

    fn compression(&mut self) -> Result<bool> {
        let m = mode::mode_sense10(&self.sg, 0x0f, 0, true).map_err(map_err)?;
        let p = m.page(0x0f, 0).ok_or_else(|| Error::Unsupported("no Data Compression mode page".into()))?;
        Ok(p.body().first().is_some_and(|b| b & 0x80 != 0))
    }

    fn set_compression(&mut self, on: bool) -> Result<()> {
        // DCE in MP 0Fh, and SELECT DATA COMPRESSION ALGORITHM in MP 10h (byte 14), which
        // would otherwise force DCE back on (IBM LTO SCSI Reference, MP 0Fh note 49).
        let mut pages = Vec::new();
        let dc = mode::mode_sense10(&self.sg, 0x0f, 0, true).map_err(map_err)?;
        let mut p0f = dc.page(0x0f, 0).ok_or_else(|| Error::Unsupported("no Data Compression mode page".into()))?;
        p0f.data[0] &= 0x7f;
        if let Some(b) = p0f.data.get_mut(2) {
            *b = if on { *b | 0x80 } else { *b & 0x7f };
        }
        pages.extend_from_slice(&p0f.data);
        let dcfg = mode::mode_sense10(&self.sg, 0x10, 0, true).map_err(map_err)?;
        if let Some(mut p10) = dcfg.page(0x10, 0) {
            p10.data[0] &= 0x7f;
            if let Some(b) = p10.data.get_mut(14) {
                *b = on as u8;
            }
            pages.extend_from_slice(&p10.data);
        }
        let sel = mode::ModeData { device_specific: mode::BUFFERED, pages, ..Default::default() };
        mode::mode_select10(&self.sg, &sel, false).map_err(map_err)?;
        if self.compression()? != on {
            return Err(Error::Unsupported(format!("drive did not accept compression {}", if on { "on" } else { "off" })));
        }
        Ok(())
    }

    fn format(&mut self, layout: PartitionLayout) -> Result<()> {
        match layout {
            PartitionLayout::Single => tape::format_medium(&self.sg, 0).map_err(map_err),
            PartitionLayout::Two { .. } => Err(Error::Unsupported("partitioning arrives in phase 4".into())),
        }
    }

    fn preferred_density(&mut self, medium_type: u8) -> Result<u8> {
        let page = preferred_page(&self.sg)?.1;
        page.body()
            .chunks_exact(4)
            .find(|d| d[0] == medium_type)
            .map(|d| d[1])
            .ok_or_else(|| Error::Unsupported(format!("no preferred density entry for medium type {medium_type:#04x}")))
    }

    fn set_preferred_density(&mut self, medium_type: u8, density: u8) -> Result<()> {
        let mut page = preferred_page(&self.sg)?.1;
        let body = page.body_mut();
        let d = body
            .chunks_exact_mut(4)
            .find(|d| d[0] == medium_type)
            .ok_or_else(|| Error::Unsupported(format!("no preferred density entry for medium type {medium_type:#04x}")))?;
        d[1] = density;
        page.data[0] &= 0x7f; // PS must be zero on MODE SELECT
        let sel = mode::ModeData { device_specific: mode::BUFFERED, pages: page.data, ..Default::default() };
        mode::mode_select10(&self.sg, &sel, false).map_err(map_err)
    }
}

/// IBM Preferred Cartridge Type page (MP 30h[44h]).
fn preferred_page(sg: &SgDevice) -> Result<(u8, mode::ModePage)> {
    let m = mode::mode_sense10(sg, 0x30, 0x44, true).map_err(|e| match e.sense() {
        Some(s) if s.key == SenseKey::IllegalRequest => {
            Error::Unsupported("drive has no Preferred Cartridge Type mode page (not an IBM LTO-8?)".into())
        }
        _ => map_err(e),
    })?;
    let page = m.page(0x30, 0x44).ok_or_else(|| Error::Unsupported("Preferred Cartridge Type page missing".into()))?;
    Ok((m.device_specific, page))
}

pub struct RealChanger {
    info: ChangerInfo,
    sg: SgDevice,
    addrs: ElementAddresses,
}

impl RealChanger {
    pub fn open(sg_path: &str) -> Result<Self> {
        let sg = SgDevice::open(sg_path)?;
        let inq = inquiry::inquiry(&sg)?;
        if inq.peripheral_type != inquiry::PDT_CHANGER {
            return Err(Error::Invalid(format!("{sg_path} is not a media changer")));
        }
        let serial = inquiry::unit_serial(&sg).unwrap_or_default();
        let addrs = smc::element_addresses(&sg)?;
        let info = ChangerInfo {
            vendor: inq.vendor,
            product: inq.product,
            revision: inq.revision,
            serial,
            backend: Backend::Real,
            location: sg_path.to_string(),
        };
        Ok(RealChanger { info, sg, addrs })
    }
}

impl Changer for RealChanger {
    fn info(&self) -> &ChangerInfo {
        &self.info
    }

    fn inventory(&mut self) -> Result<Inventory> {
        let mut elements = Vec::new();
        let a = self.addrs;
        for (kind, (first, count)) in [
            (ElementType::Transport, a.transport),
            (ElementType::Storage, a.storage),
            (ElementType::ImportExport, a.import_export),
            (ElementType::DataTransfer, a.data_transfer),
        ] {
            if count == 0 {
                continue;
            }
            let dvcid = kind == ElementType::DataTransfer;
            for s in smc::read_element_status(&self.sg, kind, first, count, dvcid)? {
                elements.push(Element {
                    drive_serial: s.drive_serial(),
                    drive_product: s.drive_product(),
                    kind: s.kind,
                    address: s.address,
                    full: s.full,
                    barcode: s.primary_tag.clone(),
                    source: s.source,
                    exception: s.except.then_some((s.asc, s.ascq)),
                });
            }
        }
        Ok(Inventory { elements })
    }

    fn move_medium(&mut self, src: u16, dst: u16) -> Result<()> {
        smc::move_medium(&self.sg, self.addrs.transport.0, src, dst).map_err(Error::Scsi)
    }

    fn rescan(&mut self) -> Result<()> {
        smc::initialize_element_status(&self.sg).map_err(Error::Scsi)
    }
}
