mod app;
mod bulk_rename;
mod clipboard;
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
use gpui_kit::assets::Assets;
use gpui_kit::*;

fn main() {
    let application = gpui_kit::application().with_assets(Assets);
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
