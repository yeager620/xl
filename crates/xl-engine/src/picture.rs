//! Pictures (images) placed on a sheet. Read-only: xl shows them where the
//! file puts them and moves them with inserted/deleted lines; the image
//! itself is kept byte for byte.

use std::sync::Arc;

use crate::address::{CellRef, SheetId};
use crate::formula::Shift;

/// The image's encoding, from its leading bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PictureFormat {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Tiff,
    Webp,
    /// Something xl can't draw (EMF, WMF, …).
    Other,
}

impl PictureFormat {
    pub fn sniff(bytes: &[u8]) -> PictureFormat {
        match bytes {
            [0x89, b'P', b'N', b'G', ..] => PictureFormat::Png,
            [0xFF, 0xD8, 0xFF, ..] => PictureFormat::Jpeg,
            [b'G', b'I', b'F', b'8', ..] => PictureFormat::Gif,
            [b'B', b'M', ..] => PictureFormat::Bmp,
            [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => PictureFormat::Tiff,
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => PictureFormat::Webp,
            _ => PictureFormat::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Picture {
    /// The cell the top-left corner sits in, and the offset within it (pixels).
    pub anchor: CellRef,
    pub offset: (f64, f64),
    /// Two-cell anchors: the cell of the bottom-right corner and the offset
    /// within it. The picture stretches with the lines between, as it does
    /// in the saved file.
    pub end: Option<(CellRef, (f64, f64))>,
    /// Width and height in pixels as read (used when there's no `end`).
    pub size: (f64, f64),
    /// The encoded image, shared by every picture showing the same file.
    pub data: Arc<[u8]>,
    pub format: PictureFormat,
    pub name: String,
    /// Alternative text (the picture's description).
    pub alt: Option<String>,
}

impl Picture {
    /// Follow inserted/deleted lines on its own sheet. Corners on deleted
    /// lines stay where the lines closed up (as charts do).
    pub fn apply_shift(&mut self, op: Shift, home: SheetId) {
        if op.sheet() != home {
            return;
        }
        let rows = matches!(op, Shift::InsertRows { .. } | Shift::DeleteRows { .. });
        let map = |pos: u32| {
            op.map_pos(pos).unwrap_or(match op {
                Shift::DeleteRows { at, .. } | Shift::DeleteCols { at, .. } => at,
                _ => pos,
            })
        };
        let shift = |c: &mut CellRef| {
            if rows {
                c.row = map(c.row);
            } else {
                c.col = map(c.col);
            }
        };
        shift(&mut self.anchor);
        if let Some((end, _)) = &mut self.end {
            shift(end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_formats() {
        assert_eq!(PictureFormat::sniff(b"\x89PNG\r\n\x1a\n"), PictureFormat::Png);
        assert_eq!(PictureFormat::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), PictureFormat::Jpeg);
        assert_eq!(PictureFormat::sniff(b"GIF89a"), PictureFormat::Gif);
        assert_eq!(PictureFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "), PictureFormat::Webp);
        assert_eq!(PictureFormat::sniff(&[0x01, 0, 0, 0]), PictureFormat::Other);
    }
}
