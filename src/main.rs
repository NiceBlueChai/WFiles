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

// **用到什么图标，就得打包什么图标**——没打包的会渲染成一片空白
// （`AssetSource::load` 报错，元素照画、只是没有内容，不崩不报）。
//
// 组件默认那套是 106 个，底下这些都不在里面：视图切换的「列表/网格/分列」，
// 以及 2026-10-18 一次审计查出来的 19 个（工具栏的新建文件夹/剪切/粘贴、
// 侧栏的个人文件夹/桌面/下载、菜单的移到回收站…，全中招）。
// 护栏是 `asset_tests::every_referenced_icon_is_packed`：它扫源码，
// 少写一个就红——所以**加图标只改这一处**，别去别处找第二份清单。
//
// 不用全量图标（`AllAssets`）：1800 个 SVG 会给二进制加 7 MB，
// 而真正在用的就这几十个。
assets::icon_assets!(
    AppIcons,
    [
        // 视图切换（组件默认集里没有）
        List,
        LayoutGrid,
        Columns3,
        // 侧栏与位置
        House,
        Monitor,
        Download,
        Cloud,
        // 文件操作
        FolderPlus,
        Scissors,
        ClipboardPaste,
        SquarePen,
        TextCursorInput,
        Trash,
        Link,
        Signature,
        FileArchive,
        SearchX,
        // 面板与状态
        FileDiff,
        Columns2,
        X,
        Command,
        Terminal,
    ]
);

struct WFilesAssets;

impl AssetSource for WFilesAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(data) = AppIcons.load(path)? {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = AppIcons.list(path)?;
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

#[cfg(test)]
mod asset_tests {
    use super::WFilesAssets;
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName;
    use std::fs;

    /// 源码里用到的图标变体名（去重，保持出现顺序）。
    ///
    /// 认的是「`IconName` 双冒号 + 标识符」这个形状，只收紧跟着的标识符字符，
    /// 所以 `IconName::House` 收 `House`，而后面跟的东西（括号、逗号）不进名字。
    fn referenced_icons() -> Vec<String> {
        let root = env!("CARGO_MANIFEST_DIR");
        let mut out: Vec<String> = Vec::new();
        for entry in fs::read_dir(format!("{root}/src")).expect("读不到 src/") {
            let path = entry.expect("扫 src/ 出错").path();
            // macOS 在外置卷上给每个文件配一个 `._xxx` 伴生文件（资源叉），
            // 它不是源码、也不是 UTF-8——不跳过的话这一读就崩。
            let file = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if file.starts_with("._") {
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let text = fs::read_to_string(&path).expect("源码文件读不到");
            let needle = "IconName::";
            for (at, _) in text.match_indices(needle) {
                let rest = &text[at + needle.len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if name.is_empty() || out.contains(&name) {
                    continue;
                }
                out.push(name);
            }
        }
        out
    }

    /// **用到什么图标，就得打包什么图标。**
    ///
    /// 2026-10-18 实测：源码里用了 53 个变体，其中 19 个既不在
    /// `gpui-kit-assets` 的默认 106 个里、也没写进 `main.rs` 的 `icon_assets!`
    /// ——它们在界面上是**一片空白**：`AssetSource::load` 报错，元素照画、
    /// 只是没有内容。中招的是工具栏的「新建文件夹 / 剪切 / 粘贴」、侧栏的
    /// 「个人文件夹 / 桌面 / 下载」、菜单里的「移到回收站」这些，一眼就看得出。
    ///
    /// 所以这条测试**扫源码**，不维护登记表：登记表一定会漏，而漏了没人知道
    /// ——界面上不崩不报，只是少一个图形。判据钉在真正渲染用的那个资产源
    /// （[`WFilesAssets`]）上，于是"忘了补进清单"和"清单里写错了名字"
    /// 这两件事同一条测试都管得住。
    #[test]
    fn every_referenced_icon_is_packed() {
        // 关联常量不是图标：这一条是全部变体那张表，测试自己要读它。
        const NOT_ICONS: &[&str] = &["ALL"];

        let referenced = referenced_icons();
        // 护栏：扫描器坏了会扫到 0 个，上面却什么都不报——「扫不到」和
        // 「都打包好了」长得一模一样。53 是 2026-10-18 实测数，以后只会更大。
        assert!(
            referenced.len() >= 53,
            "只扫到 {} 个图标，比实测的 53 个还少——扫描器多半坏了",
            referenced.len()
        );

        let mut missing = Vec::new();
        let mut unknown = Vec::new();
        for name in &referenced {
            if NOT_ICONS.contains(&name.as_str()) {
                continue;
            }
            let found = IconName::ALL
                .iter()
                .find(|icon| format!("{icon:?}") == *name);
            match found {
                None => unknown.push(name.clone()),
                Some(icon) => {
                    let packed = WFilesAssets
                        .load(&icon.path())
                        .ok()
                        .flatten()
                        .is_some_and(|bytes| !bytes.is_empty());
                    if !packed {
                        missing.push(format!("{name} -> {}", icon.path()));
                    }
                }
            }
        }

        assert!(
            unknown.is_empty(),
            "源码里出现了图标枚举里没有的变体名（扫描器扫错了？）：{unknown:?}"
        );
        assert!(
            missing.is_empty(),
            "这些图标用到了、但没打进二进制，界面上会是一片空白：\n  {}",
            missing.join("\n  ")
        );
    }
}
