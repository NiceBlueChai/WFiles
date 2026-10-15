mod analyze;
mod analyze_entity;
mod app;
mod archive;
mod bulk_rename;
mod cli;
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
mod instance;
mod keymap;
mod logfile;
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

fn main() {
    // 命令行认得的东西全在 `cli` 里：语言，以及"这一次要开到哪儿"。
    // 语言要在读设置**之前**定下来，优先级是「命令行 > 设置 > 系统环境 > 中文」。
    // 存在进程级而不是顺手 `Settings::update` 写进去：那一刻 `Settings`
    // 这个 `Global` 还没装上（装它的是 `WFilesApp::new`，在 open_window 回调里），
    // 而写进设置文件又意味着"跑一次 `--lang=en` 把用户的偏好改了"。
    //
    // 命令行最靠前，因为它说的是"这一次启动我要什么"——一个脚本、一次录屏演示，
    // 不该因为配置文件里存着别的语言就跑偏。
    let cli = cli::parse(std::env::args_os().skip(1));
    i18n::set_cli_locale(cli.locale);

    // 位子只有一个：抢到的那一份当主实例，负责听后面来的启动。
    // 抢不到说明已经开着一份（锁是进程活着期间一直拿着的，崩溃也会放）。
    let mut claim = instance::claim();
    // 已经开着一份，而这一次启动又有话要说（要开哪儿）——交给它去办，
    // 说完自己退，用户得到的是"那扇窗跳过去"而不是第二扇一模一样的窗。
    // 送不到（对面正开着还没挂门牌、正在退、版本对不上）就走下面的老路：
    // 自己开窗。**不能**把用户敲的那条路径默默吞掉。
    if claim.is_none() && instance::hand_off(&cli) {
        return;
    }

    let application = gpui_kit::application().with_assets(WFilesAssets);
    // `move`：这一段要活到应用退出，里面用到的 `cli`、`claim` 都得跟着走
    // （`gpui_kit::application().run` 收的是 `'static` 闭包）。
    application.run(move |cx| {
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
            // 命令行说开到哪儿，就落在**这一次**开出来的那一栏上；
            // 没给就照上次的会话恢复（那是 `WFilesApp::new` 的默认走法）。
            let app = cx.new(|cx| WFilesApp::new(window, cx, cli.launch));
            // 主实例：门牌现在就挂出去，后面来的启动都找得到这扇窗。
            // 挂门牌失败（端口绑不上、状态目录写不了）就不做转发，窗口照开。
            if let Some(claim) = claim.take() {
                app.update(cx, |app, cx| app.serve_forwarding(claim, window, cx));
            }
            app
        })
        .expect("无法打开 WFiles 窗口");
    });
}
