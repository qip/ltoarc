//! Raw read speed through the read-ahead thread: `readbench /dev/sgN START BLOCKS`.
//! Reads blocks START.. of partition 0 of the loaded cartridge and discards them.
use ltoarc_engine::readahead::{Blocks, Extent, ReadAhead};
use ltoarc_format::stream::BlockSource;
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let start: u64 = a[2].parse().unwrap();
    let n: u64 = a[3].parse().unwrap();
    let mut d = ltoarc_device::real::RealDrive::open(&a[1], None).unwrap();
    let ra = ReadAhead::new(2 << 30);
    let ext = [Extent { partition: 0, start, end: Some(start + n - 1) }];
    let t = Instant::now();
    let (stats, bytes) = std::thread::scope(|s| {
        let r = s.spawn(|| ra.read(&mut d, 1 << 20, &ext));
        let mut b = Blocks::new(&ra);
        let mut bytes = 0u64;
        let mut first = None;
        loop {
            match b.next_block() {
                Ok(Some(x)) => {
                    if first.is_none() {
                        first = Some(Instant::now());
                        println!("first block after {:.1} s", t.elapsed().as_secs_f64());
                    }
                    bytes += x.len() as u64;
                    if bytes % (1 << 30) < (1 << 20) {
                        let s = first.unwrap().elapsed().as_secs_f64();
                        println!("  {} MB at {:.1} MB/s", bytes / 1_000_000, bytes as f64 / s.max(0.001) / 1e6);
                    }
                }
                Ok(None) if b.last.is_some_and(|l| l + 1 >= start + n) => break,
                Ok(None) => continue,
                Err(e) => panic!("{e}"),
            }
        }
        (r.join().unwrap(), bytes)
    });
    let secs = t.elapsed().as_secs_f64();
    println!("{} MB in {secs:.1} s: {:.1} MB/s; {} locates, {} pauses", bytes / 1_000_000, bytes as f64 / secs / 1e6, stats.locates, stats.pauses);
}
