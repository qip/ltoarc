//! Single-thread throughput of each content hash algorithm and of record encryption:
//! `cargo run --release --example hashbench`.
use ltoarc_engine::hash::Algo;
use std::time::Instant;

fn main() {
    let data: Vec<u8> = (0..(256u32 << 20)).map(|i| (i.wrapping_mul(2654435761) >> 7) as u8).collect();
    for algo in Algo::ALL {
        let t = Instant::now();
        let mut h = algo.hasher();
        for chunk in data.chunks(16 << 20) {
            h.update(chunk);
        }
        let _ = h.finish();
        println!("{:7} {:7.0} MB/s", algo.name(), data.len() as f64 / t.elapsed().as_secs_f64() / 1e6);
    }
    let mut s = ltoarc_engine::crypt::Sealer::new(&ltoarc_engine::crypt::Key::generate());
    let t = Instant::now();
    for chunk in data.chunks(16 << 20) {
        std::hint::black_box(s.seal_parts(b"\x03\x00", &[&chunk[..72], &chunk[72..]]));
    }
    println!("{:7} {:7.0} MB/s (seal, including the copy)", "chacha", data.len() as f64 / t.elapsed().as_secs_f64() / 1e6);
}
