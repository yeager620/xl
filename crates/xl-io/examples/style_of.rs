//! Print a cell's style: `cargo run -p xl-io --example style_of -- file.xlsx A1`
fn main() {
    let mut a = std::env::args().skip(1);
    let (path, cell) = (a.next().unwrap(), a.next().unwrap());
    let wb = xl_io::open(std::path::Path::new(&path)).unwrap().workbook;
    let s = wb.sheet_ids()[0];
    println!("{:?}", wb.style(xl_engine::Addr::new(s, xl_engine::CellRef::parse(&cell).unwrap())));
}
