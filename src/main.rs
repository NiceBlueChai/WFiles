mod analyze;
mod analyze_entity;
mod app;
mod archive;
mod bulk_rename;
mod clipboard;
mod cloud;
mod compare;
mod compare_entity;
mod diff;
mod diff_entity;
mod drag;
mod format;
mod ftp;
mod i18n;
mod keymap;
mod marks;
mod meta;
mod model;
mod offline;
mod ops;
mod ops_entity;
mod palette;
mod pane;
// 性能基准只在测试构建里存在：里面的 `FileEntry::for_test` 是
// `#[cfg(test)]` 的，非 test 构建会报"找不到这个函数"。
#[cfg(test)]
mod perf;
mod preview;
mod query;
mod remote;
mod remotes;
mod restore;
mod resume;
mod s3;
mod search;
mod search_entity;
mod session;
mod settings;
mod sftp;
mod term;
mod term_entity;
mod terminal;
mod throttle;
mod thumbnails;
mod volumes;
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

/// 命令行里认得的参数，处理完剩下的（路径之类）可以直接忽略。
///
/// **只认 `--lang`，不引一套参数解析库**：全应用就这一个参数，
/// 而参数解析器会带来它自己的报错格式、`--help` 排版和一批 flags——
/// 为一件事装一整套框架不划算。看不懂的参数**默默忽略**而不是报错：
/// 从文件管理器或别的程序启动时，参数不由用户控制，报错退出是帮倒忙。
fn read_cli_locale() -> Option<i18n::Locale> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        // `--lang=en` 与 `--lang en` 两种写法都认。
        if let Some(code) = arg.strip_prefix("--lang=") {
            return i18n::Locale::parse(code);
        }
        if arg == "--lang" {
            return args.next().and_then(|code| i18n::Locale::parse(&code));
        }
    }
    None
}

fn main() {
    // 语言定在读设置**之前**记下来，优先级是「命令行 > 设置 > 系统环境 > 中文」。
    // 存在进程级而不是顺手 `Settings::update` 写进去：那一刻 `Settings`
    // 这个 `Global` 还没装上（装它的是 `WFilesApp::new`，在 open_window 回调里），
    // 而写进设置文件又意味着"跑一次 `--lang=en` 把用户的偏好改了"。
    //
    // 命令行最靠前，因为它说的是"这一次启动我要什么"——一个脚本、一次录屏演示，
    // 不该因为配置文件里存着别的语言就跑偏。
    i18n::set_cli_locale(read_cli_locale());
    let application = gpui_kit::application().with_assets(WFilesAssets);
    application.run(|cx| {
        gpui_kit::init(cx);
        app::init(cx);
        pane::init(cx);

        // 上次异常退出（断电、崩溃）留下的续传暂存没人收：开机扫一遍。
        // 这一趟在开窗之前，慢慢扫也不占界面线程。
        resume::journal().sweep();

        // 上次退出时的窗口位置尺寸还记得就用上；记不得或者那份已经不可用了
        // （换了显示器、手改坏了配置）就居中开一个默认大小。
        let restored = settings::Settings::load()
            .layout
            .filter(settings::Layout::usable);
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
