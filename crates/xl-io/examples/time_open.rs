//! Time loading a file: `cargo run --release -p xl-io --example time_open -- file`
use std::time::Instant;
fn main() {
    let path = std::env::args().nth(1).expect("usage: time_open <file>");
    let t = Instant::now();
    let loaded = xl_io::open(std::path::Path::new(&path)).unwrap();
    println!("total {:?}  (cells {}, recalc {:?}, evaluated {})", t.elapsed(), loaded.cells, loaded.recalc.elapsed, loaded.recalc.evaluated);
}
