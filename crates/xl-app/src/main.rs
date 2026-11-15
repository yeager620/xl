mod app;
mod assets;
mod bigdata_ui;
#[cfg(test)]
mod bigdata_tests;
mod chart_paint;
mod charts;
mod chrome;
mod complete;
mod cond_ui;
mod data;
mod fill;
mod find;
mod home;
mod ime;
mod lifecycle;
mod notes_ui;
mod outline;
mod palette;
mod pictures;
mod paint;
mod pivot_ui;
mod recent;
mod refs;
mod solver_ui;
mod templates;
#[cfg(test)]
mod tests;
mod theme;
mod validation_ui;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size};

use crate::app::Spreadsheet;

/// UI and cell typeface. Source Sans 3's default figures are tabular, so
/// columns of numbers line up.
pub const FONT: &str = "Source Sans 3";

pub fn load_fonts(cx: &App) {
    cx.text_system()
        .add_fonts(vec![
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-Regular.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-It.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-Semibold.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-SemiboldIt.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-Bold.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/SourceSans3-BoldIt.ttf").as_slice()),
        ])
        .expect("failed to load bundled fonts");
}

fn main() {
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_platform::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        load_fonts(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("xl".into()), ..Default::default() }),
                app_id: Some("io.github.yeager620.xl".into()),
                window_min_size: Some(size(px(560.), px(360.))),
                // GNOME on Wayland doesn't draw window frames; we draw our own
                // title bar (falls back to the system's where it provides one).
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Spreadsheet::new(path, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
