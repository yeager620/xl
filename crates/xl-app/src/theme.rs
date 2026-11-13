use gpui::{Hsla, WindowAppearance, rgb, rgba};

/// Design tokens. "Quiet pro tool": neutral graphite chrome, a white (or
/// near-black) canvas for the grid, and one deep-blue accent reserved for
/// selection, focus and active state.
#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    /// The grid and editable surfaces.
    pub canvas: Hsla,
    /// Title bar, headers, bottom bar.
    pub chrome: Hsla,
    /// Hover state on chrome controls.
    pub chrome_hover: Hsla,
    /// Pressed / toggled-on state on chrome controls.
    pub chrome_active: Hsla,
    /// Dividers between chrome regions.
    pub divider: Hsla,
    /// Gridlines.
    pub rule: Hsla,
    pub ink: Hsla,
    pub ink_muted: Hsla,
    pub ink_faint: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    /// Selected range fill.
    pub selection: Hsla,
    /// Row/column header of the selection.
    pub header_selected: Hsla,
    pub error: Hsla,
    pub scrollbar: Hsla,
    pub popover: Hsla,
    /// Formula reference highlight colors.
    pub ref_colors: [Hsla; 6],
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex: u32) -> Hsla {
    rgba(hex).into()
}

impl Theme {
    pub fn for_appearance(a: WindowAppearance) -> Self {
        match a {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            _ => Self::light(),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            canvas: c(0xFFFFFF),
            chrome: c(0xF1F2F4),
            chrome_hover: c(0xE4E6EA),
            chrome_active: c(0xD9DCE2),
            divider: c(0xD7DADF),
            rule: c(0xE4E6E9),
            ink: c(0x1E2024),
            ink_muted: c(0x6A6F78),
            ink_faint: c(0x9AA0A9),
            accent: c(0x1F5FBF),
            on_accent: c(0xFFFFFF),
            selection: ca(0x1F5FBF1F),
            header_selected: c(0xDCE4F2),
            error: c(0xC4271B),
            scrollbar: ca(0x1E202440),
            popover: c(0xFFFFFF),
            ref_colors: [c(0x1F5FBF), c(0x1E8449), c(0x8E44AD), c(0xC0661A), c(0xC2185B), c(0x00838F)],
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            canvas: c(0x1D1E21),
            chrome: c(0x26282C),
            chrome_hover: c(0x303237),
            chrome_active: c(0x3A3D43),
            divider: c(0x37393F),
            rule: c(0x2E3035),
            ink: c(0xE3E5E9),
            ink_muted: c(0x9196A0),
            ink_faint: c(0x666B74),
            accent: c(0x6EA2F2),
            on_accent: c(0x0F1A2C),
            selection: ca(0x6EA2F229),
            header_selected: c(0x2F3A4C),
            error: c(0xF2766B),
            scrollbar: ca(0xE3E5E94D),
            popover: c(0x2B2D32),
            ref_colors: [c(0x6EA2F2), c(0x5CC98A), c(0xC39BE0), c(0xE9A15E), c(0xF07DA8), c(0x4DC9D4)],
        }
    }
}
