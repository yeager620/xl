//! Cell styles: font emphasis, colors, fill, borders and alignment.
//! (Number formats are kept separately; see `format`.)

/// 0xRRGGBB
pub type Rgb = u32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HAlign {
    /// Text left, numbers right (Excel's default).
    #[default]
    General,
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VAlign {
    /// Excel's default.
    #[default]
    Bottom,
    Center,
    Top,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LineStyle {
    #[default]
    None,
    Thin,
    Medium,
    Thick,
    Dashed,
    Dotted,
    Double,
}

impl LineStyle {
    /// Width in pixels when drawn.
    pub fn width(self) -> f32 {
        match self {
            LineStyle::None => 0.0,
            LineStyle::Thin | LineStyle::Dashed | LineStyle::Dotted => 1.0,
            LineStyle::Medium => 2.0,
            LineStyle::Thick | LineStyle::Double => 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Border {
    pub style: LineStyle,
    /// None = automatic (the theme's text color).
    pub color: Option<Rgb>,
}

impl Border {
    pub const THIN: Border = Border { style: LineStyle::Thin, color: None };

    pub fn is_none(&self) -> bool {
        self.style == LineStyle::None
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Text color; None = automatic.
    pub color: Option<Rgb>,
    /// Background fill; None = no fill.
    pub fill: Option<Rgb>,
    pub align: HAlign,
    pub valign: VAlign,
    /// Wrap text onto multiple lines within the column width.
    pub wrap: bool,
    /// Font size in tenths of a point (110 = 11pt); None = default.
    pub font_size: Option<u16>,
    /// Font family; None = the app's default.
    pub font_name: Option<std::sync::Arc<str>>,
    pub top: Border,
    pub bottom: Border,
    pub left: Border,
    pub right: Border,
}

impl Style {
    pub fn is_default(&self) -> bool {
        *self == Style::default()
    }

    pub fn has_borders(&self) -> bool {
        !(self.top.is_none() && self.bottom.is_none() && self.left.is_none() && self.right.is_none())
    }
}

/// Relative luminance (0..1) of an sRGB color, for picking readable text.
pub fn luminance(rgb: Rgb) -> f32 {
    let lin = |c: u32| {
        let c = c as f32 / 255.0;
        if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin((rgb >> 16) & 0xFF) + 0.7152 * lin((rgb >> 8) & 0xFF) + 0.0722 * lin(rgb & 0xFF)
}

/// Excel's legacy indexed color palette (indices 0..=63).
pub const INDEXED_COLORS: [Rgb; 64] = [
    0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF, //
    0x000000, 0xFFFFFF, 0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00, 0xFF00FF, 0x00FFFF, //
    0x800000, 0x008000, 0x000080, 0x808000, 0x800080, 0x008080, 0xC0C0C0, 0x808080, //
    0x9999FF, 0x993366, 0xFFFFCC, 0xCCFFFF, 0x660066, 0xFF8080, 0x0066CC, 0xCCCCFF, //
    0x000080, 0xFF00FF, 0xFFFF00, 0x00FFFF, 0x800080, 0x800000, 0x008080, 0x0000FF, //
    0x00CCFF, 0xCCFFFF, 0xCCFFCC, 0xFFFF99, 0x99CCFF, 0xFF99CC, 0xCC99FF, 0xFFCC99, //
    0x3366FF, 0x33CCCC, 0x99CC00, 0xFFCC00, 0xFF9900, 0xFF6600, 0x666699, 0x969696, //
    0x003366, 0x339966, 0x003300, 0x333300, 0x993300, 0x993366, 0x333399, 0x333333, //
];

/// Apply an Excel tint (-1..1) to a color, in HSL lightness like Excel does.
pub fn apply_tint(rgb: Rgb, tint: f64) -> Rgb {
    if tint == 0.0 {
        return rgb;
    }
    let (r, g, b) = (((rgb >> 16) & 0xFF) as f64 / 255.0, ((rgb >> 8) & 0xFF) as f64 / 255.0, (rgb & 0xFF) as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let mut l = (max + min) / 2.0;
    let d = max - min;
    let (h, s) = if d == 0.0 {
        (0.0, 0.0)
    } else {
        let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } / 6.0;
        (h, s)
    };
    l = if tint < 0.0 { l * (1.0 + tint) } else { l * (1.0 - tint) + tint };
    let hue = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        (hue(p, q, h + 1.0 / 3.0), hue(p, q, h), hue(p, q, h - 1.0 / 3.0))
    };
    let c = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tints() {
        assert_eq!(apply_tint(0x4472C4, 0.0), 0x4472C4);
        // Excel's "Blue, Accent 1, Lighter 80%" is D9E1F2.
        let light = apply_tint(0x4472C4, 0.799_981_688_894_314_2);
        let close = |a: u32, b: u32| (0..3).all(|i| ((a >> (i * 8)) & 0xFF).abs_diff((b >> (i * 8)) & 0xFF) <= 2);
        assert!(close(light, 0xD9E1F2), "{light:06X}");
        assert_eq!(apply_tint(0xFFFFFF, -0.5), 0x808080);
    }

    #[test]
    fn luminance_orders_colors() {
        assert!(luminance(0xFFFFFF) > 0.99);
        assert!(luminance(0x000000) < 0.01);
        assert!(luminance(0xFFFF00) > luminance(0x0000FF));
    }
}
