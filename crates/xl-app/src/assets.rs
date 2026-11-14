//! Bundled assets: Lucide icons (ISC, see assets/icons/LICENSE-lucide.txt).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

fn icon(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "icons/align-center.svg" => include_bytes!("../../../assets/icons/align-center.svg"),
        "icons/align-left.svg" => include_bytes!("../../../assets/icons/align-left.svg"),
        "icons/align-right.svg" => include_bytes!("../../../assets/icons/align-right.svg"),
        "icons/wrap-text.svg" => include_bytes!("../../../assets/icons/wrap-text.svg"),
        "icons/chevron-up.svg" => include_bytes!("../../../assets/icons/chevron-up.svg"),
        "icons/check.svg" => include_bytes!("../../../assets/icons/check.svg"),
        "icons/highlighter.svg" => include_bytes!("../../../assets/icons/highlighter.svg"),
        "icons/table-cells-merge.svg" => include_bytes!("../../../assets/icons/table-cells-merge.svg"),
        "icons/baseline.svg" => include_bytes!("../../../assets/icons/baseline.svg"),
        "icons/bold.svg" => include_bytes!("../../../assets/icons/bold.svg"),
        "icons/chevron-down.svg" => include_bytes!("../../../assets/icons/chevron-down.svg"),
        "icons/command.svg" => include_bytes!("../../../assets/icons/command.svg"),
        "icons/copy.svg" => include_bytes!("../../../assets/icons/copy.svg"),
        "icons/decimals-arrow-left.svg" => include_bytes!("../../../assets/icons/decimals-arrow-left.svg"),
        "icons/decimals-arrow-right.svg" => include_bytes!("../../../assets/icons/decimals-arrow-right.svg"),
        "icons/dollar-sign.svg" => include_bytes!("../../../assets/icons/dollar-sign.svg"),
        "icons/grid-2x2.svg" => include_bytes!("../../../assets/icons/grid-2x2.svg"),
        "icons/italic.svg" => include_bytes!("../../../assets/icons/italic.svg"),
        "icons/menu.svg" => include_bytes!("../../../assets/icons/menu.svg"),
        "icons/minus.svg" => include_bytes!("../../../assets/icons/minus.svg"),
        "icons/paint-bucket.svg" => include_bytes!("../../../assets/icons/paint-bucket.svg"),
        "icons/panel-bottom.svg" => include_bytes!("../../../assets/icons/panel-bottom.svg"),
        "icons/panel-top.svg" => include_bytes!("../../../assets/icons/panel-top.svg"),
        "icons/percent.svg" => include_bytes!("../../../assets/icons/percent.svg"),
        "icons/plus.svg" => include_bytes!("../../../assets/icons/plus.svg"),
        "icons/redo-2.svg" => include_bytes!("../../../assets/icons/redo-2.svg"),
        "icons/search.svg" => include_bytes!("../../../assets/icons/search.svg"),
        "icons/sheet.svg" => include_bytes!("../../../assets/icons/sheet.svg"),
        "icons/sigma.svg" => include_bytes!("../../../assets/icons/sigma.svg"),
        "icons/square.svg" => include_bytes!("../../../assets/icons/square.svg"),
        "icons/square-function.svg" => include_bytes!("../../../assets/icons/square-function.svg"),
        "icons/strikethrough.svg" => include_bytes!("../../../assets/icons/strikethrough.svg"),
        "icons/table-2.svg" => include_bytes!("../../../assets/icons/table-2.svg"),
        "icons/underline.svg" => include_bytes!("../../../assets/icons/underline.svg"),
        "icons/undo-2.svg" => include_bytes!("../../../assets/icons/undo-2.svg"),
        "icons/x.svg" => include_bytes!("../../../assets/icons/x.svg"),
        _ => return None,
    })
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(icon(path).map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
