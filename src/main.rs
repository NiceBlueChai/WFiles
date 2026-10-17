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

use app::{Starting, WFilesApp};
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

    // 问用法、问版本是纯粹的提问：不开窗、不碰实例位子，答完就走（退出码 0）。
    // **排在「两句话打架」之前**：用户没打算开窗，他那两句话也就没真的打架，
    // 而当面答他要的东西比数落一句更有用。再往后一步就是抢位子（`instance::claim`）——
    // 排在这里也意味着这两个开关**不会**被转发给已经开着的那一扇。
    if let Some(question) = cli.question() {
        match question {
            cli::Question::Help => {
                let locale = settings::Settings::load().locale();
                println!("{}", i18n::tr_in(locale, cli::USAGE));
            }
            cli::Question::Version => println!("{}", cli::VERSION),
        }
        return;
    }

    // 命令行自己打架（`--new-window` 与 `--new-tab` 一起给）：说清楚，别开窗。
    // 这句话说给谁看，也照上面那条优先级来（`--lang` > 设置 > 环境）——
    // 英文的机器上不该冒出一句中文用法错误。
    if let Some(problem) = cli.problem {
        let locale = settings::Settings::load().locale();
        eprintln!("{}", i18n::tr_in(locale, problem));
        eprintln!("{}", i18n::tr_in(locale, cli::USAGE));
        std::process::exit(2);
    }

    // 位子只有一个：抢到的那一份当主实例，负责听后面来的启动。
    // 抢不到说明已经开着一份（锁是进程活着期间一直拿着的，崩溃也会放）。
    let mut claim = instance::claim();
    // 抢到位子即主窗：会话、窗口布局、崩溃恢复现场都归它写。
    // 没抢到的那一扇是副窗，**只读不写**（理由见 `app::Starting`）。
    // 判据是"抢到没有"而不是"谁是第一个"：没抢到就一定另有一份活着，
    // 那一份正管着这些文件——这也正是"送不到就自己开窗"那一条退路
    // 该走的答案（那一扇也不该去动别人的会话）。
    let primary = claim.is_some();
    // 已经开着一份，而这一次启动又有话要说（要开哪儿）——交给它去办，
    // 说完自己退，用户得到的是"那扇窗跳过去"而不是第二扇一模一样的窗。
    // 送不到（对面正开着还没挂门牌、正在退、版本对不上）就走下面的老路：
    // 自己开窗。**不能**把用户敲的那条路径默默吞掉。
    //
    // `--new-window` 是这条规则的例外：它要的就是自己开一扇，
    // 所以连问都不问——问成了反而是把用户正看着的那扇窗挪走。
    if claim.is_none() && !cli.new_window && instance::hand_off(&cli) {
        return;
    }
    if claim.is_none() && cli.new_window {
        // 给排查看留一句：这一次的窗是从哪儿来的。
        logfile::info("启动：--new-window 要自己开一扇，不转发");
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
        let mut bounds = match restored {
            Some(layout) => Bounds {
                origin: Point::new(px(layout.x), px(layout.y)),
                size: size(px(layout.width), px(layout.height)),
            },
            None => Bounds::centered(None, size(px(1180.), px(760.)), cx),
        };
        // 副窗从主窗的位置往右下错开一步。不错开的话两扇窗一模一样地叠着，
        // 用户按了 `--new-window` 看到的却是"什么都没发生"——新窗其实开好了，
        // 就压在旧窗后面。错开之后还要保证没被推出屏幕（见 `Layout::cascaded`）。
        if !primary {
            bounds = offset_for_secondary(bounds, cx);
        }
        let window_bounds = WindowBounds::Windowed(bounds);

        let window_options = WindowOptions {
            window_bounds: Some(window_bounds),
            ..Default::default()
        };

        gpui_kit::open_window(window_options, cx, |window, cx| {
            window.set_window_title("WFiles");
            // 命令行说开到哪儿，就落在**这一次**开出来的那一栏上；
            // 没给就照上次的会话恢复——除非这一扇是 `--new-window` 要的
            // "新"窗，那它就从主页起。
            let app = cx.new(|cx| {
                // 抢到位子的是主窗：会话、布局、恢复现场都归它。
                // 没抢到的那一扇是副窗，**只读不写**（见 `Starting`）；
                // 命令行明说"自己开一扇"时它还要一扇干净的窗——不恢复会话。
                let starting = if primary {
                    Starting::primary(cli.launch)
                } else {
                    Starting {
                        launch: cli.launch,
                        fresh: cli.new_window,
                        primary: false,
                    }
                };
                WFilesApp::new(window, cx, starting)
            });
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

/// 副窗开在哪儿：主窗那块屏幕上的可见范围里，往右下错开一步。
///
/// 屏幕信息只有平台层知道，所以这块算术在 `main` 里取、在
/// [`settings::Layout::cascaded`] 里算（那边是纯的，有测试）。
fn offset_for_secondary(bounds: Bounds<Pixels>, cx: &mut App) -> Bounds<Pixels> {
    let width = bounds.size.width.as_f32();
    let height = bounds.size.height.as_f32();
    let base = settings::Layout {
        x: bounds.origin.x.as_f32(),
        y: bounds.origin.y.as_f32(),
        width,
        height,
    };
    // 主窗中心落在哪块屏幕上，新窗就开在哪块屏幕上——多屏时把副窗开到
    // 另一块屏上，用户得先去把它找出来。找不到（屏幕信息拿不到）就用第一块。
    let center = Point::new(px(base.x + width / 2.), px(base.y + height / 2.));
    let displays = cx.displays();
    let visible = displays
        .iter()
        .find(|display| display.bounds().contains(&center))
        .or_else(|| displays.first())
        .map(|display| {
            let visible = display.visible_bounds();
            (
                visible.origin.x.as_f32(),
                visible.origin.y.as_f32(),
                visible.size.width.as_f32(),
                visible.size.height.as_f32(),
            )
        });

    let stepped = base.cascaded(visible);
    Bounds {
        origin: Point::new(px(stepped.x), px(stepped.y)),
        size: bounds.size,
    }
}
