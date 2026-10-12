mod app;
mod archive;
mod bulk_rename;
mod clipboard;
mod drag;
mod format;
mod model;
mod ops;
mod ops_entity;
mod palette;
mod pane;
mod preview;
mod session;
mod settings;
mod terminal;
mod thumbnails;
mod watch;

#[cfg(test)]
mod ui_tests;

use app::WFilesApp;
use gpui_kit::assets::{self, Assets};
use gpui_kit::*;
use std::borrow::Cow;

// 组件默认那套图标里没有"列表/网格"这两个视图图标，
// 而全量图标（AllAssets）会把 1800 个 SVG 全打进二进制（7 MB）。
// 只挑出真正用到的这几个，其余的回落到默认那套。
assets::icon_assets!(ViewIcons, [List, LayoutGrid, Columns3]);

struct WFilesAssets;

impl AssetSource for WFilesAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = ViewIcons.load(path)? {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = ViewIcons.list(path)?;
        all.extend(Assets.list(path)?);
        Ok(all)
    }
}

fn main() {
    let application = gpui_kit::application().with_assets(WFilesAssets);
    application.run(|cx| {
        gpui_kit::init(cx);
        app::init(cx);
        pane::init(cx);

        // 上次退出时的窗口位置尺寸还记得就用上；记不得或者那份已经不可用了
        // （换了显示器、手改坏了配置）就居中开一个默认大小。
        let restored = settings::Settings::load().layout.filter(settings::Layout::usable);
        let window_bounds = match restored {
            Some(layout) => WindowBounds::Windowed(Bounds {
                origin: Point::new(px(layout.x), px(layout.y)),
                size: size(px(layout.width), px(layout.height)),
            }),
            None => WindowBounds::centered(size(px(1180.), px(760.)), cx),
        };

        let window_options = WindowOptions {
            window_bounds: Some(window_bounds),
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, |window, cx| {
            window.set_window_title("WFiles");
            cx.new(|cx| WFilesApp::new(window, cx))
        })
        .expect("无法打开 WFiles 窗口");
    });
}
