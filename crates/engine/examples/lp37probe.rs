//! Which IBM LP 37h/60h counters move for which tape motion (diagnosis only).
//! `lp37probe /dev/sgN STEP...` with STEP one of
//!   read:START:BLOCKS:MBPS   read blocks START.. of partition 0, paced (0 = full speed)
//!   write:BLOCKS:MBPS        write incompressible 1 MiB blocks at end of data, paced
//! After each step it prints the parameters that changed.
use ltoarc_device::{Position, ReadStatus, TapeDrive};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn snap(d: &mut dyn TapeDrive) -> BTreeMap<u16, u64> {
    let p = d.log_page(0x37, 0x60).unwrap();
    p.params.iter().filter_map(|x| Some((x.code, x.as_u64()?))).collect()
}

fn pace(t: Instant, done: u64, mbps: f64) {
    if mbps > 0.0 {
        let due = Duration::from_secs_f64(done as f64 / (mbps * 1e6));
        if let Some(w) = due.checked_sub(t.elapsed()) {
            std::thread::sleep(w);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut d = ltoarc_device::real::RealDrive::open(&a[1], None).unwrap();
    let mut before = snap(&mut d);
    let mut data = vec![0u8; 1 << 20];
    let mut x = 0x9e3779b97f4a7c15u64;
    for c in data.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
    }
    for step in &a[2..] {
        let f: Vec<&str> = step.split(':').collect();
        let t = Instant::now();
        let mut bytes = 0u64;
        match f[0] {
            "read" => {
                let (start, n, mbps): (u64, u64, f64) = (f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap());
                d.locate(Position { partition: 0, object: start }).unwrap();
                let t = Instant::now();
                let mut buf = vec![0u8; 1 << 20];
                for _ in 0..n {
                    pace(t, bytes, mbps);
                    if let ReadStatus::Data(k) = d.read_block(&mut buf).unwrap() {
                        bytes += k as u64;
                    }
                }
            }
            "write" => {
                let (n, mbps): (u64, f64) = (f[1].parse().unwrap(), f[2].parse().unwrap());
                d.locate_eod(0).unwrap();
                let t = Instant::now();
                for i in 0..n {
                    pace(t, bytes, mbps);
                    data[0..8].copy_from_slice(&i.to_le_bytes());
                    d.write_block(&data).unwrap();
                    bytes += data.len() as u64;
                }
                d.write_filemarks(1).unwrap();
            }
            "rburst" | "wburst" => {
                // BURSTS:BLOCKS:SLEEP (rburst starts at START first): known stops.
                let o = if f[0] == "rburst" { 1 } else { 0 };
                let (bursts, n, sleep): (u64, u64, f64) = (f[1 + o].parse().unwrap(), f[2 + o].parse().unwrap(), f[3 + o].parse().unwrap());
                if o == 1 {
                    d.locate(Position { partition: 0, object: f[1].parse().unwrap() }).unwrap();
                } else {
                    d.locate_eod(0).unwrap();
                }
                let mut buf = vec![0u8; 1 << 20];
                for b in 0..bursts {
                    if b > 0 {
                        std::thread::sleep(Duration::from_secs_f64(sleep));
                    }
                    for i in 0..n {
                        if o == 1 {
                            if let ReadStatus::Data(k) = d.read_block(&mut buf).unwrap() {
                                bytes += k as u64;
                            }
                        } else {
                            data[0..8].copy_from_slice(&(b * n + i).to_le_bytes());
                            d.write_block(&data).unwrap();
                            bytes += data.len() as u64;
                        }
                    }
                }
                if o == 0 {
                    d.write_filemarks(1).unwrap();
                }
            }
            other => panic!("unknown step {other}"),
        }
        let secs = t.elapsed().as_secs_f64();
        let after = snap(&mut d);
        let changed: Vec<String> = after
            .iter()
            .filter(|(k, v)| before.get(k) != Some(v))
            .map(|(k, v)| format!("{k:04x}:{}->{}", before.get(k).copied().unwrap_or(0), v))
            .collect();
        println!("{step}: {} MB in {secs:.1} s = {:.1} MB/s", bytes / 1_000_000, bytes as f64 / secs / 1e6);
        println!("  {}", changed.join(" "));
        before = after;
    }
}
