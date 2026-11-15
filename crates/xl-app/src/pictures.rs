//! Pictures on the sheet (read-only): where they go on screen, and the
//! decoded images to paint.
//!
//! Each picture's bytes become one `gpui::Image` (hashed once, kept here).
//! Painting asks GPUI's asset cache for its decoded `RenderImage`
//! (`Image::use_render_image`): decoding runs in the background the first
//! time, the view is redrawn when it's ready, and later frames reuse it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, BorderStyle, Bounds, Pixels};
use xl_engine::picture::{Picture, PictureFormat};

use crate::app::Spreadsheet;
use crate::paint::{Op, rect};

/// GPUI images for picture bytes, keyed by the bytes' address. The bytes
/// are held too, so an address can't be reused for other bytes while cached.
#[derive(Default)]
pub struct PictureCache(RefCell<HashMap<usize, Cached>>);

/// Picture bytes and the GPUI image made from them.
type Cached = (Arc<[u8]>, Arc<gpui::Image>);

fn gpui_format(f: PictureFormat) -> Option<gpui::ImageFormat> {
    Some(match f {
        PictureFormat::Png => gpui::ImageFormat::Png,
        PictureFormat::Jpeg => gpui::ImageFormat::Jpeg,
        PictureFormat::Gif => gpui::ImageFormat::Gif,
        PictureFormat::Bmp => gpui::ImageFormat::Bmp,
        PictureFormat::Tiff => gpui::ImageFormat::Tiff,
        PictureFormat::Webp => gpui::ImageFormat::Webp,
        PictureFormat::Other => return None,
    })
}

impl PictureCache {
    /// The GPUI image for a picture; None for formats xl can't draw.
    pub fn image(&self, p: &Picture) -> Option<Arc<gpui::Image>> {
        let format = gpui_format(p.format)?;
        let key = p.data.as_ptr() as usize;
        let mut map = self.0.borrow_mut();
        let entry = map.entry(key).or_insert_with(|| (p.data.clone(), Arc::new(gpui::Image::from_bytes(format, p.data.to_vec()))));
        Some(entry.1.clone())
    }

    /// Forget images no workbook (or undo history) uses any more, and drop
    /// their decoded copies from GPUI's asset cache.
    pub fn prune(view: &gpui::Entity<Spreadsheet>, cx: &mut App) {
        let cache = &view.read(cx).picture_cache;
        let mut stale = Vec::new();
        cache.0.borrow_mut().retain(|_, (bytes, image)| {
            let used = Arc::strong_count(bytes) > 1;
            if !used {
                stale.push(image.clone());
            }
            used
        });
        for image in stale {
            image.remove_asset(cx);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }
}

impl Spreadsheet {
    /// Screen position and size of a picture: (x, y, w, h) in window pixels.
    pub(crate) fn picture_rect(&self, p: &Picture) -> (f64, f64, f64, f64) {
        let x = f64::from(self.viewport.origin.x) + self.hw() + self.grid_x(p.anchor.col) + p.offset.0;
        let y = f64::from(self.viewport.origin.y) + self.hh() + self.grid_y(p.anchor.row) + p.offset.1;
        // Two-cell anchors stretch with the lines they span (and shrink over hidden ones).
        let (w, h) = match p.end {
            Some((end, (ex, ey))) => {
                ((self.col_x(end.col) + ex - self.col_x(p.anchor.col) - p.offset.0).max(0.0), (self.row_y(end.row) + ey - self.row_y(p.anchor.row) - p.offset.1).max(0.0))
            }
            None => p.size,
        };
        // xl's columns are wider than Excel's, relative to its rows; draw the
        // picture at its own proportions rather than stretched sideways.
        let squash = xl_io::from_excel_height(0.75) / xl_io::from_excel_width_px(1.0);
        (x, y, w * squash, h)
    }

    /// Paint operations for the current sheet's pictures, back to front,
    /// each clipped to its pane.
    pub(crate) fn picture_ops(&self, clip_of: &dyn Fn(u32, u32) -> Bounds<Pixels>) -> Vec<Op> {
        let t = self.theme;
        let mut ops = Vec::new();
        for p in &self.wb.layout(self.cur).pictures {
            let (x, y, w, h) = self.picture_rect(p);
            if w < 1.0 || h < 1.0 {
                continue;
            }
            let bounds = rect(x, y, w, h);
            let clip = clip_of(p.anchor.row, p.anchor.col);
            if bounds.intersect(&clip).size.width <= Pixels::ZERO || bounds.intersect(&clip).size.height <= Pixels::ZERO {
                continue;
            }
            match self.picture_cache.image(p) {
                Some(image) => ops.push(Op::Image { image, bounds, clip }),
                // A placeholder where an EMF/WMF or similar picture would be.
                None => ops.push(Op::Quad(gpui::quad(bounds, gpui::Corners::default(), t.chrome, gpui::Edges::all(gpui::px(1.)), t.divider, BorderStyle::Dashed), clip)),
            }
        }
        ops
    }
}
