//! Print every cell of a workbook: `cargo run -p xl-io --example dump -- file.xlsx`
use xl_engine::Addr;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <file>");
    let loaded = xl_io::open(std::path::Path::new(&path)).unwrap_or_else(|e| panic!("{e}"));
    let wb = &loaded.workbook;
    println!("{} cells, recalc {:?}, warnings {:?}", loaded.cells, loaded.recalc.elapsed, loaded.warnings);
    for &id in wb.sheet_ids() {
        println!("[{}]", wb.sheet_name(id));
        for (c, _) in wb.store(id).iter() {
            let a = Addr::new(id, c);
            println!("  {c:<5} {:<45} => {:<16} fmt={:?}", wb.input_text(a), wb.display(a).text, wb.format_code(a));
        }
        for (col, w) in wb.col_widths(id) {
            println!("  col {} width {w}px", xl_engine::col_name(*col));
        }
    }
}
