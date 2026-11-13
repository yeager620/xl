mod refs;
mod sheet_view;
mod theme;

use std::borrow::Cow;
use std::path::PathBuf;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

use crate::sheet_view::Spreadsheet;

fn main() {
    let path = std::env::args().nth(1).map(PathBuf::from);
    gpui_platform::application().run(move |cx: &mut App| {
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-Regular.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-Medium.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf").as_slice()),
            ])
            .expect("failed to load bundled fonts");

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
                app_id: Some("dev.xl.Xl".into()),
                window_min_size: Some(size(px(480.), px(320.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Spreadsheet::new(path, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
