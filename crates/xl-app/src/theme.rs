use gpui::{Hsla, WindowAppearance, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg: Hsla,
    pub surface: Hsla,
    pub border: Hsla,
    pub header_bg: Hsla,
    pub header_active_bg: Hsla,
    pub header_fg: Hsla,
    pub header_active_fg: Hsla,
    pub grid: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub selection: Hsla,
    pub editor_bg: Hsla,
    pub error: Hsla,
    pub scrollbar: Hsla,
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
            bg: c(0xFFFFFF),
            surface: c(0xF7F7F8),
            border: c(0xE4E4E7),
            header_bg: c(0xF6F6F7),
            header_active_bg: c(0xE6E8FF),
            header_fg: c(0x8A8A93),
            header_active_fg: c(0x3B3FC4),
            grid: c(0xEDEDF0),
            text: c(0x1A1A1E),
            muted: c(0x75757E),
            accent: c(0x4F52E5),
            accent_fg: c(0xFFFFFF),
            selection: ca(0x4F52E51A),
            editor_bg: c(0xFFFFFF),
            error: c(0xD92D20),
            scrollbar: ca(0x00000030),
            ref_colors: [c(0x2563EB), c(0x16A34A), c(0x9333EA), c(0xEA580C), c(0xDB2777), c(0x0D9488)],
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: c(0x141417),
            surface: c(0x1B1B1F),
            border: c(0x2B2B31),
            header_bg: c(0x1B1B1F),
            header_active_bg: c(0x2C2B55),
            header_fg: c(0x7C7C86),
            header_active_fg: c(0xC7CBFF),
            grid: c(0x24242A),
            text: c(0xEBEBEF),
            muted: c(0x8C8C96),
            accent: c(0x8487FF),
            accent_fg: c(0x0F0F14),
            selection: ca(0x8487FF24),
            editor_bg: c(0x1E1E23),
            error: c(0xFF6B5E),
            scrollbar: ca(0xFFFFFF33),
            ref_colors: [c(0x60A5FA), c(0x4ADE80), c(0xC084FC), c(0xFB923C), c(0xF472B6), c(0x2DD4BF)],
        }
    }
}
