//! Pictures embedded in xlsx files: read with their place, size and bytes,
//! moved with inserted lines, kept on save.

use std::path::PathBuf;
use std::sync::Arc;

use rust_xlsxwriter::{Image, Workbook as XlsxBook};
use xl_engine::CellRef;
use xl_engine::picture::PictureFormat;
use xl_io::SavePlan;

/// A 2×2 red PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd, 0xd4, 0x9a, 0x73, 0x00, 0x00,
    0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x44, 0x0c, 0x10, 0x0a, 0x00, 0x1f, 0xee, 0x03, 0xfd, 0x8b, 0x5f, 0x14, 0xd4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

fn tmp(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("xl-picture-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// A sheet with the same picture twice: 40×30 at C3 (offset 5, 3), and at F10.
fn picture_file(name: &str) -> PathBuf {
    let path = tmp(name);
    let mut book = XlsxBook::new();
    let ws = book.add_worksheet();
    ws.write_string(0, 0, "Logo below").unwrap();
    let image = Image::new_from_buffer(PNG).unwrap().set_width(40).set_height(30).set_alt_text("Company logo");
    ws.insert_image_with_offset(2, 2, &image, 5, 3).unwrap();
    ws.insert_image(9, 5, &Image::new_from_buffer(PNG).unwrap()).unwrap();
    book.save(&path).unwrap();
    path
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1.0
}

#[test]
fn reads_pictures_with_place_size_and_bytes() {
    let loaded = xl_io::open(&picture_file("read.xlsx")).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let pics = &loaded.workbook.layout(s).pictures;
    assert_eq!(pics.len(), 2);
    let p = &pics[0];
    assert_eq!(p.anchor, CellRef::new(2, 2));
    assert_eq!(&*p.data, PNG);
    assert_eq!(p.format, PictureFormat::Png);
    assert_eq!(p.alt.as_deref(), Some("Company logo"));
    assert!(close(p.offset.0, xl_io::from_excel_width_px(5.0)) && close(p.offset.1, xl_io::from_excel_height(3.0 * 0.75)), "{:?}", p.offset);
    assert!(close(p.size.0, xl_io::from_excel_width_px(40.0)) && close(p.size.1, xl_io::from_excel_height(30.0 * 0.75)), "{:?}", p.size);
    // rust_xlsxwriter writes two-cell anchors: the far corner is known.
    assert!(p.end.is_some_and(|(c, _)| c == CellRef::new(3, 2)), "{:?}", p.end);
    assert!(Arc::ptr_eq(&pics[0].data, &pics[1].data), "one image file, one copy of its bytes");
    assert_eq!(pics[1].anchor, CellRef::new(9, 5));
    assert!(loaded.unsupported.is_empty(), "pictures are shown and kept: {:?}", loaded.unsupported);
}

#[test]
fn pictures_move_with_inserted_rows_and_survive_saving() {
    let src = picture_file("shift.xlsx");
    let mut loaded = xl_io::open(&src).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    loaded.workbook.apply_shift(xl_engine::Shift::InsertRows { sheet: s, at: 0, count: 2 });
    loaded.workbook.apply_shift(xl_engine::Shift::InsertCols { sheet: s, at: 3, count: 1 });
    let p = &loaded.workbook.layout(s).pictures[0];
    assert_eq!(p.anchor, CellRef::new(4, 2), "moved down two rows; the column insert is right of it");
    assert_eq!(p.end.map(|e| e.0), Some(CellRef::new(5, 2)));
    assert_eq!(loaded.workbook.layout(s).pictures[1].anchor, CellRef::new(11, 6));
    // Deleting the anchor row closes up onto it.
    loaded.workbook.apply_shift(xl_engine::Shift::DeleteRows { sheet: s, at: 4, count: 1 });
    assert_eq!(loaded.workbook.layout(s).pictures[0].anchor, CellRef::new(4, 2));

    // Saved into the original, the drawing follows; reopened, it's in the same place.
    let pkg = loaded.package.as_ref().unwrap();
    assert_eq!(xl_io::save_plan(&loaded.workbook, pkg), SavePlan::Patch);
    let out = tmp("shift-saved.xlsx");
    xl_io::save(&loaded.workbook, &out, s, Some(pkg)).unwrap();
    let back = xl_io::open(&out).unwrap();
    let pics = &back.workbook.layout(back.workbook.sheet_ids()[0]).pictures;
    assert_eq!(pics.iter().map(|p| p.anchor).collect::<Vec<_>>(), loaded.workbook.layout(s).pictures.iter().map(|p| p.anchor).collect::<Vec<_>>());
    assert_eq!(&*pics[0].data, PNG);
}

#[test]
fn fresh_rewrite_keeps_pictures() {
    let loaded = xl_io::open(&picture_file("fresh.xlsx")).unwrap();
    let s = loaded.workbook.sheet_ids()[0];
    let out = tmp("fresh-saved.xlsx");
    // No package: written from scratch.
    xl_io::save(&loaded.workbook, &out, s, None).unwrap();
    let back = xl_io::open(&out).unwrap();
    let (before, after) = (&loaded.workbook.layout(s).pictures, &back.workbook.layout(back.workbook.sheet_ids()[0]).pictures);
    assert_eq!(after.len(), 2);
    for (a, b) in before.iter().zip(after) {
        assert_eq!(a.anchor, b.anchor);
        assert_eq!(a.data, b.data);
        assert_eq!(a.alt, b.alt);
        assert!(close(a.offset.0, b.offset.0) && close(a.offset.1, b.offset.1), "{:?} vs {:?}", a.offset, b.offset);
        assert!(close(a.size.0, b.size.0) && close(a.size.1, b.size.1), "{:?} vs {:?}", a.size, b.size);
    }
}
