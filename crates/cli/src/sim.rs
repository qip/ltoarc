//! `ltoarc sim`: create simulated hardware for testing and demos.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use ltoarc_device::sim::{DriveSpec, LibrarySpec, SimWorld, TapeSpec, WorldSpec};
use ltoarc_device::{expand_barcode_range, parse_barcode, Generation, MediaKind};

#[derive(Subcommand)]
pub enum SimCmd {
    /// Create a simulated world with one library (or add libraries with a spec file).
    Create {
        dir: PathBuf,
        /// TOML world spec (see ltoarc_device::sim::WorldSpec); overrides the flags below.
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long, default_value = "lib1")]
        library: String,
        #[arg(long, default_value_t = 23)]
        slots: usize,
        #[arg(long = "ie", default_value_t = 1)]
        ie_slots: usize,
        /// Drive generation, optionally ":unattached" (repeatable), e.g. LTO-8, LTO-6:unattached.
        #[arg(long = "drive", default_value = "LTO-8")]
        drives: Vec<String>,
        /// Barcode or range (repeatable), e.g. DC8001L8..DC8005L8, CLNU01L1.
        #[arg(long = "tapes")]
        tapes: Vec<String>,
        /// Capacity of every simulated cartridge, e.g. 512MiB (default: native capacity).
        #[arg(long)]
        capacity: Option<String>,
    },
    /// Raise (or clear) a TapeAlert flag on a simulated drive, e.g. 20 for "clean now".
    Tapealert {
        dir: PathBuf,
        /// Drive serial (as `ltoarc hw list` shows it).
        #[arg(long)]
        drive: String,
        /// TapeAlert flag number (1-64), e.g. 20 "clean now", 21 "clean periodic".
        #[arg(long)]
        flag: u8,
        /// Clear the flag instead of raising it.
        #[arg(long)]
        clear: bool,
    },
}

pub fn run(cmd: SimCmd) -> Result<()> {
    let (dir, spec, library, slots, ie_slots, drives, tapes, capacity) = match cmd {
        SimCmd::Create { dir, spec, library, slots, ie_slots, drives, tapes, capacity } => (dir, spec, library, slots, ie_slots, drives, tapes, capacity),
        SimCmd::Tapealert { dir, drive, flag, clear } => {
            SimWorld::open(&dir)?.set_tapealert(&drive, flag, !clear)?;
            println!("TapeAlert {flag} {} on {drive}", if clear { "cleared" } else { "raised" });
            return Ok(());
        }
    };
    let spec = match spec {
        Some(p) => toml::from_str(&std::fs::read_to_string(&p)?)?,
        None => {
            let capacity = capacity.as_deref().map(parse_size).transpose()?;
            let mut lib = LibrarySpec { name: library, slots, ie_slots, drives: Vec::new(), tapes: Vec::new() };
            for d in &drives {
                let (g, flag) = d.split_once(':').unwrap_or((d, ""));
                let generation: Generation = g.parse().map_err(|e: String| anyhow!(e))?;
                lib.drives.push(DriveSpec { attached: flag != "unattached", ..DriveSpec::new(generation) });
            }
            for range in &tapes {
                for bc in expand_barcode_range(range).map_err(|e| anyhow!(e))? {
                    let b = parse_barcode(&bc);
                    let cartridge = match (b.kind, b.generation) {
                        (MediaKind::Cleaning, _) => Generation::Lto8,
                        (_, Some(g)) => g,
                        (_, None) => bail!("{bc}: cannot tell the generation; use a label ending in L5..L9 or M8"),
                    };
                    lib.tapes.push(TapeSpec { barcode: Some(bc), cartridge, kind: b.kind, capacity, slot: None });
                }
            }
            WorldSpec { libraries: vec![lib], standalone: Vec::new() }
        }
    };
    let world = SimWorld::create(&dir, &spec)?;
    println!("created simulated world at {}", world.root().display());
    println!("use it with: ltoarc --sim {} hw scan", world.root().display());
    Ok(())
}

pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| anyhow!("bad size '{s}'"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1048576.0,
        "g" | "gb" => 1e9,
        "gib" => 1073741824.0,
        "t" | "tb" => 1e12,
        "tib" => 1099511627776.0,
        other => bail!("unknown size unit '{other}'"),
    };
    Ok((n * mult) as u64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::parse_size("64MiB").unwrap(), 64 << 20);
        assert_eq!(super::parse_size("1.5TB").unwrap(), 1_500_000_000_000);
        assert!(super::parse_size("3 parsecs").is_err());
    }
}
