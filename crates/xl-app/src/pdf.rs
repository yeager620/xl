//! PDF export with krilla: draws the pages laid out by print_layout as
//! vector graphics with real (selectable, searchable) text in the embedded,
//! subsetted Source Sans 3 faces.

use krilla::Document;
use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point, Rect as KRect, Size, Transform};
use krilla::metadata::Metadata;
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{Fill, FillRule, Stroke, StrokeDash};
use krilla::surface::Surface;
use krilla::text::{Font, TextDirection};
use xl_engine::Rgb;
use xl_engine::picture::PictureFormat;

use crate::print_layout::{FACES, Item, PrintDoc, Rect, ascent_descent};

fn color(c: Rgb) -> rgb::Color {
    rgb::Color::new((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn fill(c: Rgb) -> Fill {
    Fill { paint: color(c).into(), opacity: NormalizedF32::ONE, rule: FillRule::NonZero }
}

fn stroke(c: Rgb, width: f64, dash: Option<f64>) -> Stroke {
    Stroke { paint: color(c).into(), width: width.max(0.1) as f32, dash: dash.map(|d| StrokeDash { array: vec![d as f32, d as f32], offset: 0.0 }), ..Default::default() }
}

fn rect_path(r: Rect) -> Option<krilla::geom::Path> {
    let mut pb = PathBuilder::new();
    pb.push_rect(KRect::from_xywh(r.x as f32, r.y as f32, r.w.max(0.01) as f32, r.h.max(0.01) as f32)?);
    pb.finish()
}

fn with_clip(surface: &mut Surface, clip: Option<Rect>, f: impl FnOnce(&mut Surface)) {
    match clip.and_then(rect_path) {
        Some(p) => {
            surface.push_clip_path(&p, &FillRule::NonZero);
            f(surface);
            surface.pop();
        }
        None => f(surface),
    }
}

/// Render a print job to PDF bytes.
pub fn render(doc: &PrintDoc, title: &str) -> Result<Vec<u8>, String> {
    let mut pdf = Document::new();
    pdf.set_metadata(Metadata::new().title(title.to_string()).creator("xl".to_string()));
    let fonts: Vec<Font> = FACES.iter().map(|d| Font::new((*d).into(), 0).ok_or("the bundled font couldn't be loaded")).collect::<Result<_, _>>()?;
    for page in &doc.pages {
        let settings = PageSettings::from_wh(page.width as f32, page.height as f32).ok_or("bad page size")?;
        let mut p = pdf.start_page_with(settings);
        let mut surface = p.surface();
        for item in &page.items {
            draw(&mut surface, item, &fonts);
        }
        surface.finish();
        p.finish();
    }
    if doc.pages.is_empty() {
        // An empty document still needs a page.
        let mut p = pdf.start_page_with(PageSettings::from_wh(612.0, 792.0).unwrap());
        p.surface().finish();
        p.finish();
    }
    pdf.finish().map_err(|e| format!("{e:?}"))
}

fn draw(surface: &mut Surface, item: &Item, fonts: &[Font]) {
    match item {
        Item::Rect { r, color } => {
            if let Some(p) = rect_path(*r) {
                surface.set_stroke(None);
                surface.set_fill(Some(fill(*color)));
                surface.draw_path(&p);
            }
        }
        Item::Line { a, b, width, color, dash } => {
            let mut pb = PathBuilder::new();
            pb.move_to(a.0 as f32, a.1 as f32);
            pb.line_to(b.0 as f32, b.1 as f32);
            if let Some(p) = pb.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(stroke(*color, *width, *dash)));
                surface.draw_path(&p);
                surface.set_stroke(None);
            }
        }
        Item::Path { points, closed, fill: f, stroke: st, clip } => {
            let mut pb = PathBuilder::new();
            for (i, (x, y)) in points.iter().enumerate() {
                if i == 0 {
                    pb.move_to(*x as f32, *y as f32);
                } else {
                    pb.line_to(*x as f32, *y as f32);
                }
            }
            if *closed {
                pb.close();
            }
            let Some(p) = pb.finish() else { return };
            with_clip(surface, *clip, |surface| {
                surface.set_fill(f.map(fill));
                surface.set_stroke(st.map(|(c, w)| stroke(c, w, None)));
                surface.draw_path(&p);
                surface.set_stroke(None);
            });
        }
        Item::Text { x, y, text, face, size, color: c, underline, strike, clip } => {
            if text.trim().is_empty() || *size <= 0.0 {
                return;
            }
            let font = fonts[*face].clone();
            with_clip(surface, *clip, |surface| {
                surface.set_stroke(None);
                surface.set_fill(Some(fill(*c)));
                surface.draw_text(Point::from_xy(*x as f32, *y as f32), font, *size as f32, text, false, TextDirection::Auto);
                if *underline || *strike {
                    let w = crate::print_layout::text_width(text, *face, *size);
                    let (asc, _) = ascent_descent(*face, *size);
                    let t = (*size / 16.0).max(0.3);
                    for (on, dy) in [(*underline, size * 0.12), (*strike, -asc * 0.3)] {
                        if on && let Some(p) = rect_path(Rect::new(*x, y + dy, w, t)) {
                            surface.draw_path(&p);
                        }
                    }
                }
            });
        }
        Item::Image { r, data, format, clip } => {
            let bytes: krilla::Data = data.to_vec().into();
            let img = match format {
                PictureFormat::Png => krilla::image::Image::from_png(bytes, true),
                PictureFormat::Jpeg => krilla::image::Image::from_jpeg(bytes, true),
                PictureFormat::Gif => krilla::image::Image::from_gif(bytes, true),
                PictureFormat::Webp => krilla::image::Image::from_webp(bytes, true),
                _ => return,
            };
            let (Ok(img), Some(size)) = (img, Size::from_wh(r.w.max(0.1) as f32, r.h.max(0.1) as f32)) else { return };
            with_clip(surface, *clip, |surface| {
                surface.push_transform(&Transform::from_translate(r.x as f32, r.y as f32));
                surface.draw_image(img, size);
                surface.pop();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print_layout::{DocInfo, layout};
    use xl_engine::{Addr, CellRef, Workbook};

    fn sample() -> Workbook {
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let mut items = vec![(Addr::new(s, CellRef::new(0, 0)), "Quarterly Report".to_string())];
        for r in 1..80u32 {
            items.push((Addr::new(s, CellRef::new(r, 0)), format!("Line {r}")));
            items.push((Addr::new(s, CellRef::new(r, 1)), format!("{}", r as f64 * 1.5)));
        }
        wb.set_inputs(items);
        wb.set_format(Addr::new(s, CellRef::new(1, 1)), Some("0.00"));
        wb.set_style(Addr::new(s, CellRef::new(0, 0)), &xl_engine::Style { bold: true, fill: Some(0xFFFF00), ..Default::default() });
        let p = &mut wb.layout_mut(s).unwrap().page;
        p.header = "&CConfidential".into();
        p.footer = "&RPage &P of &N".into();
        wb
    }

    #[test]
    fn exports_pages_with_real_text() {
        let wb = sample();
        let doc = layout(&wb, &[wb.sheet_ids()[0]], &DocInfo::default());
        assert_eq!(doc.pages.len(), 2);
        let bytes = render(&doc, "Test").unwrap();
        assert!(bytes.starts_with(b"%PDF-"));
        let parsed = lopdf::Document::load_mem(&bytes).unwrap();
        assert_eq!(parsed.get_pages().len(), 2);
        let first = parsed.extract_text(&[1]).unwrap();
        for want in ["Quarterly Report", "Line 1", "1.50", "Confidential", "Page 1 of 2"] {
            assert!(first.contains(want), "{want:?} not in page 1 text: {first:?}");
        }
        let second = parsed.extract_text(&[2]).unwrap();
        assert!(second.contains("Page 2 of 2") && second.contains("Line 79"), "{second:?}");
    }

    #[test]
    fn charts_and_landscape_pages_render() {
        let mut wb = sample();
        let s = wb.sheet_ids()[0];
        wb.layout_mut(s).unwrap().page.orientation = xl_engine::page::Orientation::Landscape;
        wb.layout_mut(s).unwrap().page.fit = Some((1, 1));
        let values = xl_engine::formula::parse_formula("Sheet1!$B$2:$B$10", wb.sheets()).unwrap();
        let series = vec![xl_engine::chart::ChartSeries::new(None, None, std::sync::Arc::new(values))];
        let chart = xl_engine::chart::Chart { title: Some("Totals".into()), ..xl_engine::chart::Chart::new(xl_engine::chart::ChartKind::Column, series, CellRef::new(2, 3), (400.0, 250.0)) };
        wb.layout_mut(s).unwrap().charts.push(chart);
        let doc = layout(&wb, &[s], &DocInfo::default());
        assert_eq!(doc.pages.len(), 1);
        assert!(doc.pages[0].width > doc.pages[0].height);
        let bytes = render(&doc, "Chart").unwrap();
        let parsed = lopdf::Document::load_mem(&bytes).unwrap();
        assert!(parsed.extract_text(&[1]).unwrap().contains("Totals"));
    }

    /// `XL_PDF_DUMP=out.pdf cargo test -p xl dump_sample_pdf` writes a
    /// sample with most features, for looking at.
    #[test]
    fn dump_sample_pdf() {
        let Ok(out) = std::env::var("XL_PDF_DUMP") else { return };
        use xl_engine::{Border, HAlign, Style};
        let mut wb = Workbook::new();
        let s = wb.sheet_ids()[0];
        let a = |x: &str| Addr::new(s, CellRef::parse(x).unwrap());
        let mut items = vec![(a("A1"), "Sales report".to_string()), (a("A3"), "Region".into()), (a("B3"), "Q1".into()), (a("C3"), "Q2".into()), (a("D3"), "Notes".into())];
        for (i, (r, q1, q2)) in [("North", 1200.5, 1350.0), ("South", 980.0, 1010.25), ("East", 1500.0, 1499.0), ("West", 700.0, 912.0)].iter().enumerate() {
            let row = 4 + i;
            items.push((a(&format!("A{row}")), r.to_string()));
            items.push((a(&format!("B{row}")), q1.to_string()));
            items.push((a(&format!("C{row}")), q2.to_string()));
        }
        items.push((a("D4"), "A long note that should wrap inside its cell nicely".into()));
        items.push((a("A9"), "Overflowing text runs into the empty cells to the right".into()));
        items.push((a("B8"), "=SUM(B4:B7)".into()));
        wb.set_inputs(items);
        for c in ["B4", "B5", "B6", "B7", "C4", "C5", "C6", "C7", "B8"] {
            wb.set_format(a(c), Some("$#,##0.00"));
        }
        wb.set_style(a("A1"), &Style { bold: true, font_size: Some(160), ..Default::default() });
        for c in ["A3", "B3", "C3", "D3"] {
            wb.set_style(a(c), &Style { bold: true, fill: Some(0x4472C4), color: Some(0xFFFFFF), bottom: Border::THIN, align: HAlign::Center, ..Default::default() });
        }
        wb.set_style(a("D4"), &Style { wrap: true, italic: true, ..Default::default() });
        wb.set_row_height(s, 3, Some(60.0));
        wb.layout_mut(s).unwrap().merges.push(xl_engine::Range::new(CellRef::parse("A1").unwrap(), CellRef::parse("D1").unwrap()));
        let p = &mut wb.layout_mut(s).unwrap().page;
        p.gridlines = true;
        p.headings = true;
        p.header = "&L&A&CConfidential&R&D".into();
        p.footer = "&CPage &P of &N".into();
        let values = xl_engine::formula::parse_formula("Sheet1!$B$4:$B$7", wb.sheets()).unwrap();
        let cats = xl_engine::formula::parse_formula("Sheet1!$A$4:$A$7", wb.sheets()).unwrap();
        let series = vec![xl_engine::chart::ChartSeries::new(None, Some(std::sync::Arc::new(cats)), std::sync::Arc::new(values))];
        wb.layout_mut(s).unwrap().charts.push(xl_engine::chart::Chart {
            title: Some("Q1 by region".into()),
            ..xl_engine::chart::Chart::new(xl_engine::chart::ChartKind::Column, series, CellRef::parse("A12").unwrap(), (420.0, 260.0))
        });
        let doc = layout(&wb, &[s], &DocInfo { date: "2026-11-15".into(), ..Default::default() });
        std::fs::write(out, render(&doc, "Sample").unwrap()).unwrap();
    }
}
