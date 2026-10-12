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

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, |window, cx| {
            window.set_window_title("WFiles");
            cx.new(|cx| WFilesApp::new(window, cx))
        })
        .expect("无法打开 WFiles 窗口");
    });
}
